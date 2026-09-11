use anyhow::Result;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

use crate::agent::harness::command_guard::CommandGuard;
use crate::agent::harness::traits::{
    truncate_text, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};

use super::args;

/// 命令执行超时
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// 单条输出流的截断上限
const STREAM_LIMIT_BYTES: usize = 48 * 1024;
/// 参数个数上限
const MAX_ARGS: usize = 64;

/// 在工作区内执行外部命令
///
/// 安全边界（详见 `harness::command_guard`）：
/// 1. **不使用 shell**：直接以 argv 方式 spawn，杜绝 `;` `&&` `|` 等注入面；
/// 2. 硬黑名单优先于用户配置：`cmd` / `powershell` / `curl` / `rm` / `reg` 等
///    即使被写进允许列表也会被拦截；
/// 3. 解释器的求值参数（`python -c`、`node -e`）一律拒绝；
/// 4. 参数中出现的路径必须位于工作区内，敏感路径（`.ssh`、`config.json`）直接拒绝；
/// 5. 子进程只继承白名单环境变量，API Key / Token 不会泄漏；
/// 6. 超时或取消时子进程被强杀（`kill_on_drop`）。
pub struct RunCommand;

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "run_command",
            "执行命令",
            "在工作区内执行一个受信任的开发命令（如 cargo / git / node / python）。不接受 shell 语法，敏感命令（cmd、powershell、curl、rm、reg 等）会被强制拦截。执行前会请求用户批准。",
            Permission::Execute,
            json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "程序名（PATH 中可找到）或工作区内的相对路径，例如 cargo" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "参数数组，例如 [\"build\", \"--release\"]。不要传 shell 语法"
                    },
                    "cwd": { "type": "string", "description": "工作目录（必须位于工作区内）；省略表示默认工作区根目录" }
                },
                "required": ["program"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;

        let program = args::required_str(&args, "program")?;
        let raw_args: Vec<String> = match args.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                if items.len() > MAX_ARGS {
                    anyhow::bail!("参数过多（上限 {} 个）", MAX_ARGS);
                }
                items
                    .iter()
                    .map(|item| {
                        item.as_str().map(|s| s.to_string()).ok_or_else(|| {
                            anyhow::anyhow!("args 里每一项都必须是字符串")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            Some(_) => anyhow::bail!("args 必须是字符串数组"),
        };
        let cwd = args::optional_str(&args, "cwd");

        // 全部静态检查与路径解析都在 command_guard 内完成
        let guard = CommandGuard::from_allowlist(&cx.services.command_allowlist);
        let checked = guard
            .check(&program, &raw_args, cwd.as_deref(), &cx.services.workspaces)
            .map_err(|e| anyhow::anyhow!(e))?;

        cx.ensure_not_cancelled()?;

        let mut command = tokio::process::Command::new(&checked.program);
        command
            .args(&checked.args)
            .current_dir(&checked.cwd)
            .env_clear()
            .envs(checked.env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let outcome = tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await;

        let output = match outcome {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => anyhow::bail!("命令启动失败：{}", e),
            Err(_) => anyhow::bail!(
                "命令执行超时（{} 秒）已被终止：{}",
                COMMAND_TIMEOUT.as_secs(),
                checked.display
            ),
        };

        let (stdout, stdout_truncated) = truncate_text(
            &String::from_utf8_lossy(&output.stdout),
            STREAM_LIMIT_BYTES,
        );
        let (stderr, stderr_truncated) = truncate_text(
            &String::from_utf8_lossy(&output.stderr),
            STREAM_LIMIT_BYTES,
        );

        let exit_code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "被信号终止".to_string());

        let mut body = format!(
            "命令：{}\n工作目录：{}\n退出码：{}\n",
            checked.display,
            checked.cwd.display(),
            exit_code
        );
        if !stdout.trim().is_empty() {
            body.push_str("\n── stdout ──\n");
            body.push_str(&stdout);
            if stdout_truncated {
                body.push_str("\n（stdout 已截断）");
            }
        }
        if !stderr.trim().is_empty() {
            body.push_str("\n── stderr ──\n");
            body.push_str(&stderr);
            if stderr_truncated {
                body.push_str("\n（stderr 已截断）");
            }
        }
        if stdout.trim().is_empty() && stderr.trim().is_empty() {
            body.push_str("\n（命令没有任何输出）");
        }

        let preview = truncate_text(
            &format!("{} → 退出码 {}", checked.display, exit_code),
            300,
        )
        .0;

        let mut tool_output = ToolOutput::text(body).with_preview(preview);
        tool_output.truncated = stdout_truncated || stderr_truncated;
        Ok(tool_output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{
        DenyAllApprover, NullSink, ToolLimits, ToolServices,
    };
    use crate::config::types::{ToolConfig, ToolMode};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        sink: Arc<NullSink>,
        cancel: Arc<AtomicBool>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-sh-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let cfg = ToolConfig::with_single_root(
                &dir,
                true,
                "测试",
            );
            let set = WorkspaceSet::from_config(&cfg, &dir);
            Self {
                services: ToolServices::minimal(dir.clone(), set, ToolMode::Full),
                dir,
                sink: Arc::new(NullSink),
                cancel: Arc::new(AtomicBool::new(false)),
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: "s1",
                stream_id: "st1",
                step: 0,
                cancel: self.cancel.clone(),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: Duration::from_secs(30),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(DenyAllApprover),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

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
        let out = block_on(RunCommand.call(json!({"program": "echo", "args": ["hello"]}), &cx))
            .unwrap();
        assert!(out.content.contains("退出码：0"), "{}", out.content);
        assert!(out.content.contains("hello"));
    }

    #[test]
    fn refuses_absolute_path_argument_outside_workspace() {
        let fx = Fixture::new("outside");
        let cx = fx.ctx();
        let outside = if cfg!(windows) { "C:\\Windows\\win.ini" } else { "/etc/hosts" };
        let err = block_on(RunCommand.call(
            json!({"program": "cat", "args": [outside]}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("阻止"), "{err}");
    }
}
