use crate::platform::{self, PlatformCapabilities};

/// 返回当前平台能力，供前端渲染准确的提示/禁用状态。
#[tauri::command]
pub fn get_platform_capabilities() -> PlatformCapabilities {
    // 叠加运行时状态：Wayland 门户快捷键是异步绑定的，不能只返回启动瞬间的值。
    platform::current_capabilities()
}
