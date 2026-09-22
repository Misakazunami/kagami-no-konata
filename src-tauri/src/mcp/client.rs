//! MCP 客户端（stdio 传输，JSON-RPC 2.0，按行分帧）
//!
//! 只实现真正需要的一小部分：`initialize` 握手、`tools/list`、`tools/call`。
//! 不实现 resources / prompts / 采样等其余能力——多一个未使用的协议面，
//! 就多一份需要审计的攻击面。
//!
//! **为什么用阻塞 I/O 而不是 `tokio::process`**：`tokio::process::Child` 的生命周期
//! 绑在创建它的 runtime 上，而 MCP 服务器要在**应用启动时**拉起（那时只有一个临时
//! runtime），临时 runtime 一销毁，子进程的管道就废了——这是实测踩到的坑。
//! stdio 传输本来就是"写一行、读一行"的阻塞协议，因此这里用 `std::process`
//! 加一条读线程 + 超时通道，任何 runtime 都能安全调用。
//!
//! 关键安全前提：**服务器命令由用户在 `config.json` 里写死**，模型永远无法
//! 添加、修改或启动一个服务器；环境变量只传用户为该服务器显式列出的项
//! （复用 `command_guard::sanitized_env` 的白名单，应用自己的密钥不会泄漏）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::types::McpServerConfig;

/// 客户端与服务器声明的协议版本
const PROTOCOL_VERSION: &str = "2024-11-05";
/// 单条响应的体积上限（防止一个恶意服务器把内存打满）
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// `tools/list` 最多翻多少页（防分页服务器无限循环）
const MAX_TOOL_PAGES: usize = 10;
/// 握手/列工具的超时（发生在启动路径上，必须短）
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// MCP 服务器暴露的一个工具
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// 一次工具调用的结果
#[derive(Debug, Clone)]
pub struct McpCallResult {
    pub text: String,
    pub is_error: bool,
}

/// 构造启动命令
///
/// Windows 上 `.cmd`/`.bat` 不是 PE 镜像，`CreateProcess` 无法直接执行
/// （`npx`、`uvx` 这类常见 MCP 入口都是 `.cmd` shim）。这里在解析到脚本宿主体
/// 时自动用 `cmd.exe /d /s /c` 包装；`command` 与 `args` 都来自用户写死的
/// 配置（模型永远改不了），因此拼接不构成注入面。
fn build_command(cfg: &McpServerConfig) -> Command {
    #[cfg(windows)]
    {
        if let Some(script) = resolve_windows_script(cfg.command.trim()) {
            let mut tail = format!("\"{}\"", script.display());
            for arg in &cfg.args {
                tail.push(' ');
                tail.push_str(&format!("\"{}\"", arg));
            }
            let mut cmd = Command::new(crate::agent::harness::command_guard::windows_cmd_exe());
            cmd.arg("/d").arg("/s").arg("/c").arg(format!("\"{}\"", tail));
            return cmd;
        }
    }
    let mut cmd = Command::new(cfg.command.trim());
    cmd.args(&cfg.args);
    cmd
}

/// Windows：把裸命令或 `.cmd`/`.bat` 解析成脚本路径；`.exe` 与内置程序返回 None
#[cfg(windows)]
fn resolve_windows_script(command: &str) -> Option<std::path::PathBuf> {
    let has_separator = command.contains('\\') || command.contains('/');
    let extension = std::path::Path::new(command)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());

    match extension.as_deref() {
        Some("cmd") | Some("bat") => {
            if has_separator {
                Some(std::path::PathBuf::from(command))
            } else {
                find_in_path(command)
            }
        }
        Some(_) => None,
        None if has_separator => None,
        // 无扩展名裸名：按 PATHEXT 找，只有命中 .cmd/.bat 才需要包装
        None => {
            let pathext = std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            pathext
                .split(';')
                .map(|ext| ext.trim().to_ascii_lowercase())
                .filter(|ext| ext == ".cmd" || ext == ".bat")
                .find_map(|ext| find_in_path(&format!("{}{}", command, ext)))
        }
    }
}

#[cfg(windows)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

pub struct McpClient {
    server_id: String,
    child: Child,
    /// 写线程入口：`stdin` 被移进线程，`write_all` 卡在满管道时不会把调用方
    /// 永远钉死在 `Mutex<McpClient>` 上（调用方超时后会 kill 子进程，
    /// 阻塞中的写随之获得 EPIPE 而退出）
    writes: mpsc::Sender<Vec<u8>>,
    writer: Option<JoinHandle<()>>,
    /// 读线程把子进程输出的每一行送进来（带超时地等它）；
    /// `Err` 表示这一行非法/超限（例如超过 1 MB），由调用方原样上报
    lines: Receiver<std::result::Result<String, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 进程由我们自己管：客户端没了就把服务器收掉，别留孤儿进程。
        // kill 之后管道两端关闭：写线程从阻塞的 write_all 返回并因 Sender
        // 随后被释放而退出，读线程拿到 EOF。
        let _ = self.child.kill();
        let _ = self.child.wait();
        // **绝不 join 这两个线程**：
        // - 读线程可能因孙进程（npx/uvx 包一层）仍持有 stdout 而永远等不到 EOF；
        // - 写线程要等 Sender 释放才结束，而 Drop 执行时字段尚未释放，join 必死锁。
        // 二者都 detach 掉：进程退出不会等它们。
        if let Some(reader) = self.reader.take() {
            drop(reader);
        }
        if let Some(writer) = self.writer.take() {
            drop(writer);
        }
    }
}

impl McpClient {
    /// 主动结束服务器进程（用户停用/取消信任时调用）
    ///
    /// 与 `Drop` 相同：kill 之后管道关闭，读写线程自然退出。之后这个客户端
    /// 不能再发请求（用户重新启用需要重启应用来重建连接，如实报错即可）。
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// 启动服务器并完成握手（阻塞，调用方负责放到后台线程或用启动路径执行）
    pub fn connect(cfg: &McpServerConfig) -> Result<Self> {
        if cfg.command.trim().is_empty() {
            anyhow::bail!("MCP 服务器「{}」没有配置启动命令", cfg.id);
        }

        let mut child = build_command(cfg)
            .env_clear()
            // 白名单环境变量 + 用户为该服务器显式配置的项
            .envs(crate::agent::harness::command_guard::sanitized_env())
            .envs(
                cfg.env
                    .iter()
                    .map(|item| (item.key.clone(), item.value.clone())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if std::env::var_os("KONATA_MCP_DEBUG").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .with_context(|| format!("启动 MCP 服务器「{}」失败（{}）", cfg.id, cfg.command))?;

        let stdin = child.stdin.take().context("MCP 服务器没有 stdin")?;
        let stdout = child.stdout.take().context("MCP 服务器没有 stdout")?;

        // 写线程：调用方只往 channel 里投递，永不阻塞在满管道上
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>();
        let writer = std::thread::spawn(move || {
            let mut stdin = stdin;
            for payload in write_rx {
                if stdin
                    .write_all(&payload)
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break; // 子进程已退出/管道已断
                }
            }
        });

        let (sender, lines) = mpsc::channel::<std::result::Result<String, String>>();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_bounded_line(&mut reader, MAX_RESPONSE_BYTES) {
                    Ok(Some(line)) => {
                        if sender.send(Ok(line)).is_err() {
                            break; // 客户端已销毁
                        }
                    }
                    Ok(None) => break, // 对端关闭
                    Err(e) => {
                        // 超限/IO 错误：如实上报给等待中的请求，然后收手
                        let _ = sender.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });

        let mut client = Self {
            server_id: cfg.id.clone(),
            child,
            writes: write_tx,
            writer: Some(writer),
            lines,
            reader: Some(reader),
            next_id: 1,
        };

        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "kagami-no-konata", "version": env!("CARGO_PKG_VERSION")}
            }),
            HANDSHAKE_TIMEOUT,
        )?;
        if init.get("serverInfo").is_none() && init.get("protocolVersion").is_none() {
            anyhow::bail!("MCP 服务器「{}」的 initialize 响应缺少 serverInfo", cfg.id);
        }
        client.notify("notifications/initialized", json!({}))?;

        Ok(client)
    }

    /// 列出服务器提供的工具（自动翻页，直到没有 `nextCursor`）
    pub fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let mut out: Vec<McpToolInfo> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("tools/list", params, HANDSHAKE_TIMEOUT)?;
            let tools = result
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for tool in tools {
                match serde_json::from_value::<McpToolInfo>(tool) {
                    Ok(info) if !info.name.is_empty() => out.push(info),
                    Ok(_) => eprintln!("[mcp] 忽略没有名字的工具（服务器 {}）", self.server_id),
                    Err(e) => {
                        eprintln!("[mcp] 忽略无法解析的工具声明（服务器 {}）：{}", self.server_id, e)
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .filter(|cursor| !cursor.is_empty())
                .map(|cursor| cursor.to_string());
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// 调用一个工具
    pub fn call_tool(
        &mut self,
        name: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<McpCallResult> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            timeout,
        )?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // content 是数组：只取文本项，其余（图片/资源）转成一句说明
        let mut parts: Vec<String> = Vec::new();
        if let Some(items) = result.get("content").and_then(|v| v.as_array()) {
            for item in items {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some(other) => parts.push(format!(
                        "（服务器返回了 {} 类型的内容，暂不支持展示）",
                        other
                    )),
                    None => {}
                }
            }
        }
        if parts.is_empty() {
            parts.push("（服务器没有返回文本内容）".to_string());
        }

        Ok(McpCallResult {
            text: parts.join("\n"),
            is_error,
        })
    }

    /// 发一条请求并按 id 等它的响应（期间跳过服务器的通知）
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&payload)?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!(
                    "MCP 服务器「{}」在 {} 秒内没有响应 {}",
                    self.server_id,
                    timeout.as_secs(),
                    method
                );
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(message)) => anyhow::bail!(
                    "MCP 服务器「{}」输出异常：{}",
                    self.server_id,
                    message
                ),
                Err(RecvTimeoutError::Timeout) => anyhow::bail!(
                    "MCP 服务器「{}」在 {} 秒内没有响应 {}",
                    self.server_id,
                    timeout.as_secs(),
                    method
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("MCP 服务器「{}」已退出", self.server_id)
                }
            };
            if line.len() > MAX_RESPONSE_BYTES {
                anyhow::bail!("MCP 服务器「{}」的单条响应过大", self.server_id);
            }
            let message: Value = serde_json::from_str(line.trim()).with_context(|| {
                format!(
                    "MCP 服务器「{}」输出了非 JSON 内容（前 200 字符：{}）",
                    self.server_id,
                    crate::agent::harness::truncate_text(line.trim(), 200).0
                )
            })?;

            // 通知（没有 id）在等响应时直接跳过
            if message.get("id").is_none() {
                continue;
            }
            if message.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
                let text = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("未知错误");
                anyhow::bail!(
                    "MCP 服务器「{}」返回错误 {}：{}",
                    self.server_id,
                    code,
                    text
                );
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send(&payload)
    }

    fn send(&mut self, payload: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(payload)?;
        line.push(b'\n');
        // 投递给写线程即返回：即使子进程不读 stdin、管道已满，也不会在这里
        // 永久阻塞（调用方的超时因此始终有效）
        self.writes.send(line).map_err(|_| {
            anyhow::anyhow!("向 MCP 服务器「{}」写入失败（写线程已退出）", self.server_id)
        })
    }
}

/// 有上限的按行读取（一次只保留一条线，超限立即报错）
///
/// `BufRead::lines` 会把整行（可能几百 MB）完整读进内存后才交给调用方，
/// 单条响应上限形同虚设；这里边读边计数，超过 `max` 立刻失败。
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            buf.extend_from_slice(&available[..position]);
            reader.consume(position + 1);
            break;
        }
        buf.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("单条响应超过 {} KB", max / 1024),
            ));
        }
    }
    if buf.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("单条响应超过 {} KB", max / 1024),
        ));
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_line_reader_splits_and_strips_crlf() {
        let data: &[u8] = b"line one\r\nline two\nlast";
        let mut reader = BufReader::new(Cursor::new(data));
        assert_eq!(
            read_bounded_line(&mut reader, 1024).unwrap().as_deref(),
            Some("line one")
        );
        assert_eq!(
            read_bounded_line(&mut reader, 1024).unwrap().as_deref(),
            Some("line two")
        );
        assert_eq!(
            read_bounded_line(&mut reader, 1024).unwrap().as_deref(),
            Some("last")
        );
        assert_eq!(read_bounded_line(&mut reader, 1024).unwrap(), None);
    }

    /// 单条超限必须在**读满之前**失败，而不是先把整行分配出来再检查
    #[test]
    fn bounded_line_reader_rejects_oversized_lines() {
        let mut data = vec![b'x'; MAX_RESPONSE_BYTES + 100];
        data.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(data));
        let err = read_bounded_line(&mut reader, MAX_RESPONSE_BYTES).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
