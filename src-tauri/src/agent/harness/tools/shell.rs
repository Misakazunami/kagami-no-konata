use anyhow::Result;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::harness::command_guard::{CheckedCommand, CommandGuard};
use crate::agent::harness::traits::{
    truncate_text, HeadTailBuffer, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
    ToolStatus, ToolStream, OUTPUT_HEAD_TAIL_BYTES,
};

use super::args;

/// 单条输出流的截断上限（头部 1/3 + 尾部 2/3，增量维护）
const STREAM_LIMIT_BYTES: usize = OUTPUT_HEAD_TAIL_BYTES;
/// 参数个数上限
const MAX_ARGS: usize = 64;
/// 一次调用里最多几步（`steps`）
const MAX_STEPS: usize = 5;
/// 状态轮询间隔：同时用于检测取消与超时，决定"停止"按钮的响应延迟
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 进程被杀掉后，等待读取任务收尾的时间上限
const READER_GRACE: Duration = Duration::from_secs(2);
/// UI 卡片预览长度上限
const PREVIEW_CHARS: usize = 300;

/// 在工作区内执行一个或多个受信任的开发命令
///
/// 安全边界（详见 `harness::command_guard`）：
/// 1. **不使用 shell**：直接以 argv 方式 spawn，杜绝 `;` `&&` `|` 等注入面。
///    需要连续跑多条命令时用 `steps`（严格按数组顺序串行执行），不要拼 shell 语法；
/// 2. 硬黑名单优先于用户配置：`cmd` / `powershell` / `curl` / `rm` / `reg` 等
///    即使被写进允许列表也会被拦截；
/// 3. 解释器的求值参数（`python -c`、`node -e`）一律拒绝；
/// 4. 参数中出现的路径必须位于工作区内，敏感路径（`.ssh`、`config.json`）直接拒绝；
/// 5. 子进程只继承白名单环境变量，API Key / Token 不会泄漏；
/// 6. 超时或取消时子进程被强杀（`kill_on_drop`），**已经收到的输出会被保留**
///    并标记成 `timeout` / `cancelled`，而不是整条调用报错。
pub struct RunCommand;

/// 解析出来的一步命令（尚未通过 command_guard）
struct PlannedStep {
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
}

impl PlannedStep {
    fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// 一步的执行结局
enum StepOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "run_command",
            "执行命令",
            "在工作区内执行受信任的开发命令（如 cargo / git / node / python）。不接受 shell 语法（没有管道、重定向、&&），需要连续执行多条时用 steps 数组；敏感命令（cmd、powershell、curl、rm、reg 等）会被强制拦截。执行前会请求用户批准。",
            Permission::Execute,
            json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "程序名（PATH 中可找到）或工作区内的相对路径，例如 cargo。与 steps 二选一" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "参数数组，例如 [\"build\", \"--release\"]。不要传 shell 语法"
                    },
                    "cwd": { "type": "string", "description": "工作目录（必须位于工作区内）；省略表示默认工作区根目录" },
                    "steps": {
                        "type": "array",
                        "maxItems": MAX_STEPS,
                        "description": format!("按顺序串行执行的多条命令（最多 {} 步，一次审批）。任一步被守卫拒绝则整批都不执行", MAX_STEPS),
                        "items": {
                            "type": "object",
                            "properties": {
                                "program": { "type": "string" },
                                "args": { "type": "array", "items": { "type": "string" } },
                                "cwd": { "type": "string" }
                            },
                            "required": ["program"],
                            "additionalProperties": false
                        }
                    },
                    "stop_on_error": {
                        "type": "boolean",
                        "description": "某一步退出码非零时是否中止后续步骤（默认 true）"
                    },
                    "max_output_lines": {
                        "type": "integer",
                        "description": "每一步只保留最后 N 行输出（1~2000），适合 cargo / git 这类噪声很大的命令；UI 的实时输出不受影响"
                    }
                },
                "additionalProperties": false
            }),
        )
    }

    /// 多步命令必须让用户一眼看清"到底要跑哪几条"，参数 JSON 做不到这点
    fn approval_summary(&self, args: &Value, _cx: &ToolCtx<'_>) -> Option<String> {
        let steps = parse_steps(args).ok()?;
        if steps.len() <= 1 {
            return None;
        }
        let lines: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(index, step)| format!("{}. {}", index + 1, step.display()))
            .collect();
        Some(format!(
            "将按顺序执行 {} 条命令：\n{}",
            steps.len(),
            lines.join("\n")
        ))
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;

        let steps = parse_steps(&args)?;
        let stop_on_error = args::optional_bool(&args, "stop_on_error", true);
        let max_output_lines = args::bounded_usize(&args, "max_output_lines", 0, 0, 2000);

        // ─── 先做完全部静态检查，再执行任何一步 ───
        //
        // 顺序在这里很关键：如果第 2 步会被守卫拒绝，那么第 1 步就不该被执行，
        // 否则用户看到的是"整批被拒绝"，副作用却已经发生了。
        let guard = CommandGuard::from_allowlist(&cx.services.command_allowlist);
        let default_cwd = args::optional_str(&args, "cwd");
        let mut checked: Vec<CheckedCommand> = Vec::with_capacity(steps.len());
        for (index, step) in steps.iter().enumerate() {
            let cwd = step.cwd.as_deref().or(default_cwd.as_deref());
            let result = guard
                .check(&step.program, &step.args, cwd, &cx.services.workspaces)
                .map_err(|e| {
                    if steps.len() > 1 {
                        anyhow::anyhow!("第 {} 步「{}」被拒绝：{}", index + 1, step.display(), e)
                    } else {
                        anyhow::anyhow!(e)
                    }
                })?;
            checked.push(result);
        }

        cx.ensure_not_cancelled()?;

        // 整个序列共享一个截止时间：配置里的超时是"这一次调用"的预算
        let deadline = Instant::now() + cx.limits.call_timeout;
        let mut sections: Vec<String> = Vec::new();
        let mut status = ToolStatus::Ok;
        let mut truncated = false;
        let mut last_exit_code = "0".to_string();
        let mut stop_note: Option<String> = None;

        for (index, command) in checked.iter().enumerate() {
            // 单条命令不加 step 头：保持与历史一致的紧凑输出（省 token 也更易读）
            let header = if checked.len() > 1 {
                format!(
                    "── step {}/{} ──\n$ {}\n工作目录：{}",
                    index + 1,
                    checked.len(),
                    command.display,
                    command.cwd.display()
                )
            } else {
                format!(
                    "命令：{}\n工作目录：{}",
                    command.display,
                    command.cwd.display()
                )
            };

            if Instant::now() >= deadline {
                status = ToolStatus::Timeout;
                sections.push(format!("{}\n退出码：（未执行）", header));
                stop_note = Some(format!(
                    "（本次调用的超时预算（{} 秒）已用尽，已跳过剩余 {} 步）",
                    cx.limits.call_timeout.as_secs(),
                    checked.len() - index
                ));
                break;
            }

            let (report, outcome, step_truncated) =
                run_step(command, cx, deadline, max_output_lines).await?;
            truncated |= step_truncated;

            let (step_status, exit_code) = match outcome {
                StepOutcome::Exited(exit_status) => {
                    let code = exit_status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "被信号终止".to_string());
                    let ok = exit_status.success();
                    (
                        if ok { ToolStatus::Ok } else { ToolStatus::Error },
                        code,
                    )
                }
                StepOutcome::TimedOut => (ToolStatus::Timeout, "超时".to_string()),
                StepOutcome::Cancelled => (ToolStatus::Cancelled, "已停止".to_string()),
            };
            last_exit_code = exit_code.clone();
            sections.push(format!("{}\n退出码：{}{}", header, exit_code, report));

            if step_status == ToolStatus::Ok {
                continue;
            }
            status = step_status;
            // 显式的"失败也继续"只对**退出码非零**有意义：
            // 超时/取消时继续跑后面的步骤毫无意义（时间预算已经没了/用户要停）
            if step_status == ToolStatus::Error && !stop_on_error {
                continue;
            }
            let remaining = checked.len() - index - 1;
            if remaining > 0 {
                stop_note = Some(format!(
                    "（因第 {} 步未成功，已跳过剩余 {} 步）",
                    index + 1,
                    remaining
                ));
            }
            break;
        }

        if let Some(note) = stop_note {
            sections.push(note);
        }

        let body = sections.join("\n\n");
        let preview = truncate_text(
            &format!(
                "{} → {}",
                steps
                    .iter()
                    .map(|s| s.display())
                    .collect::<Vec<_>>()
                    .join(" ; "),
                match status {
                    ToolStatus::Ok => format!("全部完成（退出码 {}）", last_exit_code),
                    _ => format!("{}（退出码 {}）", status.as_str(), last_exit_code),
                }
            ),
            PREVIEW_CHARS,
        )
        .0;

        let mut output = ToolOutput::text(body)
            .with_preview(preview)
            .with_status(status);
        output.truncated = truncated;
        Ok(output)
    }
}

/// 解析 `steps` 或单条命令形式；两者同时出现视为调用错误
fn parse_steps(args: &Value) -> Result<Vec<PlannedStep>> {
    let has_steps = matches!(args.get("steps"), Some(Value::Array(_)));
    let has_program = args
        .get("program")
        .map(|v| !v.is_null())
        .unwrap_or(false);

    if has_steps && has_program {
        anyhow::bail!("steps 与 program 不能同时提供：多步请只用 steps");
    }

    if has_steps {
        let items = args
            .get("steps")
            .and_then(|v| v.as_array())
            .expect("已确认是数组");
        if items.is_empty() {
            anyhow::bail!("steps 不能为空");
        }
        if items.len() > MAX_STEPS {
            anyhow::bail!("steps 步数过多（上限 {} 步）", MAX_STEPS);
        }
        let mut steps = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            if !item.is_object() {
                anyhow::bail!("steps[{}] 必须是对象", index);
            }
            let program = item
                .get("program")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("steps[{}] 缺少 program", index))?;
            steps.push(PlannedStep {
                program,
                args: parse_arg_list(item.get("args"), &format!("steps[{}].args", index))?,
                cwd: item
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }
        return Ok(steps);
    }

    let program = args::required_str(args, "program")?;
    Ok(vec![PlannedStep {
        program,
        args: parse_arg_list(args.get("args"), "args")?,
        cwd: args::optional_str(args, "cwd"),
    }])
}

fn parse_arg_list(value: Option<&Value>, field: &str) -> Result<Vec<String>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            if items.len() > MAX_ARGS {
                anyhow::bail!("参数过多（上限 {} 个）", MAX_ARGS);
            }
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| anyhow::anyhow!("{} 里每一项都必须是字符串", field))
                })
                .collect()
        }
        Some(_) => anyhow::bail!("{} 必须是字符串数组", field),
    }
}

/// 执行一步：流式转发输出、按截止时间/取消标志提前收手
///
/// 返回 `(文本报告, 结局, 是否被截断)`。stdout / stderr 都是**增量**收集的
/// （内存占用有界），并且已经通过 [`ToolStream`] 发给了 UI。
async fn run_step(
    command: &CheckedCommand,
    cx: &ToolCtx<'_>,
    deadline: Instant,
    max_output_lines: usize,
) -> Result<(String, StepOutcome, bool)> {
    let mut child = tokio::process::Command::new(&command.program);
    child
        .args(&command.args)
        .current_dir(&command.cwd)
        .env_clear()
        .envs(command.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = child
        .spawn()
        .map_err(|e| anyhow::anyhow!("命令启动失败（{}）：{}", command.program.display(), e))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let out_buf = Arc::new(Mutex::new(HeadTailBuffer::new(STREAM_LIMIT_BYTES)));
    let err_buf = Arc::new(Mutex::new(HeadTailBuffer::new(STREAM_LIMIT_BYTES)));

    let out_task = spawn_reader(
        stdout,
        out_buf.clone(),
        ToolStream::new(cx, "run_command", "stdout"),
    );
    let err_task = spawn_reader(
        stderr,
        err_buf.clone(),
        ToolStream::new(cx, "run_command", "stderr"),
    );

    // 轮询而不是 `timeout(child.wait())`：既要按截止时间收手，也要让"停止"按钮
    // 有 50ms 级的响应，还要能在收手前把已经收到的输出留在缓冲里
    let outcome = loop {
        if cx.cancelled() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(READER_GRACE, child.wait()).await;
            break StepOutcome::Cancelled;
        }
        match child.try_wait() {
            Ok(Some(status)) => break StepOutcome::Exited(status),
            Ok(None) => {}
            Err(e) => anyhow::bail!("等待命令退出失败：{}", e),
        }
        if Instant::now() >= deadline {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(READER_GRACE, child.wait()).await;
            break StepOutcome::TimedOut;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    // 进程已结束（或已被杀）：给读取任务一点时间把管道里剩下的字节读完。
    // 读不完也不能卡住整轮生成——缓冲是共享的，直接取快照即可。
    for task in [out_task, err_task] {
        let _ = tokio::time::timeout(READER_GRACE, task).await;
    }

    let (mut stdout_text, mut stdout_truncated) = snapshot(&out_buf);
    let (mut stderr_text, mut stderr_truncated) = snapshot(&err_buf);

    // 行数裁剪在最后做：cargo / git 的关键信息都在尾部，前几千行基本是噪音
    if max_output_lines > 0 {
        let (trimmed, was_trimmed) = HeadTailBuffer::tail_lines(&stdout_text, max_output_lines);
        stdout_text = trimmed;
        stdout_truncated |= was_trimmed;
        let (trimmed, was_trimmed) = HeadTailBuffer::tail_lines(&stderr_text, max_output_lines);
        stderr_text = trimmed;
        stderr_truncated |= was_trimmed;
    }

    let mut body = String::new();
    if !stdout_text.trim().is_empty() {
        body.push_str("\n── stdout ──\n");
        body.push_str(&stdout_text);
    }
    if !stderr_text.trim().is_empty() {
        body.push_str("\n── stderr ──\n");
        body.push_str(&stderr_text);
    }
    if stdout_text.trim().is_empty() && stderr_text.trim().is_empty() {
        body.push_str("\n（命令没有任何输出）");
    }

    Ok((body, outcome, stdout_truncated || stderr_truncated))
}

/// 逐行读取某个管道：既进增量缓冲，也发增量事件
fn spawn_reader<R>(
    pipe: Option<R>,
    buffer: Arc<Mutex<HeadTailBuffer>>,
    mut stream: ToolStream,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(pipe) = pipe else { return };
        let mut reader = BufReader::new(pipe);
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {}
                // 管道被关闭（例如子进程被杀）：已经读到的内容照常保留
                Err(_) => break,
            }
            // 非 UTF-8 输出不能整个丢掉：用 lossy 转换保住可读部分
            let raw = String::from_utf8_lossy(&buf);
            let line = raw.trim_end_matches(['\n', '\r']).to_string();
            if let Ok(mut guard) = buffer.lock() {
                guard.push_line(&line);
            }
            stream.push(&line);
            stream.push("\n");
        }
        stream.flush();
    })
}

/// 取缓冲快照（不等待仍在运行的读取任务）
fn snapshot(buffer: &Arc<Mutex<HeadTailBuffer>>) -> (String, bool) {
    match buffer.lock() {
        Ok(guard) => guard.clone().finish(),
        Err(poisoned) => poisoned.into_inner().clone().finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{
        DenyAllApprover, EventSink, ToolLimits, ToolServices, EVENT_TOOL_OUTPUT,
    };
    use crate::config::types::{ToolConfig, ToolMode, DEFAULT_COMMAND_ALLOWLIST};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 记录事件的 sink（用于断言流式输出事件）
    struct RecordingSink {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }

        fn payloads(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        sink: Arc<RecordingSink>,
        cancel: Arc<AtomicBool>,
        timeout: Duration,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("konata-sh-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            Self {
                services: ToolServices::minimal(dir.clone(), set, ToolMode::Full),
                dir,
                sink: RecordingSink::new(),
                cancel: Arc::new(AtomicBool::new(false)),
                timeout: Duration::from_secs(30),
            }
        }

        /// 允许列表里补上测试所需的程序（默认列表不含 false / sleep / seq）
        fn allow(&mut self, extra: &[&str]) {
            self.services.command_allowlist = DEFAULT_COMMAND_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .chain(extra.iter().map(|s| s.to_string()))
                .collect();
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: "s1",
                stream_id: "st1",
                call_id: "c1",
                step: 0,
                cancel: self.cancel.clone(),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: self.timeout,
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(DenyAllApprover),
            }
        }

        /// 写一个多行文件到工作区
        fn write_lines(&self, name: &str, count: usize) {
            let mut data = String::new();
            for i in 0..count {
                data.push_str(&format!("row-{:04}\n", i));
            }
            std::fs::write(self.dir.join(name), data).unwrap();
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    // ─── 静态检查（回归既有行为） ───

    #[test]
    fn blocks_sensitive_program_through_tool() {
        let fx = Fixture::new("sensitive");
        let cx = fx.ctx();
        let err = block_on(RunCommand.call(json!({"program": "cmd", "args": ["/c", "dir"]}), &cx))
            .unwrap_err();
        assert!(err.to_string().contains("敏感命令已被阻止"), "{err}");
    }

    #[test]
    fn blocks_shell_script_program() {
        let fx = Fixture::new("script");
        let cx = fx.ctx();
        let err = block_on(RunCommand.call(json!({"program": "cleanup.bat"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("敏感命令已被阻止"), "{err}");
    }

    #[test]
    fn blocks_python_eval_flag() {
        let fx = Fixture::new("pyeval");
        let cx = fx.ctx();
        let err = block_on(RunCommand.call(
            json!({"program": "python", "args": ["-c", "import os; os.system('x')"]}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("求值参数"), "{err}");
    }

    #[test]
    fn rejects_non_array_args() {
        let fx = Fixture::new("badargs");
        let cx = fx.ctx();
        let err = block_on(RunCommand.call(
            json!({"program": "cargo", "args": "build"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("字符串数组"), "{err}");
    }

    #[test]
    fn refuses_absolute_path_argument_outside_workspace() {
        let fx = Fixture::new("outside");
        let cx = fx.ctx();
        let outside = if cfg!(windows) {
            "C:\\Windows\\win.ini"
        } else {
            "/etc/hosts"
        };
        let err = block_on(RunCommand.call(
            json!({"program": "cat", "args": [outside]}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("阻止"), "{err}");
    }

    #[test]
    fn runs_allowed_command_when_available() {
        let fx = Fixture::new("run");
        let cx = fx.ctx();
        // 用宿主上一定存在的只读命令验证真实执行路径（echo 在允许列表内）
        let program = if cfg!(windows) { "cmd" } else { "echo" };
        if program == "cmd" {
            // Windows 下不执行 shell，直接验证拦截；真实执行留给集成环境
            assert!(block_on(RunCommand.call(json!({"program": program}), &cx)).is_err());
            return;
        }
        let out =
            block_on(RunCommand.call(json!({"program": "echo", "args": ["hello"]}), &cx)).unwrap();
        assert!(out.content.contains("退出码：0"), "{}", out.content);
        assert!(out.content.contains("hello"));
        assert_eq!(out.status, ToolStatus::Ok);
    }

    // ─── steps（多步命令） ───

    #[test]
    fn rejects_steps_and_program_together() {
        let fx = Fixture::new("both");
        let cx = fx.ctx();
        let err = block_on(RunCommand.call(
            json!({
                "program": "echo",
                "steps": [{"program": "echo", "args": ["a"]}]
            }),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("不能同时提供"), "{err}");
    }

    #[test]
    fn rejects_too_many_steps() {
        let fx = Fixture::new("toomany");
        let cx = fx.ctx();
        let steps: Vec<Value> = (0..MAX_STEPS + 1)
            .map(|_| json!({"program": "echo"}))
            .collect();
        let err = block_on(RunCommand.call(json!({ "steps": steps }), &cx)).unwrap_err();
        assert!(err.to_string().contains("步数过多"), "{err}");
    }

    #[test]
    fn approval_summary_lists_every_step() {
        let fx = Fixture::new("summary");
        let cx = fx.ctx();
        let summary = RunCommand
            .approval_summary(&json!({"steps": [
                {"program": "cargo", "args": ["build", "--release"]},
                {"program": "cargo", "args": ["test"]}
            ]}), &cx)
            .expect("多步命令必须给出摘要");
        assert!(summary.contains("2 条命令"), "{summary}");
        assert!(summary.contains("1. cargo build --release"), "{summary}");
        assert!(summary.contains("2. cargo test"), "{summary}");
        // 单步命令不需要额外摘要：参数 JSON 已经足够清楚
        assert!(RunCommand
            .approval_summary(&json!({"program": "cargo", "args": ["test"]}), &cx)
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn runs_multi_step_sequence_in_order() {
        let mut fx = Fixture::new("steps");
        fx.allow(&[]);
        let cx = fx.ctx();
        let out = block_on(RunCommand.call(
            json!({"steps": [
                {"program": "echo", "args": ["first"]},
                {"program": "echo", "args": ["second"]}
            ]}),
            &cx,
        ))
        .unwrap();
        assert_eq!(out.status, ToolStatus::Ok);
        assert!(out.content.contains("── step 1/2 ──"), "{}", out.content);
        assert!(out.content.contains("── step 2/2 ──"), "{}", out.content);
        let first = out.content.find("first").expect("step1 输出");
        let second = out.content.find("second").expect("step2 输出");
        assert!(first < second, "两步必须按顺序执行：{}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn multi_step_validates_every_step_before_running_any() {
        let mut fx = Fixture::new("precheck");
        fx.allow(&[]);
        let cx = fx.ctx();
        // 第一步合法、第二步命中硬黑名单：整批必须拒绝
        let err = block_on(RunCommand.call(
            json!({"steps": [
                {"program": "echo", "args": ["should-not-run"]},
                {"program": "rm", "args": ["-rf", "."]}
            ]}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("第 2 步"), "{err}");
        // 没有任何增量输出事件 = 第一步根本没被执行
        assert!(
            fx.sink.payloads(EVENT_TOOL_OUTPUT).is_empty(),
            "整批被拒绝时不得执行任何一步"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stops_after_failing_step_by_default() {
        let mut fx = Fixture::new("stoperr");
        fx.allow(&["false"]);
        let cx = fx.ctx();
        // false 必定退出码非零：默认 stop_on_error 下第二步不该执行
        let out = block_on(RunCommand.call(
            json!({"steps": [
                {"program": "false"},
                {"program": "echo", "args": ["never"]}
            ]}),
            &cx,
        ))
        .unwrap();
        assert_eq!(out.status, ToolStatus::Error);
        assert!(!out.content.contains("never"), "{}", out.content);
        assert!(out.content.contains("已跳过剩余 1 步"), "{}", out.content);

        // 显式 continue：第二步要执行，但整体状态仍是 error
        let out2 = block_on(RunCommand.call(
            json!({"steps": [
                {"program": "false"},
                {"program": "echo", "args": ["after"]}
            ], "stop_on_error": false}),
            &cx,
        ))
        .unwrap();
        assert_eq!(out2.status, ToolStatus::Error);
        assert!(out2.content.contains("after"), "{}", out2.content);
    }

    // ─── 输出裁剪与流式事件 ───

    #[cfg(unix)]
    #[test]
    fn truncates_long_output_keeping_head_and_tail() {
        let mut fx = Fixture::new("trunc");
        fx.allow(&[]);
        // 造一段远超上限的输出（16384 行 ≈ 200 KB）
        fx.write_lines("big.txt", 16384);

        let cx = fx.ctx();
        let out = block_on(RunCommand.call(json!({"program": "cat", "args": ["big.txt"]}), &cx))
            .unwrap();
        assert!(out.truncated, "超长输出必须标记截断");
        assert!(out.content.contains("已省略"), "{}", &out.content[..200.min(out.content.len())]);
        assert!(out.content.contains("row-0000"), "头部必须保留");
        assert!(out.content.contains("row-16383"), "尾部必须保留");
        assert!(
            out.content.len() < 200 * 1024,
            "增量缓冲必须把内存限制在合理范围，实际 {}",
            out.content.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_output_lines_keeps_tail_only() {
        let mut fx = Fixture::new("lines");
        fx.allow(&[]);
        fx.write_lines("lines.txt", 300);
        let cx = fx.ctx();
        let out = block_on(RunCommand.call(
            json!({"program": "cat", "args": ["lines.txt"], "max_output_lines": 10}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("已省略前 290 行"), "{}", out.content);
        assert!(out.content.contains("row-0299"), "{}", out.content);
        // 只保留最后 10 行 → 更早的行（含 row-0289）必须已被丢掉
        assert!(!out.content.contains("row-0289"), "{}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn streams_output_chunks_with_full_ids() {
        let mut fx = Fixture::new("stream");
        fx.allow(&[]);
        let cx = fx.ctx();
        let out =
            block_on(RunCommand.call(json!({"program": "echo", "args": ["streamed-line"]}), &cx))
                .unwrap();
        assert_eq!(out.status, ToolStatus::Ok);

        let chunks = fx.sink.payloads(EVENT_TOOL_OUTPUT);
        assert!(!chunks.is_empty(), "至少要有一条流式输出事件");
        let joined: String = chunks
            .iter()
            .map(|c| c["data"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(joined.contains("streamed-line"), "{joined}");
        for chunk in &chunks {
            assert_eq!(chunk["session_id"], "s1");
            assert_eq!(chunk["stream_id"], "st1");
            assert_eq!(chunk["call_id"], "c1");
            assert_eq!(chunk["stream"], "stdout");
        }
    }

    // ─── 超时与取消 ───

    #[cfg(unix)]
    #[test]
    fn timeout_returns_partial_output_instead_of_error() {
        let mut fx = Fixture::new("timeout");
        fx.allow(&["sleep"]);
        fx.timeout = Duration::from_millis(300);
        let cx = fx.ctx();
        let started = Instant::now();
        let out = block_on(RunCommand.call(
            json!({"program": "sleep", "args": ["5"]}),
            &cx,
        ))
        .unwrap();
        assert_eq!(out.status, ToolStatus::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "必须按配置的超时收手，实际 {:?}",
            started.elapsed()
        );
        assert!(out.content.contains("超时"), "{}", out.content);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_marks_status_and_keeps_partial_output() {
        let mut fx = Fixture::new("cancel");
        fx.allow(&["sleep"]);
        let fx = fx;
        let cx = fx.ctx();
        // 取消标志在调用进行中置位：轮询循环必须在 50ms 级响应并保留已有输出
        let cancel = fx.cancel.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime.block_on(async {
            let call = RunCommand.call(json!({"program": "sleep", "args": ["5"]}), &cx);
            let killer = async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                cancel.store(true, Ordering::Relaxed);
            };
            let (result, _) = tokio::join!(call, killer);
            result.expect("取消不应变成调用错误")
        });
        assert_eq!(out.status, ToolStatus::Cancelled);
        assert!(out.content.contains("已停止"), "{}", out.content);
    }
}
