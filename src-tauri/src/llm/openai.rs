use anyhow::{anyhow, Result};
use futures::stream::Stream;
use reqwest::Client;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use super::types::{
    ChatChunk, ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, LlmError,
    ModelInfo, ModelsResponse, StreamChunk,
};

/// 全局共享 HTTP 客户端（复用连接池与 TLS 会话）
///
/// 注意：流式响应可能持续较久，因此不设整体超时，
/// 只限制连接建立时间与单次读取的空闲时间。
fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(180))
            .build()
            .expect("Failed to build shared HTTP client")
    })
}

/// OpenAI 兼容 API 客户端（可廉价克隆，内部共享连接池）
#[derive(Clone)]
pub struct OpenAiClient {
    client: Client,
    base_url: String,
    api_key: String,
}

impl OpenAiClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            client: shared_http_client().clone(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// 获取可用模型列表
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if let Ok(err) = serde_json::from_str::<LlmError>(&body) {
                return Err(anyhow!("LLM API error: {}", err.error.message));
            }
            return Err(anyhow!("LLM API error (HTTP {}): {}", status, body));
        }

        let models_response: ModelsResponse = response.json().await?;
        Ok(models_response.data)
    }

    /// 获取文本嵌入向量
    pub async fn embed(&self, model: &str, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", self.base_url);

        let request = EmbeddingRequest {
            model: model.to_string(),
            input: texts,
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if let Ok(err) = serde_json::from_str::<LlmError>(&body) {
                return Err(anyhow!("Embedding API error: {}", err.error.message));
            }
            return Err(anyhow!("Embedding API error (HTTP {}): {}", status, body));
        }

        let embedding_response: EmbeddingResponse = response.json().await?;
        Ok(embedding_response.data.into_iter().map(|d| d.embedding).collect())
    }

    /// 非流式调用
    pub async fn chat(&self, request: &ChatRequest) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if let Ok(err) = serde_json::from_str::<LlmError>(&body) {
                return Err(anyhow!("LLM API error: {}", err.error.message));
            }
            return Err(anyhow!("LLM API error (HTTP {}): {}", status, body));
        }

        let chat_response: ChatResponse = response.json().await?;
        chat_response
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("No response from LLM"))
    }

    /// 流式调用，返回 SSE 事件流（区分正文和思考内容）
    ///
    /// 解析策略：
    /// - 字节级缓冲，仅在遇到完整事件分隔符 `\n\n` 后才解码，
    ///   避免多字节字符（如中文）被 TCP 分块切断导致乱码
    /// - `reasoning_content` 字段（DeepSeek 等 API）→ 直接映射为 `StreamChunk::Thinking`
    /// - `content` 字段中的 `<think>...</think>` 标签 → 解析后拆分为 Thinking/Content chunk
    /// - 其余 content → `StreamChunk::Content`
    /// - `tool_calls` 字段 → `StreamChunk::ToolCallDelta`（分片累积由 harness 负责）
    pub async fn chat_stream(
        &self,
        request: &ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if let Ok(err) = serde_json::from_str::<LlmError>(&body) {
                return Err(anyhow!("LLM API error: {}", err.error.message));
            }
            return Err(anyhow!("LLM API error (HTTP {}): {}", status, body));
        }

        let byte_stream = response.bytes_stream();
        // 状态：字节缓冲 / think 标签状态 / 待发送 chunk 队列
        let stream = futures::stream::unfold(
            (
                byte_stream,
                Vec::<u8>::new(),
                0u8,
                VecDeque::<StreamChunk>::new(),
            ),
            |(mut byte_stream, mut buffer, mut think_state, mut pending)| async move {
                use futures::StreamExt;
                loop {
                    // 如果有待发送的 chunk，优先发送
                    if let Some(chunk) = pending.pop_front() {
                        return Some((
                            Ok(chunk),
                            (byte_stream, buffer, think_state, pending),
                        ));
                    }

                    // 在字节缓冲中查找完整事件分隔符 \n\n
                    if let Some(pos) = find_event_separator(&buffer) {
                        let event_bytes: Vec<u8> = buffer.drain(..pos + 2).collect();
                        // 只解码完整事件，多字节字符不会在分隔符处被切断
                        let event = String::from_utf8_lossy(&event_bytes);
                        if parse_sse_event(&event, &mut think_state, &mut pending) {
                            // 收到 [DONE]
                            return None;
                        }
                        continue;
                    }

                    // 从流中读取更多数据
                    match byte_stream.next().await {
                        Some(Ok(bytes)) => {
                            buffer.extend_from_slice(&bytes);
                        }
                        Some(Err(e)) => {
                            return Some((Err(anyhow!("Stream error: {}", e)), (byte_stream, buffer, think_state, pending)));
                        }
                        None => {
                            // 流结束：flush 残余的不完整事件
                            if !buffer.is_empty() {
                                let event_bytes = std::mem::take(&mut buffer);
                                let event = String::from_utf8_lossy(&event_bytes);
                                let _ = parse_sse_event(&event, &mut think_state, &mut pending);
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

/// 在字节缓冲中查找 `\n\n` 分隔符
fn find_event_separator(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// 解析一个完整 SSE 事件（可能包含多行 data:）
///
/// 返回是否遇到 `[DONE]` 终止标记
fn parse_sse_event(
    event: &str,
    think_state: &mut u8,
    pending: &mut VecDeque<StreamChunk>,
) -> bool {
    for line in event.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            let data = data.trim();
            if data == "[DONE]" {
                return true;
            }
            if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
                if let Some(choice) = chunk.choices.first() {
                    // 优先处理 reasoning_content（结构化思考字段）
                    if let Some(reasoning) = &choice.delta.reasoning_content {
                        if !reasoning.is_empty() {
                            pending.push_back(StreamChunk::Thinking(reasoning.clone()));
                        }
                    }
                    // 处理正文内容（可能包含 <think> 标签）
                    if let Some(content) = &choice.delta.content {
                        if !content.is_empty() {
                            parse_content_chunks(content, think_state, pending);
                        }
                    }
                    // 工具调用增量：只入队交给上层累积，绝不混入正文
                    if let Some(calls) = &choice.delta.tool_calls {
                        for call in calls {
                            pending.push_back(StreamChunk::ToolCallDelta(call.clone().into()));
                        }
                    }
                }
            }
        }
    }
    false
}

/// 解析内容中的 `<think>...</think>` 标签，将结果追加到 pending 列表
///
/// `think_state` 是跨 chunk 持久的状态：0=正常模式, 1=在思考标签内
fn parse_content_chunks(content: &str, think_state: &mut u8, pending: &mut VecDeque<StreamChunk>) {
    let mut remaining = content;

    while !remaining.is_empty() {
        if *think_state == 0 {
            // 正常模式：查找 <think> 开始标签
            if let Some(start) = remaining.find("<think>") {
                // 标签前的正文
                if start > 0 {
                    pending.push_back(StreamChunk::Content(remaining[..start].to_string()));
                }
                *think_state = 1;
                remaining = &remaining[start + 7..]; // 跳过 "<think>"
            } else {
                // 没有思考标签，全部是正文
                pending.push_back(StreamChunk::Content(remaining.to_string()));
                return;
            }
        } else {
            // 思考模式：查找 </think> 结束标签
            if let Some(end) = remaining.find("</think>") {
                // 标签前的思考内容
                if end > 0 {
                    pending.push_back(StreamChunk::Thinking(remaining[..end].to_string()));
                }
                *think_state = 0;
                remaining = &remaining[end + 8..]; // 跳过 "</think>"
            } else {
                // 没有结束标签，全部是思考内容
                pending.push_back(StreamChunk::Thinking(remaining.to_string()));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ToolCallDelta;

    /// 把若干 SSE 事件喂给解析器，返回收集到的 chunk
    fn parse(events: &[&str]) -> (Vec<StreamChunk>, bool) {
        let mut think_state = 0u8;
        let mut pending: VecDeque<StreamChunk> = VecDeque::new();
        let mut done = false;
        for event in events {
            if parse_sse_event(&format!("data: {}\n\n", event), &mut think_state, &mut pending) {
                done = true;
                break;
            }
        }
        (pending.into_iter().collect(), done)
    }

    fn deltas(chunks: &[StreamChunk]) -> Vec<ToolCallDelta> {
        chunks
            .iter()
            .filter_map(|chunk| match chunk {
                StreamChunk::ToolCallDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_plain_content() {
        let (chunks, done) = parse(&[r#"{"choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#]);
        assert!(!done);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], StreamChunk::Content(text) if text == "你好"));
    }

    #[test]
    fn parses_tool_call_fragments_without_leaking_into_content() {
        // OpenAI / DeepSeek 的真实分片形态：id 与 name 只在首片出现，arguments 逐片追加
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"README.md\"}"}}]}}]}"#,
        ]);

        let deltas = deltas(&chunks);
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].id.as_deref(), Some("call_1"));
        assert_eq!(deltas[0].name.as_deref(), Some("read_file"));
        assert_eq!(deltas[1].arguments.as_deref(), Some("{\"path\":"));
        assert_eq!(deltas[2].arguments.as_deref(), Some("\"README.md\"}"));

        // 工具增量绝不能混进正文
        assert!(chunks
            .iter()
            .all(|c| !matches!(c, StreamChunk::Content(_))));
    }

    #[test]
    fn assembles_fragments_into_a_usable_call() {
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\":\"a.txt\","}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"content\":\"hi\"}"}}]}}]}"#,
        ]);
        let mut acc = crate::agent::harness::accumulate::ToolCallAccumulator::default();
        for delta in deltas(&chunks) {
            acc.push(delta);
        }
        let calls = acc.finish("stream-1");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert_eq!(calls[0].arguments["content"], "hi");
        assert!(calls[0].parse_error.is_none());
    }

    #[test]
    fn keeps_parallel_tool_call_indexes_separate() {
        // SSE 的 data 行必须保持单行（多行会被 SSE 规范拆成多个 data: 行）
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        ]);
        let mut acc = crate::agent::harness::accumulate::ToolCallAccumulator::default();
        for delta in deltas(&chunks) {
            acc.push(delta);
        }
        let calls = acc.finish("s");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[1].name, "list_dir");
    }

    #[test]
    fn parses_reasoning_and_tool_calls_together() {
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"reasoning_content":"我想想","tool_calls":[{"index":0,"id":"c","function":{"name":"get_current_time","arguments":"{}"}}]}}]}"#,
        ]);
        assert!(matches!(&chunks[0], StreamChunk::Thinking(text) if text == "我想想"));
        assert_eq!(deltas(&chunks).len(), 1);
    }

    #[test]
    fn handles_done_marker() {
        let (_, done) = parse(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#, "[DONE]"]);
        assert!(done);
    }

    #[test]
    fn ignores_malformed_json_without_panicking() {
        let (chunks, done) = parse(&["{not json", r#"{"choices":[]}"#]);
        assert!(!done);
        assert!(chunks.is_empty());
    }

    #[test]
    fn think_tags_inside_content_are_split() {
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"content":"<think>推理</think>答案"}}]}"#,
        ]);
        assert!(matches!(&chunks[0], StreamChunk::Thinking(t) if t == "推理"));
        assert!(matches!(&chunks[1], StreamChunk::Content(t) if t == "答案"));
    }
}
