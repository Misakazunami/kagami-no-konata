use serde::{Deserialize, Serialize};

/// 反序列化 `content` 时把 `null` 视为空串
///
/// 不少 OpenAI 兼容端点在消息只带 `tool_calls` 时会返回 `"content": null`，
/// 若直接按 `String` 解析会让整个响应反序列化失败。
fn null_as_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn default_function_kind() -> String {
    "function".to_string()
}

/// 工具调用中的函数调用
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON 字符串（流式响应下由分片拼接而成）
    #[serde(default)]
    pub arguments: String,
}

/// 模型请求的一次工具调用
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: default_function_kind(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

/// LLM 消息格式（OpenAI 兼容）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    /// `tool` 角色消息按规范可以没有 content，因此这里允许缺省与 null
    #[serde(default, deserialize_with = "null_as_empty")]
    pub content: String,
    /// assistant 消息请求的工具调用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// `tool` 角色消息回应的调用 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            ..Default::default()
        }
    }

    /// assistant 消息 + 它请求的工具调用（工具循环回灌时使用）
    pub fn assistant_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }

    /// 工具执行结果消息
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// 暴露给模型的工具声明（OpenAI `tools` 字段元素）
#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: default_function_kind(),
            function: FunctionSchema {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Chat completion 请求体
#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    /// 最大输出 tokens；为 None 时字段完全不进请求体（由服务商决定默认上限）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub temperature: f32,
    pub stream: bool,
    /// 是否启用思考模式（需要模型支持）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    /// 可用工具声明；为 None 表示本次不使用工具（悬浮窗恒为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

/// 非流式响应
#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
pub struct ChatChoice {
    pub message: LlmMessage,
}

/// 流式响应 chunk
///
/// `choices` 允许缺省：部分提供商的 usage/保活帧只有 `usage` 字段，
/// 缺省值让它们走"忽略"而不是被当成解析错误刷日志。
#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChatChunkChoice>,
}

#[derive(Debug, Deserialize)]
pub struct ChatChunkChoice {
    pub delta: ChatDelta,
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatDelta {
    pub content: Option<String>,
    /// 思考内容（部分 API 如 DeepSeek 使用此字段返回思考过程）
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 工具调用增量（OpenAI / DeepSeek 均为按 index 分片追加）
    #[serde(default)]
    pub tool_calls: Option<Vec<RawToolCallDelta>>,
}

/// 原始工具调用增量
#[derive(Debug, Clone, Deserialize)]
pub struct RawToolCallDelta {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<RawFunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// 工具调用增量（已归一化，供累积器消费）
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallDelta {
    /// 提供商给出的分片序号
    ///
    /// `None` = 该网关没有推送 `index`（协议允许但不多见）。必须保留 `None`
    /// 让累积器去判断"这是续片还是新调用"，直接 `unwrap_or(0)` 会把并行调用
    /// 合并进同一个槽位。
    pub index: Option<usize>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

impl From<RawToolCallDelta> for ToolCallDelta {
    fn from(raw: RawToolCallDelta) -> Self {
        let function = raw.function.unwrap_or(RawFunctionDelta {
            name: None,
            arguments: None,
        });
        Self {
            index: raw.index,
            id: raw.id,
            name: function.name,
            arguments: function.arguments,
        }
    }
}

/// 流式响应中的内容块类型
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 正文内容
    Content(String),
    /// 思考内容
    Thinking(String),
    /// 工具调用增量（绝不作为正文外发）
    ToolCallDelta(ToolCallDelta),
    /// 提供商给出的结束原因（`stop` / `length` / `content_filter` / ...）
    ///
    /// **为什么必须一路带到 harness**：`length` 表示这次输出被 `max_tokens`
    /// 截断了（推理模型把预算全花在思考上时尤其常见）。历史实现把
    /// `finish_reason` 反序列化后直接丢弃，于是"模型被截断"和"模型正常说完"
    /// 在 runner 眼里完全一样：既没有正文、也没有工具调用，回合就静悄悄地
    /// 结束了，用户只看到前面几张工具卡片（真实报障）。
    Finish(String),
}

/// GET /models 响应
#[derive(Debug, Deserialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelInfo>,
}

/// 模型信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub owned_by: Option<String>,
}

/// 嵌入请求体
#[derive(Debug, Serialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

/// 嵌入响应
#[derive(Debug, Deserialize)]
pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddingData {
    pub embedding: Vec<f32>,
}

/// LLM 错误响应
#[derive(Debug, Deserialize)]
pub struct LlmError {
    pub error: LlmErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct LlmErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub error_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_message_serializes_without_tool_fields_when_absent() {
        let json = serde_json::to_value(LlmMessage::user("你好")).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "你好");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("tool_call_id").is_none());
    }

    #[test]
    fn llm_message_accepts_null_content() {
        // 只带 tool_calls 的 assistant 消息常见于 OpenAI 兼容端点
        let msg: LlmMessage = serde_json::from_str(
            r#"{"role":"assistant","content":null,"tool_calls":[
                 {"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}"#,
        )
        .unwrap();
        assert_eq!(msg.content, "");
        let calls = msg.tool_calls.unwrap();
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].kind, "function");
    }

    #[test]
    fn tool_message_carries_call_id() {
        let json = serde_json::to_value(LlmMessage::tool("c1", "结果")).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "c1");
        assert_eq!(json["content"], "结果");
    }

    #[test]
    fn raw_delta_normalizes_missing_fields() {
        let raw: RawToolCallDelta =
            serde_json::from_str(r#"{"function":{"arguments":"{\"a\""}}"#).unwrap();
        let delta: ToolCallDelta = raw.into();
        assert_eq!(delta.index, None, "缺 index 必须保留 None 交给累积器判断");
        assert_eq!(delta.id, None);
        assert_eq!(delta.name, None);
        assert_eq!(delta.arguments.as_deref(), Some("{\"a\""));
    }

    #[test]
    fn tool_schema_uses_function_type() {
        let schema =
            ToolSchema::function("read_file", "读取文件", serde_json::json!({"type":"object"}));
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "read_file");
    }
}
