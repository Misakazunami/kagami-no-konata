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
use std::process::{Child, ChildStdin, Command, Stdio};
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

pub struct McpClient {
    server_id: String,
    child: Child,
    stdin: ChildStdin,
    /// 读线程把子进程输出的每一行送进来（带超时地等它）
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 进程由我们自己管：客户端没了就把服务器收掉，别留孤儿进程
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            // 读线程会因为管道关闭而自然结束，不强求 join 成功
            let _ = reader.join();
        }
    }
}

impl McpClient {
    /// 启动服务器并完成握手（阻塞，调用方负责放到后台线程或用启动路径执行）
    pub fn connect(cfg: &McpServerConfig) -> Result<Self> {
        if cfg.command.trim().is_empty() {
            anyhow::bail!("MCP 服务器「{}」没有配置启动命令", cfg.id);
        }

        let mut child = Command::new(cfg.command.trim())
            .args(&cfg.args)
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

        let (sender, lines) = mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break; // 客户端已销毁
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut client = Self {
            server_id: cfg.id.clone(),
            child,
            stdin,
            lines,
            reader: Some(reader),
            next_id: 1,
        };

        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "KonataMirror", "version": env!("CARGO_PKG_VERSION")}
            }),
            HANDSHAKE_TIMEOUT,
        )?;
        if init.get("serverInfo").is_none() && init.get("protocolVersion").is_none() {
            anyhow::bail!("MCP 服务器「{}」的 initialize 响应缺少 serverInfo", cfg.id);
        }
        client.notify("notifications/initialized", json!({}))?;

        Ok(client)
    }

    /// 列出服务器提供的工具
    pub fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", json!({}), HANDSHAKE_TIMEOUT)?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(tools.len());
        for tool in tools {
            match serde_json::from_value::<McpToolInfo>(tool) {
                Ok(info) if !info.name.is_empty() => out.push(info),
                Ok(_) => eprintln!("[mcp] 忽略没有名字的工具（服务器 {}）", self.server_id),
                Err(e) => {
                    eprintln!("[mcp] 忽略无法解析的工具声明（服务器 {}）：{}", self.server_id, e)
                }
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
                Ok(line) => line,
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
        self.stdin
            .write_all(&line)
            .with_context(|| format!("向 MCP 服务器「{}」写入失败", self.server_id))?;
        self.stdin
            .flush()
            .with_context(|| format!("刷新 MCP 服务器「{}」输入失败", self.server_id))?;
        Ok(())
    }
}
