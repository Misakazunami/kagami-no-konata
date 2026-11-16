//! 其它平台兜底：明确返回“不可用”，不伪装成功。

use std::time::Duration;

use anyhow::{anyhow, Result};

use super::{GlobalShortcutBackend, PlatformCapabilities, SessionType, WindowPositioningBackend};

pub fn detect() -> PlatformCapabilities {
    PlatformCapabilities {
        os: std::env::consts::OS,
        session_type: SessionType::Unknown,
        tray_click_events: false,
        tray_menu: false,
        tray_reliable: false,
        window_positioning: false,
        window_positioning_backend: WindowPositioningBackend::None,
        always_on_top: false,
        global_shortcut: false,
        global_shortcut_backend: GlobalShortcutBackend::None,
        screen_capture: false,
        screen_capture_only: false,
        input_injection: false,
        activate_window: false,
        os_control: false,
        close_action_default: "hide",
        notes: vec!["当前平台未实现桌面集成，桌面能力不可用。".to_string()],
    }
}

pub fn apply_startup_env() {}

pub fn start_global_shortcut_service(_app: tauri::AppHandle) {}

pub fn init_float_window_layer_shell(_window: &tauri::WebviewWindow) {}

pub fn position_float_window_layer_shell(_position: &str) -> bool {
    false
}

pub fn move_float_window_by(_dx: i32, _dy: i32) -> Result<()> {
    Err(anyhow!("layer-shell 仅支持 Linux Wayland 会话"))
}

pub fn cursor_position() -> Option<(i32, i32)> {
    None
}

pub fn move_cursor(_x: i32, _y: i32) -> Result<()> {
    Err(anyhow!("当前平台不支持鼠标绝对定位"))
}

pub fn activate_window(_title: &str, _settle: Duration) -> Result<String> {
    Err(anyhow!("当前平台不支持按标题激活窗口"))
}
