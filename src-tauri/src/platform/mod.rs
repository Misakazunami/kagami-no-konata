//! 平台能力探测与平台相关操作。
//!
//! 业务代码不要散落 `#[cfg]`：窗口定位、截图、键鼠、托盘、快捷键等差异
//! 统一收敛到这里。`cfg` 只出现在本模块的 re-export 和各平台实现内部。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use unsupported as imp;

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

/// 当前图形会话类型。
///
/// 各变体只在对应平台构造（Windows 恒为 `Native`），但序列化/匹配是跨平台的，
/// 因此允许在非 Linux 构建里出现“未构造”的告警。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    X11,
    Wayland,
    /// Windows/macOS 等非 Linux 图形会话
    Native,
    /// 没有检测到 DISPLAY / WAYLAND_DISPLAY（例如 CI、无头环境）
    Unknown,
}

/// 当前平台的能力集合。
///
/// 前端通过 `get_platform_capabilities` 读取同一份数据，避免设置页写死
/// “Linux 一定有托盘”之类的假设。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformCapabilities {
    pub os: &'static str,
    pub session_type: SessionType,
    /// Linux 使用 AppIndicator，通常不会触发 `TrayIconEvent`
    pub tray_click_events: bool,
    /// 是否提供托盘菜单（即使没有左键事件）
    pub tray_menu: bool,
    /// 托盘是否“可靠可见”。GNOME 默认没有 AppIndicator 扩展，保守设为 false。
    pub tray_reliable: bool,
    pub window_positioning: bool,
    pub always_on_top: bool,
    pub global_shortcut: bool,
    pub screen_capture: bool,
    /// 截图可用，但只用于“看”，不能注入键鼠（Wayland 实验模式）
    pub screen_capture_only: bool,
    pub input_injection: bool,
    pub activate_window: bool,
    /// 完整桌面控制（截图 + 键鼠 + 窗口激活）是否可用
    pub os_control: bool,
    /// 新安装时“关闭主窗口”的默认行为
    pub close_action_default: &'static str,
    /// 给用户/日志看的人类可读说明
    pub notes: Vec<String>,
}

/// 当前平台能力（进程内只探测一次）。
pub fn capabilities() -> &'static PlatformCapabilities {
    static CAPS: OnceLock<PlatformCapabilities> = OnceLock::new();
    CAPS.get_or_init(imp::detect)
}

/// 在 Tauri/GTK 初始化之前设置必要的环境变量。
pub fn apply_startup_env() {
    imp::apply_startup_env();
}

/// 当前鼠标在虚拟桌面中的绝对坐标（物理像素）。
pub fn cursor_position() -> Option<(i32, i32)> {
    imp::cursor_position()
}

/// 把鼠标移动到虚拟桌面绝对坐标（支持负坐标副屏）。
pub fn move_cursor(x: i32, y: i32) -> Result<()> {
    imp::move_cursor(x, y)
}

/// 按标题包含匹配激活窗口，并等待 `settle` 让系统完成切换。
pub fn activate_window(title: &str, settle: Duration) -> Result<String> {
    imp::activate_window(title, settle)
}

/// 托盘左键是否应该直接弹菜单。
///
/// Windows 用左键切换窗口；Linux AppIndicator 只能弹菜单。
pub fn tray_menu_on_left_click() -> bool {
    !capabilities().tray_click_events
}
