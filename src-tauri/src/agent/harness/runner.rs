use anyhow::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::llm::backend::ChatBackend;
use crate::llm::types::{LlmMessage, StreamChunk, ToolCall, ToolSchema};

use super::accumulate::{AssembledCall, ToolCallAccumulator};
use super::registry::ToolRegistry;
use super::traits::{
    truncate_text, ApprovalRequest, Approver, EventSink, ToolCtx, ToolDecision, ToolLimits,
    ToolServices,
};

/// 事件名的唯一来源是 `harness::traits`，这里只转出 runner 自己发出的两个
pub use super::traits::{EVENT_TOOL_RESULT, EVENT_TOOL_START};

/// UI 卡片里的结果预览长度上限
const PREVIEW_CHARS: usize = 600;
/// 事件里参数预览长度上限
const ARG_PREVIEW_CHARS: usize = 400;

/// 外层超时相对工具自身超时的宽限
///
/// `call_timeout` 是给**工具自己**用的截止时间（`run_command` 会据此在超时前
/// 主动收手并把已收到的输出带回来）。runner 的外层超时只是兜底，防止某个工具
/// 彻底卡死；给出的宽限要足够让工具走完"收尾 + 发事件"的路径。
const CALL_TIMEOUT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// 模型返回"既没有正文、也没有工具调用"时的重试次数
const EMPTY_TURN_RETRIES: usize = 2;

/// 空回合时推给模型的提醒
///
/// 关键是**给出可执行的下一步**：`length` 截断的场合让它把动作拆小
/// （一次写一个小文件），而不是原样重试同一个巨型输出。
/// `tools_offered` 为 false 表示这一轮已经不给工具了（步数用尽），
/// 此时只能要一段文字收尾，不能再要求它调用工具。
fn empty_turn_hint(finish_reason: Option<&str>, tools_offered: bool) -> String {
    let cause = match finish_reason {
        Some("length") => {
            "上一次响应因为达到输出长度上限（max_tokens）被截断，没有产生任何内容。\
             请把动作拆小：一次只写一个文件、或先写文件的一部分，不要试图一次性输出整份长文件。"
        }
        Some("content_filter") => "上一次响应被提供商的内容过滤拦截，没有产生任何内容。",
        Some(other) => {
            return format!(
                "上一次响应被提供商以「{}」结束，没有产生任何正文或工具调用。{}",
                other,
                if tools_offered {
                    "请继续：要么调用工具推进计划，要么用文字汇报当前进展。"
                } else {
                    "请直接用文字汇报当前进展与未完成的部分。"
                }
            )
        }
        None => "上一次响应是空的（既没有正文，也没有工具调用）。",
    };
    let next_step = if tools_offered {
        "请继续：要么调用工具推进计划，要么用文字汇报当前进展。"
    } else {
        "请直接用文字汇报当前进展与未完成的部分。"
    };
    format!("{}{}", cause, next_step)
}

/// 重试若干次仍为空时写给用户的说明
///
/// 宁可显式告诉用户"这一轮什么都没发生、原因是什么"，也不要落一条空白消息。
fn empty_turn_notice(finish_reason: Option<&str>) -> String {
    let detail = finish_reason.unwrap_or("未给出原因");
    format!(
        "（本轮没有收到模型的任何输出，任务尚未推进。提供商返回的结束原因：{}。\
         可以重试一次，或把任务拆得更小（例如分文件实现）；若反复出现，\
         请检查设置的 max_tokens 是否过小。）",
        detail
    )
}

/// 一次工具调用的完整记录（落库 + UI 轨迹）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRecord {
    pub call_id: String,
    pub tool: String,
    pub tool_label: String,
    pub arguments_json: String,
    pub step: usize,
    pub status: String,
    pub result_preview: Option<String>,
    pub error: Option<String>,
    pub truncated: bool,
    pub duration_ms: i64,
    pub approval: Option<String>,
    /// 工具声明的额外 token 估算（子代理开销），随记录一起展示
    #[serde(default)]
    pub extra_tokens: usize,
}

/// 工具循环的产出
pub struct HarnessOutcome {
    /// 用户可见的正文（含工具调用前后的全部文字）
    pub content: String,
    pub invocations: Vec<InvocationRecord>,
    /// 实际执行过的工具轮数
    pub steps: usize,
    /// 是否用尽了步数上限（最后一轮被强制收尾）
    pub hit_step_limit: bool,
    /// 工具自己声明的额外 token 估算（子代理等"隐藏开销"），用于统计展示
    pub extra_tokens: usize,
    /// 模型**始终没有产出任何内容**（重试后仍然为空）
    ///
    /// 此时 `content` 里只有一句给用户看的说明；调用方据此判断
    /// "这一轮其实什么都没做成"（子代理会把它当成失败，而不是一条结论）。
    pub empty_turn: bool,
}

/// 一次生成所需的全部运行参数
pub struct HarnessRun<'a> {
    pub backend: &'a dyn ChatBackend,
    pub registry: &'a ToolRegistry,
    /// 服务句柄**按值持有**：子代理需要在"父级服务的只读克隆"上再跑一次循环，
    /// 引用版本没法构造出这种临时值。克隆发生在每次生成开始时，代价可忽略。
    pub services: ToolServices,
    pub session_id: &'a str,
    pub stream_id: &'a str,
    pub cancel: Arc<AtomicBool>,
    pub emit: Arc<dyn EventSink>,
    pub approver: Arc<dyn Approver>,
    /// 是否向模型暴露工具（悬浮窗恒为 false）
    pub tools_enabled: bool,
    /// 配置里免审批的工具名
    pub auto_approve: Vec<String>,
    pub limits: ToolLimits,
    pub max_steps: usize,
}

impl HarnessRun<'_> {
    /// 执行工具循环
    ///
    /// 不变式：
    /// 1. 只有 `StreamChunk::Content` 会经 `on_chunk` 外发——工具参数与结果
    ///    绝不混进正文（否则摘要、token 统计、记忆提取全会吃到 JSON）。
    /// 2. 每一轮生成前、每次工具调用前都检查 `cancel`。
    /// 3. 工具失败不冒泡为 `Err`，而是回灌给模型让它自行修正。
    /// 4. 步数耗尽时追加一次**不带工具**的生成，保证用户总能拿到自然语言回答。
    pub async fn execute<F, G>(
        self,
        mut messages: Vec<LlmMessage>,
        on_chunk: F,
        on_thinking: G,
    ) -> Result<HarnessOutcome>
    where
        F: Fn(&str) + Send + Sync,
        G: Fn(&str) + Send + Sync,
    {
        let schemas: Vec<ToolSchema> = if self.tools_enabled && !self.registry.is_empty() {
            self.registry.schemas(self.services.mode)
        } else {
            Vec::new()
        };

        let mut content = String::new();
        let mut invocations: Vec<InvocationRecord> = Vec::new();
        let mut extra_tokens = 0usize;
        let mut session_grants: HashSet<String> = HashSet::new();
        let mut tool_rounds = 0usize;
        let mut hit_step_limit = false;
        // 模型"什么都没说也没做"时的重试次数（见 EMPTY_TURN_RETRIES）
        let mut empty_retries = 0usize;
        let mut empty_turn = false;

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }

            // 步数用尽后不再提供工具，强制模型收尾
            let offer_tools = !schemas.is_empty() && tool_rounds < self.max_steps;
            if !offer_tools && tool_rounds >= self.max_steps && !schemas.is_empty() {
                hit_step_limit = true;
            }
            let tools_arg = if offer_tools { Some(schemas.clone()) } else { None };

            let mut stream = self.backend.chat_stream(messages.clone(), tools_arg).await?;
            let mut accumulator = ToolCallAccumulator::default();
            let mut step_text = String::new();
            let mut finish_reason: Option<String> = None;

            let mut stream_error: Option<String> = None;
            while let Some(chunk) = stream.next().await {
                if self.cancel.load(Ordering::Relaxed) {
                    break;
                }
                match chunk {
                    Ok(StreamChunk::Content(text)) => {
                        step_text.push_str(&text);
                        content.push_str(&text);
                        on_chunk(&text);
                    }
                    Ok(StreamChunk::Thinking(text)) => on_thinking(&text),
                    Ok(StreamChunk::ToolCallDelta(delta)) => accumulator.push(delta),
                    // 提供商声明的结束原因：`length` = 被 max_tokens 截断
                    Ok(StreamChunk::Finish(reason)) => finish_reason = Some(reason),
                    Err(e) => {
                        stream_error = Some(e.to_string());
                        break;
                    }
                }
            }

            if self.cancel.load(Ordering::Relaxed) {
                break;
            }

            if let Some(message) = stream_error {
                // 网络中断时把**已经流出的正文**保留下来（用户已经看到了），
                // 补一句说明后正常收尾。工具调用不再组装/执行：参数很可能被
                // 截断，拿半截 JSON 去调工具比报错更危险。
                if content.trim().is_empty() {
                    return Err(anyhow::anyhow!("流式响应中断：{}", message));
                }
                let note = format!("\n\n（连接中断：{}。以上为已生成的部分内容）", message);
                content.push_str(&note);
                on_chunk(&note);
                break;
            }

            let calls = accumulator.finish(self.stream_id);
            if calls.is_empty() {
                // ─── 空回合保护 ───
                //
                // 现实里常见的一幕：推理模型把 max_tokens 预算全花在思考上，
                // 或者网关在流里回了半截就断开，于是这一轮既没有正文、也没有
                // 工具调用。历史实现把这当成"模型说完了"，直接结束生成——
                // 用户只看到前面几张工具卡片，任务却停在半路（真实报障：
                // "请求开始实现之后只调用了工具就结束本轮了"）。
                //
                // 这里先把空回合当成"没说完"：推一条提醒让模型接着做，
                // 重试若干次仍为空才收手，并且**留下一句可见的说明**，
                // 绝不再产出空白回合。
                if content.trim().is_empty() {
                    if empty_retries < EMPTY_TURN_RETRIES {
                        empty_retries += 1;
                        eprintln!(
                            "[harness] 空回合（第 {}/{} 次）finish_reason={:?}，提示模型继续",
                            empty_retries, EMPTY_TURN_RETRIES, finish_reason
                        );
                        messages.push(LlmMessage::user(empty_turn_hint(
                            finish_reason.as_deref(),
                            offer_tools,
                        )));
                        continue;
                    }
                    eprintln!(
                        "[harness] 连续 {} 次空回合，放弃并如实告知用户 finish_reason={:?}",
                        EMPTY_TURN_RETRIES, finish_reason
                    );
                    empty_turn = true;
                    content = empty_turn_notice(finish_reason.as_deref());
                    on_chunk(&content);
                }
                break;
            }

            // 把这一轮正文与工具请求写回历史
            let tool_calls: Vec<ToolCall> = calls
                .iter()
                .map(|c| ToolCall::new(c.id.clone(), c.name.clone(), c.raw_arguments.clone()))
                .collect();
            messages.push(LlmMessage::assistant_tool_calls(step_text.clone(), tool_calls));

            // 正文里插入空行，使流式渲染结果与最终落库内容一致
            if !step_text.is_empty() {
                content.push_str("\n\n");
                on_chunk("\n\n");
            }

            // 只读工具可并行；有副作用的工具串行执行（审批必须串行）
            let mut index = 0usize;
            while index < calls.len() {
                if self.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let is_read_only = self
                    .registry
                    .permission_of(&calls[index].name)
                    .map(|p| p.is_read_only())
                    .unwrap_or(false);

                if is_read_only {
                    let mut batch = Vec::new();
                    while index < calls.len()
                        && self
                            .registry
                            .permission_of(&calls[index].name)
                            .map(|p| p.is_read_only())
                            .unwrap_or(false)
                    {
                        batch.push(calls[index].clone());
                        index += 1;
                    }
                    let futures = batch.iter().map(|call| {
                        self.execute_call(call, tool_rounds, Some(&session_grants))
                    });
                    let results = futures::future::join_all(futures).await;
                    for (record, message) in results {
                        extra_tokens += record.extra_tokens;
                        invocations.push(record);
                        messages.push(message);
                    }
                } else {
                    let call = calls[index].clone();
                    index += 1;
                    let (record, message) = self
                        .execute_call(&call, tool_rounds, Some(&session_grants))
                        .await;
                    // 「本次会话允许」在后续同类调用中生效
                    if record.approval.as_deref() == Some("allow_session") {
                        session_grants.insert(record.tool.clone());
                    }
                    extra_tokens += record.extra_tokens;
                    invocations.push(record);
                    messages.push(message);
                }
            }

            tool_rounds += 1;
        }

        Ok(HarnessOutcome {
            content,
            invocations,
            steps: tool_rounds,
            hit_step_limit,
            extra_tokens,
            empty_turn,
        })
    }

    /// 执行单个工具调用，返回记录与回灌给模型的消息
    async fn execute_call(
        &self,
        call: &AssembledCall,
        step: usize,
        grants: Option<&HashSet<String>>,
    ) -> (InvocationRecord, LlmMessage) {
        let started = Instant::now();
        let descriptor = self.registry.descriptor_of(&call.name);
        let tool_label = descriptor
            .as_ref()
            .map(|d| d.label.to_string())
            .unwrap_or_else(|| call.name.clone());

        let mut record = InvocationRecord {
            extra_tokens: 0,
            call_id: call.id.clone(),
            tool: call.name.clone(),
            tool_label: tool_label.clone(),
            arguments_json: if call.raw_arguments.is_empty() {
                "{}".to_string()
            } else {
                call.raw_arguments.clone()
            },
            step,
            status: "running".to_string(),
            result_preview: None,
            error: None,
            truncated: false,
            duration_ms: 0,
            approval: None,
        };

        // ─── 参数解析失败：直接回灌，让模型自我修正 ───
        if let Some(err) = &call.parse_error {
            return self.finish_error(record, started, format!("参数不合法：{}", err), "error");
        }

        // ─── 工具不存在或当前模式不可见 ───
        let Some(tool) = self.registry.find(&call.name, self.services.mode) else {
            let available = self.registry.visible_names(self.services.mode).join(", ");
            let hint = if self.registry.descriptor_of(&call.name).is_some() {
                format!(
                    "工具「{}」在当前模式下不可用（当前模式：{:?}）",
                    call.name, self.services.mode
                )
            } else {
                format!("不存在名为「{}」的工具", call.name)
            };
            return self.finish_error(
                record,
                started,
                format!("{}。可用工具：{}", hint, available),
                "error",
            );
        };

        let permission = tool.descriptor().permission;
        let args_preview = truncate_text(&call.raw_arguments, ARG_PREVIEW_CHARS).0;

        // 工具上下文要在**审批之前**建好：审批摘要允许做只读检查（例如统计删除规模），
        // 因此它同样需要 workspaces / limits 这些服务句柄
        let cx = ToolCtx {
            session_id: self.session_id,
            stream_id: self.stream_id,
            call_id: &call.id,
            step,
            cancel: self.cancel.clone(),
            services: &self.services,
            limits: self.limits,
            emit: self.emit.clone(),
            approver: self.approver.clone(),
        };

        self.emit.emit(
            EVENT_TOOL_START,
            json!({
                "session_id": self.session_id,
                "stream_id": self.stream_id,
                "call_id": call.id,
                "tool": call.name,
                "tool_label": tool_label,
                "args_preview": args_preview,
                "args": call.arguments,
                "permission": permission.as_str(),
                "risk": permission.risk_label(),
                "step": step,
                "status": "running",
            }),
        );

        // ─── 审批 ───
        if permission.requires_approval() {
            let pre_granted = grants.map(|g| g.contains(&call.name)).unwrap_or(false)
                || self.auto_approve.iter().any(|name| name == &call.name);

            if pre_granted {
                record.approval = Some("auto".to_string());
            } else {
                let decision = self
                    .approver
                    .request(ApprovalRequest {
                        session_id: self.session_id.to_string(),
                        stream_id: self.stream_id.to_string(),
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        tool_label: tool_label.clone(),
                        args: call.arguments.clone(),
                        permission,
                        timeout: self.limits.approval_timeout,
                        // 只读检查：工具按参数与服务算一段人类可读的说明
                        summary: tool.approval_summary(&call.arguments, &cx),
                    })
                    .await;
                record.approval = Some(decision.as_str().to_string());
                if decision == ToolDecision::Deny {
                    return self.finish_error(
                        record,
                        started,
                        "用户拒绝了这次调用，请勿重复请求同一操作，改为向用户说明情况".to_string(),
                        "denied",
                    );
                }
            }
        } else {
            record.approval = Some("auto".to_string());
        }

        // ─── 执行（超时 + 取消） ───
        let outcome = tokio::time::timeout(
            self.limits.call_timeout + CALL_TIMEOUT_GRACE,
            tool.call(call.arguments.clone(), &cx),
        )
        .await;

        match outcome {
            Ok(Ok(output)) => {
                // 兜底截断：工具自己没截干净时也不能把上下文撑爆
                let (body, truncated) = truncate_text(&output.content, self.limits.max_output_bytes);
                let truncated = truncated || output.truncated;
                let preview = output
                    .preview
                    .unwrap_or_else(|| truncate_text(&body, PREVIEW_CHARS).0);

                // 工具自己判定的状态（超时/取消/非零退出）优先于默认的 "ok"：
                // 内容仍然是**部分/失败结果**，保留它并如实记状态，模型与用户都能
                // 看到"已经跑到哪儿了"，比一句干巴巴的失败有用得多。
                record.status = output.status.as_str().to_string();
                record.truncated = truncated;
                record.result_preview = Some(preview.clone());
                record.error = output.status.error_note().map(|note| note.to_string());
                record.extra_tokens = output.extra_tokens;
                record.duration_ms = started.elapsed().as_millis() as i64;

                let message =
                    tool_message(&record.call_id, &record.tool, &record.status, &body);
                self.emit_result(&record, &preview);
                (record, message)
            }
            Ok(Err(e)) => {
                let message = e.to_string();
                // 用户中途停止：记成 cancelled，不再让模型继续
                let status = if self.cancel.load(Ordering::Relaxed) {
                    "cancelled"
                } else {
                    "error"
                };
                self.finish_error(record, started, message, status)
            }
            Err(_) => {
                // 走到这里说明工具连自己的截止时间都没守住（外层兜底超时）
                let timeout = (self.limits.call_timeout + CALL_TIMEOUT_GRACE).as_secs();
                self.finish_error(
                    record,
                    started,
                    format!("工具执行超时（{} 秒）且未返回任何输出，已被强制终止", timeout),
                    "timeout",
                )
            }
        }
    }

    fn emit_result(&self, record: &InvocationRecord, preview: &str) {
        self.emit.emit(
            EVENT_TOOL_RESULT,
            json!({
                "session_id": self.session_id,
                "stream_id": self.stream_id,
                "call_id": record.call_id,
                "tool": record.tool,
                "tool_label": record.tool_label,
                "status": record.status,
                "preview": preview,
                "duration_ms": record.duration_ms,
                "truncated": record.truncated,
            }),
        );
    }

    fn finish_error(
        &self,
        mut record: InvocationRecord,
        started: Instant,
        message: String,
        status: &str,
    ) -> (InvocationRecord, LlmMessage) {
        record.status = status.to_string();
        record.error = Some(message.clone());
        record.duration_ms = started.elapsed().as_millis() as i64;
        let preview = truncate_text(&message, PREVIEW_CHARS).0;
        record.result_preview = Some(preview.clone());
        self.emit_result(&record, &preview);
        let message_out = tool_message(&record.call_id, &record.tool, status, &message);
        (record, message_out)
    }
}

/// 构造回灌给模型的工具结果消息
///
/// 明确标注 `untrusted`：标签内的内容来自文件/命令/网络，属于**数据**而非指令，
/// 用于削弱提示注入的效力。
fn tool_message(call_id: &str, tool: &str, status: &str, body: &str) -> LlmMessage {
    LlmMessage::tool(
        call_id,
        format!(
            "<tool_result tool=\"{}\" status=\"{}\" untrusted=\"true\">\n{}\n</tool_result>",
            tool, status, body
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{
        AllowAllApprover, DenyAllApprover, Permission, Tool, ToolDescriptor,
        ToolInfo, ToolOutput, ToolStatus,
    };
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::llm::types::ToolSchema;
    use anyhow::anyhow;
    use futures::stream::Stream;
    use serde_json::Value;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Duration;

    // ─── 测试替身 ───────────────────────────────────────

    struct MockBackend {
        script: Mutex<Vec<Vec<StreamChunk>>>,
        seen_tools: Mutex<Vec<Option<Vec<ToolSchema>>>>,
        seen_messages: Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl MockBackend {
        fn new(script: Vec<Vec<StreamChunk>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                seen_tools: Mutex::new(Vec::new()),
                seen_messages: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl ChatBackend for MockBackend {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> Result<String> {
            Ok("mock".to_string())
        }

        async fn chat_stream(
            &self,
            messages: Vec<LlmMessage>,
            tools: Option<Vec<ToolSchema>>,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
            self.seen_tools.lock().unwrap().push(tools);
            self.seen_messages.lock().unwrap().push(messages);
            let next = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    vec![StreamChunk::Content("（无脚本）".to_string())]
                } else {
                    script.remove(0)
                }
            };
            Ok(Box::pin(futures::stream::iter(next.into_iter().map(Ok))))
        }
    }

    struct RecordingSink {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }
        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.clone())
                .collect()
        }
        fn payloads(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl EventSink for RecordingSink {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    struct MockTool {
        name: &'static str,
        permission: Permission,
        result: Result<String, String>,
        sleep_ms: u64,
        calls: Arc<Mutex<usize>>,
        /// 工具自行上报的状态（模拟"部分输出 + 超时/失败"）
        status: ToolStatus,
    }

    impl MockTool {
        fn new(name: &'static str, permission: Permission) -> (Arc<Self>, Arc<Mutex<usize>>) {
            let calls = Arc::new(Mutex::new(0));
            (
                Arc::new(Self {
                    name,
                    permission,
                    result: Ok("工具结果".to_string()),
                    sleep_ms: 0,
                    calls: calls.clone(),
                    status: ToolStatus::Ok,
                }),
                calls,
            )
        }

        /// 造一个会自行上报非 ok 状态的工具
        fn with_status(
            name: &'static str,
            permission: Permission,
            status: ToolStatus,
        ) -> (Arc<Self>, Arc<Mutex<usize>>) {
            let (mut tool, calls) = Self::new(name, permission);
            Arc::get_mut(&mut tool)
                .expect("刚构造的 Arc 必然是独占的")
                .status = status;
            (tool, calls)
        }
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                self.name,
                "测试工具",
                "测试用",
                self.permission,
                json!({"type": "object", "properties": {}}),
            )
        }

        async fn call(&self, _args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
            *self.calls.lock().unwrap() += 1;
            if self.sleep_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
            }
            cx.ensure_not_cancelled()?;
            match &self.result {
                Ok(text) => Ok(ToolOutput::text(text.clone()).with_status(self.status)),
                Err(e) => Err(anyhow!(e.clone())),
            }
        }
    }

    fn temp_workspace(tag: &str) -> (std::path::PathBuf, WorkspaceSet) {
        let dir = std::env::temp_dir().join(format!("konata-run-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ToolConfig::with_single_root(
            &dir,
            true,
            "测试",
        );
        let set = WorkspaceSet::from_config(&cfg, &dir);
        (dir, set)
    }

    fn tool_call_chunk(index: usize, id: &str, name: &str, args: &str) -> StreamChunk {
        StreamChunk::ToolCallDelta(crate::llm::types::ToolCallDelta {
            index: Some(index),
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            arguments: Some(args.to_string()),
        })
    }

    struct Fixture {
        dir: std::path::PathBuf,
        services: ToolServices,
        registry: ToolRegistry,
        sink: Arc<RecordingSink>,
        limits: ToolLimits,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(tools: Vec<Arc<dyn Tool>>, mode: ToolMode) -> Fixture {
        let (dir, workspaces) = temp_workspace("fx");
        Fixture {
            services: ToolServices::minimal(dir.clone(), workspaces, mode),
            registry: ToolRegistry::new(tools),
            sink: RecordingSink::new(),
            limits: ToolLimits {
                max_output_bytes: 64 * 1024,
                call_timeout: Duration::from_secs(5),
                approval_timeout: Duration::from_millis(50),
            },
            dir,
        }
    }

    fn run(
        fx: &Fixture,
        backend: &Arc<MockBackend>,
        approver: Arc<dyn Approver>,
        messages: Vec<LlmMessage>,
    ) -> HarnessOutcome {
        run_with(fx, backend, approver, messages, true, 8)
    }

    fn run_with(
        fx: &Fixture,
        backend: &Arc<MockBackend>,
        approver: Arc<dyn Approver>,
        messages: Vec<LlmMessage>,
        tools_enabled: bool,
        max_steps: usize,
    ) -> HarnessOutcome {
        let cancel = Arc::new(AtomicBool::new(false));
        let run = HarnessRun {
            backend: backend.as_ref(),
            registry: &fx.registry,
            services: fx.services.clone(),
            session_id: "s1",
            stream_id: "stream-1",
            cancel,
            emit: fx.sink.clone(),
            approver,
            tools_enabled,
            auto_approve: Vec::new(),
            limits: fx.limits,
            max_steps,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(run.execute(messages, |_| {}, |_| {}))
            .expect("harness run")
    }

    fn messages() -> Vec<LlmMessage> {
        vec![LlmMessage::system("sys"), LlmMessage::user("你好")]
    }

    // ─── 用例 ───────────────────────────────────────────

    #[test]
    fn plain_answer_without_tool_calls() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![vec![StreamChunk::Content("你好呀".to_string())]]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(outcome.content, "你好呀");
        assert!(outcome.invocations.is_empty());
        assert_eq!(outcome.steps, 0);
        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(fx.sink.names().is_empty());
    }

    /// 工具自行上报的状态必须覆盖默认的 "ok"，且部分结果要原样回灌给模型
    #[test]
    fn tool_reported_status_is_recorded_and_content_still_returned() {
        let (tool, _) = MockTool::with_status("slow", Permission::Read, ToolStatus::Timeout);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![
                tool_call_chunk(0, "c1", "slow", "{}"),
            ],
            vec![StreamChunk::Content("看到了部分输出。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(outcome.invocations.len(), 1);
        let record = &outcome.invocations[0];
        assert_eq!(record.status, "timeout");
        assert!(
            record.error.as_deref().unwrap_or("").contains("超时"),
            "{:?}",
            record.error
        );
        assert!(
            record
                .result_preview
                .as_deref()
                .unwrap_or("")
                .contains("工具结果"),
            "部分结果必须保留在预览里：{:?}",
            record.result_preview
        );

        // 回灌消息里带着真实状态与部分内容（模型据此修正策略）
        let seen = backend.seen_messages.lock().unwrap();
        let last = seen.last().expect("第二轮请求");
        let tool_msg = last
            .iter()
            .find(|m| m.role == "tool")
            .expect("必须回灌 tool 角色消息");
        assert!(tool_msg.content.contains("status=\"timeout\""), "{}", tool_msg.content);
        assert!(tool_msg.content.contains("工具结果"), "{}", tool_msg.content);

        // 事件里的状态同样不是 running/ok
        let result = &fx.sink.payloads(EVENT_TOOL_RESULT)[0];
        assert_eq!(result["status"], "timeout");
    }

    // ─── 空回合保护（真实报障回归） ───────────────────────
    //
    // 报障现象：任务会话里说"开始实现计划"，模型调了几个工具（update_plan /
    // list_dir）之后本轮就结束了，什么都没实现，气泡里连一个字都没有。
    // 根因是"既没有正文、也没有工具调用"的响应被当成"模型说完了"。

    /// 空回合必须重试：模型下一轮继续干活，而不是把回合结束时停在半路
    #[test]
    fn empty_turn_is_retried_instead_of_ending_the_run() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            // 第 1 轮：只调用工具
            vec![tool_call_chunk(0, "c1", "probe", "{}")],
            // 第 2 轮：空响应（历史实现就在这里静悄悄结束了）
            Vec::new(),
            // 第 3 轮：被提醒后接着做
            vec![StreamChunk::Content("继续实现：已经写出第一个文件。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());

        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.content, "继续实现：已经写出第一个文件。");
        assert!(!outcome.content.trim().is_empty(), "绝不能再产出空白回合");

        // 提醒确实推给了模型（而不是重发一模一样的请求）
        let seen = backend.seen_messages.lock().unwrap();
        assert_eq!(seen.len(), 3, "三次请求：工具轮 + 空回合 + 继续");
        let reminder = seen[2].last().expect("提醒消息");
        assert_eq!(reminder.role, "user");
        assert!(reminder.content.contains("空"), "{}", reminder.content);
    }

    /// 反复空响应时必须重试有上限，并且留下一句用户可见的说明
    #[test]
    fn repeated_empty_turns_end_with_a_visible_notice() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "probe", "{}")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());

        // 重试最多 EMPTY_TURN_RETRIES 次，随后如实告知用户
        assert_eq!(backend.seen_messages.lock().unwrap().len(), 1 + EMPTY_TURN_RETRIES + 1);
        assert!(outcome.empty_turn, "必须标记为空回合，调用方才能区别对待");
        assert!(
            outcome.content.contains("没有收到模型的任何输出"),
            "{}",
            outcome.content
        );
        assert_eq!(outcome.steps, 1);
    }

    /// `finish_reason=length`（被 max_tokens 截断）必须被翻译成可执行的建议：
    /// 把动作拆小，而不是原样重试同一个巨型输出
    #[test]
    fn truncated_turn_tells_the_model_to_split_the_work() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![StreamChunk::Finish("length".to_string())],
            vec![StreamChunk::Content("好的，我先写第一个文件。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());

        assert_eq!(outcome.content, "好的，我先写第一个文件。");
        let seen = backend.seen_messages.lock().unwrap();
        let reminder = seen[1].last().expect("提醒消息").content.clone();
        assert!(reminder.contains("max_tokens"), "{reminder}");
        assert!(reminder.contains("拆小"), "{reminder}");
    }

    /// 端到端（真实工具）：模型在一次请求里用 `steps` 跑两条命令，
    /// 两步按序回灌、状态为 ok、事件齐备
    #[cfg(unix)]
    #[test]
    fn real_run_command_steps_end_to_end() {        let fx = fixture(
            vec![Arc::new(crate::agent::harness::tools::shell::RunCommand) as Arc<dyn Tool>],
            ToolMode::Full,
        );
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(
                0,
                "c1",
                "run_command",
                r#"{"steps":[{"program":"echo","args":["alpha"]},{"program":"echo","args":["beta"]}]}"#,
            )],
            vec![StreamChunk::Content("两条都跑完了。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(outcome.invocations.len(), 1);
        let record = &outcome.invocations[0];
        assert_eq!(record.tool, "run_command");
        assert_eq!(record.status, "ok", "{:?}", record.error);
        assert!(record.arguments_json.contains("steps"));

        // 事件：开始 + 结果 + 至少一条增量输出
        let names = fx.sink.names();
        assert!(names.contains(&EVENT_TOOL_START.to_string()), "{names:?}");
        assert!(names.contains(&EVENT_TOOL_RESULT.to_string()), "{names:?}");
        assert!(
            names
                .iter()
                .any(|n| n == crate::agent::harness::traits::EVENT_TOOL_OUTPUT),
            "流式输出事件必须由真实工具发出：{names:?}"
        );

        // 模型看到的是两步的输出，且顺序正确
        let seen = backend.seen_messages.lock().unwrap();
        let tool_msg = seen
            .last()
            .expect("第二轮请求")
            .iter()
            .find(|m| m.role == "tool")
            .expect("必须回灌 tool 角色消息")
            .content
            .clone();
        assert!(tool_msg.contains("step 1/2"), "{tool_msg}");
        assert!(tool_msg.contains("step 2/2"), "{tool_msg}");
        let alpha = tool_msg.find("alpha").expect("第一步输出");
        let beta = tool_msg.find("beta").expect("第二步输出");
        assert!(alpha < beta, "{tool_msg}");
    }

    #[test]
    fn float_mode_never_offers_tools() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![vec![StreamChunk::Content("只有聊天".to_string())]]);

        let outcome = run_with(&fx, &backend, Arc::new(AllowAllApprover), messages(), false, 8);
        assert_eq!(outcome.content, "只有聊天");
        let seen = backend.seen_tools.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].is_none(), "悬浮窗链路绝不能携带 tools 字段");
    }

    #[test]
    fn executes_tool_then_answers() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![
                StreamChunk::Content("我先查一下。".to_string()),
                tool_call_chunk(0, "c1", "probe", "{}"),
            ],
            vec![StreamChunk::Content("查到了。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(outcome.invocations.len(), 1);
        assert_eq!(outcome.invocations[0].status, "ok");
        assert_eq!(outcome.steps, 1);
        // 正文包含两轮文本，且工具参数不会混入正文
        assert!(outcome.content.contains("我先查一下。"));
        assert!(outcome.content.contains("查到了。"));
        assert!(!outcome.content.contains("tool_call"));

        let names = fx.sink.names();
        assert_eq!(names, vec![EVENT_TOOL_START, EVENT_TOOL_RESULT]);

        // 事件载荷是前后端契约的一部分（前端按 stream_id/call_id 关联卡片）
        let start = &fx.sink.payloads(EVENT_TOOL_START)[0];
        assert_eq!(start["stream_id"], "stream-1");
        assert_eq!(start["call_id"], "c1");
        assert_eq!(start["tool"], "probe");
        assert_eq!(start["permission"], "read");
        assert_eq!(start["status"], "running");

        let result = &fx.sink.payloads(EVENT_TOOL_RESULT)[0];
        assert_eq!(result["call_id"], "c1");
        assert_eq!(result["status"], "ok");
        assert_eq!(result["truncated"], false);
        assert!(result["preview"].as_str().unwrap().contains("工具结果"));
    }

    #[test]
    fn tool_result_is_fed_back_as_untrusted_tool_message() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "probe", "{}")],
            vec![StreamChunk::Content("完成".to_string())],
        ]);

        run(&fx, &backend, Arc::new(AllowAllApprover), messages());

        let seen = backend.seen_messages.lock().unwrap();
        let second = seen.last().unwrap();
        let tool_msg = second
            .iter()
            .find(|m| m.role == "tool")
            .expect("必须回灌 tool 消息");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("c1"));
        assert!(tool_msg.content.contains("untrusted=\"true\""));
        assert!(tool_msg.content.contains("工具结果"));
        // assistant 消息必须带上 tool_calls
        let assistant = second
            .iter()
            .find(|m| m.tool_calls.is_some())
            .expect("必须带 tool_calls");
        assert_eq!(assistant.tool_calls.as_ref().unwrap()[0].function.name, "probe");
    }

    #[test]
    fn tool_error_is_reported_to_model_not_propagated() {
        let calls = Arc::new(Mutex::new(0));
        let tool: Arc<dyn Tool> = Arc::new(FailingTool {
            calls: calls.clone(),
        });
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "boom", "{}")],
            vec![StreamChunk::Content("工具坏了，我直接回答。".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(outcome.invocations[0].status, "error");
        assert!(outcome.invocations[0].error.is_some());
        assert!(outcome.content.contains("我直接回答"));

        let seen = backend.seen_messages.lock().unwrap();
        let tool_msg = seen
            .last()
            .unwrap()
            .iter()
            .find(|m| m.role == "tool")
            .unwrap();
        assert!(tool_msg.content.contains("status=\"error\""));
    }

    struct FailingTool {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl Tool for FailingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "boom",
                "会失败的工具",
                "测试用",
                Permission::Read,
                json!({"type": "object"}),
            )
        }
        async fn call(&self, _args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
            *self.calls.lock().unwrap() += 1;
            Err(anyhow!("磁盘炸了"))
        }
    }

    #[test]
    fn unknown_tool_name_is_rejected_with_available_list() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "hack_the_planet", "{}")],
            vec![StreamChunk::Content("抱歉".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(outcome.invocations[0].status, "error");
        let err = outcome.invocations[0].error.clone().unwrap();
        assert!(err.contains("不存在名为"), "{err}");
        assert!(err.contains("probe"), "错误信息必须列出可用工具：{err}");
    }

    #[test]
    fn invalid_arguments_are_reported_without_execution() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "probe", "{\"path\": ")],
            vec![StreamChunk::Content("好".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(*calls.lock().unwrap(), 0, "参数不合法时不得执行");
        assert_eq!(outcome.invocations[0].status, "error");
        assert!(outcome.invocations[0]
            .error
            .as_ref()
            .unwrap()
            .contains("参数不合法"));
    }

    #[test]
    fn denied_approval_blocks_execution() {
        let (tool, calls) = MockTool::new("danger", Permission::WriteFs);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "danger", "{}")],
            vec![StreamChunk::Content("那我就不做了".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(DenyAllApprover), messages());
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(outcome.invocations[0].status, "denied");
        assert_eq!(outcome.invocations[0].approval.as_deref(), Some("deny"));
    }

    #[test]
    fn allow_session_grants_followup_calls() {
        let (tool, calls) = MockTool::new("danger", Permission::WriteFs);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "danger", "{}")],
            vec![tool_call_chunk(0, "c2", "danger", "{}")],
            vec![StreamChunk::Content("都做完了".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(outcome.invocations[0].approval.as_deref(), Some("allow_once"));
        // 第二次是同一个工具：AllowOnce 不产生会话授权，仍会再次询问
        assert_eq!(outcome.invocations[1].approval.as_deref(), Some("allow_once"));
    }

    #[test]
    fn step_limit_forces_final_answer_without_tools() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "probe", "{}")],
            vec![tool_call_chunk(0, "c2", "probe", "{}")],
            vec![StreamChunk::Content("收尾回答".to_string())],
        ]);

        let outcome = run_with(&fx, &backend, Arc::new(AllowAllApprover), messages(), true, 2);
        assert!(outcome.hit_step_limit);
        assert_eq!(outcome.steps, 2);
        assert_eq!(*calls.lock().unwrap(), 2);
        assert!(outcome.content.contains("收尾回答"));

        // 最后一轮必须不带工具
        let seen = backend.seen_tools.lock().unwrap();
        assert!(seen.last().unwrap().is_none());
    }

    #[test]
    fn readonly_mode_hides_write_tools_from_model_and_blocks_call() {
        let (tool, calls) = MockTool::new("danger", Permission::WriteFs);
        let fx = fixture(vec![tool], ToolMode::ReadOnly);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "danger", "{}")],
            vec![StreamChunk::Content("不行".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(outcome.invocations[0].status, "error");
        let seen = backend.seen_tools.lock().unwrap();
        assert!(
            seen[0].is_none(),
            "只读模式下没有任何可见工具时，请求体不应包含 tools 字段"
        );
    }

    #[test]
    fn parallel_readonly_calls_are_all_executed() {
        let (a, calls_a) = MockTool::new("probe_a", Permission::Read);
        let (b, calls_b) = MockTool::new("probe_b", Permission::Read);
        let fx = fixture(vec![a, b], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![
                tool_call_chunk(0, "c1", "probe_a", "{}"),
                tool_call_chunk(1, "c2", "probe_b", "{}"),
            ],
            vec![StreamChunk::Content("两个都查完了".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert_eq!(*calls_a.lock().unwrap(), 1);
        assert_eq!(*calls_b.lock().unwrap(), 1);
        assert_eq!(outcome.invocations.len(), 2);
    }

    #[test]
    fn oversized_output_is_truncated() {
        let calls = Arc::new(Mutex::new(0));
        let tool: Arc<dyn Tool> = Arc::new(HugeTool {
            calls: calls.clone(),
        });
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "huge", "{}")],
            vec![StreamChunk::Content("好".to_string())],
        ]);

        let outcome = run(&fx, &backend, Arc::new(AllowAllApprover), messages());
        assert!(outcome.invocations[0].truncated);
        let seen = backend.seen_messages.lock().unwrap();
        let tool_msg = seen.last().unwrap().iter().find(|m| m.role == "tool").unwrap();
        assert!(tool_msg.content.contains("已截断"));
        assert!(tool_msg.content.len() < 70 * 1024);
    }

    struct HugeTool {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl Tool for HugeTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "huge",
                "超大输出",
                "测试用",
                Permission::Read,
                json!({"type": "object"}),
            )
        }
        async fn call(&self, _args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
            *self.calls.lock().unwrap() += 1;
            Ok(ToolOutput::text("x".repeat(200 * 1024)))
        }
    }

    #[test]
    fn cancellation_stops_before_tool_execution() {
        let (tool, calls) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "probe", "{}")],
            vec![StreamChunk::Content("不该出现".to_string())],
        ]);

        let cancel = Arc::new(AtomicBool::new(true));
        let run = HarnessRun {
            backend: backend.as_ref(),
            registry: &fx.registry,
            services: fx.services.clone(),
            session_id: "s1",
            stream_id: "stream-1",
            cancel,
            emit: fx.sink.clone(),
            approver: Arc::new(AllowAllApprover),
            tools_enabled: true,
            auto_approve: Vec::new(),
            limits: fx.limits,
            max_steps: 8,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(run.execute(messages(), |_| {}, |_| {}))
            .unwrap();

        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(outcome.invocations.is_empty());
        assert!(outcome.content.is_empty());
    }

    #[test]
    fn auto_approved_tools_skip_prompt() {
        let (tool, calls) = MockTool::new("danger", Permission::WriteFs);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let backend = MockBackend::new(vec![
            vec![tool_call_chunk(0, "c1", "danger", "{}")],
            vec![StreamChunk::Content("搞定".to_string())],
        ]);

        let cancel = Arc::new(AtomicBool::new(false));
        let run = HarnessRun {
            backend: backend.as_ref(),
            registry: &fx.registry,
            services: fx.services.clone(),
            session_id: "s1",
            stream_id: "stream-1",
            cancel,
            emit: fx.sink.clone(),
            approver: Arc::new(DenyAllApprover),
            tools_enabled: true,
            auto_approve: vec!["danger".to_string()],
            limits: fx.limits,
            max_steps: 8,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(run.execute(messages(), |_| {}, |_| {}))
            .unwrap();

        assert_eq!(*calls.lock().unwrap(), 1, "免审批工具应当直接执行");
        assert_eq!(outcome.invocations[0].approval.as_deref(), Some("auto"));
    }

    #[test]
    fn registry_info_snapshot_matches_mode() {
        let (tool, _) = MockTool::new("probe", Permission::Read);
        let fx = fixture(vec![tool], ToolMode::Standard);
        let infos: Vec<ToolInfo> = fx.registry.infos(fx.services.mode);
        assert_eq!(infos.len(), 1);
        assert!(infos[0].enabled);
    }

    // ─── 流中断：已流出的正文必须保留 ───

    /// 先产出内容再报错的流（模拟网络抖动/网关断流）
    struct BrokenStreamBackend {
        chunks: Mutex<Vec<anyhow::Result<StreamChunk>>>,
    }

    impl BrokenStreamBackend {
        fn new(chunks: Vec<anyhow::Result<StreamChunk>>) -> Arc<Self> {
            Arc::new(Self {
                chunks: Mutex::new(chunks),
            })
        }
    }

    #[async_trait::async_trait]
    impl ChatBackend for BrokenStreamBackend {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> Result<String> {
            Ok("mock".to_string())
        }

        async fn chat_stream(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Option<Vec<ToolSchema>>,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
            let chunks = std::mem::take(&mut *self.chunks.lock().unwrap());
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    fn run_broken(
        fx: &Fixture,
        backend: &Arc<BrokenStreamBackend>,
    ) -> Result<HarnessOutcome> {
        let cancel = Arc::new(AtomicBool::new(false));
        let run = HarnessRun {
            backend: backend.as_ref(),
            registry: &fx.registry,
            services: fx.services.clone(),
            session_id: "s1",
            stream_id: "stream-1",
            cancel,
            emit: fx.sink.clone(),
            approver: Arc::new(AllowAllApprover),
            tools_enabled: true,
            auto_approve: Vec::new(),
            limits: fx.limits,
            max_steps: 8,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(run.execute(messages(), |_| {}, |_| {}))
    }

    /// 用户已经看到一半回复时连接断开：部分内容必须保留并说明中断，
    /// 而不是把整轮变成"发送失败"、让已显示的文字消失
    #[test]
    fn partial_content_survives_a_stream_break() {
        let fx = fixture(vec![], ToolMode::Standard);
        let backend = BrokenStreamBackend::new(vec![
            Ok(StreamChunk::Content("前半段。".to_string())),
            Err(anyhow!("connection reset by peer")),
        ]);

        let outcome = run_broken(&fx, &backend).expect("必须保留部分输出而不是报错");
        assert!(outcome.content.contains("前半段。"), "{}", outcome.content);
        assert!(outcome.content.contains("连接中断"), "{}", outcome.content);
        assert!(
            outcome.invocations.is_empty(),
            "中断时不得执行半截工具调用"
        );
    }

    /// 一个字都没产出就断开：仍然按错误上报（没有可保留的内容）
    #[test]
    fn stream_break_without_content_is_an_error() {
        let fx = fixture(vec![], ToolMode::Standard);
        let backend = BrokenStreamBackend::new(vec![Err(anyhow!("broken pipe"))]);
        let result = run_broken(&fx, &backend);
        assert!(result.is_err(), "没有任何内容时应当报错");
    }
}
