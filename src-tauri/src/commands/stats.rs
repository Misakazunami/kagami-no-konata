use serde::Serialize;
use tauri::State;

use crate::store::chat_store::UsageStats;
use crate::AppState;

/// 获取使用统计
#[tauri::command]
pub async fn get_usage_stats(state: State<'_, AppState>) -> Result<UsageStats, String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store.get_usage_stats().map_err(|e| e.to_string())
}

/// 重置使用统计
#[tauri::command]
pub async fn reset_usage_stats(state: State<'_, AppState>) -> Result<(), String> {
    let store = state.chat_store.lock().map_err(|e| e.to_string())?;
    store.reset_usage_stats().map_err(|e| e.to_string())
}

/// 会话级 token 用量（界面用于任务状态条）
#[derive(Debug, Clone, Serialize)]
pub struct SessionUsage {
    pub session_id: String,
    /// 会话内消息正文的累计 token（含模型返回的 usage 估算）
    pub message_tokens: i64,
    /// 工具（主要是子代理）自算的隐藏开销
    pub tool_extra_tokens: i64,
    pub total_tokens: i64,
}

/// 读取某个会话的 token 用量（消息正文 + 工具隐藏开销）
#[tauri::command]
pub async fn get_session_usage(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<SessionUsage, String> {
    let (message_tokens, tool_extra_tokens) = {
        let store = state.chat_store.lock().map_err(|e| e.to_string())?;
        store
            .session_token_usage(&session_id)
            .map_err(|e| e.to_string())?
    };
    Ok(SessionUsage {
        session_id,
        message_tokens,
        tool_extra_tokens,
        total_tokens: message_tokens + tool_extra_tokens,
    })
}
