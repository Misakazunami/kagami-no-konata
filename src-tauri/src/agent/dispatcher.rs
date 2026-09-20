use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use super::chat_agent::ChatAgent;
use super::router::{IntentRouter, RouteDecision};
use super::system_agent::SystemAgent;
use super::traits::{Agent, AgentContext, AgentResponse, StreamChunkCallback, StreamThinkingCallback};

/// 构造"人设包裹"用的转述指令
///
/// 刻意**不写任何具体角色名**：当前人格由 `messages[0]` 的人格提示词唯一决定，
/// 而这里再写死一个名字（历史实现写死的是"此方"）会在用户切换人格后产生
/// 两条互相冲突的指令（"你是小樱" vs "以此方的身份和语气"），导致模型串戏。
fn persona_wrap_hint(agent_name: &str, content: &str, previous: Option<&str>) -> String {
    let hint = format!(
        "【后台工具输出（来自{}）】：\n{}\n请不要直接朗读这段原文，而是保持你当前角色的身份、语气与口癖，\
         用自然的方式把关键信息转述给用户，不要变成生硬的技术报告。\
         注意：这只代表工具返回的文本，不要额外声称你执行了任何实际操作。",
        agent_name, content
    );
    match previous {
        Some(prev) => format!("{}\n{}", prev, hint),
        None => hint,
    }
}

/// Agent 调度器 —— 支持多智能体注册与意图判定路由
pub struct AgentDispatcher {
    chat_agent: Arc<ChatAgent>,
    agents: Arc<RwLock<HashMap<String, Arc<dyn Agent>>>>,
    router: Arc<RwLock<IntentRouter>>,
}

impl AgentDispatcher {
    pub fn new(chat_agent: ChatAgent) -> Self {
        let chat_arc = Arc::new(chat_agent);
        let system_arc = Arc::new(SystemAgent::new());

        let mut agents_map: HashMap<String, Arc<dyn Agent>> = HashMap::new();
        agents_map.insert(chat_arc.id().to_string(), chat_arc.clone());
        agents_map.insert(system_arc.id().to_string(), system_arc.clone());

        let manifests = agents_map.values().map(|a| a.manifest()).collect();
        let router = IntentRouter::new(manifests);

        Self {
            chat_agent: chat_arc,
            agents: Arc::new(RwLock::new(agents_map)),
            router: Arc::new(RwLock::new(router)),
        }
    }

    /// 注册额外的自定义 Agent
    pub fn register_agent(&self, agent: Arc<dyn Agent>) {
        let mut agents = self.agents.write().unwrap_or_else(|e| e.into_inner());
        agents.insert(agent.id().to_string(), agent);

        let manifests = agents.values().map(|a| a.manifest()).collect();
        let mut router = self.router.write().unwrap_or_else(|e| e.into_inner());
        router.update_manifests(manifests);
    }

    /// 获取 chat agent 的共享引用（用于更新配置等）
    pub fn chat_agent(&self) -> &ChatAgent {
        &self.chat_agent
    }

    /// 执行意图决策
    pub fn decide_route(&self, input: &str) -> RouteDecision {
        let router = self.router.read().unwrap_or_else(|e| e.into_inner());
        router.route(input)
    }

    /// 调度用户请求到对应 Agent（非流式）
    pub async fn dispatch(&self, ctx: &AgentContext) -> Result<AgentResponse> {
        let decision = self.decide_route(&ctx.user_input);

        let target_agent = {
            let agents = self.agents.read().unwrap_or_else(|e| e.into_inner());
            agents
                .get(&decision.target_agent)
                .cloned()
                .unwrap_or_else(|| self.chat_agent.clone())
        };

        // 如果需要由人设包装（例如桌面宠物风格回答指令结果）
        if decision.wrap_in_persona && decision.target_agent != "chat" {
            let mut sub_ctx = ctx.clone();
            sub_ctx.user_input = decision.clean_input;
            let sub_res = target_agent.handle(&sub_ctx).await?;

            let mut persona_ctx = ctx.clone();
            persona_ctx.system_hint = Some(persona_wrap_hint(
                target_agent.manifest().name.as_str(),
                sub_res.content.as_str(),
                ctx.system_hint.as_deref(),
            ));
            self.chat_agent.handle(&persona_ctx).await
        } else {
            let mut run_ctx = ctx.clone();
            run_ctx.user_input = decision.clean_input;
            target_agent.handle(&run_ctx).await
        }
    }

    /// 流式调度（支持 cancel、chunk 回调、thinking 回调以及人设包装）
    pub async fn dispatch_stream(
        &self,
        ctx: &AgentContext,
        cancel: Arc<AtomicBool>,
        on_chunk: impl Fn(&str) + Send + Sync + 'static,
        on_thinking: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<AgentResponse> {
        let decision = self.decide_route(&ctx.user_input);

        let target_agent = {
            let agents = self.agents.read().unwrap_or_else(|e| e.into_inner());
            agents
                .get(&decision.target_agent)
                .cloned()
                .unwrap_or_else(|| self.chat_agent.clone())
        };

        let on_chunk_box: StreamChunkCallback = Box::new(on_chunk);
        let on_thinking_box: StreamThinkingCallback = Box::new(on_thinking);

        if decision.wrap_in_persona && decision.target_agent != "chat" {
            // 在执行后台 Agent 前，通过思考流提供轻量进度提示，消除白屏等待感
            on_thinking_box(&format!(
                "[正在执行工具: {}（{}）...]\n",
                target_agent.manifest().name,
                decision.reason
            ));

            let mut sub_ctx = ctx.clone();
            sub_ctx.user_input = decision.clean_input;
            let sub_res = target_agent.handle(&sub_ctx).await?;

            let mut persona_ctx = ctx.clone();
            persona_ctx.system_hint = Some(persona_wrap_hint(
                target_agent.manifest().name.as_str(),
                sub_res.content.as_str(),
                ctx.system_hint.as_deref(),
            ));

            self.chat_agent
                .handle_stream(&persona_ctx, cancel, on_chunk_box, on_thinking_box)
                .await
        } else {
            let mut run_ctx = ctx.clone();
            run_ctx.user_input = decision.clean_input;
            target_agent
                .handle_stream(&run_ctx, cancel, on_chunk_box, on_thinking_box)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_hint_never_hardcodes_a_persona_name() {
        let hint = persona_wrap_hint("系统工具智能体", "当前本地时间：2025-01-01", None);
        assert!(hint.contains("系统工具智能体"));
        assert!(hint.contains("2025-01-01"));
        // 换人格后仍然成立的指令：不绑定任何具体角色名
        assert!(hint.contains("当前角色"), "{hint}");
        assert!(hint.contains("语气"), "{hint}");
        assert!(!hint.contains("此方"), "转述指令不得写死角色名：{hint}");
        assert!(!hint.contains("小樱"), "{hint}");
    }

    #[test]
    fn wrap_hint_forbids_faking_execution() {
        let hint = persona_wrap_hint("系统工具智能体", "只能查看时间", None);
        assert!(hint.contains("不要额外声称你执行了任何实际操作"), "{hint}");
        assert!(hint.contains("不要直接朗读"), "{hint}");
    }

    #[test]
    fn wrap_hint_preserves_existing_system_hint() {
        let hint = persona_wrap_hint("系统工具智能体", "内容", Some("【保持简短】"));
        assert!(hint.starts_with("【保持简短】\n"), "{hint}");
        assert!(hint.contains("内容"));
    }
}
