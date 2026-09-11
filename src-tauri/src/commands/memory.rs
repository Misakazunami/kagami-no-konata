use tauri::State;

use crate::store::memory_store::MemoryEntry;
use crate::AppState;

/// 单次返回给界面的记忆条数上限
const MAX_UI_MEMORIES: usize = 500;

/// 获取记忆列表（供界面展示，不返回向量）
#[tauri::command]
pub async fn list_memories(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<MemoryEntry>, String> {
    let limit = limit.map(|n| n.min(MAX_UI_MEMORIES));
    let store = state.memory_store.lock().map_err(|e| e.to_string())?;
    store.list_memories_for_ui(limit).map_err(|e| e.to_string())
}

/// 删除指定记忆
#[tauri::command]
pub async fn delete_memory(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let store = state.memory_store.lock().map_err(|e| e.to_string())?;
    store.delete_memory(&id).map_err(|e| e.to_string())
}

/// 清空所有记忆
#[tauri::command]
pub async fn clear_memories(state: State<'_, AppState>) -> Result<(), String> {
    let store = state.memory_store.lock().map_err(|e| e.to_string())?;
    store.clear_memories().map_err(|e| e.to_string())
}
