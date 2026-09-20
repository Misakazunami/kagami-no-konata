use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::harness::subagent::{ChildOutcome, MAX_SUMMARY_BYTES, MAX_TASKS_PER_CALL};
use crate::agent::harness::traits::{
    truncate_text, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput, ToolServices, ToolStatus,
};

/// 派发只读子代理去并行调查
///
/// 为什么值得做：模型只能在一个上下文里顺序思考，一轮生成又有步数上限。
/// "分别看看 A/B/C 三个模块再汇总"这种任务，顺序读会很快烧完预算；
/// 子代理让每个子任务在自己的上下文里跑一小段只读循环，只把结论带回来。
///
/// 权限是 `Read`（子代理只能读，不产生任何副作用），但**仍然有预算与取消**：
/// 每轮生成默认只允许 2 个子代理任务，且共享用户的"停止"。
pub struct SpawnSubagents;

/// 时间预算看门狗的 RAII 守卫
///
/// 正常路径会显式 abort；但若整个工具 future 被外层超时 drop（极端卡顿），
/// Drop 也要把看门狗一起带走，否则它会空转到 deadline。
struct WatchdogTask(tokio::task::JoinHandle<()>);

impl Drop for WatchdogTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[async_trait::async_trait]
impl Tool for SpawnSubagents {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_subagents",
            "派发只读子代理",
            "把 1-3 个**只读**调查任务并行派给子代理：它们各自读文件/搜索，然后只把结论带回来。适合「分别看看这几个模块/文件再汇总」这类调查；不适合需要写入、执行命令或联网的工作（子代理没有这些能力）。每轮生成最多 2 个子代理任务。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_TASKS_PER_CALL,
                        "description": format!("要并行调查的任务（最多 {} 个）", MAX_TASKS_PER_CALL),
                        "items": {
                            "type": "object",
                            "properties": {
                                "goal": {
                                    "type": "string",
                                    "description": "这个子代理要回答的问题（越具体越好，例如「认证流程在哪几个文件里实现，可能的坑是什么」）"
                                },
                                "paths": {
                                    "type": "string",
                                    "description": "可选：建议优先查看的路径或目录，例如 `src/auth` 或 `工作区id:src/llm`"
                                }
                            },
                            "required": ["goal"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["tasks"],
                "additionalProperties": false
            }),
        )
    }

    /// 子代理批次用自己的时间预算：整批可能跨十几次工具调用与多次 LLM 往返，
    /// 父级的 `call_timeout`（默认 60 秒）会把它直接掐死
    fn timeout_budget(&self, services: &ToolServices) -> Option<Duration> {
        Some(services.subagent_timeout)
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;

        // 子代理能力由运行时提供：拿不到就是不允许（悬浮窗、子代理自身、无工具链路）
        let Some(runtime) = cx.services.subagent.as_ref() else {
            anyhow::bail!("当前环境不支持子代理（子代理不能再派子代理）");
        };

        let tasks = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("缺少必填参数「tasks」（数组）"))?;
        if tasks.is_empty() {
            anyhow::bail!("tasks 不能为空");
        }
        if tasks.len() > runtime.max_tasks_per_call() {
            anyhow::bail!(
                "任务过多（上限 {} 个）",
                runtime.max_tasks_per_call()
            );
        }

        // 先把任务解析干净，再决定要不要花预算
        let mut parsed: Vec<(String, Option<String>)> = Vec::with_capacity(tasks.len());
        for (index, task) in tasks.iter().enumerate() {
            let goal = task
                .get("goal")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("tasks[{}] 缺少 goal", index))?;
            if goal.chars().count() > 500 {
                anyhow::bail!("tasks[{}] 的 goal 过长（上限 500 字）", index);
            }
            let paths = task
                .get("paths")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            parsed.push((goal, paths));
        }

        // 整批的时间预算：到点前必须带着已完成的部分返回，而不是被父级外层超时掐断。
        // 历史故障：一次子代理批次跑到 70 秒被强制终止，已完成的结论与统计全部丢失。
        let started = std::time::Instant::now();
        let budget = cx.services.subagent_timeout;
        let deadline = started + budget;

        // 批次共享的取消标志：用户停止或时间预算用尽都会置位。
        // 子代理的 `HarnessRun` 在块间/分片间检查取消，因此能收手并带回部分结论。
        let batch_cancel = Arc::new(AtomicBool::new(false));
        let watchdog = WatchdogTask(tokio::spawn({
            let flag = batch_cancel.clone();
            let parent_cancel = cx.cancel.clone();
            async move {
                loop {
                    if parent_cancel.load(Ordering::Relaxed)
                        || std::time::Instant::now() >= deadline
                    {
                        flag.store(true, Ordering::Relaxed);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }));

        // 第一步：一次性判定名额与时间（必须在**发请求之前**决定跳过谁），
        // 并向 UI 广播 queued / running / skipped 的粗粒度进度
        struct Prepared {
            task_id: String,
            goal_preview: String,
            goal: String,
            paths: Option<String>,
            slot: usize,
            model: String,
        }
        let mut prepared: Vec<Prepared> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut out_of_time: Vec<String> = Vec::new();

        for (idx, (goal, paths)) in parsed.into_iter().enumerate() {
            let task_id = format!("{}-sub-{}", cx.call_id, idx);
            let goal_preview = truncate_text(&goal, 40).0;

            cx.emit.emit(
                crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                json!({
                    "session_id": cx.session_id,
                    "stream_id": cx.stream_id,
                    "parent_call_id": cx.call_id,
                    "task_id": task_id,
                    "goal_preview": goal_preview,
                    "status": "queued",
                }),
            );

            if cx.cancelled() {
                cx.emit.emit(
                    crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                    json!({
                        "session_id": cx.session_id,
                        "stream_id": cx.stream_id,
                        "parent_call_id": cx.call_id,
                        "task_id": task_id,
                        "goal_preview": goal_preview,
                        "status": "cancelled",
                    }),
                );
                break;
            }

            if std::time::Instant::now() >= deadline {
                cx.emit.emit(
                    crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                    json!({
                        "session_id": cx.session_id,
                        "stream_id": cx.stream_id,
                        "parent_call_id": cx.call_id,
                        "task_id": task_id,
                        "goal_preview": goal_preview,
                        "status": "skipped",
                        "reason": "时间预算不足",
                    }),
                );
                out_of_time.push(goal);
                continue;
            }

            // 名额序号决定用哪个子模型（自动选择：子模型池按序号轮转）。
            // 手动模式 / 未配置子模型时池里只有一个条目，取到的就是主轮次模型。
            let Some(slot) = runtime.take_slot() else {
                cx.emit.emit(
                    crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                    json!({
                        "session_id": cx.session_id,
                        "stream_id": cx.stream_id,
                        "parent_call_id": cx.call_id,
                        "task_id": task_id,
                        "goal_preview": goal_preview,
                        "status": "skipped",
                        "reason": "本轮名额已用完",
                    }),
                );
                skipped.push(goal);
                continue;
            };
            let model_label = runtime.model_hint(slot).unwrap_or_default();

            cx.emit.emit(
                crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                json!({
                    "session_id": cx.session_id,
                    "stream_id": cx.stream_id,
                    "parent_call_id": cx.call_id,
                    "task_id": task_id,
                    "goal_preview": goal_preview,
                    "status": "running",
                    "model": model_label.clone(),
                }),
            );

            prepared.push(Prepared {
                task_id,
                goal_preview,
                goal,
                paths,
                slot,
                model: model_label,
            });
        }

        // 第二步：并行执行（同一批共享时间预算与取消标志）。
        // 串行会把两个子代理的墙钟时间叠加，正是超时的主要来源。
        let futures = prepared.into_iter().map(|task| {
            let cancel = batch_cancel.clone();
            async move {
                let start = std::time::Instant::now();
                let result = runtime
                    .run_child(cx, &task.goal, task.paths.as_deref(), task.slot, cancel)
                    .await;
                (task, start.elapsed().as_millis() as i64, result)
            }
        });
        let results = futures::future::join_all(futures).await;
        // 批次已结束，看门狗立即停掉（Drop 是兜底：外层超时 drop 掉本 future 时也生效）
        watchdog.0.abort();

        let hit_deadline = !cx.cancelled() && batch_cancel.load(Ordering::Relaxed);

        // 第三步：收拢结果并广播完成状态
        let mut outcomes: Vec<(String, std::result::Result<ChildOutcome, String>)> = Vec::new();
        let mut extra_tokens = 0usize;
        for (task, duration_ms, result) in results {
            let (status_str, tokens) = match &result {
                Ok(child) => ("done", child.tokens),
                Err(_) => ("error", 0),
            };
            cx.emit.emit(
                crate::agent::harness::traits::EVENT_SUBAGENT_STATUS,
                json!({
                    "session_id": cx.session_id,
                    "stream_id": cx.stream_id,
                    "parent_call_id": cx.call_id,
                    "task_id": task.task_id,
                    "goal_preview": task.goal_preview,
                    "status": status_str,
                    "duration_ms": duration_ms,
                    "model": task.model,
                }),
            );
            extra_tokens += tokens;
            outcomes.push((task.goal, result));
        }

        if outcomes.is_empty() {
            if !out_of_time.is_empty() {
                anyhow::bail!(
                    "子代理时间预算（{} 秒）已用尽，本轮未能启动任何子任务；请缩小调查范围或调大 tools.subagent_timeout_secs",
                    budget.as_secs()
                );
            }
            anyhow::bail!(
                "本轮子代理预算已用完（剩 {} 个），请自己按顺序调查或等下一轮再派",
                runtime.children_left()
            );
        }

        let mut body = String::new();
        let mut ok_count = 0usize;
        let mut tool_calls = 0usize;
        for (index, (goal, result)) in outcomes.iter().enumerate() {
            body.push_str(&format!("── 子代理 {}/{} ──\n任务：{}\n", index + 1, outcomes.len(), goal));
            match result {
                Ok(child) => {
                    ok_count += 1;
                    tool_calls += child.tool_calls;
                    body.push_str(&format!(
                        "（{} 轮，{} 次工具调用，约 {} tokens）\n{}\n\n",
                        child.steps, child.tool_calls, child.tokens, child.summary
                    ));
                    if child.truncated {
                        body.push_str(&format!("（结论已截断到 {} KB）\n", MAX_SUMMARY_BYTES / 1024));
                    }
                }
                Err(error) => {
                    body.push_str(&format!("（失败）{}\n\n", error));
                }
            }
        }
        if !skipped.is_empty() {
            body.push_str(&format!(
                "（本轮子代理名额已用完，以下 {} 个任务未执行：{}）\n",
                skipped.len(),
                skipped.join("；")
            ));
        }
        if !out_of_time.is_empty() {
            body.push_str(&format!(
                "（时间预算不足，以下 {} 个任务未启动：{}）\n",
                out_of_time.len(),
                out_of_time.join("；")
            ));
        }
        if hit_deadline {
            body.push_str(&format!(
                "（本批子代理时间预算（{} 秒）已用尽：以上为**已完成的部分结论**，请基于它们继续，不要重复调查；如需完整调查可调大 tools.subagent_timeout_secs 或缩小任务范围）\n",
                budget.as_secs()
            ));
        }

        let preview = truncate_text(
            &format!(
                "子代理 {} 个任务 · 成功 {} · {} 次工具调用 · 约 {} tokens{}",
                outcomes.len(),
                ok_count,
                tool_calls,
                extra_tokens,
                if hit_deadline { " · 超时收尾" } else { "" }
            ),
            300,
        )
        .0;

        let mut output = ToolOutput::text(body)
            .with_preview(preview)
            .with_extra_tokens(extra_tokens);
        // 超时收尾优先标记：内容仍是已完成的部分结论，不能被界面显示成"完成"；
        // 全部失败时则标成失败
        if hit_deadline {
            output = output.with_status(ToolStatus::Timeout);
        } else if ok_count == 0 {
            output = output.with_status(ToolStatus::Error);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::subagent::AgentRuntime;
    use crate::agent::harness::traits::{
        DenyAllApprover, EventSink, ToolLimits, ToolServices,
    };
    use crate::agent::harness::WorkspaceSet;
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::llm::backend::ChatBackend;
    use crate::llm::types::{LlmMessage, StreamChunk, ToolSchema};
    use anyhow::Result as AnyResult;
    use futures::stream::Stream;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 脚本化后端：每轮按顺序吐出一段内容（子代理的"回答"）
    struct ScriptBackend {
        script: Mutex<Vec<String>>,
        seen_messages: Mutex<Vec<Vec<LlmMessage>>>,
        seen_tools: Mutex<Vec<Option<Vec<ToolSchema>>>>,
        /// 永远返回空响应（模拟"模型什么都没说也没做"的提供商）
        always_empty: bool,
    }

    impl ScriptBackend {
        fn new(responses: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(responses.into_iter().map(|s| s.to_string()).collect()),
                seen_messages: Mutex::new(Vec::new()),
                seen_tools: Mutex::new(Vec::new()),
                always_empty: false,
            })
        }

        /// 每一轮都返回空响应——用于验证"子代理什么都没产出"的处理路径
        fn empty() -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(Vec::new()),
                seen_messages: Mutex::new(Vec::new()),
                seen_tools: Mutex::new(Vec::new()),
                always_empty: true,
            })
        }
    }

    #[async_trait::async_trait]
    impl ChatBackend for ScriptBackend {
        async fn chat(&self, _messages: Vec<LlmMessage>) -> AnyResult<String> {
            Ok(String::new())
        }

        async fn chat_stream(
            &self,
            messages: Vec<LlmMessage>,
            tools: Option<Vec<ToolSchema>>,
        ) -> AnyResult<Pin<Box<dyn Stream<Item = AnyResult<StreamChunk>> + Send>>> {
            self.seen_messages.lock().unwrap().push(messages);
            // 子代理**必须**能拿到工具（它是靠工具做调查的），但也必须只有只读工具
            self.seen_tools.lock().unwrap().push(tools);
            let next = {
                if self.always_empty {
                    String::new()
                } else {
                    let mut script = self.script.lock().unwrap();
                    if script.is_empty() {
                        "（没有更多脚本）".to_string()
                    } else {
                        script.remove(0)
                    }
                }
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamChunk::Content(next),
            )])))
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl EventSink for Recorder {
        fn emit(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }
    }

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        cancel: Arc<AtomicBool>,
        sink: Arc<Recorder>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str, backend: Arc<dyn ChatBackend>, max_children: usize) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("konata-spawn-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a.txt"), "文件 A 的内容").unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            let mut services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
            services.subagent = Some(Arc::new(AgentRuntime::new(backend, max_children)));
            Self {
                dir,
                services,
                cancel: Arc::new(AtomicBool::new(false)),
                sink: Arc::new(Recorder::default()),
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: "s1",
                stream_id: "st1",
                call_id: "spawn-1",
                step: 0,
                cancel: self.cancel.clone(),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: Duration::from_secs(10),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(DenyAllApprover),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn runs_one_child_and_returns_compressed_summary() {
        let backend = ScriptBackend::new(vec!["要点一：认证在 auth.rs\n要点二：无风险"]);
        let fx = Fixture::new("one", backend.clone(), 2);
        let cx = fx.ctx();
        let out = block_on(SpawnSubagents.call(
            json!({"tasks": [{"goal": "看认证", "paths": "src/auth"}]}),
            &cx,
        ))
        .unwrap();

        assert!(out.content.contains("── 子代理 1/1 ──"), "{}", out.content);
        assert!(out.content.contains("要点一：认证在 auth.rs"), "{}", out.content);
        assert!(out.preview.as_deref().unwrap_or("").contains("1 个任务"));
        assert!(out.extra_tokens > 0, "子代理用量必须被估算出来");
        assert_eq!(out.status, crate::agent::harness::ToolStatus::Ok);

        // 子代理的提示词必须是只读调查员规则
        let seen = backend.seen_messages.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0][0].content.contains("只读调查子代理"), "{}", seen[0][0].content);

        // 给子代理的 schema 里不能有写入/执行类工具
        let tools = backend.seen_tools.lock().unwrap();
        let names: Vec<String> = tools[0]
            .as_ref()
            .unwrap()
            .iter()
            .map(|schema| schema.function.name.clone())
            .collect();
        assert!(names.contains(&"read_file".to_string()), "{names:?}");
        for forbidden in ["write_file", "run_command", "delete_path", "web_fetch"] {
            assert!(!names.contains(&forbidden.to_string()), "{names:?}");
        }
    }

    #[test]
    fn runs_multiple_children_and_reports_each() {
        let backend = ScriptBackend::new(vec!["结论 A", "结论 B"]);
        let fx = Fixture::new("many", backend, 2);
        let cx = fx.ctx();
        let out = block_on(SpawnSubagents.call(
            json!({"tasks": [{"goal": "任务 A"}, {"goal": "任务 B"}]}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("子代理 1/2"), "{}", out.content);
        assert!(out.content.contains("子代理 2/2"), "{}", out.content);
        assert!(out.content.contains("结论 A") && out.content.contains("结论 B"));
    }

    #[test]
    fn budget_limit_is_reported_not_silently_dropped() {
        let backend = ScriptBackend::new(vec!["只有一个能跑"]);
        let fx = Fixture::new("budget", backend, 1);
        let cx = fx.ctx();
        let out = block_on(SpawnSubagents.call(
            json!({"tasks": [{"goal": "任务 A"}, {"goal": "任务 B"}]}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("任务 A"), "{}", out.content);
        assert!(!out.content.contains("任务 B\n"), "第二个任务不该被执行");
        assert!(
            out.content.contains("名额已用完") && out.content.contains("任务 B"),
            "必须明确告知哪些任务被跳过：{}",
            out.content
        );
    }

    #[test]
    fn exhausted_budget_is_an_error_that_tells_the_model_what_to_do() {
        let backend = ScriptBackend::new(vec!["第一个"]);
        let fx = Fixture::new("exhausted", backend, 1);
        let cx = fx.ctx();
        let _ = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "A"}]}), &cx)).unwrap();
        let err = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "B"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("预算已用完"), "{err}");
        assert!(err.to_string().contains("自己按顺序调查"), "{err}");
    }

    #[test]
    fn child_tool_events_are_decorated_with_parent_call_id() {
        let backend = ScriptBackend::new(vec!["结论"]);
        let fx = Fixture::new("events", backend, 1);
        let cx = fx.ctx();
        block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "看文件"}]}), &cx)).unwrap();
        // 子代理没有调用工具，因此这里只验证"没有任何未装饰的事件漏出去"
        for (_, payload) in fx.sink.events.lock().unwrap().iter() {
            if payload.get("depth").is_some() {
                assert_eq!(payload["depth"], 1);
                assert_eq!(payload["parent_call_id"], "spawn-1");
            }
        }
    }

    #[test]
    fn rejects_bad_task_payloads() {
        let backend = ScriptBackend::new(vec!["x"]);
        let fx = Fixture::new("bad", backend, 2);
        let cx = fx.ctx();

        let err = block_on(SpawnSubagents.call(json!({"tasks": []}), &cx)).unwrap_err();
        assert!(err.to_string().contains("不能为空"), "{err}");

        let err = block_on(SpawnSubagents.call(json!({"tasks": [{"paths": "x"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("缺少 goal"), "{err}");

        let err = block_on(SpawnSubagents.call(json!({}), &cx)).unwrap_err();
        assert!(err.to_string().contains("tasks"), "{err}");
    }

    #[test]
    fn unavailable_runtime_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("konata-spawn-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ToolConfig::with_single_root(&dir, true, "测试");
        let set = WorkspaceSet::from_config(&cfg, &dir);
        let services = ToolServices::minimal(dir.clone(), set, ToolMode::Standard);
        let cancel = Arc::new(AtomicBool::new(false));
        let sink = Arc::new(Recorder::default());
        let cx = ToolCtx {
            session_id: "s1",
            stream_id: "st1",
            call_id: "c1",
            step: 0,
            cancel,
            services: &services,
            limits: ToolLimits {
                max_output_bytes: 1024,
                call_timeout: Duration::from_secs(1),
                approval_timeout: Duration::from_secs(1),
            },
            emit: sink,
            approver: Arc::new(DenyAllApprover),
        };
        let err = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "x"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("不支持子代理"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failing_child_marks_the_call_as_error() {
        // 子代理始终没有产出任何结论（模型连续空响应）→ 整条调用记为失败
        let backend = ScriptBackend::empty();
        let fx = Fixture::new("fail", backend, 1);
        let cx = fx.ctx();
        let out = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "看点什么"}]}), &cx))
            .unwrap();
        assert_eq!(out.status, crate::agent::harness::ToolStatus::Error);
        assert!(out.content.contains("失败"), "{}", out.content);
    }

    #[test]
    fn cancellation_stops_dispatching_new_children() {
        let backend = ScriptBackend::new(vec!["不该跑起来"]);
        let fx = Fixture::new("cancel", backend.clone(), 2);
        let cx = fx.ctx();
        fx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "A"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("停止"), "{err}");
        assert!(
            backend.seen_messages.lock().unwrap().is_empty(),
            "取消后不得再发起子代理请求"
        );
    }

    /// 时间预算用尽时必须带部分结论收尾，而不是被父级外层超时掐断
    #[test]
    fn deadline_returns_partial_result_with_timeout_status() {
        /// 永远慢一拍的提供商：用来把子代理拖过时间预算
        struct SlowBackend {
            delay: Duration,
        }

        #[async_trait::async_trait]
        impl ChatBackend for SlowBackend {
            async fn chat(&self, _messages: Vec<LlmMessage>) -> AnyResult<String> {
                Ok(String::new())
            }

            async fn chat_stream(
                &self,
                _messages: Vec<LlmMessage>,
                _tools: Option<Vec<ToolSchema>>,
            ) -> AnyResult<Pin<Box<dyn Stream<Item = AnyResult<StreamChunk>> + Send>>> {
                tokio::time::sleep(self.delay).await;
                Ok(Box::pin(futures::stream::iter(vec![Ok(
                    StreamChunk::Content("迟到的结论".to_string()),
                )])))
            }
        }

        let mut fx = Fixture::new(
            "deadline",
            Arc::new(SlowBackend {
                delay: Duration::from_millis(400),
            }),
            2,
        );
        fx.services.subagent_timeout = Duration::from_millis(100);
        let cx = fx.ctx();
        let out = block_on(SpawnSubagents.call(
            json!({"tasks": [{"goal": "慢任务"}]}),
            &cx,
        ))
        .unwrap();

        // 工具自己收尾：状态是超时、内容说明预算用尽、部分结果仍被带回
        assert_eq!(out.status, ToolStatus::Timeout);
        assert!(out.content.contains("时间预算"), "{}", out.content);
        // 进度事件必须如实展示任务跑过（而不是什么都没发生）
        let events = fx.sink.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(event, payload)| event == "subagent-status" && payload["status"] == "running"),
            "缺少 running 事件"
        );
    }

    /// 时间预算已耗尽时不再启动新任务，并明确告知模型
    #[test]
    fn expired_deadline_skips_all_tasks_with_clear_error() {
        let backend = ScriptBackend::new(vec!["不会被执行"]);
        let mut fx = Fixture::new("expired", backend.clone(), 2);
        // 预算为 0：deadline 在调用开始的那一刻就已过去
        fx.services.subagent_timeout = Duration::ZERO;
        let cx = fx.ctx();
        let err = block_on(SpawnSubagents.call(json!({"tasks": [{"goal": "A"}]}), &cx)).unwrap_err();
        assert!(err.to_string().contains("时间预算"), "{err}");
        assert!(
            backend.seen_messages.lock().unwrap().is_empty(),
            "过期后不得再发起子代理请求"
        );
    }

    /// 工具声明的时间预算必须等于服务里的子代理预算（runner 靠它放宽外层窗口）
    #[test]
    fn timeout_budget_is_the_subagent_budget() {
        let fx = Fixture::new("timeout-budget", ScriptBackend::new(vec!["x"]), 2);
        assert_eq!(
            SpawnSubagents.timeout_budget(&fx.services),
            Some(fx.services.subagent_timeout)
        );
    }
}
