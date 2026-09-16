use anyhow::Result;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::RwLock;

use crate::config::types::LlmProvider;

use super::backend::ChatBackend;
use super::openai::OpenAiClient;
use super::types::{ChatRequest, LlmMessage, ModelInfo, StreamChunk, ToolSchema};

/// 单次请求级别的参数覆盖（不改写用户配置，只作用于这一轮）
///
/// 存在的理由：会话级的"深度思考"开关与提供商配置里的默认值必须能分开——
/// 前者是**这一轮**的选择，后者是持久化的默认。`thinking` 为 `None`
/// 表示"沿用提供商默认"，与接入该开关之前的行为逐字节一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderOverrides {
    /// 本轮是否开启深度思考（`Some(false)` 会显式下发 `enable_thinking: false`）
    pub thinking: Option<bool>,
}

struct ProxyInner {
    client: OpenAiClient,
    provider: LlmProvider,
    overrides: ProviderOverrides,
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
        Self::with_overrides(provider, ProviderOverrides::default())
    }

    /// 带单轮参数覆盖创建代理（会话级模型路由使用：每个模型一个实例）
    pub fn with_overrides(provider: &LlmProvider, overrides: ProviderOverrides) -> Self {
        Self {
            inner: RwLock::new(ProxyInner {
                client: OpenAiClient::new(&provider.api_base_url, &provider.api_key),
                provider: provider.clone(),
                overrides,
            }),
        }
    }

    /// 使用新提供商配置热更新（无需可变引用）
    ///
    /// 覆盖项**故意不重置**：它描述的是"这一轮"的选择，
    /// 与提供商配置的持久化字段互不相干。
    pub fn update_provider(&self, provider: &LlmProvider) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.client = OpenAiClient::new(&provider.api_base_url, &provider.api_key);
        guard.provider = provider.clone();
    }

    /// 快照当前配置（克隆后立即释放锁，避免跨 await 持锁）
    fn snapshot(&self) -> (OpenAiClient, LlmProvider, ProviderOverrides) {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        (guard.client.clone(), guard.provider.clone(), guard.overrides)
    }

    /// 获取可用模型列表
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (client, _, _) = self.snapshot();
        client.list_models().await
    }

    /// 获取文本嵌入向量
    pub async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let (client, provider, _) = self.snapshot();
        client.embed(&provider.embedding_model, texts).await
    }

    /// 非流式调用
    pub async fn chat(&self, messages: Vec<LlmMessage>) -> Result<String> {
        let (client, request) = {
            let (client, provider, overrides) = self.snapshot();
            (
                client,
                build_chat_request(&provider, overrides, messages, false, None),
            )
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
            let (client, provider, overrides) = self.snapshot();
            (
                client,
                build_chat_request(&provider, overrides, messages, true, tools),
            )
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
    overrides: ProviderOverrides,
    messages: Vec<LlmMessage>,
    stream: bool,
    tools: Option<Vec<ToolSchema>>,
) -> ChatRequest {
    let tools = tools.filter(|t| !t.is_empty());
    // 单轮覆盖优先；未覆盖时沿用提供商的持久化默认（历史行为）。
    //
    // **能力过滤必须放在合并之后**：先把 `Some(false)` 过滤成 `None` 再回落
    // 提供商默认 `true`，会把用户的"显式关闭"反转成开启（真实缺陷）。
    // 判定为不支持时一律不发该字段——严格端点会对未知字段直接 400。
    let enable_thinking = match overrides.thinking {
        Some(value) => Some(value),
        None => provider.enable_thinking.then_some(true),
    }
    .filter(|_| crate::llm::capabilities::provider_supports_thinking(provider, &provider.model));
    ChatRequest {
        model: provider.model.clone(),
        messages,
        max_tokens: provider.max_tokens,
        temperature: provider.temperature,
        stream,
        enable_thinking,
        tool_choice: tools.as_ref().map(|_| "auto".to_string()),
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> LlmProvider {
        let mut p = LlmProvider::new("测试", "https://example.com/v1", "sk-test");
        // 用"显式开关型"模型（Qwen3 家族）作为默认，原生推理模型见专门用例
        p.model = "qwen3-32b".to_string();
        p
    }

    fn request_json(provider: &LlmProvider, overrides: ProviderOverrides) -> serde_json::Value {
        let req = build_chat_request(provider, overrides, vec![LlmMessage::user("hi")], true, None);
        serde_json::to_value(&req).unwrap()
    }

    /// 未覆盖 + 提供商默认关闭 ⇒ 请求体里**没有** enable_thinking（与接入前一致）
    #[test]
    fn thinking_field_absent_when_unset() {
        let json = request_json(&provider(), ProviderOverrides::default());
        assert!(json.get("enable_thinking").is_none(), "{json}");
    }

    /// 提供商默认开启 ⇒ 下发 true
    #[test]
    fn provider_default_turns_thinking_on() {
        let mut p = provider();
        p.enable_thinking = true;
        let json = request_json(&p, ProviderOverrides::default());
        assert_eq!(json["enable_thinking"], serde_json::json!(true));
    }

    /// 会话级开关必须能**关掉**提供商默认打开的思考
    #[test]
    fn session_override_can_disable_provider_default() {
        let mut p = provider();
        p.enable_thinking = true;
        let json = request_json(&p, ProviderOverrides { thinking: Some(false) });
        assert_eq!(
            json["enable_thinking"],
            serde_json::json!(false),
            "显式关闭必须真的发出去，而不是退回'不发字段'"
        );
    }

    #[test]
    fn session_override_can_enable_thinking() {
        let json = request_json(&provider(), ProviderOverrides { thinking: Some(true) });
        assert_eq!(json["enable_thinking"], serde_json::json!(true));
        // 其他字段不受影响
        assert_eq!(json["model"], "qwen3-32b");
        assert_eq!(json["stream"], true);
        assert!(json.get("tools").is_none());
    }

    /// 原生推理模型（官方端点不认 `enable_thinking`）绝不能收到该字段，
    /// 即使提供商默认打开了思考
    #[test]
    fn native_reasoner_never_receives_the_field() {
        let mut p = provider();
        p.model = "gpt-5".to_string();
        p.enable_thinking = true;
        let json = request_json(&p, ProviderOverrides { thinking: Some(true) });
        assert!(
            json.get("enable_thinking").is_none(),
            "严格端点会 400：{json}"
        );
    }

    /// 用户显式关闭必须保留"关闭"语义，而不能因为模型不支持该字段
    /// 就回落到提供商默认（历史缺陷：Some(false) 被 filter 成 None，
    /// 再被 provider.enable_thinking=true 反转成开启）
    #[test]
    fn explicit_off_is_never_flipped_back_to_on() {
        let mut p = provider();
        p.model = "gpt-4o".to_string();
        p.enable_thinking = true;
        let json = request_json(&p, ProviderOverrides { thinking: Some(false) });
        assert!(
            json.get("enable_thinking").is_none(),
            "不支持时不下发；绝不能变成 true：{json}"
        );
    }

    /// 覆盖项属于"这一轮"：热更新提供商配置不得把它冲掉
    #[test]
    fn override_survives_provider_hot_update() {
        let proxy = LlmProxy::with_overrides(&provider(), ProviderOverrides { thinking: Some(false) });
        let mut updated = provider();
        updated.model = "deepseek-chat".to_string();
        proxy.update_provider(&updated);

        let (_, provider, overrides) = proxy.snapshot();
        assert_eq!(provider.model, "deepseek-chat");
        assert_eq!(overrides.thinking, Some(false));
    }
}
