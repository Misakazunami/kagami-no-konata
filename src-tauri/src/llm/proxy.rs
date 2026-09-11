use anyhow::Result;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::RwLock;

use crate::config::types::LlmProvider;

use super::backend::ChatBackend;
use super::openai::OpenAiClient;
use super::types::{ChatRequest, LlmMessage, ModelInfo, StreamChunk, ToolSchema};

struct ProxyInner {
    client: OpenAiClient,
    provider: LlmProvider,
}

/// LLM 统一调用代理
///
/// 内部使用 RwLock 保存提供商配置，可在共享引用上热更新
/// （切换模型/提供商后立即生效，无需重建代理）。
pub struct LlmProxy {
    inner: RwLock<ProxyInner>,
}

impl LlmProxy {
    /// 从提供商配置创建代理
    pub fn new(provider: &LlmProvider) -> Self {
        Self {
            inner: RwLock::new(ProxyInner {
                client: OpenAiClient::new(&provider.api_base_url, &provider.api_key),
                provider: provider.clone(),
            }),
        }
    }

    /// 使用新提供商配置热更新（无需可变引用）
    pub fn update_provider(&self, provider: &LlmProvider) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.client = OpenAiClient::new(&provider.api_base_url, &provider.api_key);
        guard.provider = provider.clone();
    }

    /// 快照当前配置（克隆后立即释放锁，避免跨 await 持锁）
    fn snapshot(&self) -> (OpenAiClient, LlmProvider) {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        (guard.client.clone(), guard.provider.clone())
    }

    /// 获取可用模型列表
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (client, _) = self.snapshot();
        client.list_models().await
    }

    /// 获取文本嵌入向量
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let (client, provider) = self.snapshot();
        client.embed(&provider.embedding_model, texts).await
    }

    /// 非流式调用
    pub async fn chat(&self, messages: Vec<LlmMessage>) -> Result<String> {
        let (client, request) = {
            let (client, provider) = self.snapshot();
            (client, build_chat_request(&provider, messages, false, None))
        };
        client.chat(&request).await
    }

    /// 流式调用
    pub async fn chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.chat_stream_with_tools(messages, None).await
    }

    /// 带工具声明的流式调用
    pub async fn chat_stream_with_tools(
        &self,
        messages: Vec<LlmMessage>,
        tools: Option<Vec<ToolSchema>>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let (client, request) = {
            let (client, provider) = self.snapshot();
            (client, build_chat_request(&provider, messages, true, tools))
        };
        client.chat_stream(&request).await
    }
}

#[async_trait::async_trait]
impl ChatBackend for LlmProxy {
    async fn chat(&self, messages: Vec<LlmMessage>) -> Result<String> {
        LlmProxy::chat(self, messages).await
    }

    async fn chat_stream(
        &self,
        messages: Vec<LlmMessage>,
        tools: Option<Vec<ToolSchema>>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.chat_stream_with_tools(messages, tools).await
    }
}

fn build_chat_request(
    provider: &LlmProvider,
    messages: Vec<LlmMessage>,
    stream: bool,
    tools: Option<Vec<ToolSchema>>,
) -> ChatRequest {
    let tools = tools.filter(|t| !t.is_empty());
    ChatRequest {
        model: provider.model.clone(),
        messages,
        max_tokens: provider.max_tokens,
        temperature: provider.temperature,
        stream,
        enable_thinking: if provider.enable_thinking {
            Some(true)
        } else {
            None
        },
        tool_choice: tools.as_ref().map(|_| "auto".to_string()),
        tools,
    }
}
