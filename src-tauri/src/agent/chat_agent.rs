use anyhow::Result;
use chrono::Datelike;
use futures::StreamExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use crate::llm::backend::ChatBackend;
use crate::llm::proxy::LlmProxy;
use crate::llm::types::{LlmMessage, StreamChunk};
use crate::persona::engine::PersonaEngine;
use crate::store::memory_store::MemoryType;

use super::harness::{tool_usage_rules, HarnessRun, ToolRegistry, ToolRuntime};
use super::traits::{
    Agent, AgentContext, AgentManifest, AgentResponse, Capability, StreamChunkCallback,
    StreamThinkingCallback,
};

/// 剥离文本中的 `<think>...</think>` 标签及其内容
pub fn strip_think_tags(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    let mut in_think = false;

    loop {
        if !in_think {
            if let Some(start) = remaining.find("<think>") {
                result.push_str(&remaining[..start]);
                remaining = &remaining[start + 7..];
                in_think = true;
            } else {
                result.push_str(remaining);
                break;
            }
        } else {
            if let Some(end) = remaining.find("</think>") {
                remaining = &remaining[end + 8..];
                in_think = false;
            } else {
                // 未闭合的思考标签，丢弃剩余内容
                break;
            }
        }
    }

    result
}

/// 防御性去重：若历史末尾是与当前输入内容相同的 User 消息，则截掉该条
///
/// 场景：调用方先落库用户消息再取最近 N 条历史时，末尾即为当前输入本身，
/// 若不处理，build_messages 会把它再追加一次，LLM 收到两遍相同输入。
pub fn dedupe_trailing_input<'a>(
    conversation: &'a [crate::agent::context::Message],
    user_input: &str,
) -> &'a [crate::agent::context::Message] {
    match conversation.last() {
        Some(last)
            if last.role == crate::agent::context::Role::User
                && strip_think_tags(&last.content) == user_input =>
        {
            &conversation[..conversation.len() - 1]
        }
        _ => conversation,
    }
}

/// 判断错误是否属于"该提供商不支持 tool calling"
///
/// 项目允许用户填写任意 OpenAI 兼容端点，其中不少会直接 400/422 拒绝带
/// `tools` 字段的请求。此时若直接失败，用户会看到"生成失败"而完全无法对话，
/// 因此识别出这种情况后自动降级为纯对话重试一次。
fn is_tools_unsupported(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    let mentions_tools = text.contains("tool") || text.contains("function");
    let looks_like_rejection = text.contains("400")
        || text.contains("404")
        || text.contains("422")
        || text.contains("unsupported")
        || text.contains("not support")
        || text.contains("不支持")
        || text.contains("invalid");
    mentions_tools && looks_like_rejection
}

/// 步数耗尽是"模型绕圈子"的信号，留一条诊断日志便于排查
fn log_step_limit(outcome: &crate::agent::harness::HarnessOutcome) {
    if outcome.hit_step_limit {
        eprintln!(
            "[harness] 工具步数用尽（已执行 {} 轮），最后一轮已强制不带工具收尾",
            outcome.steps
        );
    }
}

/// 聊天 Agent —— 核心对话实现
///
/// 不可变共享（Send + Sync）：LLM 配置与人格引擎均为内部可变，
/// 可在多个并发请求间安全复用。
pub struct ChatAgent {
    llm: Arc<LlmProxy>,
    /// 面向工具循环的后端抽象（与 `llm` 是同一个对象，仅类型不同）
    backend: Arc<dyn ChatBackend>,
    personas: Arc<RwLock<PersonaEngine>>,
    /// 内置工具集合（无状态，可跨请求共享）
    tools: Arc<ToolRegistry>,
}

impl ChatAgent {
    pub fn new(
        llm: LlmProxy,
        personas: Arc<RwLock<PersonaEngine>>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let llm = Arc::new(llm);
        let backend: Arc<dyn ChatBackend> = llm.clone();
        Self {
            llm,
            backend,
            personas,
            tools,
        }
    }

    /// 直接注入后端（单元测试用；生产走 `new`）
    #[allow(dead_code)]
    pub fn with_backend(
        backend: Arc<dyn ChatBackend>,
        personas: Arc<RwLock<PersonaEngine>>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let llm = Arc::new(LlmProxy::new(
            crate::config::types::LlmConfig::default().active_provider(),
        ));
        Self {
            llm,
            backend,
            personas,
            tools,
        }
    }

    /// 工具注册表（供命令层展示工具清单）
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// 热更新 LLM 提供商配置
    pub fn update_provider(&self, provider: &crate::config::types::LlmProvider) {
        self.llm.update_provider(provider);
    }

    /// 从磁盘重新加载所有人格（内置 + 用户自定义）
    ///
    /// 加载失败时保留旧配置并在控制台输出错误，不再静默吞错。
    pub fn reload_personas(&self, app_data_dir: &Path) {
        match PersonaEngine::load_all(app_data_dir) {
            Ok(engine) => {
                let mut guard = self.personas.write().unwrap_or_else(|e| e.into_inner());
                *guard = engine;
            }
            Err(e) => {
                eprintln!("[persona] 重载人格失败，保留旧配置: {}", e);
            }
        }
    }

    /// 组装 LLM 消息列表
    ///
    /// 历史已由调用方截断为最近 N 条；更早内容以持久化摘要形式注入 system prompt。
    fn build_messages(&self, ctx: &AgentContext) -> Result<Vec<LlmMessage>> {
        let mut messages = Vec::new();

        // 1. System prompt（人设或任务模式注入）
        let mut system_prompt = if ctx.session_type == "task" {
            let persona_name = {
                let engine = self.personas.read().unwrap_or_else(|e| e.into_inner());
                engine.get_persona(&ctx.persona_id).map(|p| p.name.clone())
            };
            super::task_prompt::build_task_system_prompt(
                &ctx.task_mode,
                persona_name.as_deref(),
                &ctx.user_nickname,
            )
        } else {
            let engine = self.personas.read().unwrap_or_else(|e| e.into_inner());
            engine.build_system_prompt(
                &ctx.persona_id,
                &ctx.user_nickname,
                ctx.user_info.as_ref(),
            )?
        };

        // 注入当前时间（星期使用中文，避免 %A 输出英文与提示词不协调）
        let now = chrono::Local::now();
        let weekday_cn = match now.weekday() {
            chrono::Weekday::Mon => "星期一",
            chrono::Weekday::Tue => "星期二",
            chrono::Weekday::Wed => "星期三",
            chrono::Weekday::Thu => "星期四",
            chrono::Weekday::Fri => "星期五",
            chrono::Weekday::Sat => "星期六",
            chrono::Weekday::Sun => "星期日",
        };
        system_prompt.push_str(&format!(
            "\n\n【当前时间】{} {}（{}）",
            now.format("%Y年%m月%d日"),
            now.format("%H:%M"),
            weekday_cn,
        ));

        // 注入持久化的会话摘要（早期对话）
        if let Some(summary) = ctx.context_summary.as_deref() {
            if !summary.trim().is_empty() {
                system_prompt.push_str(
                    "\n\n【之前的对话摘要】\n（以下是更早的对话总结，请参考上下文连贯性）\n",
                );
                system_prompt.push_str(summary);
                system_prompt.push('\n');
            }
        }

        // 注入相关记忆
        if !ctx.retrieved_memories.is_empty() {
            system_prompt.push_str("\n\n【关于用户的记忆】\n");
            system_prompt.push_str("（以下是你之前了解到的关于用户的信息，请自然地融入对话中，不要刻意提及）\n");
            for mem in &ctx.retrieved_memories {
                let type_label = match mem.memory_type {
                    MemoryType::Fact => "事实",
                    MemoryType::Preference => "偏好",
                    MemoryType::Experience => "经历",
                    MemoryType::Emotional => "情感",
                };
                system_prompt.push_str(&format!("- {}（{}）\n", mem.content, type_label));
            }
        }

        // 2. 工具使用规则（仅在使用工具时注入；纯对话链路保持原样）
        if let Some(runtime) = ctx.tools.as_ref().filter(|r| r.enabled) {
            system_prompt.push_str(&tool_usage_rules(runtime, &self.tools));

            // 3. 会话级任务计划：放在规则之后、system prompt 的最后一块，
            //    让"做到哪一步了"成为模型动手前看到的最后一条信息。
            //    计划只属于有工具的主窗口链路——悬浮窗保持与接入工具前完全一致。
            if let Some(section) = ctx.plan.as_ref().and_then(|plan| plan.prompt_section()) {
                system_prompt.push_str(&section);
            }

            // 4. 工作记忆：模型自己记下的跨轮结论，整段带 untrusted 标记。
            //    放在最后是因为它最"次要"——真值判断仍以本轮工具结果为准。
            if let Some(section) = crate::agent::notes::prompt_section(&ctx.notes) {
                system_prompt.push_str(&section);
            }
        }

        messages.push(LlmMessage::system(system_prompt));

        // 3. 最近历史消息（剥离思考标签）
        //    防御性兜底：若调用方取历史时已包含当前输入（末尾 User 消息与
        //    user_input 相同），跳过该条，避免当前消息被重复注入两次
        let history = dedupe_trailing_input(&ctx.conversation, &ctx.user_input);
        for msg in history {
            messages.push(LlmMessage {
                role: msg.role.to_string(),
                content: strip_think_tags(&msg.content),
                ..Default::default()
            });
        }

        // 4. 当前用户输入（如有 system_hint 则附加到 LLM 输入中，不存储）
        let llm_user_input = match &ctx.system_hint {
            Some(hint) if !hint.is_empty() => format!("{}\n{}", ctx.user_input, hint),
            _ => ctx.user_input.clone(),
        };
        messages.push(LlmMessage::user(llm_user_input));

        Ok(messages)
    }

    /// 解析本轮生成使用的后端
    ///
    /// 返回 `(主轮次后端, 子代理模型池)`：
    /// - `ctx.models` 为空（未做会话级选择）→ 复用共享后端，子代理池里只有一个
    ///   无名条目（即"与主轮次同模型"，与未引入模型路由时完全一致）；
    /// - 有会话级方案 → 每个模型一个**请求级** backend，子代理在池上轮转。
    fn backends_for(
        &self,
        ctx: &AgentContext,
    ) -> (Arc<dyn ChatBackend>, Vec<crate::llm::router::ChildModel>) {
        match ctx.models.as_deref() {
            Some(plan) => (plan.main_backend(), plan.child_models()),
            None => (
                self.backend.clone(),
                vec![crate::llm::router::ChildModel::new(
                    self.backend.clone(),
                    String::new(),
                )],
            ),
        }
    }

    /// 组装一次工具循环运行所需的参数
    fn harness_run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        runtime: &'a ToolRuntime,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        main_backend: &'a Arc<dyn ChatBackend>,
        child_models: Vec<crate::llm::router::ChildModel>,
    ) -> HarnessRun<'a> {
        // 服务句柄按值克隆：只读子代理要在"父级服务的只读克隆"上再跑一次循环，
        // 因此这里给父级挂上子代理运行时（子代理自己拿不到它 → 深度恒为 1 层）
        let mut services = runtime.services.clone();
        // 子代理模型池 = 子模型池；为空时退化为"与主轮次同一个后端"
        // （手动模式与未做选择时都是这条路径，行为与接入模型路由前一致）
        let child_models = if child_models.is_empty() {
            vec![crate::llm::router::ChildModel::new(
                main_backend.clone(),
                String::new(),
            )]
        } else {
            child_models
        };
        services.subagent = Some(Arc::new(
            crate::agent::harness::subagent::AgentRuntime::with_models(
                child_models,
                runtime.subagent_max_children,
            )
            .with_budgets(runtime.subagent_steps, runtime.subagent_max_tasks),
        ));

        HarnessRun {
            backend: main_backend.as_ref(),
            registry: &self.tools,
            services,
            session_id: &ctx.session_id,
            stream_id: &ctx.stream_id,
            cancel,
            emit: runtime.emit.clone(),
            approver: runtime.approver.clone(),
            tools_enabled: runtime.enabled,
            auto_approve: runtime.auto_approve.clone(),
            auto_approve_all: runtime.auto_approve_all,
            limits: runtime.limits,
            max_steps: runtime.max_steps,
        }
    }
}

#[async_trait::async_trait]
impl Agent for ChatAgent {
    fn id(&self) -> &str {
        "chat"
    }

    fn manifest(&self) -> AgentManifest {
        AgentManifest {
            id: "chat".to_string(),
            name: "聊天智能体".to_string(),
            description: "默认对话助手，处理闲聊、问答与角色扮演".to_string(),
            prefix_commands: vec![],
            trigger_keywords: vec![],
            regex_patterns: vec![],
            wrap_in_persona: false,
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Chat]
    }

    async fn handle(&self, ctx: &AgentContext) -> Result<AgentResponse> {
        let messages = self.build_messages(ctx)?;
        let (main_backend, child_models) = self.backends_for(ctx);

        // 启用工具时走工具循环（无回调：只取最终正文与轨迹）
        if let Some(runtime) = ctx.tools.as_ref().filter(|r| r.enabled) {
            let cancel = Arc::new(AtomicBool::new(false));
            let run = self.harness_run(ctx, runtime, cancel, &main_backend, child_models);
            match run.execute(messages, |_| {}, |_| {}).await {
                Ok(outcome) => {
                    log_step_limit(&outcome);
                    return Ok(AgentResponse::text(outcome.content)
                        .with_invocations(outcome.invocations)
                        .with_extra_tokens(outcome.extra_tokens)
                        .with_step_limit(outcome.hit_step_limit, outcome.steps));
                }
                Err(e) if is_tools_unsupported(&e) => {
                    eprintln!("[harness] 提供商不支持工具调用，降级为纯对话：{}", e);
                    let plain = plain_context(ctx);
                    let messages = self.build_messages(&plain)?;
                    return self.plain_stream_response(&main_backend, messages, None).await;
                }
                Err(e) => return Err(e),
            }
        }

        let stream = main_backend.chat_stream(messages, None).await?;
        let mut full_response = String::new();

        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            match chunk? {
                StreamChunk::Content(text) => full_response.push_str(&text),
                StreamChunk::Thinking(_) => {} // 非流式模式下忽略思考内容
                StreamChunk::ToolCallDelta(_) => {} // 未启用工具时不会出现
                StreamChunk::Finish(_) => {} // 结束原因只对工具循环有意义
            }
        }

        Ok(AgentResponse::text(full_response))
    }

    async fn handle_stream(
        &self,
        ctx: &AgentContext,
        cancel: Arc<AtomicBool>,
        on_chunk: StreamChunkCallback,
        on_thinking: StreamThinkingCallback,
    ) -> Result<AgentResponse> {
        let messages = self.build_messages(ctx)?;
        let (main_backend, child_models) = self.backends_for(ctx);

        // 启用工具时走工具循环；悬浮窗（ctx.tools = None）保持原有纯对话路径
        if let Some(runtime) = ctx.tools.as_ref().filter(|r| r.enabled) {
            let run = self.harness_run(ctx, runtime, cancel.clone(), &main_backend, child_models);
            let on_chunk_ref: &(dyn Fn(&str) + Send + Sync) = &|text| on_chunk(text);
            let on_thinking_ref: &(dyn Fn(&str) + Send + Sync) = &|text| on_thinking(text);
            match run.execute(messages, on_chunk_ref, on_thinking_ref).await {
                Ok(outcome) => {
                    log_step_limit(&outcome);
                    return Ok(AgentResponse::text(outcome.content)
                        .with_invocations(outcome.invocations)
                        .with_extra_tokens(outcome.extra_tokens)
                        .with_step_limit(outcome.hit_step_limit, outcome.steps));
                }
                Err(e) if is_tools_unsupported(&e) => {
                    // 已经外发过一部分正文的极端情况下也不重复输出：
                    // 这里的错误发生在请求建立阶段，尚未产生任何正文
                    eprintln!("[harness] 提供商不支持工具调用，降级为纯对话：{}", e);
                    let plain = plain_context(ctx);
                    let messages = self.build_messages(&plain)?;
                    return self
                        .plain_stream_response(
                            &main_backend,
                            messages,
                            Some((cancel, on_chunk, on_thinking)),
                        )
                        .await;
                }
                Err(e) => return Err(e),
            }
        }

        self.plain_stream_response(
            &main_backend,
            messages,
            Some((cancel, on_chunk, on_thinking)),
        )
        .await
    }
}

/// 去掉工具运行时（降级重试用）
fn plain_context(ctx: &AgentContext) -> AgentContext {
    let mut plain = ctx.clone();
    plain.tools = None;
    plain
}

impl ChatAgent {
    /// 纯对话流式路径（不携带 `tools` 字段）
    ///
    /// `backend` 由调用方按本轮模型方案给出（会话级选择），未做选择时
    /// 就是共享后端 —— 与未引入模型路由时逐字节一致。
    async fn plain_stream_response(
        &self,
        backend: &Arc<dyn ChatBackend>,
        messages: Vec<LlmMessage>,
        callbacks: Option<(
            Arc<std::sync::atomic::AtomicBool>,
            StreamChunkCallback,
            StreamThinkingCallback,
        )>,
    ) -> Result<AgentResponse> {
        let stream = backend.chat_stream(messages, None).await?;
        let mut full_response = String::new();
        let mut stream = stream;

        while let Some(chunk) = stream.next().await {
            if let Some((cancel, _, _)) = &callbacks {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
            }
            match chunk {
                Ok(StreamChunk::Content(text)) => {
                    full_response.push_str(&text);
                    if let Some((_, on_chunk, _)) = &callbacks {
                        on_chunk(&text);
                    }
                }
                Ok(StreamChunk::Thinking(text)) => {
                    if let Some((_, _, on_thinking)) = &callbacks {
                        on_thinking(&text);
                    }
                }
                // 未启用工具时不会收到工具增量；忽略以保持正文纯净
                Ok(StreamChunk::ToolCallDelta(_)) => {}
                // 纯对话链路不关心结束原因
                Ok(StreamChunk::Finish(_)) => {}
                Err(e) => {
                    // 已经流出的正文保留（用户已经看到），补一句中断说明；
                    // 什么都不产出才按错误上报
                    if full_response.trim().is_empty() {
                        return Err(e);
                    }
                    let note = format!(
                        "\n\n（连接中断：{}。以上为已生成的部分内容）",
                        e
                    );
                    full_response.push_str(&note);
                    if let Some((_, on_chunk, _)) = &callbacks {
                        on_chunk(&note);
                    }
                    break;
                }
            }
        }

        Ok(AgentResponse::text(full_response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::{Message, Role};
    use std::sync::Mutex;

    fn msg(role: Role, content: &str) -> Message {
        Message::new(role, content, "s1")
    }

    // ─── strip_think_tags ───────────────────────────

    #[test]
    fn strip_think_removes_closed_tags() {
        assert_eq!(strip_think_tags("前<think>秘密</think>后"), "前后");
        assert_eq!(strip_think_tags("<think>a</think><think>b</think>x"), "x");
        assert_eq!(strip_think_tags("无标签"), "无标签");
    }

    #[test]
    fn strip_think_discards_unclosed_tail() {
        // 未闭合：丢弃 <think> 之后的所有内容，保留前缀
        assert_eq!(strip_think_tags("可见<think>被截断的思考"), "可见");
    }

    #[test]
    fn strip_think_handles_empty_and_only_think() {
        assert_eq!(strip_think_tags(""), "");
        assert_eq!(strip_think_tags("<think></think>"), "");
        assert_eq!(strip_think_tags("<think>全部是思考</think>"), "");
    }

    // ─── dedupe_trailing_input ──────────────────────

    #[test]
    fn dedupe_skips_trailing_duplicate_user_message() {
        let conv = vec![
            msg(Role::User, "你好"),
            msg(Role::Assistant, "你好呀～"),
            msg(Role::User, "今天天气如何"), // 即当前输入（已落库）
        ];
        let out = dedupe_trailing_input(&conv, "今天天气如何");
        assert_eq!(out.len(), 2);
        assert_eq!(out.last().unwrap().content, "你好呀～");
    }

    #[test]
    fn dedupe_keeps_history_when_no_duplicate() {
        let conv = vec![
            msg(Role::User, "你好"),
            msg(Role::Assistant, "你好呀～"),
        ];
        let out = dedupe_trailing_input(&conv, "新问题");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedupe_ignores_assistant_or_mismatched_tail() {
        let conv_a = vec![msg(Role::User, "a"), msg(Role::Assistant, "a")];
        assert_eq!(dedupe_trailing_input(&conv_a, "a").len(), 2);

        let conv_b = vec![msg(Role::User, "不同内容")];
        assert_eq!(dedupe_trailing_input(&conv_b, "当前输入").len(), 1);
    }

    #[test]
    fn dedupe_compares_after_stripping_think() {
        let conv = vec![
            msg(Role::User, "问题"),
            msg(Role::Assistant, "<think>推理</think>答案"),
            msg(Role::User, "带思考的问题<think>xxx</think>"), // 存储时含标签
        ];
        // 当前输入为剥离后的内容 —— 仍应识别为重复
        let out = dedupe_trailing_input(&conv, "带思考的问题");
        assert_eq!(out.len(), 2);
    }

    // ─── build_messages 集成 ────────────────────────

    fn make_agent() -> ChatAgent {
        use crate::llm::proxy::LlmProxy;
        let llm_cfg = crate::config::types::LlmConfig::default();
        let engine = PersonaEngine::new().expect("engine");
        ChatAgent::new(
            LlmProxy::new(llm_cfg.active_provider()),
            Arc::new(RwLock::new(engine)),
            Arc::new(ToolRegistry::empty()),
        )
    }

    fn base_ctx(user_input: &str, conversation: Vec<Message>) -> AgentContext {
        AgentContext {
            user_input: user_input.to_string(),
            system_hint: None,
            conversation,
            context_summary: None,
            persona_id: crate::persona::types::DEFAULT_PERSONA_ID.to_string(),
            user_nickname: "测试用户".to_string(),
            user_info: None,
            retrieved_memories: Vec::new(),
            tools: None,
            models: None,
            plan: None,
            notes: Vec::new(),
            session_type: "chat".to_string(),
            task_mode: "plan".to_string(),
            session_id: "s1".to_string(),
            stream_id: "stream-1".to_string(),
        }
    }

    /// 构造一个最小可用的工具运行时（仅用于验证提示词注入）
    fn test_runtime() -> ToolRuntime {
        use crate::agent::harness::{
            DenyAllApprover, NullSink, ToolLimits, ToolServices, WorkspaceSet,
        };
        use crate::config::types::{ToolConfig, ToolMode};

        let dir = std::env::temp_dir().join(format!("konata-agent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ToolConfig::with_single_root(
            &dir,
            true,
            "应用工作区",
        );
        let set = WorkspaceSet::from_config(&cfg, &dir);
        ToolRuntime {
            services: ToolServices::minimal(dir.clone(), set, ToolMode::Standard),
            emit: Arc::new(NullSink),
            approver: Arc::new(DenyAllApprover),
            enabled: true,
            auto_approve: Vec::new(),
            auto_approve_all: false,
            limits: ToolLimits {
                max_output_bytes: 64 * 1024,
                call_timeout: std::time::Duration::from_secs(30),
                approval_timeout: std::time::Duration::from_secs(120),
            },
            max_steps: 8,
            subagent_max_children: 2,
            subagent_steps: 32,
            subagent_max_tasks: 3,
        }
    }

    #[test]
    fn tool_rules_injected_only_when_runtime_enabled() {
        let agent = make_agent();

        let plain = base_ctx("你好", vec![]);
        let messages = agent.build_messages(&plain).unwrap();
        assert!(
            !messages[0].content.contains("【工具使用规则】"),
            "纯对话（悬浮窗）链路不得注入工具规则"
        );

        let mut with_tools = base_ctx("你好", vec![]);
        with_tools.tools = Some(test_runtime());
        let messages = agent.build_messages(&with_tools).unwrap();
        let system = &messages[0].content;
        assert!(system.contains("【工具使用规则】"));
        assert!(system.contains("不会保留到下一轮"));
        assert!(system.contains("untrusted"));
        // 人格、时间、记忆注入顺序不受影响
        assert!(system.contains("【当前时间】"));
        assert_eq!(messages.len(), 2);
    }

    /// 任务计划只注入"有工具的链路"，且注入位置在工具规则之后（system prompt 最后一块）
    #[test]
    fn plan_section_is_injected_only_with_tools() {
        let agent = make_agent();
        let plan = crate::agent::plan::SessionPlan::new(
            vec![
                crate::agent::plan::PlanItem {
                    title: "读代码".to_string(),
                    status: crate::agent::plan::PlanStatus::Done,
                },
                crate::agent::plan::PlanItem {
                    title: "改实现".to_string(),
                    status: crate::agent::plan::PlanStatus::Doing,
                },
            ],
            Some("等用户确认".to_string()),
        );

        // 悬浮窗/纯对话链路：没有工具就完全不注入计划（提示词与接入工具前一致）
        let mut plain = base_ctx("你好", vec![]);
        plain.plan = Some(plan.clone());
        let messages = agent.build_messages(&plain).unwrap();
        assert!(
            !messages[0].content.contains("【当前任务计划】"),
            "纯对话链路不得注入计划"
        );

        // 主窗口链路：注入，并且在工具规则之后
        let mut with_tools = base_ctx("继续", vec![]);
        with_tools.tools = Some(test_runtime());
        with_tools.plan = Some(plan);
        let messages = agent.build_messages(&with_tools).unwrap();
        let system = &messages[0].content;
        let rules_at = system.find("【工具使用规则】").expect("工具规则");
        let plan_at = system.find("【当前任务计划】").expect("计划段落");
        assert!(plan_at > rules_at, "计划应排在工具规则之后");
        assert!(system.contains("[x] 读代码"), "{system}");
        assert!(system.contains("[>] 改实现"), "{system}");
        assert!(system.contains("备注：等用户确认"), "{system}");
        // 计划属于 system 段，不能混进对话历史
        assert_eq!(messages.len(), 2);
    }

    /// 会话级模型方案必须真的改变本轮使用的 backend（否则"选了模型没生效"）
    #[test]
    fn model_plan_replaces_the_backend_and_feeds_sub_agents() {
        use crate::config::types::{AppConfig, ModelRef};
        use crate::llm::router::{resolve, SessionModelPref};

        let agent = make_agent();

        // 未做选择：复用共享 backend，子代理池只有一个无名条目
        let plain = base_ctx("你好", vec![]);
        let (main, children) = agent.backends_for(&plain);
        assert!(Arc::ptr_eq(&main, &agent.backend));
        assert_eq!(children.len(), 1);
        assert!(children[0].label.is_empty());

        // 自动选择（任务会话）：主轮次与子代理都用请求级 backend
        let mut cfg = AppConfig::default();
        let provider_id = cfg.llm.providers[0].id.clone();
        cfg.models.main = Some(ModelRef::new(provider_id.clone(), "main-model"));
        cfg.models.subs = vec![
            ModelRef::new(provider_id.clone(), "sub-a"),
            ModelRef::new(provider_id, "sub-b"),
        ];
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "work");

        let mut task_ctx = base_ctx("干活", vec![]);
        task_ctx.models = Some(Arc::new(plan));
        let (main, children) = agent.backends_for(&task_ctx);
        assert!(
            !Arc::ptr_eq(&main, &agent.backend),
            "有会话级方案时必须使用请求级 backend"
        );
        assert_eq!(children.len(), 2, "两个子模型都要进子代理池");
        assert_eq!(children[0].label, "sub-a");
        assert_eq!(children[1].label, "sub-b");
    }

    /// 未配置子模型时，子代理池退化为"与主轮次同模型"（含主模型标签）
    #[test]
    fn manual_plan_gives_child_agents_the_main_model() {
        use crate::config::types::AppConfig;
        use crate::llm::router::{resolve, SessionModelPref};

        let agent = make_agent();
        let cfg = AppConfig::default();
        let provider_id = cfg.llm.providers[0].id.clone();
        let pref = SessionModelPref::manual(provider_id, "only-model");
        let plan = resolve(&cfg, Some(&pref), "chat", "plan");

        let mut ctx = base_ctx("你好", vec![]);
        ctx.models = Some(Arc::new(plan));
        let (_, children) = agent.backends_for(&ctx);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].label, "only-model");
    }

    /// 首次带 tools 的请求被提供商拒绝，第二次（不带 tools）成功
    #[derive(Default)]
    struct ToolsRejectingBackend {
        seen_tools: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl ChatBackend for ToolsRejectingBackend {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> anyhow::Result<String> {
            Ok("mock".to_string())
        }

        async fn chat_stream(
            &self,
            _messages: Vec<LlmMessage>,
            tools: Option<Vec<crate::llm::types::ToolSchema>>,
        ) -> anyhow::Result<
            std::pin::Pin<
                Box<dyn futures::stream::Stream<Item = anyhow::Result<StreamChunk>> + Send>,
            >,
        > {
            self.seen_tools.lock().unwrap().push(tools.is_some());
            if tools.is_some() {
                return Err(anyhow::anyhow!(
                    "LLM API error (HTTP 400): tools is not supported by this model"
                ));
            }
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamChunk::Content("降级后的回答".to_string()),
            )])))
        }
    }

    #[test]
    fn tools_unsupported_detection() {
        let yes = anyhow::anyhow!("LLM API error (HTTP 400): tools is not supported by this model");
        assert!(is_tools_unsupported(&yes));

        let no = anyhow::anyhow!("Stream error: connection reset by peer");
        assert!(!is_tools_unsupported(&no));

        // 工具自身的失败不会冒泡成 Err（会被回灌给模型），这里只关心"提供商拒绝 tools"
        let unrelated = anyhow::anyhow!("LLM API error (HTTP 500): internal server error");
        assert!(!is_tools_unsupported(&unrelated));
    }

    #[test]
    fn degrades_to_plain_chat_when_provider_rejects_tools() {
        let backend = Arc::new(ToolsRejectingBackend::default());
        let engine = PersonaEngine::new().expect("engine");
        let agent = ChatAgent::with_backend(
            backend.clone(),
            Arc::new(RwLock::new(engine)),
            // 必须是真实注册表：空注册表不会下发 tools，也就测不到降级
            Arc::new(crate::agent::harness::tools::builtin_registry()),
        );

        let mut ctx = base_ctx("你好", vec![]);
        ctx.tools = Some(test_runtime());

        let collected = Arc::new(Mutex::new(String::new()));
        let sink = collected.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let response = runtime
            .block_on(agent.handle_stream(
                &ctx,
                cancel,
                Box::new(move |text: &str| {
                    sink.lock().unwrap().push_str(text);
                }),
                Box::new(|_: &str| {}),
            ))
            .expect("必须降级成功而不是把错误抛给用户");

        assert_eq!(response.content, "降级后的回答");
        assert_eq!(collected.lock().unwrap().as_str(), "降级后的回答");

        let seen = backend.seen_tools.lock().unwrap();
        assert_eq!(seen.len(), 2, "应当重试一次");
        assert!(seen[0], "第一次必须带 tools");
        assert!(!seen[1], "降级后不得再带 tools 字段");
    }

    #[test]
    fn enabled_but_empty_registry_still_injects_rules() {
        // 注册表为空时规则里会写明"没有任何可用工具"，模型不应幻觉调用
        let agent = make_agent();
        let mut ctx = base_ctx("你好", vec![]);
        ctx.tools = Some(test_runtime());
        let messages = agent.build_messages(&ctx).unwrap();
        assert!(messages[0].content.contains("没有任何可用工具"));
    }

    #[test]
    fn build_messages_does_not_duplicate_current_input() {
        let agent = make_agent();
        // 模拟旧时序：历史里已包含刚落库的当前输入
        let ctx = base_ctx(
            "最新问题",
            vec![
                msg(Role::User, "旧问题"),
                msg(Role::Assistant, "旧回答"),
                msg(Role::User, "最新问题"),
            ],
        );
        let messages = agent.build_messages(&ctx).expect("messages");

        // system(1) + 历史[User, Assistant](2) + 当前输入(1)
        assert_eq!(messages.len(), 4);
        let user_contents: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            user_contents,
            vec!["旧问题", "最新问题"],
            "当前输入只能出现一次且位于末尾，不得重复注入"
        );
    }

    #[test]
    fn build_messages_appends_hint_to_final_user_input_only() {
        let agent = make_agent();
        let mut ctx = base_ctx("问题", vec![]);
        ctx.system_hint = Some("【保持简短】".to_string());
        let messages = agent.build_messages(&ctx).expect("messages");

        assert_eq!(messages.len(), 2); // system + user
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "问题\n【保持简短】");
    }

    #[test]
    fn build_messages_injects_time_summary_and_memories() {
        use crate::store::memory_store::MemoryEntry;

        let agent = make_agent();
        let mut ctx = base_ctx("问题", vec![]);
        ctx.context_summary = Some("- 用户提到喜欢猫".to_string());
        ctx.retrieved_memories = vec![MemoryEntry {
            id: "m1".to_string(),
            content: "用户喜欢猫".to_string(),
            memory_type: crate::store::memory_store::MemoryType::Preference,
            importance: 0.8,
            embedding: None,
            source_session: "s1".to_string(),
            created_at: String::new(),
            last_accessed: String::new(),
            access_count: 0,
        }];

        let messages = agent.build_messages(&ctx).expect("messages");
        let sys = &messages[0].content;
        assert!(sys.contains("【之前的对话摘要】"));
        assert!(sys.contains("关于用户的记忆"));
        assert!(sys.contains("偏好"));
        assert!(sys.contains("星期") || sys.contains("周"));
    }
}
