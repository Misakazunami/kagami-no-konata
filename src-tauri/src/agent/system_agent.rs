use anyhow::Result;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use super::harness::{Tool, ToolCtx, ToolRegistry, ToolRuntime};
use super::traits::{
    Agent, AgentContext, AgentManifest, AgentResponse, Capability, StreamChunkCallback,
    StreamThinkingCallback,
};

/// `/sys` 指令可用的只读工具集合（进程内构建一次）
fn sys_registry() -> &'static ToolRegistry {
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let full = super::harness::tools::builtin_registry();
        let read_only: Vec<Arc<dyn Tool>> = full
            .infos(crate::config::types::ToolMode::Full)
            .into_iter()
            .filter(|info| info.read_only)
            .filter_map(|info| full.find(&info.name, crate::config::types::ToolMode::Full))
            .collect();
        ToolRegistry::new(read_only)
    })
}

/// 系统工具 Agent —— 处理 `/sys` 等显式指令
///
/// 与历史实现的区别：这里**真的调用工具**（`get_current_time` /
/// `get_system_info`），而不是返回"已收到指令"这类假装执行的模板文本。
/// 悬浮窗（`ctx.tools` 为 `None`）下只能回答时间，其余明确告知不可用。
pub struct SystemAgent;

impl SystemAgent {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemAgent {
    fn default() -> Self {
        Self::new()
    }
}

/// 挑选与该指令最匹配的只读工具
fn pick_tool(input: &str) -> &'static str {
    let text = input.trim();
    if text.contains("时间") || text.contains("几点") || text.contains("日期") {
        "get_current_time"
    } else if text.contains("状态") || text.contains("会话") || text.contains("记忆条数") {
        "get_app_status"
    } else {
        "get_system_info"
    }
}

#[async_trait::async_trait]
impl Agent for SystemAgent {
    fn id(&self) -> &str {
        "system"
    }

    fn manifest(&self) -> AgentManifest {
        AgentManifest {
            id: "system".to_string(),
            name: "系统工具智能体".to_string(),
            description: "处理 /sys 等显式指令与系统状态查看".to_string(),
            prefix_commands: vec!["/sys".to_string(), "/system".to_string()],
            trigger_keywords: vec!["系统信息".to_string(), "系统状态".to_string()],
            // 不再用 `^打开.*` / `^启动.*` 抢占普通聊天：
            // 该 Agent 并不具备启动应用的能力，抢答只会返回模板化的假回答。
            regex_patterns: vec![r"^查看\s*(系统信息|系统状态|内存|cpu)".to_string()],
            // 默认由当前人格包装说话（保持当前角色人设，不绑定具体角色名）
            wrap_in_persona: true,
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::TaskExecution]
    }

    async fn handle(&self, ctx: &AgentContext) -> Result<AgentResponse> {
        let runtime: Option<&ToolRuntime> = ctx.tools.as_ref().filter(|r| r.enabled);

        // 悬浮窗等纯对话链路：只保留"看时间"这一条无需工具的能力
        let Some(runtime) = runtime else {
            if pick_tool(&ctx.user_input) == "get_current_time" {
                let now = chrono::Local::now();
                return Ok(AgentResponse::text(format!(
                    "当前本地系统时间为：{}。",
                    now.format("%Y-%m-%d %H:%M:%S")
                )));
            }
            return Ok(AgentResponse::text(
                "当前窗口只能聊天，系统类指令请在主窗口使用。".to_string(),
            ));
        };

        let name = pick_tool(&ctx.user_input);
        let Some(tool) = sys_registry().find(name, runtime.services.mode) else {
            return Ok(AgentResponse::text(format!(
                "当前模式下无法执行系统指令（{} 不可用）。",
                name
            )));
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let cx = ToolCtx {
            session_id: &ctx.session_id,
            stream_id: &ctx.stream_id,
            step: 0,
            cancel,
            services: &runtime.services,
            limits: runtime.limits,
            emit: runtime.emit.clone(),
            approver: runtime.approver.clone(),
        };

        match tool.call(json!({}), &cx).await {
            Ok(output) => Ok(AgentResponse::text(output.content)),
            Err(e) => Ok(AgentResponse::text(format!(
                "系统指令执行失败：{}",
                e
            ))),
        }
    }

    async fn handle_stream(
        &self,
        ctx: &AgentContext,
        _cancel: Arc<AtomicBool>,
        on_chunk: StreamChunkCallback,
        _on_thinking: StreamThinkingCallback,
    ) -> Result<AgentResponse> {
        let response = self.handle(ctx).await?;
        on_chunk(&response.content);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_expected_tools() {
        assert_eq!(pick_tool("现在几点了"), "get_current_time");
        assert_eq!(pick_tool("/sys 时间"), "get_current_time");
        assert_eq!(pick_tool("系统状态"), "get_app_status");
        assert_eq!(pick_tool("内存占用"), "get_system_info");
    }

    #[test]
    fn sys_registry_is_read_only() {
        let registry = sys_registry();
        assert!(registry.find("get_current_time", crate::config::types::ToolMode::Full).is_some());
        assert!(
            registry.find("write_file", crate::config::types::ToolMode::Full).is_none(),
            "系统指令不得触碰写入类工具"
        );
        assert!(registry.find("run_command", crate::config::types::ToolMode::Full).is_none());
    }
}
