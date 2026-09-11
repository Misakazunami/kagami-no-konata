use chrono::{Local, Utc};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, State, WebviewWindow};

use crate::agent::context::{Message, Role, Session};
use crate::agent::harness::approve::TauriApprover;
use crate::agent::harness::{
    ToolLimits, ToolRuntime, ToolServices, WorkspaceSet,
};
use crate::agent::traits::AgentContext;
use crate::commands::tools::{TauriEventSink, TauriOpener};
use crate::config::types::LlmProvider;
use crate::llm::proxy::LlmProxy;
use crate::llm::types::LlmMessage;
use crate::memory::extractor::MemoryExtractor;
use crate::store::chat_store::ToolInvocationRow;
use crate::AppState;

/// 交给 LLM 的历史消息条数上限（对应文档中的"保留最近 10 条"）
const CONTEXT_KEEP_RECENT: usize = 10;
/// 会话消息总数超过该值时，把"窗口之外"的历史压缩进持久化摘要
const CONTEXT_SUMMARIZE_THRESHOLD: i64 = 20;
/// 摘要生成失败时的降级窗口上限（宁可多带一些历史，也不让消息凭空消失）
const CONTEXT_FALLBACK_MAX: i64 = 40;

/// 估算 token 数量（粗略兜底：英文约 4 字符/token，中文约 1.4 字/token）
///
/// 仅在没有提供商 `usage` 数据时用于统计展示。
/// 历史实现两个分支写的是同一句 `tokens += 1` 再统一除以 2，
/// 导致中文场景系统性高估约 3 倍。
fn estimate_tokens(text: &str) -> i64 {
    let mut ascii = 0f64;
    let mut wide = 0f64;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1.0;
        } else {
            wide += 1.0;
        }
    }
    ((ascii / 4.0) + (wide / 1.4)).ceil().max(1.0) as i64
}

/// 构建增强检索查询：用户输入 + 最近几轮对话
fn build_retrieval_query(user_input: &str, conversation: &[Message]) -> String {
    // 取最近 3 条消息（不包括当前输入）
    let recent: Vec<&str> = conversation
        .iter()
        .rev()
        .take(3)
        .filter(|m| m.role != Role::System)
        .map(|m| m.content.as_str())
        .collect();

    if recent.is_empty() {
        return user_input.to_string();
    }

    let mut parts: Vec<&str> = recent.into_iter().rev().collect();
    parts.push(user_input);
    parts.join(" ")
}

/// 统一的流式事件载荷
///
/// 后端使用全局 `app.emit` 广播，主窗口与悬浮窗都会收到同一份事件，
/// 因此载荷必须携带 `session_id` + `stream_id` 供前端精确过滤
/// （历史实现只发裸字符串，前端按 `{session_id, data}` 解包，过滤条件恒为假：
/// 主窗口不显示任何流式内容，且 `isStreaming` 永远无法复位）。
fn stream_payload(session_id: &str, stream_id: &str, data: &str) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "stream_id": stream_id,
        "data": data,
    })
}

/// 生成会话摘要（把新增的历史合并进已有摘要）
async fn summarize_messages(
    llm_provider: &LlmProvider,
    previous: Option<&str>,
    messages: &[Message],
) -> anyhow::Result<String> {
    let mut transcript = String::new();
    for message in messages {
        let role = match message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
            Role::System => "系统",
        };
        let text = crate::agent::chat_agent::strip_think_tags(&message.content);
        if text.trim().is_empty() {
            continue;
        }
        transcript.push_str(&format!("{}：{}\n", role, text.trim()));
    }

    let mut prompt = String::from(
        "请把下面的对话压缩成简洁的中文要点摘要，保留对后续对话有影响的信息（人物、偏好、约定、正在进行的事情）。\
         不要添加评论，不要遗漏关键事实。\n",
    );
    if let Some(previous) = previous {
        prompt.push_str("\n已有的历史摘要（请在此基础上合并更新，不要丢弃已有信息）：\n");
        prompt.push_str(previous);
        prompt.push('\n');
    }
    prompt.push_str("\n需要并入摘要的对话：\n");
    prompt.push_str(&transcript);
    prompt.push_str("\n请直接输出更新后的完整摘要。");

    let summary = LlmProxy::new(llm_provider)
        .chat(vec![LlmMessage::user(prompt)])
        .await?;

    // 摘要本身也要有上限，否则它会长成新的上下文炸弹
    let summary = summary.trim().to_string();
    Ok(summary.chars().take(4000).collect())
}

/// 读取会话摘要与水位
///
/// **失败必须降级而不是中断**：摘要只是"锦上添花"的上下文，缺了它用户仍应能正常聊天。
/// 历史实现这里是 `?`，于是任何一次查询失败（例如旧库缺少 `context_summary` 列）
/// 都会让「发送」直接失败——用户看到的就是"发送失败"。
fn read_summary(store: &crate::store::chat_store::ChatStore, session_id: &str) -> (Option<String>, i64) {
    match store.get_summary_with_count(session_id) {
        Ok((summary, count)) => {
            let summary = if summary.trim().is_empty() {
                None
            } else {
                Some(summary)
            };
            (summary, count)
        }
        Err(e) => {
            eprintln!(
                "[context] 会话摘要读取失败，本轮按「无摘要」处理（不影响发送）: {}",
                e
            );
            (None, 0)
        }
    }
}

/// 加载对话上下文：必要时增量压缩早期历史
///
/// 返回 `(最近若干条原文, 持久化摘要)`。早期历史会被折叠进
/// `sessions.context_summary`（相关列与 store 方法此前已存在但从未被调用，
/// 导致长会话把整段历史塞进每一轮请求，最终必然撞上模型上下文窗口）。
async fn load_context(
    state: &AppState,
    session_id: &str,
    llm_provider: &LlmProvider,
    allow_summarize: bool,
) -> Result<(Vec<Message>, Option<String>), String> {
    let (total, summary, summarized_count) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        let total = store.count_messages(session_id).map_err(|e| e.to_string())?;
        let (summary, count) = read_summary(&store, session_id);
        (total, summary, count)
    };

    let mut summary = summary;
    let mut keep_recent = CONTEXT_KEEP_RECENT;

    // 一次性交互（persist=false）不做摘要，避免每次戳一下都多花一次 LLM 调用
    if allow_summarize && total > CONTEXT_SUMMARIZE_THRESHOLD {
        let target = (total - CONTEXT_KEEP_RECENT as i64).max(0);

        if target > summarized_count {
            let pending = {
                let store = state.chat_store.lock().map_err(|e| e.to_string())?;
                store
                    .get_messages_slice(session_id, summarized_count, target - summarized_count)
                    .map_err(|e| e.to_string())?
            };

            if pending.is_empty() {
                // 没有可摘要的新增内容（例如消息被删除），直接推进水位
                let store = state.chat_store.lock().map_err(|e| e.to_string())?;
                let _ = store.set_session_summary(
                    session_id,
                    summary.as_deref().unwrap_or(""),
                    target,
                );
            } else {
                match summarize_messages(llm_provider, summary.as_deref(), &pending).await {
                    Ok(new_summary) => {
                        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
                        if store
                            .set_session_summary(session_id, &new_summary, target)
                            .is_ok()
                        {
                            summary = Some(new_summary);
                        }
                    }
                    Err(e) => {
                        // 摘要失败时不能直接丢掉这段历史：临时放大原文窗口兜底
                        eprintln!("[context] 会话摘要生成失败，本次改用更大的原文窗口: {}", e);
                        keep_recent = (total - summarized_count)
                            .clamp(CONTEXT_KEEP_RECENT as i64, CONTEXT_FALLBACK_MAX)
                            as usize;
                    }
                }
            }
        }
    }

    let conversation = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .get_recent_messages(session_id, keep_recent)
            .map_err(|e| e.to_string())?
    };

    Ok((conversation, summary))
}

/// 为本次生成构建工具运行时
///
/// **悬浮窗（桌面宠物）永远返回 `None`**：桌宠只有聊天能力，
/// 既不暴露工具，也不会触发审批弹窗。判定依据是 Tauri 注入的窗口 label
/// 而不是 `persist` —— 悬浮窗的输入框发的是会落库的消息（`persist` 为默认 true），
/// 只有戳一戳才是 `persist: false`，用 `persist` 判定会漏掉桌宠聊天这条路径。
fn build_tool_runtime(
    app: &AppHandle,
    window: &WebviewWindow,
    state: &AppState,
    llm_provider: &LlmProvider,
) -> Option<ToolRuntime> {
    if window.label() == "float" {
        return None;
    }

    let (tools_cfg, app_data_dir) = {
        let config = state.config.lock().ok()?;
        let data_dir = state.app_data_dir.lock().ok()?.clone();
        (config.tools.clone(), data_dir)
    };

    if !tools_cfg.enabled {
        return None;
    }

    let workspaces = WorkspaceSet::from_config(&tools_cfg, &app_data_dir);
    let services = ToolServices {
        app_data_dir,
        workspaces,
        mode: tools_cfg.mode,
        llm_provider: Some(llm_provider.clone()),
        memory: Some(state.memory_store.clone()),
        personas: Some(state.personas.clone()),
        chat_store: Some(state.chat_store.clone()),
        web_domains: tools_cfg.web_domain_allowlist.clone(),
        command_allowlist: tools_cfg.command_allowlist.clone(),
        opener: Some(Arc::new(TauriOpener::new(app.clone()))),
    };

    Some(ToolRuntime {
        services,
        emit: Arc::new(TauriEventSink::new(app.clone())),
        approver: Arc::new(TauriApprover::new(
            app.clone(),
            state.pending_approvals.clone(),
        )),
        enabled: true,
        auto_approve: tools_cfg.auto_approve.clone(),
        limits: ToolLimits {
            max_output_bytes: tools_cfg.max_output_bytes,
            call_timeout: Duration::from_secs(60),
            approval_timeout: Duration::from_secs(tools_cfg.approval_timeout_secs),
        },
        max_steps: tools_cfg.max_steps,
    })
}

/// 发送消息并获取流式响应
///
/// - `stream_id`：本次生成的唯一标识，用于流式事件过滤与取消（不传则自动生成）
/// - `persist`：是否把本轮对话写入数据库（悬浮窗"戳一戳"等一次性交互传 false，
///   历史实现传了这个参数但后端并不存在，导致戳一下会落库、触发标题生成与记忆提取）
#[tauri::command]
// 参数个数由前端 IPC 契约决定（含 Tauri 注入的 app / window），不拆分
#[allow(clippy::too_many_arguments)]
pub async fn send_message(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    content: String,
    system_hint: Option<String>,
    stream_id: Option<String>,
    persist: Option<bool>,
) -> Result<(), String> {
    let persist = persist.unwrap_or(true);
    let stream_id = stream_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 会话必须存在：历史实现会先落库再抛出原始 SQLite 外键错误
    {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .get_session(&session_id)
            .map_err(|_| format!("会话不存在：{}", session_id))?;
    }

    // 存储用户消息（一次性交互不落库）
    let user_tokens = estimate_tokens(&content);
    if persist {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .add_message(&session_id, Role::User, &content, user_tokens, 0, None)
            .map_err(|e| e.to_string())?;
    }

    // 获取会话的人格 ID
    let persona_id = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .get_session(&session_id)
            .map(|s| s.persona_id)
            .unwrap_or_else(|_| "konata-default".to_string())
    };

    // 获取配置（使用活跃提供商）
    let (user_nickname, user_info, llm_provider, memory_enabled, auto_extract, max_memories) = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        (
            config.user.nickname.clone(),
            config.user.clone(),
            config.llm.active_provider().clone(),
            config.memory.enabled,
            config.memory.auto_extract,
            config.memory.max_context_memories,
        )
    };

    // 构建上下文（必要时增量压缩早期历史）
    let (conversation, context_summary) =
        load_context(&state, &session_id, &llm_provider, persist).await?;

    // 记忆检索：增强查询（用户输入 + 最近对话）→ embed → 检索
    // 一次性交互不做检索，省掉一次 embedding 调用
    let retrieved_memories = if memory_enabled && persist {
        let query = build_retrieval_query(&content, &conversation);
        let proxy = LlmProxy::new(&llm_provider);
        match proxy.embed(vec![query]).await {
            Ok(mut embeddings) => match embeddings.pop() {
                Some(query_embedding) => {
                    let mem_store = state.memory_store.lock().map_err(|e| e.to_string())?;
                    mem_store.recall(&query_embedding, max_memories).unwrap_or_default()
                }
                None => Vec::new(),
            },
            Err(e) => {
                eprintln!("[memory] 记忆检索跳过（embedding 失败）: {}", e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // 构建 AgentContext（含记忆 + 用户信息 + 工具运行时）
    // 工具运行时只对主窗口构建；悬浮窗得到 None，走与接入工具前完全一致的纯对话链路
    let tools = build_tool_runtime(&app, &window, &state, &llm_provider);
    let ctx = AgentContext {
        user_input: content.clone(),
        system_hint,
        conversation,
        context_summary,
        persona_id,
        user_nickname,
        user_info: Some(user_info),
        retrieved_memories,
        tools,
        session_id: session_id.clone(),
        stream_id: stream_id.clone(),
    };

    // 注册取消标志（供 stop_generation 使用）
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut flags = state.cancel_flags.lock().map_err(|e| e.to_string())?;
        flags.insert(
            stream_id.clone(),
            crate::ActiveStream {
                session_id: session_id.clone(),
                cancel: cancel.clone(),
            },
        );
    }

    // 流式调用（计时）—— 通过 AgentDispatcher 进行智能体路由分发
    let start_time = std::time::Instant::now();
    let app_handle = app.clone();
    let app_handle_think = app.clone();
    let thinking_buffer = Arc::new(Mutex::new(String::new()));
    let thinking_buffer_cb = thinking_buffer.clone();

    let chunk_sid = session_id.clone();
    let chunk_stream = stream_id.clone();
    let thinking_sid = session_id.clone();
    let thinking_stream = stream_id.clone();

    let result = state
        .dispatcher
        .dispatch_stream(
            &ctx,
            cancel.clone(),
            move |chunk: &str| {
                let _ = app_handle.emit(
                    "stream-chunk",
                    stream_payload(&chunk_sid, &chunk_stream, chunk),
                );
            },
            move |thinking: &str| {
                if let Ok(mut buf) = thinking_buffer_cb.lock() {
                    buf.push_str(thinking);
                }
                let _ = app_handle_think.emit(
                    "stream-thinking-chunk",
                    stream_payload(&thinking_sid, &thinking_stream, thinking),
                );
            },
        )
        .await;

    // 无论成功、失败还是被取消，都要回收取消标志（否则 map 会持续泄漏）
    {
        if let Ok(mut flags) = state.cancel_flags.lock() {
            flags.remove(&stream_id);
        }
    }

    let response = match result {
        Ok(response) => response,
        Err(e) => {
            let message = e.to_string();
            eprintln!("[chat] 生成失败 session={} stream={}: {}", session_id, stream_id, message);
            // 出错也必须通知前端：否则前端会永远停在"正在生成"状态
            let _ = app.emit(
                "stream-error",
                json!({
                    "session_id": &session_id,
                    "stream_id": &stream_id,
                    "message": &message,
                }),
            );
            return Err(message);
        }
    };

    // 计算统计
    let thinking_ms = start_time.elapsed().as_millis() as i64;
    let assistant_tokens = estimate_tokens(&response.content);
    let total_tokens = user_tokens + assistant_tokens;

    // 提取思考内容
    let thinking_content = {
        let buf = thinking_buffer.lock().unwrap_or_else(|e| e.into_inner());
        if buf.is_empty() {
            None
        } else {
            Some(buf.clone())
        }
    };

    // 先发统计再发结束信号：前端在 stream-end 时就把这条消息落位，
    // 顺序反了会让统计信息永远晚一拍（直到下次从数据库回读才补上）
    let _ = app.emit(
        "message-stats",
        json!({
            "session_id": &session_id,
            "stream_id": &stream_id,
            "token_count": total_tokens,
            "thinking_ms": thinking_ms,
        }),
    );
    let _ = app.emit(
        "stream-end",
        stream_payload(&session_id, &stream_id, &response.content),
    );

    // 一次性交互（如"戳一戳"）到此结束：不落库、不生成标题、不提取记忆
    if !persist {
        return Ok(());
    }

    // 存储 AI 回复（含元数据和思考内容）
    let invocations = response.tool_invocations.clone();
    let full_conversation = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        let assistant_message = store
            .add_message(
                &session_id,
                Role::Assistant,
                &response.content,
                assistant_tokens,
                thinking_ms,
                thinking_content,
            )
            .map_err(|e| e.to_string())?;
        // 记录使用统计
        let _ = store.record_usage(user_tokens, assistant_tokens, thinking_ms);

        // 工具轨迹：只保存预览，供 UI 回放；永不回灌模型，也不进摘要/记忆
        if !invocations.is_empty() {
            let now = Utc::now().to_rfc3339();
            let rows: Vec<ToolInvocationRow> = invocations
                .iter()
                .map(|record| ToolInvocationRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    message_id: None,
                    stream_id: stream_id.clone(),
                    step: record.step as i64,
                    tool_name: record.tool.clone(),
                    tool_label: record.tool_label.clone(),
                    arguments_json: record.arguments_json.clone(),
                    status: record.status.clone(),
                    result_preview: record.result_preview.clone(),
                    error: record.error.clone(),
                    truncated: record.truncated,
                    duration_ms: record.duration_ms,
                    approval: record.approval.clone(),
                    created_at: now.clone(),
                })
                .collect();
            if let Err(e) = store.record_tool_invocations(&rows) {
                eprintln!("[harness] 工具轨迹落库失败: {}", e);
            }
            if let Err(e) = store.attach_tool_invocations_to_message(&stream_id, &assistant_message.id)
            {
                eprintln!("[harness] 工具轨迹关联消息失败: {}", e);
            }
        }

        store.get_messages(&session_id).map_err(|e| e.to_string())?
    };

    // 通知所有窗口会话已更新（用于跨窗口同步）
    let _ = app.emit("session-updated", &session_id);

    // 自动标题生成
    let should_generate_title = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        match store.get_session(&session_id) {
            Ok(session) => session.title == "新会话",
            Err(_) => false,
        }
    };

    if should_generate_title {
        let title_prompt = format!(
            "请用一句话概括以下对话主题，不超过15个字，不要加标点，不要加引号：\n用户：{}",
            content
        );
        let title_msgs = vec![LlmMessage::user(title_prompt)];

        let title_proxy = LlmProxy::new(&llm_provider);
        if let Ok(title) = title_proxy.chat(title_msgs).await {
            let title = title.trim().trim_matches('"').trim_matches('\'');
            let title = if title.chars().count() > 20 {
                title.chars().take(20).collect::<String>()
            } else {
                title.to_string()
            };
            if !title.is_empty() {
                let store = state.chat_store.lock().map_err(|e| e.to_string())?;
                let _ = store.update_session_title(&session_id, &title);
                let _ = app.emit("session-title-updated", (&session_id, &title));
            }
        }
    }

    // 记忆提取（响应已发送，用户无感知延迟）
    if memory_enabled && auto_extract {
        // 先获取已有记忆用于去重
        let existing_memories = {
            let mem_store = state.memory_store.lock().map_err(|e| e.to_string())?;
            mem_store.list_memories(Some(30)).unwrap_or_default()
        };

        // 只把最近若干条消息交给提取器（长会话时全量输入既慢又贵）
        let extract_window: Vec<Message> = full_conversation
            .iter()
            .rev()
            .take(CONTEXT_KEEP_RECENT)
            .rev()
            .cloned()
            .collect();

        match MemoryExtractor::extract(
            &llm_provider,
            &extract_window,
            &session_id,
            &existing_memories,
        )
        .await
        {
            Ok(entries) if !entries.is_empty() => {
                // 同步存储到 DB（update 操作会覆盖已有记录）
                let mem_store = state.memory_store.lock().map_err(|e| e.to_string())?;
                for entry in &entries {
                    let _ = mem_store.store_memory(entry);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// 停止生成
///
/// 优先按 `stream_id` 精确取消；未提供时退化为取消该会话下的全部生成。
/// 返回是否真的取消到了任务。
#[tauri::command]
pub async fn stop_generation(
    state: State<'_, AppState>,
    stream_id: Option<String>,
    session_id: Option<String>,
) -> Result<bool, String> {
    let mut hit_streams: Vec<String> = Vec::new();
    {
        let flags = state.cancel_flags.lock().map_err(|e| e.to_string())?;
        for (id, active) in flags.iter() {
            let hit = match (&stream_id, &session_id) {
                (Some(stream), _) => id == stream,
                (None, Some(session)) => &active.session_id == session,
                (None, None) => false,
            };
            if hit {
                active.cancel.store(true, Ordering::SeqCst);
                hit_streams.push(id.clone());
            }
        }
    }

    // 等待审批中的生成不会因为 cancel 标志自行退出：必须把审批通道一起结束，
    // 否则用户点了停止仍要卡到审批超时（默认 120 秒）
    for stream in &hit_streams {
        let cancelled = crate::commands::tools::cancel_approvals(&state, stream);
        if cancelled > 0 {
            eprintln!("[harness] 停止生成，已终止 {} 个等待中的审批", cancelled);
        }
    }

    Ok(!hit_streams.is_empty())
}

/// 创建新会话
#[tauri::command]
pub async fn create_session(
    state: State<'_, AppState>,
    title: Option<String>,
    persona_id: Option<String>,
) -> Result<String, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    let title = title
        .map(|t| t.trim().chars().take(64).collect::<String>())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "新会话".to_string());
    let persona_id = persona_id.unwrap_or_else(|| "konata-default".to_string());
    let session = store
        .create_session(&persona_id, &title)
        .map_err(|e| e.to_string())?;
    Ok(session.id)
}

/// 查找或创建今日会话（优先复用空会话）
#[tauri::command]
pub async fn find_or_create_today_session(
    state: State<'_, AppState>,
) -> Result<Session, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;

    // 1. 优先查找最近的空会话（无消息），直接复用
    if let Some(session) = store.find_latest_empty_session().map_err(|e| e.to_string())? {
        return Ok(session);
    }

    // 2. 查找今日已有会话（有消息的）
    let today = Local::now().format("%Y-%m-%d").to_string();
    if let Some(session) = store.find_session_by_date(&today).map_err(|e| e.to_string())? {
        return Ok(session);
    }

    // 3. 都没有，创建新会话
    store
        .create_session("konata-default", "新会话")
        .map_err(|e| e.to_string())
}

/// 更新会话标题
#[tauri::command]
pub async fn update_session_title(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
) -> Result<(), String> {
    let title = title.trim().chars().take(64).collect::<String>();
    if title.is_empty() {
        return Err("标题不能为空".to_string());
    }

    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .update_session_title(&session_id, &title)
        .map_err(|e| e.to_string())
}

/// 获取所有会话列表
#[tauri::command]
pub async fn get_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<crate::agent::context::Session>, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store.list_sessions().map_err(|e| e.to_string())
}

/// 获取会话消息
#[tauri::command]
pub async fn get_messages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<Message>, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .get_messages(&session_id)
        .map_err(|e| e.to_string())
}

/// 删除会话
#[tauri::command]
pub async fn delete_session(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .delete_session(&session_id)
        .map_err(|e| e.to_string())?;
    drop(store);
    // 通知所有窗口会话已被删除
    let _ = app.emit("session-deleted", &session_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message::new(role, content, "s1")
    }

    #[test]
    fn estimate_tokens_distinguishes_ascii_and_cjk() {
        // 400 个 ASCII 字符 ≈ 100 token
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
        // 140 个汉字 ≈ 100 token（历史实现会算成 70，且与英文走同一分支）
        assert_eq!(estimate_tokens(&"此".repeat(140)), 100);
        // 空字符串也至少记为 1
        assert_eq!(estimate_tokens(""), 1);
        // 中英混合是分别加权后的和，不再共用同一个分支
        assert!(estimate_tokens(&"此".repeat(70)) > estimate_tokens(&"a".repeat(70)));
    }

    /// 旧库缺列时，摘要读取必须降级为"无摘要"而不是让发送失败
    #[test]
    fn read_summary_degrades_when_schema_is_broken() {
        use crate::store::chat_store::ChatStore;

        let dir = std::env::temp_dir().join(format!("konata-summary-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // 造一个"另一条开发线"留下的 sessions 表：没有 context_summary / summarized_count
        let conn = crate::store::db::open_connection(&dir).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '新会话',
                persona_id TEXT NOT NULL DEFAULT 'konata-default',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        let store = ChatStore::new(conn);

        // 修复前这里会 Err 并顺着 `?` 冒到前端（"发送失败"）
        let (summary, count) = read_summary(&store, "s1");
        assert!(summary.is_none());
        assert_eq!(count, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_summary_returns_existing_summary() {
        use crate::store::chat_store::ChatStore;

        let dir = std::env::temp_dir().join(format!("konata-summary-ok-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::db::init_db(&dir).unwrap();
        let store = ChatStore::new(conn);
        store.create_session("konata-default", "t").unwrap();
        let session_id = store.list_sessions().unwrap()[0].id.clone();
        store.set_session_summary(&session_id, "之前聊过猫", 3).unwrap();

        let (summary, count) = read_summary(&store, &session_id);
        assert_eq!(summary.as_deref(), Some("之前聊过猫"));
        assert_eq!(count, 3);

        // 空串摘要按"无摘要"处理
        store.set_session_summary(&session_id, "", 0).unwrap();
        let (summary, _) = read_summary(&store, &session_id);
        assert!(summary.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_payload_carries_session_and_stream_id() {
        let payload = stream_payload("sess-1", "stream-1", "你好");
        assert_eq!(payload["session_id"], "sess-1");
        assert_eq!(payload["stream_id"], "stream-1");
        assert_eq!(payload["data"], "你好");
        // 前端按 session_id 过滤，缺失会让流式渲染整体失效
        assert!(payload.get("session_id").is_some());
    }

    #[test]
    fn build_retrieval_query_puts_current_input_last() {
        let conversation = vec![
            msg(Role::User, "旧问题"),
            msg(Role::Assistant, "旧回答"),
        ];
        let query = build_retrieval_query("新问题", &conversation);
        assert!(query.ends_with("新问题"));
        assert!(query.contains("旧问题"));

        assert_eq!(build_retrieval_query("孤立问题", &[]), "孤立问题");
    }
}
