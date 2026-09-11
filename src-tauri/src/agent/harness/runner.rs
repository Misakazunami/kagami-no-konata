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

pub const EVENT_TOOL_START: &str = "tool-call-start";
pub const EVENT_TOOL_RESULT: &str = "tool-call-result";

/// UI 卡片里的结果预览长度上限
const PREVIEW_CHARS: usize = 600;
/// 事件里参数预览长度上限
const ARG_PREVIEW_CHARS: usize = 400;

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
}

/// 一次生成所需的全部运行参数
pub struct HarnessRun<'a> {
    pub backend: &'a dyn ChatBackend,
    pub registry: &'a ToolRegistry,
    pub services: &'a ToolServices,
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
        let mut session_grants: HashSet<String> = HashSet::new();
        let mut tool_rounds = 0usize;
        let mut hit_step_limit = false;

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

            while let Some(chunk) = stream.next().await {
                if self.cancel.load(Ordering::Relaxed) {
                    break;
                }
                match chunk? {
                    StreamChunk::Content(text) => {
                        step_text.push_str(&text);
                        content.push_str(&text);
                        on_chunk(&text);
                    }
                    StreamChunk::Thinking(text) => on_thinking(&text),
                    StreamChunk::ToolCallDelta(delta) => accumulator.push(delta),
                }
            }

            if self.cancel.load(Ordering::Relaxed) {
                break;
            }

            let calls = accumulator.finish(self.stream_id);
            if calls.is_empty() {
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
        let cx = ToolCtx {
            session_id: self.session_id,
            stream_id: self.stream_id,
            step,
            cancel: self.cancel.clone(),
            services: self.services,
            limits: self.limits,
            emit: self.emit.clone(),
            approver: self.approver.clone(),
        };

        let outcome = tokio::time::timeout(self.limits.call_timeout, tool.call(call.arguments.clone(), &cx)).await;

        match outcome {
            Ok(Ok(output)) => {
                // 兜底截断：工具自己没截干净时也不能把上下文撑爆
                let (body, truncated) = truncate_text(&output.content, self.limits.max_output_bytes);
                let truncated = truncated || output.truncated;
                let preview = output
                    .preview
                    .unwrap_or_else(|| truncate_text(&body, PREVIEW_CHARS).0);

                record.status = "ok".to_string();
                record.truncated = truncated;
                record.result_preview = Some(preview.clone());
                record.duration_ms = started.elapsed().as_millis() as i64;

                self.emit_result(&record, &preview);
                (record, tool_message(&call.id, &call.name, "ok", &body))
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
                let timeout = self.limits.call_timeout.as_secs();
                self.finish_error(
                    record,
                    started,
                    format!("工具执行超时（{} 秒）", timeout),
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
        ToolInfo, ToolOutput,
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
                }),
                calls,
            )
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
                Ok(text) => Ok(ToolOutput::text(text.clone())),
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
            index,
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
            services: &fx.services,
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
            services: &fx.services,
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
            services: &fx.services,
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
}
