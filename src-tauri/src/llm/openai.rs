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
        let stream = futures::stream::unfold(
            (byte_stream, SseState::default()),
            |(mut byte_stream, mut state)| async move {
                use futures::StreamExt;
                loop {
                    // 1. 先把已解析出的 chunk 发完（收尾顺序：内容 → 错误）
                    if let Some(chunk) = state.pending.pop_front() {
                        return Some((Ok(chunk), (byte_stream, state)));
                    }

                    // 2. 提供商在流内报的错：等上面队列排空后再上报，
                    //    否则半截答案会被一句报错吞掉
                    if let Some(message) = state.error.take() {
                        state.finished = true;
                        return Some((Err(anyhow!(message)), (byte_stream, state)));
                    }

                    // 3. 流已结束（[DONE] 或对端关闭）且无残留内容
                    if state.finished {
                        return None;
                    }

                    // 4. 在字节缓冲中查找完整事件分隔符（LF 或 CRLF）
                    if let Some(end) = find_event_separator(&state.buffer) {
                        let event_bytes: Vec<u8> = state.buffer.drain(..end).collect();
                        // 只解码完整事件，多字节字符不会在分隔符处被切断
                        let event = String::from_utf8_lossy(&event_bytes);
                        state.handle_event(&event);
                        continue;
                    }

                    // 5. 从流中读取更多数据
                    match byte_stream.next().await {
                        Some(Ok(bytes)) => {
                            state.buffer.extend_from_slice(&bytes);
                        }
                        Some(Err(e)) => {
                            state.finished = true;
                            return Some((
                                Err(anyhow!("Stream error: {}", e)),
                                (byte_stream, state),
                            ));
                        }
                        None => {
                            // 对端关闭：flush 残余的不完整事件。
                            // **必须继续走上面的循环**而不是直接 return None——
                            // 最后一个事件（常常正好是 finish_reason 或最后一片
                            // 工具参数）否则会被解析出来后原地丢掉。
                            if !state.buffer.is_empty() {
                                let event_bytes = std::mem::take(&mut state.buffer);
                                let event = String::from_utf8_lossy(&event_bytes).to_string();
                                state.handle_event(&event);
                            }
                            // 暂存的标签前缀按当前状态吐出（绝不吞掉最后一个分片）
                            state.flush_tag_carry();
                            state.finished = true;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

/// SSE 解析状态（`futures::stream::unfold` 的累积状态）
#[derive(Default)]
struct SseState {
    /// 未凑齐一个完整事件的字节
    buffer: Vec<u8>,
    /// `<think>` 标签状态：0=正常, 1=思考中
    think_state: u8,
    /// 可能被分片切断的标签前缀（`<thi` / `</thi`…）：留给下一个事件再判定，
    /// 否则正文里会漏出标签文本
    tag_carry: String,
    /// 已解析、待下发的 chunk
    pending: VecDeque<StreamChunk>,
    /// 对端已结束
    finished: bool,
    /// 提供商在流内报告的错误（延迟到 pending 排空后上报）
    error: Option<String>,
}

impl SseState {
    /// 解析一个完整 SSE 事件，把结果并入自身状态
    fn handle_event(&mut self, event: &str) {
        match parse_sse_event(
            event,
            &mut self.think_state,
            &mut self.tag_carry,
            &mut self.pending,
        ) {
            Ok(true) => self.finished = true,
            Ok(false) => {}
            // 首个错误胜出：后续事件通常只是同一故障的重复
            Err(message) => {
                if self.error.is_none() {
                    self.error = Some(message.to_string());
                }
            }
        }
    }

    /// 流结束（EOF）时把暂存的标签前缀按当前状态吐出，绝不丢内容
    fn flush_tag_carry(&mut self) {
        if self.tag_carry.is_empty() {
            return;
        }
        let tail = std::mem::take(&mut self.tag_carry);
        if self.think_state == 0 {
            self.pending.push_back(StreamChunk::Content(tail));
        } else {
            self.pending.push_back(StreamChunk::Thinking(tail));
        }
    }
}

/// 在字节缓冲中查找事件结束位置（返回值可直接用于 `drain(..end)`）
///
/// SSE 规范允许 `\n` 与 `\r\n` 两种行结束符。只认 `\n\n` 的话，使用 CRLF 的
/// 网关（大量 Java 服务/代理）永远凑不出"完整事件"，整个响应会被缓冲到
/// 连接关闭才解析——界面全程没有流式输出，长回复还会退化成 O(n²) 扫描。
fn find_event_separator(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|pos| pos + 2);
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 解析一个完整 SSE 事件（可能包含多行 data:）
///
/// 返回值：
/// - `Ok(true)` 收到 `[DONE]` 终止标记
/// - `Ok(false)` 正常（含"这一行不是我们能识别的格式"，已记诊断日志）
/// - `Err(msg)` 提供商在流内报错（如 `{"error":{"message":...}}`）
///
/// 历史实现的坑：只认 `"data: "`（带空格）、JSON 解析失败一律静默丢弃。
/// 于是网关用 `data:{...}` 或直接在 200 响应里回一段错误 JSON 时，
/// 整个响应会被悄悄吃掉——上层看到的是"既没正文也没工具调用"的空回合。
fn parse_sse_event(
    event: &str,
    think_state: &mut u8,
    tag_carry: &mut String,
    pending: &mut VecDeque<StreamChunk>,
) -> Result<bool> {
    let mut done = false;

    // 非标准网关会在 200 响应里直接回一段裸 JSON（没有 `data:` 前缀），
    // 例如 `{"error":{"message":"upstream timeout"}}`。只按 data: 行解析时
    // 这类载荷会被整段忽略，上层看到的是"空回合"，真正的错误无处可见。
    // 这里给裸 JSON 补上 data: 前缀后走同一条解析路径。
    let bare = event.trim();
    let has_data_line = event
        .lines()
        .any(|line| line.trim_start().starts_with("data:"));
    let normalized;
    let event = if !has_data_line && (bare.starts_with('{') || bare.starts_with('[')) {
        normalized = format!("data: {}", bare.replace(['\r', '\n'], " "));
        normalized.as_str()
    } else {
        event
    };

    for line in event.lines() {
        // `data:` 与 `data: ` 两种写法都接受（部分网关不带空格）
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            done = true;
            continue;
        }

        match serde_json::from_str::<ChatChunk>(data) {
            Ok(chunk) => {
                if chunk.choices.is_empty() {
                    // 空 choices 的帧有两类：usage/保活统计（忽略），以及
                    // `{"error":{...}}` 这类会被 `#[serde(default)]` 解析成空
                    // choices 的错误载荷（必须上报，否则真错误被静默吞掉）
                    if let Some(message) = extract_stream_error(data) {
                        return Err(anyhow!("LLM 流式响应报错：{}", message));
                    }
                    continue;
                }
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
                            parse_content_chunks(content, think_state, tag_carry, pending);
                        }
                    }
                    // 工具调用增量：只入队交给上层累积，绝不混入正文
                    if let Some(calls) = &choice.delta.tool_calls {
                        for call in calls {
                            pending.push_back(StreamChunk::ToolCallDelta(call.clone().into()));
                        }
                    }
                    // 结束原因必须带上去：`length` 意味着输出被 max_tokens 截断，
                    // runner 据此才能区分"说完了"和"被切断了"
                    if let Some(reason) = &choice.finish_reason {
                        if !reason.is_empty() {
                            pending.push_back(StreamChunk::Finish(reason.clone()));
                        }
                    }
                }
            }
            Err(parse_error) => {
                // 不是标准 chunk：先看是不是提供商塞进来的错误对象
                if let Some(message) = extract_stream_error(data) {
                    return Err(anyhow!("LLM 流式响应报错：{}", message));
                }
                // 其余情况如实记诊断，但不打断流（可能是 keep-alive 之类的噪声）
                eprintln!(
                    "[llm] 忽略无法解析的流式事件（{}）：{}",
                    parse_error,
                    truncate_for_log(data)
                );
            }
        }
    }

    Ok(done)
}

/// 从非 chunk 载荷里提取提供商的错误信息
///
/// 兼容 `{"error":{"message":...}}` 与 `{"error":"..."}` 两种形态；
/// 不是错误载荷时返回 `None`。
fn extract_stream_error(data: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let error = value.get("error")?;
    // `{"error":null}` / `{"error":{}}` 是网关的占位/统计帧，不是真错误；
    // 当成错误会把健康的流当场打断（真实缺陷：用户看到"提供商未给出原因"）
    if error.is_null() || error.as_object().is_some_and(|object| object.is_empty()) {
        return None;
    }
    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| error.as_str())
        .unwrap_or("（提供商未给出原因）");
    Some(message.to_string())
}

/// 日志里的事件预览（多字节安全，避免把整段响应刷进终端）
fn truncate_for_log(text: &str) -> String {
    let truncated: String = text.chars().take(300).collect();
    if truncated.chars().count() < text.chars().count() {
        format!("{}…", truncated)
    } else {
        truncated
    }
}

/// 解析内容中的 `<think>...</think>` 标签，将结果追加到 pending 列表
///
/// `think_state` 是跨 chunk 持久的状态：0=正常模式, 1=在思考标签内。
/// `tag_carry` 暂存"可能是标签前缀"的尾巴：网关按子词切流时 `<thi` + `nk>`
/// 会跨 chunk，不暂存的话标签文本会漏进正文、后续 `</think>` 也无法纠正。
fn parse_content_chunks(
    content: &str,
    think_state: &mut u8,
    tag_carry: &mut String,
    pending: &mut VecDeque<StreamChunk>,
) {
    let mut text = std::mem::take(tag_carry);
    text.push_str(content);
    let mut remaining = text.as_str();

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
                // 没有完整标签：把"可能是 `<think>` 前缀"的尾巴留给下一个 chunk
                let (emit, tail) = split_possible_tag_prefix(remaining, "<think>");
                if !emit.is_empty() {
                    pending.push_back(StreamChunk::Content(emit.to_string()));
                }
                *tag_carry = tail.to_string();
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
                let (emit, tail) = split_possible_tag_prefix(remaining, "</think>");
                if !emit.is_empty() {
                    pending.push_back(StreamChunk::Thinking(emit.to_string()));
                }
                *tag_carry = tail.to_string();
                return;
            }
        }
    }
}

/// 若字符串尾部是 `tag` 的真前缀（如 `<thi`），把它留给下一个分片
///
/// 返回 `(可立即输出的部分, 暂存的尾缀)`；找不到这样的尾缀时原样返回全文。
fn split_possible_tag_prefix<'a>(text: &'a str, tag: &str) -> (&'a str, &'a str) {
    // 只可能是真前缀：长度严格小于 tag；允许暂存整个 text（text 本身就是前缀时）
    let max_carry = (tag.len() - 1).min(text.len());
    for len in (1..=max_carry).rev() {
        if !text.is_char_boundary(text.len() - len) {
            continue;
        }
        let suffix = &text[text.len() - len..];
        if tag.starts_with(suffix) {
            return (&text[..text.len() - len], suffix);
        }
    }
    (text, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ToolCallDelta;

    /// 把若干 SSE 事件喂给解析器，返回收集到的 chunk
    fn parse(events: &[&str]) -> (Vec<StreamChunk>, bool) {
        let mut think_state = 0u8;
        let mut tag_carry = String::new();
        let mut pending: VecDeque<StreamChunk> = VecDeque::new();
        let mut done = false;
        for event in events {
            match parse_sse_event(
                &format!("data: {}\n\n", event),
                &mut think_state,
                &mut tag_carry,
                &mut pending,
            ) {
                Ok(true) => {
                    done = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => panic!("不该报错的事件：{} -> {}", event, e),
            }
        }
        (pending.into_iter().collect(), done)
    }

    /// 单事件解析结果（断言"错误路径"用）
    fn parse_one(event: &str) -> Result<bool> {
        let mut think_state = 0u8;
        let mut tag_carry = String::new();
        let mut pending: VecDeque<StreamChunk> = VecDeque::new();
        parse_sse_event(
            &format!("data: {}\n\n", event),
            &mut think_state,
            &mut tag_carry,
            &mut pending,
        )
    }

    /// 非标准 `data:{...}`（不带空格）也必须能解析
    ///
    /// 历史实现对这种写法整段丢弃——整个响应会变成"什么都没有"
    #[test]
    fn accepts_data_prefix_without_space() {
        let mut think_state = 0u8;
        let mut tag_carry = String::new();
        let mut pending: VecDeque<StreamChunk> = VecDeque::new();
        let done = parse_sse_event(
            "data:{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            &mut think_state,
            &mut tag_carry,
            &mut pending,
        )
        .unwrap();
        assert!(!done);
        assert!(matches!(pending.front(), Some(StreamChunk::Content(t)) if t == "hi"));
    }

    /// 提供商在流内报错时必须上报，绝不能静默丢弃（否则就是一个空回合）
    #[test]
    fn provider_stream_error_is_surfaced() {
        let err =
            parse_one(r#"{"error":{"message":"upstream timeout","type":"server_error"}}"#)
                .unwrap_err();
        assert!(err.to_string().contains("upstream timeout"), "{err}");

        // 字符串形态的 error 同样识别
        let err = parse_one(r#"{"error":"rate limited"}"#).unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");

        // 普通噪声载荷不报错，只是被忽略
        assert!(parse_one(r#"{"noise":true}"#).is_ok());
        // 网关的占位帧（error 为 null / 空对象）不能当成错误打断健康的流
        assert!(parse_one(r#"{"error":null}"#).is_ok());
        assert!(parse_one(r#"{"error":{}}"#).is_ok());
        // 只有带内容的 error 才是真错误
        assert!(parse_one(r#"{"error":{"code":500,"message":"boom"}}"#).is_err());
    }

    /// 200 + 裸 JSON（没有 `data:` 前缀）也必须能解析：
    /// 错误体要上报，正常 chunk 要照常入队
    #[test]
    fn bare_json_payload_without_data_prefix_is_parsed() {
        let mut state = SseState::default();
        state.handle_event(r#"{"error":{"message":"upstream timeout"}}"#);
        let error = state.error.as_deref().unwrap_or_default();
        assert!(error.contains("upstream timeout"), "{error}");

        let mut state = SseState::default();
        state.handle_event(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        assert!(
            matches!(state.pending.front(), Some(StreamChunk::Content(t)) if t == "hi"),
            "{:?}",
            state.pending
        );
    }

    /// CRLF 分隔符必须被识别，否则整个响应要等到连接关闭才会解析
    #[test]
    fn crlf_event_separators_are_recognized() {
        let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\n";
        let end = find_event_separator(event).expect("CRLF 分隔符必须被识别");
        assert_eq!(end, event.len());
        let text = String::from_utf8_lossy(&event[..end]);
        let mut think_state = 0u8;
        let mut tag_carry = String::new();
        let mut pending: VecDeque<StreamChunk> = VecDeque::new();
        parse_sse_event(&text, &mut think_state, &mut tag_carry, &mut pending).unwrap();
        assert!(matches!(pending.front(), Some(StreamChunk::Content(t)) if t == "hi"));

        // 混用分隔符时取最早结束的那个事件
        let mixed = b"data: a\n\ndata: b\r\n\r\n";
        assert_eq!(find_event_separator(mixed), Some(9));
    }

    /// `<think>` 标签被网关切在分片边界时必须暂存，不能把 `<thi` 当正文输出
    #[test]
    fn think_tag_split_across_chunks_is_carried() {
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"content":"<thi"}}]}"#,
            r#"{"choices":[{"delta":{"content":"nk>推理</think>答案"}}]}"#,
        ]);
        assert!(matches!(&chunks[0], StreamChunk::Thinking(t) if t == "推理"), "{chunks:?}");
        assert!(matches!(&chunks[1], StreamChunk::Content(t) if t == "答案"), "{chunks:?}");
        // 正文里绝不出现标签碎片
        for chunk in &chunks {
            if let StreamChunk::Content(text) = chunk {
                assert!(!text.contains("<thi"), "{text}");
            }
        }

        // 结束标签被拆分同样要暂存
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"content":"<think>推理</thi"}}]}"#,
            r#"{"choices":[{"delta":{"content":"nk>答案"}}]}"#,
        ]);
        assert!(matches!(&chunks[0], StreamChunk::Thinking(t) if t == "推理"), "{chunks:?}");
        assert!(matches!(&chunks[1], StreamChunk::Content(t) if t == "答案"), "{chunks:?}");
    }

    /// 普通文本尾部恰好是 `<` 这类可能前缀时也不能丢字符
    #[test]
    fn trailing_lt_is_not_lost_when_no_tag_follows() {
        let (chunks, _) = parse(&[
            r#"{"choices":[{"delta":{"content":"1 < 2"}}]}"#,
        ]);
        // 本次事件里 `< 2` 不是标签前缀，直接输出
        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Content(t) if t == "1 < 2")),
            "{chunks:?}"
        );

        // 只有孤立的 `<`：暂存（已输出的部分不受影响），由 EOF flush 兜底
        let mut state = SseState::default();
        state.handle_event("data: {\"choices\":[{\"delta\":{\"content\":\"abc<\"}}]}\n\n");
        assert!(
            matches!(state.pending.front(), Some(StreamChunk::Content(t)) if t == "abc"),
            "{:?}",
            state.pending
        );
        assert_eq!(state.tag_carry, "<", "可能是标签前缀的尾巴必须暂存");
        state.flush_tag_carry();
        assert!(
            state
                .pending
                .iter()
                .any(|c| matches!(c, StreamChunk::Content(t) if t == "<")),
            "{:?}",
            state.pending
        );
    }

    /// `finish_reason` 必须带上去：`length` 意味着输出被 max_tokens 截断
    #[test]
    fn finish_reason_is_forwarded() {
        let (chunks, _) = parse(&[r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#]);
        assert!(matches!(chunks.first(), Some(StreamChunk::Finish(r)) if r == "length"));
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
