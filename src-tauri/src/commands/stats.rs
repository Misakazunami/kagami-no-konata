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
