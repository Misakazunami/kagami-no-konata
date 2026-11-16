use crate::platform::{self, PlatformCapabilities};

/// 返回当前平台能力，供前端渲染准确的提示/禁用状态。
#[tauri::command]
pub fn get_platform_capabilities() -> PlatformCapabilities {
    platform::capabilities().clone()
}
