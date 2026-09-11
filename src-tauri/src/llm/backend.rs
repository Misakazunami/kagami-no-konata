use anyhow::Result;
use futures::stream::Stream;
use std::pin::Pin;

use super::types::{LlmMessage, StreamChunk, ToolSchema};

/// LLM 调用后端抽象
///
/// 存在的唯一理由：让工具循环（`agent::harness::loop`）可以在**完全离线**的
/// 条件下被测试。`ChatAgent` 只依赖这个 trait，测试注入脚本化的 `MockBackend`，
/// 生产环境注入 `LlmProxy`。
#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    /// 非流式调用（保留给摘要/标题等辅助链路通过同一抽象发起请求）
    #[allow(dead_code)]
    async fn chat(&self, messages: Vec<LlmMessage>) -> Result<String>;

    /// 流式调用
    ///
    /// `tools` 为 `None` 时请求体里不会出现 `tools` 字段——
    /// 悬浮窗链路恒为 `None`，因此行为与未接入工具前逐字节一致。
    async fn chat_stream(
        &self,
        messages: Vec<LlmMessage>,
        tools: Option<Vec<ToolSchema>>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>>;
}
