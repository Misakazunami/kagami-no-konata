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
use crate::commands::tools::{ensure_main_window, TauriEventSink, TauriOpener};
use crate::config::types::LlmProvider;
use crate::llm::proxy::LlmProxy;
use crate::llm::types::LlmMessage;
use crate::memory::extractor::MemoryExtractor;
use crate::store::chat_store::{ChatStore, RewindOutcome, ToolInvocationRow};
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

/// 把"写库失败"翻译成用户能看懂的话
///
/// 会话刚被删除时，SQLite 只会回一句 `FOREIGN KEY constraint failed`——
/// 直接抛给界面就变成「发送失败：FOREIGN KEY constraint failed」（真实报障）。
/// 这里先确认会话到底还在不在，再决定说什么。
fn persist_failure(store: &ChatStore, session_id: &str, error: anyhow::Error) -> String {
    if store.get_session(session_id).is_err() {
        format!("会话已被删除（{}），请新建一个会话再发送", session_id)
    } else {
        format!("保存消息失败：{}", error)
    }
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

/// 读取会话级任务计划
///
/// 与 `read_summary` 同样的降级原则：计划只是上下文的一部分，
/// 读失败（例如旧库缺 `session_plans` 表）绝不能让「发送」失败。
fn read_plan(
    store: &crate::store::chat_store::ChatStore,
    session_id: &str,
) -> Option<crate::agent::plan::SessionPlan> {
    match store.get_plan(session_id) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!(
                "[plan] 任务计划读取失败，本轮按「没有计划」处理（不影响发送）: {}",
                e
            );
            None
        }
    }
}

/// 读取会话工作记忆
///
/// 与计划同样的降级原则：读失败（旧库缺表等）只记录，绝不让「发送」失败。
fn read_notes(
    store: &crate::store::chat_store::ChatStore,
    session_id: &str,
) -> Vec<crate::agent::notes::SessionNote> {
    match store.list_notes(session_id) {
        Ok(notes) => notes,
        Err(e) => {
            eprintln!(
                "[notes] 工作记忆读取失败，本轮按「没有笔记」处理（不影响发送）: {}",
                e
            );
            Vec::new()
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
/// 配置 → 运行时限额的映射
///
/// 单独抽出来是为了能被测试覆盖：这些数值直接决定"一条命令能跑多久、输出留多少"，
/// 而 `build_tool_runtime` 需要 `AppHandle`，在单测里构造不出来。
fn tool_limits(cfg: &crate::config::types::ToolConfig) -> ToolLimits {
    ToolLimits {
        max_output_bytes: cfg.max_output_bytes,
        // 可配（默认 60 秒）：首次编译这类长命令需要更大的预算，
        // 超时时工具会主动收手并把已收到的输出带回来
        call_timeout: Duration::from_secs(cfg.call_timeout_secs),
        approval_timeout: Duration::from_secs(cfg.approval_timeout_secs),
    }
}

/// 会话类型 × 任务模式 → `(工具可见性, 步数预算)`
///
/// 任务会话的 Plan 与 Work 共用配置里的步数预算（下限 20）：只读调查同样可能
/// 横跨十几次工具调用（真实案例：代码审查读到第 8 轮还没读完核心文件），
/// 写死小数字只会让模型被强制收尾、计划面板永远空白。
/// 普通聊天仍限制为 3 轮，避免闲聊陷入多轮复杂思考。
///
/// 可见性：Plan 恒为只读（联网只读另由 `plan_network` 打开）；Work 恒为
/// `Full`（执行类工具必须可见，否则"任务模式"没有编码能力），审批与硬黑名单
/// 都不因可见性放宽而失效。
pub(crate) fn effective_tool_limits(
    session_type: &str,
    task_mode: &str,
    tools_cfg: &crate::config::types::ToolConfig,
) -> (crate::config::types::ToolMode, usize) {
    if session_type == "task" {
        // Work 与 Plan 同预算；Plan 的权限由调用方额外锁成只读。
        //
        // Work 恒为 Full：任务会话是用户显式创建的执行上下文，编码任务需要
        // run_command 可见；每一次命令/写入仍要审批（除非会话级 AUTO），
        // 敏感命令也仍被 command_guard 硬拦截——可见性不等于放行。
        let mode = if task_mode == "plan" {
            crate::config::types::ToolMode::ReadOnly
        } else {
            crate::config::types::ToolMode::Full
        };
        (mode, tools_cfg.max_steps.max(20))
    } else {
        (
            crate::config::types::ToolMode::ReadOnly,
            3.min(tools_cfg.max_steps),
        )
    }
}

// 参数逐个来自会话上下文；拆结构体只会把调用点变啰嗦（与 send_message 同一约定）
#[allow(clippy::too_many_arguments)]
fn build_tool_runtime(
    app: &AppHandle,
    window: &WebviewWindow,
    state: &AppState,
    llm_provider: &LlmProvider,
    session_type: &str,
    task_mode: &str,
    target_workspace_id: Option<&str>,
    session_id: &str,
    auto_approve_all: bool,
) -> Option<ToolRuntime> {
    if window.label() == "float" {
        return None;
    }

    let (mut tools_cfg, app_data_dir) = {
        let config = state.config.lock().ok()?;
        (config.tools.clone(), state.app_data_dir.clone())
    };

    if !tools_cfg.enabled {
        return None;
    }

    // 如果任务会话绑定了特定的工作区，则优先将该工作区置顶/设为该轮的默认工作区
    if let Some(ws_id) = target_workspace_id {
        if let Some(pos) = tools_cfg.workspaces.iter().position(|w| w.id == ws_id) {
            let mut target = tools_cfg.workspaces.remove(pos);
            // 标记为主工作区，使模型可以直接以相对路径访问该目录
            target.id = crate::config::types::DEFAULT_WORKSPACE_ID.to_string();
            tools_cfg.workspaces.insert(0, target);
        }
    }

    // 根据会话类型与模式动态决定工具权限与步数限制：
    // - 普通会话 (chat)：锁定为轻量只读，步数较小（如 3 轮），防止闲聊陷入多轮复杂思考
    // - 任务会话 (task)：Plan 与 Work 共用同一份步数预算（`max_steps`，下限 20）——
    //   只读调查同样可能跨十几次工具调用，写死小数字会让模型读不完就被强制收尾
    // - Plan 模式额外允许联网只读（每次仍需审批）与会话级写入（update_plan / save_note）
    let (effective_mode, effective_max_steps) =
        effective_tool_limits(session_type, task_mode, &tools_cfg);
    // 只有任务 Plan 模式放开"联网只读"：调查阶段需要查版本/报错资料
    let plan_network = session_type == "task" && task_mode == "plan";

    let workspaces = WorkspaceSet::from_config(&tools_cfg, &app_data_dir);
    // 写类工具改动前的快照：让"模型动过的文件"可以一键回滚
    let snapshots = Arc::new(crate::agent::harness::SnapshotStore::new(
        &app_data_dir,
        state.chat_store.clone(),
    ));
    let services = ToolServices {
        app_data_dir,
        workspaces,
        mode: effective_mode,
        llm_provider: Some(llm_provider.clone()),
        memory: Some(state.memory_store.clone()),
        personas: Some(state.personas.clone()),
        chat_store: Some(state.chat_store.clone()),
        web_domains: tools_cfg.web_domain_allowlist.clone(),
        command_allowlist: tools_cfg.command_allowlist.clone(),
        opener: Some(Arc::new(TauriOpener::new(app.clone()))),
        snapshots: Some(snapshots),
        // 子代理运行时由 ChatAgent 在每次生成时挂上（它才持有 LLM 后端）
        subagent: None,
        working_memory: tools_cfg.working_memory,
        // 未启用/未配好时为 None：工具会给出"请去设置里填端点"的明确指引
        search: tools_cfg.search.resolved(),
        plan_network,
        // 子代理批次的整体预算（普通工具的 call_timeout 不够它用）
        subagent_timeout: Duration::from_secs(tools_cfg.subagent_timeout_secs),
    };

    // 持久化的会话授权与全局免审批清单合并：用户在审批弹窗点过
    // 「本会话允许」的工具，后续每次生成都不再弹窗
    let mut auto_approve = tools_cfg.auto_approve.clone();
    if let Ok(store) = state.chat_store.lock() {
        for tool in store.list_session_grants(session_id).unwrap_or_default() {
            if !auto_approve.contains(&tool) {
                auto_approve.push(tool);
            }
        }
    }

    Some(ToolRuntime {
        services,
        emit: Arc::new(TauriEventSink::new(app.clone())),
        approver: Arc::new(TauriApprover::new(
            app.clone(),
            state.pending_approvals.clone(),
        )),
        enabled: true,
        auto_approve,
        // 只有任务会话会持久化开启 AUTO；这里再按会话类型兜一道，
        // 防止历史脏数据让普通聊天静默放行（AUTO 的语义只在任务模式成立）
        auto_approve_all: auto_approve_all && session_type == "task",
        limits: tool_limits(&tools_cfg),
        max_steps: effective_max_steps,
        subagent_max_children: tools_cfg.subagent_max_children,
        subagent_steps: tools_cfg.subagent_steps,
        subagent_max_tasks: tools_cfg.subagent_max_tasks,
    })
}

/// 一次生成的输入参数
///
/// `send_message`（新提问）与 `regenerate_message` / `edit_message`（重试、编辑）
/// 共用同一条生成流水线，差异全部收敛在这里：
/// - `persist_user`：重试/编辑复用历史里的用户行，绝不新插一条（否则每点一次重试
///   历史里就多一条一样的提问）；
/// - `billed_user_tokens`：重试/编辑时 prompt 上轮已经计过费，传 0 只记 completion。
struct GenerationOptions {
    session_id: String,
    content: String,
    system_hint: Option<String>,
    stream_id: String,
    /// 是否把本轮写入数据库（悬浮窗"戳一戳"等一次性交互为 false）
    persist: bool,
    /// 是否把用户消息写入数据库（false = 复用已有用户消息行）
    persist_user: bool,
    /// 记入全局用量的 prompt token
    billed_user_tokens: i64,
}

/// 注册取消标志（不做空闲检查：两个窗口同会话并发生成是既有设计）
fn register_stream(
    state: &AppState,
    session_id: &str,
    stream_id: &str,
) -> Result<Arc<AtomicBool>, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut flags = state.cancel_flags.lock().map_err(|e| e.to_string())?;
    flags.insert(
        stream_id.to_string(),
        crate::ActiveStream {
            session_id: session_id.to_string(),
            cancel: cancel.clone(),
        },
    );
    Ok(cancel)
}

/// 回收取消标志（无论成功、失败还是被取消都必须执行，否则 map 会持续泄漏）
fn remove_stream(state: &AppState, stream_id: &str) {
    if let Ok(mut flags) = state.cancel_flags.lock() {
        flags.remove(stream_id);
    }
}

/// 为"改写历史"类操作（回退 / 重试 / 编辑）预占会话
///
/// 检查与注册在同一把锁内完成，避免"检查通过 → 另一个窗口恰好开始生成"的竞态。
/// 生成结束时 assistant 消息会追加到消息表末尾，若与截断并发，落点会错乱，
/// 因此这类操作必须等到该会话没有在飞生成。
fn reserve_session(
    state: &AppState,
    session_id: &str,
    stream_id: &str,
) -> Result<Arc<AtomicBool>, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut flags = state.cancel_flags.lock().map_err(|e| e.to_string())?;
    if flags
        .values()
        .any(|active| active.session_id == session_id)
    {
        return Err("该会话正在生成中，请先停止或等待完成".to_string());
    }
    flags.insert(
        stream_id.to_string(),
        crate::ActiveStream {
            session_id: session_id.to_string(),
            cancel: cancel.clone(),
        },
    );
    Ok(cancel)
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

    let user_tokens = estimate_tokens(&content);
    // 取消标志在生成开始前注册，由 wrapper 统一回收（任何早期错误也不会泄漏）
    let cancel = register_stream(&state, &session_id, &stream_id)?;
    let result = run_generation(
        &app,
        &window,
        &state,
        GenerationOptions {
            session_id: session_id.clone(),
            content,
            system_hint,
            stream_id: stream_id.clone(),
            persist,
            persist_user: true,
            billed_user_tokens: user_tokens,
        },
        cancel,
    )
    .await;
    remove_stream(&state, &stream_id);
    result
}

/// 生成主流程：调用方负责会话存在性校验与取消标志的注册 / 回收
async fn run_generation(
    app: &AppHandle,
    window: &WebviewWindow,
    state: &State<'_, AppState>,
    opts: GenerationOptions,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let GenerationOptions {
        session_id,
        content,
        system_hint,
        stream_id,
        persist,
        persist_user,
        billed_user_tokens,
    } = opts;

    // 存储用户消息（一次性交互不落库；重试/编辑复用已有用户行，不插新行）
    let user_tokens = estimate_tokens(&content);
    let mut user_message_id: Option<String> = None;
    if persist && persist_user {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        match store.add_message(&session_id, Role::User, &content, user_tokens, 0, None, None) {
            Ok(message) => user_message_id = Some(message.id),
            Err(e) => return Err(persist_failure(&store, &session_id, e)),
        }
    }

    // 获取会话的人格 ID、会话类型、任务模式、工作区、模型偏好与 AUTO 开关
    let (persona_id, session_type, task_mode, workspace_id, model_pref, auto_approve_all) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .get_session(&session_id)
            .map(|s| {
                (
                    s.persona_id,
                    s.session_type,
                    s.task_mode,
                    s.workspace_id,
                    s.model_pref,
                    s.auto_approve_all,
                )
            })
            .unwrap_or_else(|_| {
                (
                    "konata-default".to_string(),
                    "chat".to_string(),
                    "plan".to_string(),
                    None,
                    None,
                    false,
                )
            })
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

    // 本轮模型方案：会话级选择 + 主/子模型路由（详见 `llm::router`）
    //
    // 解析在这里（而不是 Agent 内部）完成：本轮生成拿到的是**快照**，
    // 因此用户在生成过程中切换模型不会影响这一轮，两个窗口并发生成也互不影响。
    let models = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        crate::llm::router::resolve(&config, model_pref.as_ref(), &session_type, &task_mode)
    };

    // 构建上下文（必要时增量压缩早期历史）
    let (conversation, context_summary) =
        load_context(state, &session_id, &llm_provider, persist).await?;

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
    let tools = build_tool_runtime(
        app,
        window,
        state,
        &llm_provider,
        &session_type,
        &task_mode,
        workspace_id.as_deref(),
        &session_id,
        auto_approve_all,
    );
    // 任务计划只服务于"有工具的链路"：悬浮窗不注入，保持纯对话行为不变
    let plan = if tools.is_some() {
        state
            .chat_store
            .lock()
            .ok()
            .and_then(|store| read_plan(&store, &session_id))
    } else {
        None
    };
    // 工作记忆同样只服务于有工具的链路；关掉开关时连读都不读
    let notes = if tools
        .as_ref()
        .map(|runtime| runtime.services.working_memory)
        .unwrap_or(false)
    {
        state
            .chat_store
            .lock()
            .ok()
            .map(|store| read_notes(&store, &session_id))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
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
        models: Some(Arc::new(models.clone())),
        plan,
        notes,
        session_type,
        task_mode,
        session_id: session_id.clone(),
        stream_id: stream_id.clone(),
    };

    // 取消标志由调用方（send_message / regenerate_message / edit_message）预先注册，
    // 这里直接使用 `cancel`；生成结束统一由调用方回收。

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
    // 工具额外开销（只读子代理自己发起过生成）也要算进去，否则用户会疑惑
    // "我只问了一句话，用量怎么涨这么多"
    let total_tokens = user_tokens + assistant_tokens + response.extra_tokens as i64;

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
            // 本轮实际用的模型（自动选择下主/子模型不同，必须让用户看得见）
            "model": models.label(),
            // 工具步数用尽被迫收尾时如实告知：界面提示"中断，可继续"，
            // 不再让用户以为模型已经答完了（长任务被静默截断是真实报障）
            "step_limit_hit": response.hit_step_limit,
            "tool_steps": response.tool_steps,
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
    // 本轮实际使用的模型：随消息落库，历史消息也能显示"这条是谁答的"
    let model_label = models.label();
    let (full_conversation, assistant_message_id) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        let assistant_message = match store.add_message(
            &session_id,
            Role::Assistant,
            &response.content,
            assistant_tokens,
            thinking_ms,
            thinking_content,
            Some(model_label.as_str()),
        ) {
            Ok(message) => message,
            Err(e) => {
                // 会话在生成期间被删除（用户在长任务跑到一半时删掉了它）：
                // 正文已经流式送达，而这条会话行已经不存在 —— 往一张不存在的
                // 会话里写回复没有任何意义，更不该把裸的 SQLite 外键错误
                // 抛成「发送失败」（真实报障：FOREIGN KEY constraint failed）。
                if store.get_session(&session_id).is_err() {
                    eprintln!(
                        "[chat] 会话已被删除，本轮回复不落库 session={} stream={}",
                        session_id, stream_id
                    );
                    return Ok(());
                }
                return Err(format!("保存回复失败：{}", e));
            }
        };
        // 记录使用统计（重试/编辑不重复计 prompt token）
        let _ = store.record_usage(billed_user_tokens, assistant_tokens, thinking_ms);

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
                    extra_tokens: record.extra_tokens as i64,
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

        let full = store.get_messages(&session_id).map_err(|e| e.to_string())?;
        (full, assistant_message.id)
    };

    // 通知所有窗口会话已更新（用于跨窗口同步）
    //
    // 载荷带 `stream_id` 与本轮落库的消息 id：主窗口据此把本地乐观消息
    // （`crypto.randomUUID`，与数据库 id 不同源）换成数据库 id——重试/编辑/回退
    // 都要凭 id 找到消息；其它窗口以及悬浮窗的生成仍然靠它触发回读同步。
    let _ = app.emit(
        "session-updated",
        json!({
            "session_id": &session_id,
            "stream_id": &stream_id,
            "message_id": &assistant_message_id,
            "user_message_id": &user_message_id,
        }),
    );

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

// ─── 历史回退 / 重试 / 编辑 ─────────────────────────────

/// 回退预览中的一个受影响轮次（界面据此展示"可一并撤销的文件改动"）
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewindAffectedStreamView {
    pub stream_id: String,
    pub files: usize,
    pub bytes: i64,
}

/// 回退预览（只读，不写库）
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewindPreviewView {
    pub removed: usize,
    pub affected_streams: Vec<RewindAffectedStreamView>,
}

/// 预览"回退到某条消息"会删除多少内容、涉及哪些可回滚的文件轮次
#[tauri::command]
pub async fn preview_rewind(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    message_id: String,
    inclusive: bool,
) -> Result<RewindPreviewView, String> {
    ensure_main_window(&window)?;
    let (outcome, snapshot_rows) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        let outcome = store
            .preview_rewind(&session_id, &message_id, inclusive)
            .map_err(|e| e.to_string())?;
        // 快照清单失败只影响"文件数"展示，不阻断回退预览
        let rows = store.list_session_snapshots(&session_id).unwrap_or_default();
        (outcome, rows)
    };

    let mut affected: Vec<RewindAffectedStreamView> = Vec::new();
    for row in snapshot_rows {
        if !outcome.affected_streams.contains(&row.stream_id) {
            continue;
        }
        match affected.iter_mut().find(|s| s.stream_id == row.stream_id) {
            Some(stream) => {
                stream.files += 1;
                stream.bytes += row.bytes;
            }
            None => affected.push(RewindAffectedStreamView {
                stream_id: row.stream_id,
                files: 1,
                bytes: row.bytes,
            }),
        }
    }
    // `list_session_snapshots` 按 created_at DESC 返回，首次出现顺序即"新 → 旧"：
    // 保持这个顺序，界面撤销文件改动时也按同一顺序恢复（先撤最新一轮）
    Ok(RewindPreviewView {
        removed: outcome.removed,
        affected_streams: affected,
    })
}

/// 回退：删除某条消息及其之后的全部消息（不触发生成）
///
/// `inclusive=false` 时保留目标消息本身（重试用户提问用）。
#[tauri::command]
pub async fn delete_messages_from(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    message_id: String,
    inclusive: Option<bool>,
) -> Result<RewindOutcome, String> {
    ensure_main_window(&window)?;

    // 借用 cancel_flags 做一次"会话空闲"检查与占位，避免与在飞生成交错
    let guard_id = format!("rewind-{}", uuid::Uuid::new_v4());
    reserve_session(&state, &session_id, &guard_id)?;
    let result = {
        let store = state.chat_store.lock();
        match store {
            Ok(store) => store
                .rewind_messages(&session_id, &message_id, inclusive.unwrap_or(true))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    };
    remove_stream(&state, &guard_id);

    let outcome = result?;
    // 空 stream_id：两个窗口都会回读（非空且等于本窗口 lastLocalStreamId 时才会被跳过）
    let _ = app.emit(
        "session-updated",
        json!({ "session_id": &session_id, "stream_id": "" }),
    );
    Ok(outcome)
}

/// 重试：删除原回复及其后内容，用原提问重新生成
///
/// - 目标是 assistant：删除该回复及其后全部消息，用其前一条用户消息重新生成；
/// - 目标是 user（上一轮生成失败/被取消）：保留该提问，删除其后内容后重新生成。
#[tauri::command]
pub async fn regenerate_message(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    message_id: String,
    stream_id: Option<String>,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    let stream_id = stream_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 先定位原提问与截断边界（不写库）
    let (content, inclusive) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        let target = store
            .get_message(&session_id, &message_id)
            .map_err(|e| e.to_string())?;
        match target.role {
            Role::User => (target.content, false),
            Role::Assistant => {
                let parent = store
                    .last_user_message_before(&session_id, &message_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "找不到该回复对应的用户提问，无法重试".to_string())?;
                (parent.content, true)
            }
            Role::System => return Err("系统消息不支持重试".to_string()),
        }
    };

    let cancel = reserve_session(&state, &session_id, &stream_id)?;
    // 截断必须先于生成完成：上下文读取的是截断后的历史
    let prepared = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .rewind_messages(&session_id, &message_id, inclusive)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };

    let result = match prepared {
        Ok(()) => {
            run_generation(
                &app,
                &window,
                &state,
                GenerationOptions {
                    session_id: session_id.clone(),
                    content,
                    system_hint: None,
                    stream_id: stream_id.clone(),
                    persist: true,
                    persist_user: false,
                    // prompt 已在上一次生成计过费，重试只记 completion
                    billed_user_tokens: 0,
                },
                cancel,
            )
            .await
        }
        Err(e) => Err(e),
    };
    remove_stream(&state, &stream_id);
    result
}

/// 编辑用户消息：更新正文并删除其后全部消息，然后立即重新生成
#[tauri::command]
pub async fn edit_message(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    message_id: String,
    content: String,
    stream_id: Option<String>,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("消息内容不能为空".to_string());
    }
    let stream_id = stream_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let cancel = reserve_session(&state, &session_id, &stream_id)?;
    let token_count = estimate_tokens(&content);
    let prepared = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .edit_user_message(&session_id, &message_id, &content, token_count)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };

    let result = match prepared {
        Ok(()) => {
            run_generation(
                &app,
                &window,
                &state,
                GenerationOptions {
                    session_id: session_id.clone(),
                    content,
                    system_hint: None,
                    stream_id: stream_id.clone(),
                    persist: true,
                    persist_user: false,
                    // prompt 已在上一次生成计过费，编辑重发只记 completion
                    billed_user_tokens: 0,
                },
                cancel,
            )
            .await
        }
        Err(e) => Err(e),
    };
    remove_stream(&state, &stream_id);
    result
}

/// 停止生成
///
/// 优先按 `stream_id` 精确取消；未提供时退化为取消该会话下的全部生成。
/// 返回是否真的取消到了任务。
#[tauri::command]
pub async fn stop_generation(
    app: AppHandle,
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

    if !hit_streams.is_empty() {
        if let Some(session) = session_id.as_deref() {
            mark_plan_blocked_on_stop(&app, &state, session);
        }
    }

    Ok(!hit_streams.is_empty())
}

/// 生成被用户停止后，把计划里"进行中"的条目改成"受阻"并广播
///
/// 提示词要求模型"用户中途停止时把做不下去的项标成 blocked"，但模型在
/// 取消后没有任何执行机会；不代它更新的话计划会永远停在"进行中"，
/// 用户也无从判断任务其实已经停了。
fn mark_plan_blocked_on_stop(app: &AppHandle, state: &State<'_, AppState>, session_id: &str) {
    let updated = {
        let Ok(store) = state.chat_store.lock() else {
            return;
        };
        let Ok(Some(mut plan)) = store.get_plan(session_id) else {
            return;
        };
        let mut changed = false;
        for item in &mut plan.items {
            if item.status == crate::agent::plan::PlanStatus::Doing {
                item.status = crate::agent::plan::PlanStatus::Blocked;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        plan.updated_at = Utc::now().to_rfc3339();
        if store.save_plan(session_id, &plan).is_err() {
            return;
        }
        plan
    };
    let _ = app.emit_to(
        tauri::EventTarget::webview_window("main"),
        crate::agent::harness::EVENT_PLAN_UPDATED,
        json!({
            "session_id": session_id,
            "items": updated.items,
            "note": updated.note,
        }),
    );
}

/// 创建新会话
#[tauri::command]
pub async fn create_session(
    state: State<'_, AppState>,
    title: Option<String>,
    persona_id: Option<String>,
    session_type: Option<String>,
    workspace_id: Option<String>,
    model_pref: Option<crate::llm::router::SessionModelPref>,
    task_mode: Option<String>,
) -> Result<String, String> {
    let is_task = session_type.as_deref() == Some("task");
    let title = title
        .map(|t| t.trim().chars().take(64).collect::<String>())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            if is_task {
                "新任务".to_string()
            } else {
                "新会话".to_string()
            }
        });
    let persona_id = persona_id.unwrap_or_else(|| "konata-default".to_string());
    // 任务模式在创建时即可选择（默认 plan）；非任务会话按 chat 忽略该字段
    let task_mode = if is_task {
        match task_mode.as_deref() {
            Some("work") => "work",
            _ => "plan",
        }
    } else {
        "plan"
    };

    // 新的任务会话可以按配置默认开启"自动选择"（只对任务会话有意义）
    let model_pref = match model_pref {
        Some(pref) => Some(pref),
        None if is_task => {
            let config = state.config.lock().map_err(|e| e.to_string())?;
            config
                .models
                .auto_by_default
                .then(crate::llm::router::SessionModelPref::auto)
        }
        None => None,
    };

    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    let session = store
        .create_session_with_model(
            &persona_id,
            &title,
            session_type.as_deref(),
            Some(task_mode),
            workspace_id.as_deref(),
            model_pref.as_ref(),
        )
        .map_err(|e| e.to_string())?;
    Ok(session.id)
}

/// 设置会话级模型选择（手动选定模型 / 自动选择 / 深度思考开关）
///
/// `pref` 为 `None` 时清除选择，回到"跟随全局活跃提供商"。
/// 只写数据库，不触碰任何全局状态：因此**不会影响正在跑的生成**
/// （本轮生成用的是发送那一刻的快照，见 `llm::router`）。
///
/// 刻意**不**广播 `session-updated`：那个事件会让前端重读整个会话
/// （含消息列表），而模型选择已经由前端本地同步；另一侧（悬浮窗）没有模型 UI，
/// 它下一条消息直接读数据库里的新值即可。
#[tauri::command]
pub async fn set_session_model(
    state: State<'_, AppState>,
    session_id: String,
    pref: Option<crate::llm::router::SessionModelPref>,
) -> Result<(), String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    // 会话必须存在：否则用户会以为"选上了"，其实写进了一条不存在的会话
    store
        .get_session(&session_id)
        .map_err(|_| format!("会话不存在：{}", session_id))?;
    store
        .set_session_model_pref(&session_id, pref.as_ref())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 切换会话的任务模式 (plan | work)
///
/// 模式在**每轮发送时**从会话行解析（见 `build_tool_runtime`），因此本命令
/// 不影响正在跑的生成；前端在生成中禁用切换按钮，避免界面显示的"新模式"
/// 与在飞请求的旧模式不一致。
///
/// 不广播 `session-updated`：那个事件会让前端重读整个会话（含消息列表），
/// 而主窗口已经本地同步；模式也不在悬浮窗的展示范围内。
#[tauri::command]
pub async fn set_task_mode(
    state: State<'_, AppState>,
    session_id: String,
    task_mode: String,
) -> Result<(), String> {
    let mode = match task_mode.to_lowercase().as_str() {
        "work" => "work",
        _ => "plan",
    };
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    // 会话必须存在：否则用户会以为"切过去了"，其实写进了一条不存在的会话
    store
        .get_session(&session_id)
        .map_err(|_| format!("会话不存在：{}", session_id))?;
    store
        .set_task_mode(&session_id, mode)
        .map_err(|e| e.to_string())
}

/// 开启/关闭会话级 AUTO（自动允许所有需要审批的工具调用）
///
/// 只对任务会话开放：普通聊天工具权限本就是只读轻量，AUTO 没有意义，
/// 拒绝它还能避免"以为开了 AUTO 就能执行命令"的误解。
///
/// 与 `set_task_mode` 一样：只写数据库，不影响正在跑的生成
/// （本轮生成用的是发送那一刻解析出的运行时快照）。
#[tauri::command]
pub async fn set_session_auto_approve(
    state: State<'_, AppState>,
    session_id: String,
    enabled: bool,
) -> Result<(), String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    let session = store
        .get_session(&session_id)
        .map_err(|_| format!("会话不存在：{}", session_id))?;
    if session.session_type != "task" {
        return Err("AUTO 仅对任务会话可用".to_string());
    }
    store
        .set_session_auto_approve(&session_id, enabled)
        .map_err(|e| e.to_string())
}

/// 设置/清除会话绑定的工作区（`workspace_id` 为 `None` 表示回到默认沙箱）
///
/// 工作区必须已在 `tools.workspaces` 里配置：悬空 id 会让 `build_tool_runtime`
/// 静默按默认沙箱执行，而用户以为任务跑在指定目录里。
#[tauri::command]
pub async fn set_session_workspace(
    state: State<'_, AppState>,
    session_id: String,
    workspace_id: Option<String>,
) -> Result<(), String> {
    if let Some(id) = workspace_id.as_deref() {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        if !config.tools.workspaces.iter().any(|w| w.id == id) {
            return Err(format!("工作区不存在：{}", id));
        }
    }
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .get_session(&session_id)
        .map_err(|_| format!("会话不存在：{}", session_id))?;
    store
        .set_session_workspace(&session_id, workspace_id.as_deref())
        .map_err(|e| e.to_string())
}

/// 查找或创建今日会话（优先复用空会话）
#[tauri::command]
pub async fn find_or_create_today_session(
    state: State<'_, AppState>,
) -> Result<Session, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;

    // 1. 优先查找最近的空会话（无消息），直接复用。必须是普通会话：
    //    任务会话有自己的工作区与模式，混用会让桌宠/聊天入口跑进任务工程
    if let Some(session) = store
        .find_latest_empty_session("chat")
        .map_err(|e| e.to_string())?
    {
        return Ok(session);
    }

    // 2. 查找今日已有会话（有消息的）
    let today = Local::now().format("%Y-%m-%d").to_string();
    if let Some(session) = store
        .find_session_by_date(&today, "chat")
        .map_err(|e| e.to_string())?
    {
        return Ok(session);
    }

    // 3. 都没有，创建新会话
    store
        .create_session("konata-default", "新会话", Some("chat"), Some("plan"), None)
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
///
/// 必须先停掉这个会话正在跑的生成：删除只删数据库行，生成任务仍在跑，
/// 它跑完会尝试把回复写回 `messages` —— 而那条会话行已经没了，SQLite 只会回
/// `FOREIGN KEY constraint failed`，用户看到的是「发送失败：FOREIGN KEY
/// constraint failed」（真实报障）。停止之后生成会在下一次检查点收手。
#[tauri::command]
pub async fn delete_session(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let cancelled: Vec<String> = {
        let flags = state.cancel_flags.lock().map_err(|e| e.to_string())?;
        flags
            .iter()
            .filter(|(_, active)| active.session_id == session_id)
            .map(|(id, active)| {
                active.cancel.store(true, Ordering::SeqCst);
                id.clone()
            })
            .collect()
    };
    for stream in &cancelled {
        // 正在等审批的生成不会因为 cancel 标志自行退出，审批通道要一起结束
        crate::commands::tools::cancel_approvals(&state, stream);
    }
    if !cancelled.is_empty() {
        eprintln!(
            "[chat] 删除会话 {}，已停止 {} 个进行中的生成",
            session_id,
            cancelled.len()
        );
    }

    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    // 快照索引行会随会话级联删除，但磁盘上的备份目录不会：
    // 必须在删行前把 stream 清单取出来，删完会话后清理目录
    let snapshot_streams = store
        .list_snapshot_streams(&session_id)
        .unwrap_or_default();
    store
        .delete_session(&session_id)
        .map_err(|e| e.to_string())?;
    drop(store);
    if !snapshot_streams.is_empty() {
        let snapshots = crate::agent::harness::SnapshotStore::new(
            &state.app_data_dir,
            state.chat_store.clone(),
        );
        let removed = snapshots.forget_streams(&snapshot_streams);
        eprintln!(
            "[snapshot] 会话 {} 已删除，清理 {} 个备份目录",
            session_id, removed
        );
    }
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

    /// 配置里的超时必须真的落到运行时限额上（曾经写死 60 秒，设置页改不动）
    #[test]
    fn tool_limits_follow_config() {
        let mut cfg = crate::config::types::ToolConfig::default();
        assert_eq!(tool_limits(&cfg).call_timeout, Duration::from_secs(180));

        cfg.call_timeout_secs = 600;
        cfg.max_output_bytes = 128 * 1024;
        cfg.approval_timeout_secs = 300;
        let limits = tool_limits(&cfg);
        assert_eq!(limits.call_timeout, Duration::from_secs(600));
        assert_eq!(limits.max_output_bytes, 128 * 1024);
        assert_eq!(limits.approval_timeout, Duration::from_secs(300));
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
        store.create_session("konata-default", "t", None, None, None).unwrap();
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

    /// 会话中途被删掉时，写库失败必须翻译成一句人话，
    /// 而不是把裸的 `FOREIGN KEY constraint failed` 抛成「发送失败」
    #[test]
    fn deleted_session_write_failure_is_explained() {
        let dir = std::env::temp_dir().join(format!("konata-fk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::store::db::init_db(&dir).unwrap();
        let store = ChatStore::new(conn);
        let session = store
            .create_session("konata-default", "t", Some("task"), Some("plan"), None)
            .unwrap();

        // 生成跑到一半时用户把会话删了
        store.delete_session(&session.id).unwrap();

        // 此时再写消息：底层就是那句外键错误
        let error = store
            .add_message(&session.id, Role::Assistant, "回复", 1, 0, None, None)
            .unwrap_err();
        assert!(
            error.to_string().contains("FOREIGN KEY"),
            "底层错误应是外键约束：{error}"
        );

        // 翻译后必须是可执行的提示，而不是 SQLite 原文
        let message = persist_failure(&store, &session.id, error);
        assert!(message.contains("会话已被删除"), "{message}");
        assert!(!message.contains("FOREIGN KEY"), "{message}");

        // 会话还在时不要乱改口：如实报告保存失败
        let alive = store
            .create_session("konata-default", "t2", None, None, None)
            .unwrap();
        let other = anyhow::anyhow!("disk I/O error");
        let message = persist_failure(&store, &alive.id, other);
        assert!(message.contains("保存消息失败"), "{message}");
        assert!(message.contains("disk I/O error"), "{message}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_payload_carries_session_and_stream_id() {        let payload = stream_payload("sess-1", "stream-1", "你好");
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

    /// 会话类型 × 模式 → 工具可见性与步数预算
    #[test]
    fn effective_tool_limits_follow_session_type() {
        use crate::config::types::{ToolConfig, ToolMode};

        let cfg = ToolConfig::default(); // max_steps = 32

        // 任务会话：Plan 只读、Work 恒为 Full；两者共用同一预算
        let (mode, steps) = effective_tool_limits("task", "plan", &cfg);
        assert_eq!(mode, ToolMode::ReadOnly);
        assert_eq!(steps, cfg.max_steps.max(20), "Plan 不应被写死成小预算");

        let (mode, steps) = effective_tool_limits("task", "work", &cfg);
        assert_eq!(mode, ToolMode::Full, "Work 不跟随全局配置");
        assert_eq!(steps, cfg.max_steps.max(20));

        // 即使全局配置收紧到只读/标准，Work 依然能看到执行类工具
        for configured in [ToolMode::ReadOnly, ToolMode::Standard] {
            let mut narrowed = cfg.clone();
            narrowed.mode = configured;
            assert_eq!(
                effective_tool_limits("task", "work", &narrowed).0,
                ToolMode::Full
            );
        }

        // 配置值低于下限时任务会话仍保底 20 轮
        let mut low = cfg.clone();
        low.max_steps = 4;
        assert_eq!(effective_tool_limits("task", "plan", &low).1, 20);
        assert_eq!(effective_tool_limits("task", "work", &low).1, 20);

        // 普通聊天：只读且轻量
        let (mode, steps) = effective_tool_limits("chat", "plan", &low);
        assert_eq!(mode, ToolMode::ReadOnly);
        assert_eq!(steps, 3);
    }
}
