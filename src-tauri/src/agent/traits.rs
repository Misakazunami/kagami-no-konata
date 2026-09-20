use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::context::Message;
use super::harness::ToolRuntime;
use crate::store::memory_store::MemoryEntry;

/// Agent 能力描述
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Capability {
    Chat,
    TaskExecution,
    KnowledgeQuery,
    Custom(String),
}

/// Agent 元数据清单，用于路由判定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 触发的前缀/斜杠命令（如 ["/sys", "/cmd"]）
    pub prefix_commands: Vec<String>,
    /// 触发关键词（如 ["打开", "系统状态"]）
    pub trigger_keywords: Vec<String>,
    /// 正则表达式触发模式
    pub regex_patterns: Vec<String>,
    /// 是否需要包装在当前人格的口吻中输出（适合桌面宠物）
    pub wrap_in_persona: bool,
}

/// Agent 上下文（每次调用传入）
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub user_input: String,
    /// 可选的系统提示（仅对 LLM 可见，不存储到数据库）
    pub system_hint: Option<String>,
    /// 最近 N 条对话（已由调用方截断，更早内容在 context_summary 中）
    pub conversation: Vec<Message>,
    /// 跨轮次持久化的会话摘要（本地增量维护，None 表示尚无摘要）
    pub context_summary: Option<String>,
    pub persona_id: String,
    pub user_nickname: String,
    pub user_info: Option<crate::config::types::UserConfig>,
    pub retrieved_memories: Vec<MemoryEntry>,
    /// 工具运行时；`None` 表示纯对话（悬浮窗链路恒为 `None`）
    pub tools: Option<ToolRuntime>,
    /// 本轮生成使用的模型方案（会话级选择 + 主/子模型路由）
    ///
    /// `None` 表示"未做会话级选择"：Agent 直接使用共享的全局 backend，
    /// 行为与未引入模型路由时完全一致。解析在 `send_message` 里完成，
    /// 因此用户中途换模型不会影响正在跑的生成。
    pub models: Option<Arc<crate::llm::router::ModelPlan>>,
    /// 会话级任务计划（由 `update_plan` 工具写；注入 system prompt 让模型跨轮记住进度）
    pub plan: Option<crate::agent::plan::SessionPlan>,
    /// 工作记忆（由 `save_note` 写；注入时统一带 untrusted 标记）
    pub notes: Vec<crate::agent::notes::SessionNote>,
    /// 会话类型："chat" | "task"
    pub session_type: String,
    /// 任务模式："plan" | "work"
    pub task_mode: String,
    /// 当前会话 id（工具事件与轨迹记录需要）
    pub session_id: String,
    /// 当前生成任务 id（用于跨窗口过滤与取消）
    pub stream_id: String,
}

/// Agent 响应类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseType {
    Text,
    Action(String),
}

/// Agent 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub content: String,
    pub response_type: ResponseType,
    /// 本轮生成中的工具调用轨迹（落库到 `tool_invocations` 供 UI 回放）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_invocations: Vec<crate::agent::harness::InvocationRecord>,
    /// 工具额外消耗的 token 估算（目前来自只读子代理），累加进本轮统计
    #[serde(default)]
    pub extra_tokens: usize,
    /// 工具步数用尽、模型被强制收尾（界面据此提示"中断，可继续"）
    #[serde(default)]
    pub hit_step_limit: bool,
    /// 本次生成实际执行的工具轮数（供界面展示"中断于第 N 步"）
    #[serde(default)]
    pub tool_steps: usize,
}

impl AgentResponse {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            response_type: ResponseType::Text,
            tool_invocations: Vec::new(),
            extra_tokens: 0,
            hit_step_limit: false,
            tool_steps: 0,
        }
    }

    /// 记下工具额外消耗的 token 估算（子代理等"隐藏开销"）
    pub fn with_extra_tokens(mut self, tokens: usize) -> Self {
        self.extra_tokens = tokens;
        self
    }

    pub fn with_invocations(
        mut self,
        invocations: Vec<crate::agent::harness::InvocationRecord>,
    ) -> Self {
        self.tool_invocations = invocations;
        self
    }

    /// 记下工具循环的收尾状态（步数用尽 / 实际轮数）
    pub fn with_step_limit(mut self, hit: bool, steps: usize) -> Self {
        self.hit_step_limit = hit;
        self.tool_steps = steps;
        self
    }
}

pub type StreamChunkCallback = Box<dyn Fn(&str) + Send + Sync + 'static>;
pub type StreamThinkingCallback = Box<dyn Fn(&str) + Send + Sync + 'static>;

/// Agent trait —— 面向扩展的核心抽象
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Agent 唯一标识
    fn id(&self) -> &str;

    /// Agent 清单（包含路由规则与元数据）
    fn manifest(&self) -> AgentManifest;

    /// Agent 能力描述
    #[allow(dead_code)]
    fn capabilities(&self) -> Vec<Capability>;

    /// 处理用户输入，返回响应
    async fn handle(&self, ctx: &AgentContext) -> Result<AgentResponse>;

    /// 流式处理用户输入
    async fn handle_stream(
        &self,
        ctx: &AgentContext,
        cancel: Arc<AtomicBool>,
        on_chunk: StreamChunkCallback,
        on_thinking: StreamThinkingCallback,
    ) -> Result<AgentResponse>;
}
