use anyhow::Result;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State, WebviewWindow};

use crate::agent::harness::{
    approve::cancel_pending_for_stream, EventSink, SystemOpener, ToolDecision, ToolInfo,
};
use crate::config::types::{
    is_valid_workspace_id, WorkspacePath, WorkspaceRoot, MAX_WORKSPACE_ROOTS,
};
use crate::store::chat_store::ToolInvocationRow;
use crate::AppState;

/// 工具类命令只允许主窗口调用（悬浮窗是纯聊天 surface）
pub(crate) fn ensure_main_window(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == "float" {
        return Err("悬浮窗不支持工具功能".to_string());
    }
    Ok(())
}

/// 把 Tauri 广播包装成 harness 的事件出口
pub struct TauriEventSink {
    app: AppHandle,
}

impl TauriEventSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for TauriEventSink {
    fn emit(&self, event: &str, payload: Value) {
        // 定向到主窗口：桌宠窗口不订阅、也收不到工具事件
        let _ = self
            .app
            .emit_to(tauri::EventTarget::webview_window("main"), event, payload);
    }
}

/// 用系统默认程序打开文件/链接
pub struct TauriOpener {
    app: AppHandle,
}

impl TauriOpener {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl SystemOpener for TauriOpener {
    fn open(&self, target: &str) -> Result<()> {
        use tauri_plugin_opener::OpenerExt;
        self.app
            .opener()
            .open_path(target, None::<&str>)
            .map_err(|e| anyhow::anyhow!("打开失败：{}", e))
    }
}

/// 当前主窗口的工具清单
#[tauri::command]
pub async fn list_tools(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<ToolInfo>, String> {
    ensure_main_window(&window)?;
    let mode = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        config.tools.mode
    };
    Ok(state.dispatcher.chat_agent().tools().infos(mode))
}

/// 用户对一次工具调用的审批决定
#[tauri::command]
pub async fn resolve_tool_approval(
    window: WebviewWindow,
    state: State<'_, AppState>,
    approval_id: String,
    decision: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    let parsed = ToolDecision::parse(&decision);
    let entry = {
        let mut map = state
            .pending_approvals
            .lock()
            .map_err(|e| e.to_string())?;
        map.remove(&approval_id)
    };
    match entry {
        Some(pending) => pending
            .sender
            .send(parsed)
            .map_err(|_| "审批已失效（生成可能已结束）".to_string()),
        None => Err("审批不存在或已超时".to_string()),
    }
}

/// 读取某个会话的工具调用轨迹（UI 回放用）
#[tauri::command]
pub async fn get_tool_invocations(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<ToolInvocationRow>, String> {
    ensure_main_window(&window)?;
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .get_tool_invocations(&session_id)
        .map_err(|e| e.to_string())
}

// ─── 任务计划（`update_plan` 工具写、界面读） ───────────────

/// 计划在界面上的视图（`items` 已是结构化数据，前端不再解析 JSON）
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanView {
    pub session_id: String,
    pub items: Vec<crate::agent::plan::PlanItem>,
    pub note: Option<String>,
    pub updated_at: String,
}

/// 读取某个会话的任务计划（没有计划时返回 null）
#[tauri::command]
pub async fn get_plan(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<PlanView>, String> {
    ensure_main_window(&window)?;
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    let plan = store
        .get_plan(&session_id)
        .map_err(|e| e.to_string())?
        .filter(|plan| !plan.is_empty());
    Ok(plan.map(|plan| PlanView {
        session_id: session_id.clone(),
        items: plan.items,
        note: plan.note,
        updated_at: plan.updated_at,
    }))
}

/// 用户手动清空计划（模型自己也能通过 `update_plan` 传空数组清空）
///
/// 清空后同样广播 `plan-updated`，界面不必再单独回读一次。
#[tauri::command]
pub async fn clear_plan(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store.clear_plan(&session_id).map_err(|e| e.to_string())?;
    }
    let _ = app.emit_to(
        tauri::EventTarget::webview_window("main"),
        crate::agent::harness::EVENT_PLAN_UPDATED,
        serde_json::json!({
            "session_id": session_id,
            "items": [],
            "note": serde_json::Value::Null,
        }),
    );
    Ok(())
}

/// 用户手动编辑计划（勾选完成 / 改标题 / 增删条目）
///
/// 与模型的 `update_plan` 共用同一份校验与存储（整体覆盖式提交）；
/// 写完广播 `plan-updated`，模型下一轮就会看到用户调整过的进度。
#[tauri::command]
pub async fn update_plan_items(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    items: serde_json::Value,
    note: Option<String>,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    let items = crate::agent::plan::sanitize_items(&items)?;
    let plan = crate::agent::plan::SessionPlan::new(
        items,
        note.map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    );
    {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .save_plan(&session_id, &plan)
            .map_err(|e| e.to_string())?;
    }
    let _ = app.emit_to(
        tauri::EventTarget::webview_window("main"),
        crate::agent::harness::EVENT_PLAN_UPDATED,
        serde_json::json!({
            "session_id": session_id,
            "items": plan.items,
            "note": plan.note,
        }),
    );
    Ok(())
}

// ─── 会话级持久授权（审批弹窗的「本会话允许」） ───

/// 某个会话已授权的工具名（按字母序）
#[tauri::command]
pub async fn list_session_grants(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<String>, String> {
    ensure_main_window(&window)?;
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .list_session_grants(&session_id)
        .map_err(|e| e.to_string())
}

/// 撤销某个工具的会话授权（返回是否真的撤销了一条）
#[tauri::command]
pub async fn revoke_session_grant(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    tool: String,
) -> Result<bool, String> {
    ensure_main_window(&window)?;
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .revoke_session_tool(&session_id, &tool)
        .map_err(|e| e.to_string())
}

// ─── MCP 服务器（诊断用） ───────────────────────────────

/// 已配置的 MCP 服务器（只读视图，命令与参数不对外暴露完整环境变量）
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerView {
    pub id: String,
    pub enabled: bool,
    pub trusted: bool,
    pub permission: String,
    pub command: String,
    pub args: Vec<String>,
}

fn mcp_server_views(state: &AppState) -> Result<Vec<McpServerView>, String> {
    let config = state.config.lock().map_err(|e| e.to_string())?;
    Ok(config
        .tools
        .mcp
        .servers
        .iter()
        .map(|server| McpServerView {
            id: server.id.clone(),
            enabled: server.enabled,
            trusted: server.trusted,
            permission: server.permission.as_str().to_string(),
            command: server.command.clone(),
            args: server.args.clone(),
        })
        .collect())
}

#[tauri::command]
pub async fn list_mcp_servers(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<McpServerView>, String> {
    ensure_main_window(&window)?;
    mcp_server_views(&state)
}

/// 测试连接：真的把服务器拉起来并列出它的工具
///
/// 这是"配置有没有写对"的唯一可靠答案（进程能否启动、协议是否对得上、
/// 到底暴露了哪些工具）。没有启用/未标记可信的服务器不会被启动。
#[tauri::command]
pub async fn test_mcp_server(
    window: WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<crate::mcp::McpServerStatus, String> {
    ensure_main_window(&window)?;
    let server = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        config
            .tools
            .mcp
            .servers
            .iter()
            .find(|server| server.id == id)
            .cloned()
            .ok_or_else(|| format!("没有找到 id 为 {} 的 MCP 服务器", id))?
    };
    Ok(crate::mcp::probe(&server).await)
}

// ─── 工作记忆（`save_note` 工具写、界面读） ───────────────

/// 读取某个会话的工作记忆
#[tauri::command]
pub async fn get_notes(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<crate::agent::notes::SessionNote>, String> {
    ensure_main_window(&window)?;
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store
        .list_notes(&session_id)
        .map_err(|e| e.to_string())
}

/// 清空某个会话的工作记忆（模型也能通过 `forget_note` 清空）
///
/// 这是用户对"跨轮记忆"的最后一道控制：模型记了什么、什么时候清掉，用户说了算。
#[tauri::command]
pub async fn clear_notes(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<usize, String> {
    ensure_main_window(&window)?;
    let removed = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .delete_note(&session_id, None)
            .map_err(|e| e.to_string())?
    };
    let _ = app.emit_to(
        tauri::EventTarget::webview_window("main"),
        crate::agent::harness::EVENT_NOTES_UPDATED,
        serde_json::json!({
            "session_id": session_id,
            "count": 0,
            "removed": removed,
        }),
    );
    Ok(removed)
}

// ─── 文件改动快照与回滚 ─────────────────────────────────

/// 改动记录里的单个文件（不含内部备份名）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSnapshotFile {
    pub root_id: String,
    pub rel_path: String,
    pub bytes: i64,
    pub created_at: String,
}

/// 改动记录按"轮次"（stream）分组
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSnapshotStream {
    pub stream_id: String,
    /// 该轮最早一条备份的时间（列表排序用）
    pub created_at: String,
    pub total_bytes: i64,
    pub files: Vec<SessionSnapshotFile>,
}

/// 某个会话的完整改动记录（历史轮次也能看到并回滚）
///
/// 快照行里刻意不返回 `backup_name`：界面只需要展示路径与大小，
/// 内部文件名不构成有效信息。
#[tauri::command]
pub async fn list_session_snapshots(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<SessionSnapshotStream>, String> {
    ensure_main_window(&window)?;
    let rows = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .list_session_snapshots(&session_id)
            .map_err(|e| e.to_string())?
    };

    // 行按 created_at DESC：首次出现的 stream 保留为分组顺序，
    // 组内 created_at 取最早的一条
    let mut streams: Vec<SessionSnapshotStream> = Vec::new();
    for row in rows {
        let file = SessionSnapshotFile {
            root_id: row.root_id,
            rel_path: row.rel_path,
            bytes: row.bytes,
            created_at: row.created_at.clone(),
        };
        match streams.iter_mut().find(|s| s.stream_id == row.stream_id) {
            Some(stream) => {
                stream.total_bytes += row.bytes;
                stream.created_at = row.created_at;
                stream.files.push(file);
            }
            None => streams.push(SessionSnapshotStream {
                stream_id: row.stream_id,
                created_at: row.created_at,
                total_bytes: row.bytes,
                files: vec![file],
            }),
        }
    }
    Ok(streams)
}

/// 某个 stream 的文件改动快照概况（界面据此决定要不要显示"回滚"）
#[tauri::command]
pub async fn get_snapshot(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    stream_id: String,
) -> Result<Option<crate::agent::harness::SnapshotInfo>, String> {
    ensure_main_window(&window)?;
    // 数据目录不可变：直接借用，无需（也不该）再去拿任何锁
    let store = crate::agent::harness::SnapshotStore::new(
        &state.app_data_dir,
        state.chat_store.clone(),
    );
    // 备份文件的存在性检查是磁盘 IO：放到阻塞线程，不占住 async 执行器
    let info = tokio::task::spawn_blocking(move || store.info(&session_id, &stream_id))
        .await
        .map_err(|e| format!("读取快照失败：{}", e))?;
    Ok(if info.is_empty() { None } else { Some(info) })
}

/// 回滚某一次生成造成的全部文件改动
///
/// 这里**不检查 `tools.enabled`**：用户完全可能在关掉工具之后才想撤销上一次的改动，
/// 回滚只读备份、不执行任何模型指令。
///
/// `session_id` 必须与备份记录一致：否则拿到一个旧 `stream_id` 就能回滚
/// 别的会话造成的改动（见 `SnapshotStore::restore`）。
#[tauri::command]
pub async fn restore_snapshot(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: String,
    stream_id: String,
) -> Result<crate::agent::harness::snapshot::RestoreReport, String> {
    ensure_main_window(&window)?;
    let tools_cfg = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        config.tools.clone()
    };
    let data_dir = state.app_data_dir.clone();
    // 目标路径重新过一遍工作区监狱（备份索引是我们自己写的，但边界只有一处）
    let workspaces = crate::agent::harness::WorkspaceSet::from_config(&tools_cfg, &data_dir);
    let store = crate::agent::harness::SnapshotStore::new(&data_dir, state.chat_store.clone());
    // 回滚含大量文件复制（单 stream 上限 64 MB）：放到阻塞线程上，
    // 不占住 async 执行器
    tokio::task::spawn_blocking(move || store.restore(&stream_id, &session_id, &workspaces))
        .await
        .map_err(|e| format!("回滚任务失败：{}", e))
}

// ─── 工作区管理 ─────────────────────────────────────────

/// 已解析的工作区视图（带可用性判定）
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub label: String,
    pub path: String,
    pub writable: bool,
    pub available: bool,
    pub is_default: bool,
}

fn workspace_views(state: &AppState) -> Result<Vec<WorkspaceView>, String> {
    let config = state.config.lock().map_err(|e| e.to_string())?;
    let set =
        crate::agent::harness::WorkspaceSet::from_config(&config.tools, &state.app_data_dir);
    Ok(set
        .list()
        .into_iter()
        .map(|root| WorkspaceView {
            id: root.id,
            label: root.label,
            path: root.path,
            writable: root.writable,
            available: root.available,
            is_default: root.is_default,
        })
        .collect())
}

#[tauri::command]
pub async fn list_workspaces(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<WorkspaceView>, String> {
    ensure_main_window(&window)?;
    workspace_views(&state)
}

/// 校验并规范化一个待添加的绝对路径
fn validate_new_root(path: &str, existing: &[WorkspaceRoot], app_data_dir: &std::path::Path) -> Result<String, String> {
    let raw = path.trim();
    if raw.is_empty() {
        return Err("路径不能为空".to_string());
    }
    let candidate = std::path::PathBuf::from(raw);
    if !candidate.is_absolute() {
        return Err("请填写绝对路径（例如 D:\\项目\\笔记）".to_string());
    }
    if !candidate.is_dir() {
        return Err(format!("目录不存在或不是目录：{}", candidate.display()));
    }
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|e| format!("无法解析路径：{}", e))?;

    if canonical.parent().is_none() {
        return Err("不允许把磁盘根目录设为工作区".to_string());
    }

    // 系统目录：只看**顶层组件**（`/etc`、`C:\Windows`、`/proc`…）。
    // 子串匹配会把 `/home/u/etc-projects` 这类合法目录误杀，又漏掉 `/proc`；
    // 按任意层级匹配组件则会把用户项目里名为 `bin`/`etc` 的普通目录误杀。
    let first_component = canonical
        .components()
        .find(|component| matches!(component, std::path::Component::Normal(_)))
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let blocked: &[&str] = if cfg!(windows) {
        &["windows", "system32", "syswow64"]
    } else {
        &[
            "etc", "usr", "bin", "sbin", "boot", "dev", "proc", "sys", "root", "system",
            "library",
        ]
    };
    if blocked.contains(&first_component.as_str()) {
        return Err("该目录属于系统目录，不允许作为工作区".to_string());
    }
    if canonical.join("config.json").exists() && canonical.join("data.db").exists() {
        return Err("不允许把应用数据目录本身设为工作区（其中包含配置与数据库）".to_string());
    }

    let app_data_canonical = std::fs::canonicalize(app_data_dir).unwrap_or_else(|_| app_data_dir.to_path_buf());
    // 应用数据目录及其**所有子目录**都拒绝：snapshots/（回滚备份）与 personas/
    // （人格 system prompt）都在里面，放进来等于让文件工具能改写回滚备份与人格
    if canonical.starts_with(&app_data_canonical) {
        return Err(
            "不允许把应用数据目录（或其子目录）设为工作区：其中含配置、数据库、快照与人格"
                .to_string(),
        );
    }

    for root in existing {
        let other = match &root.path {
            WorkspacePath::Absolute(p) => std::path::PathBuf::from(p),
            WorkspacePath::AppDataWorkspace => app_data_canonical.join("workspace"),
        };
        let other = std::fs::canonicalize(&other).unwrap_or(other);
        if canonical == other {
            return Err("该目录已经在工作区列表中".to_string());
        }
        if canonical.starts_with(&other) || other.starts_with(&canonical) {
            return Err(format!(
                "该目录与已有工作区「{}」互相嵌套，会导致寻址歧义",
                root.label
            ));
        }
    }

    Ok(canonical.display().to_string())
}

/// 由路径派生一个合法的工作区 id
fn derive_root_id(path: &std::path::Path, existing: &[WorkspaceRoot]) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "workspace".to_string());
    let mut slug: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() || !slug.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
        slug = "workspace".to_string();
    }
    slug.truncate(24);

    if !existing.iter().any(|r| r.id == slug) {
        return slug;
    }
    for index in 2..100 {
        let candidate = format!("{}-{}", slug, index);
        if !existing.iter().any(|r| r.id == candidate) {
            return candidate;
        }
    }
    format!("ws-{}", uuid::Uuid::new_v4().simple())
}

#[tauri::command]
pub async fn add_workspace(
    window: WebviewWindow,
    state: State<'_, AppState>,
    path: String,
    label: Option<String>,
    writable: Option<bool>,
) -> Result<WorkspaceView, String> {
    ensure_main_window(&window)?;

    let (mut config, data_dir) = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        (config.clone(), state.app_data_dir.clone())
    };

    if config.tools.workspaces.len() >= MAX_WORKSPACE_ROOTS {
        return Err(format!("工作区数量已达上限（{} 个）", MAX_WORKSPACE_ROOTS));
    }

    let canonical = validate_new_root(&path, &config.tools.workspaces, &data_dir)?;
    let id = derive_root_id(std::path::Path::new(&canonical), &config.tools.workspaces);
    config.tools.workspaces.push(WorkspaceRoot {
        id,
        label: label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                std::path::Path::new(&canonical)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "工作区".to_string())
            }),
        path: WorkspacePath::Absolute(canonical),
        // 新增的工作区默认只读，需要用户显式开启写入
        writable: writable.unwrap_or(false),
    });

    persist_config(&state, &config)?;
    workspace_views(&state)?
        .into_iter()
        .last()
        .ok_or_else(|| "工作区添加后未能读取".to_string())
}

#[tauri::command]
pub async fn update_workspace(
    window: WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    label: Option<String>,
    writable: Option<bool>,
) -> Result<WorkspaceView, String> {
    ensure_main_window(&window)?;

    let mut config = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        config.clone()
    };

    let root = config
        .tools
        .workspaces
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| format!("没有找到工作区：{}", id))?;

    if let Some(label) = label {
        let label = label.trim().to_string();
        if !label.is_empty() {
            root.label = label;
        }
    }
    if let Some(writable) = writable {
        root.writable = writable;
    }

    persist_config(&state, &config)?;

    workspace_views(&state)?
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("没有找到工作区：{}", id))
}

#[tauri::command]
pub async fn remove_workspace(
    window: WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;

    if !is_valid_workspace_id(&id) {
        return Err("工作区 id 非法".to_string());
    }

    let mut config = {
        let config = state.config.lock().map_err(|e| e.to_string())?;
        config.clone()
    };

    if config.tools.workspaces.iter().all(|r| r.id != id) {
        return Err(format!("没有找到工作区：{}", id));
    }
    if config
        .tools
        .workspaces
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.path == WorkspacePath::AppDataWorkspace)
        .unwrap_or(false)
    {
        return Err("默认工作区不能删除".to_string());
    }

    config.tools.workspaces.retain(|r| r.id != id);
    // 至少保留一个工作区
    config.tools.ensure_workspaces();

    persist_config(&state, &config)
}

fn persist_config(state: &AppState, config: &crate::config::types::AppConfig) -> Result<(), String> {
    config.validate()?;
    // 落盘在**锁外**完成：磁盘 I/O 可能长达毫秒级，持锁 I/O 会放大并发窗口；
    // 且这里绝不能先拿 data_dir（已去锁）再拿 config，锁序统一为"不进锁做 I/O"。
    crate::config::save_config(&state.app_data_dir, config).map_err(|e| e.to_string())?;
    let mut current = state.config.lock().map_err(|e| e.to_string())?;
    *current = config.clone();
    Ok(())
}

/// 设置页展示用：安全边界概览（拒绝清单 / 允许程序 / 硬拦截开关）
#[tauri::command]
pub async fn get_tool_safety_summary(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<ToolSafetySummary, String> {
    ensure_main_window(&window)?;
    let config = state.config.lock().map_err(|e| e.to_string())?;
    Ok(ToolSafetySummary {
        deny_globs: config.tools.deny_globs.clone(),
        missing_builtin_globs: config
            .tools
            .missing_builtin_deny_globs()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        command_allowlist: config.tools.command_allowlist.clone(),
        sensitive_commands_blocked: true,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSafetySummary {
    pub deny_globs: Vec<String>,
    pub missing_builtin_globs: Vec<String>,
    pub command_allowlist: Vec<String>,
    /// 敏感命令（cmd / powershell / curl / rm / reg ...）由内置黑名单硬拦截，
    /// 用户配置无法放行
    pub sensitive_commands_blocked: bool,
}

/// 停止生成时把该 stream 上等待中的审批按拒绝处理
pub fn cancel_approvals(state: &AppState, stream_id: &str) -> usize {
    cancel_pending_for_stream(&state.pending_approvals, stream_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(id: &str, label: &str, path: &str) -> WorkspaceRoot {
        WorkspaceRoot {
            id: id.to_string(),
            label: label.to_string(),
            path: WorkspacePath::Absolute(path.to_string()),
            writable: true,
        }
    }

    #[test]
    fn derive_root_id_slugifies_and_dedupes() {
        let existing = vec![root("notes", "笔记", "D:\\a")];
        assert_eq!(
            derive_root_id(std::path::Path::new("D:/我的 项目"), &existing),
            "workspace"
        );
        assert_eq!(
            derive_root_id(std::path::Path::new("D:/Notes"), &existing),
            "notes-2"
        );
        let empty: Vec<WorkspaceRoot> = Vec::new();
        assert_eq!(
            derive_root_id(std::path::Path::new("D:/MyProject"), &empty),
            "myproject"
        );
    }

    #[test]
    fn validate_new_root_rejects_relative_and_missing() {
        let data_dir = std::env::temp_dir();
        let err = validate_new_root("relative/path", &[], &data_dir).unwrap_err();
        assert!(err.contains("绝对路径"), "{err}");

        let missing = std::env::temp_dir().join("konata-not-exist-xyz");
        let err = validate_new_root(&missing.display().to_string(), &[], &data_dir).unwrap_err();
        assert!(err.contains("目录不存在"), "{err}");
    }

    #[test]
    fn validate_new_root_rejects_nested_roots() {
        let base = std::env::temp_dir().join(format!("konata-ws-{}", uuid::Uuid::new_v4()));
        let nested = base.join("inner");
        let appdata = base.join("appdata");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&appdata).unwrap();

        let existing = vec![root("base", "外层", &base.display().to_string())];
        let err = validate_new_root(&nested.display().to_string(), &existing, &appdata).unwrap_err();
        assert!(err.contains("嵌套"), "{err}");

        // 同一个目录重复添加
        let err = validate_new_root(&base.display().to_string(), &existing, &appdata).unwrap_err();
        assert!(err.contains("已经在工作区列表中"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 应用数据目录的子目录（snapshots / personas）绝不能被设为工作区
    #[test]
    fn validate_new_root_rejects_app_data_subdirectories() {
        let base = std::env::temp_dir().join(format!("konata-ws-sub-{}", uuid::Uuid::new_v4()));
        let appdata = base.join("appdata");
        let snapshots = appdata.join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();

        let err = validate_new_root(&snapshots.display().to_string(), &[], &appdata).unwrap_err();
        assert!(err.contains("应用数据目录"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
