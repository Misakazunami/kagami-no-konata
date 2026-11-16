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

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod native_input;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Mutex;
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

/// 全局快捷键使用的后端。
///
/// - `Native`：Windows / X11，由前端 `tauri-plugin-global-shortcut` 注册；
/// - `Portal`：Wayland 下通过 `org.freedesktop.portal.GlobalShortcuts`
///   申请，绑定成功前 `PlatformCapabilities::global_shortcut` 保持 false；
/// - `None`：当前会话没有可用后端。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobalShortcutBackend {
    Native,
    Portal,
    None,
}

/// 悬浮窗定位使用的后端。
///
/// - `Native`：Windows / X11，使用 `set_position` + 置顶标志；
/// - `LayerShell`：Wayland 下通过 `wlr-layer-shell`（运行时 dlopen）定位，
///   初始化成功前 `window_positioning` 保持 false；
/// - `None`：当前会话没有可用的绝对定位后端。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowPositioningBackend {
    Native,
    LayerShell,
    None,
}

/// 键鼠注入使用的后端。
///
/// - `Native`：Windows / X11，使用现有 enigo（X11 走 libxdo）；
/// - `Eis`：Wayland 下通过 XDG RemoteDesktop 门户 + libei/EIS（enigo 的
///   libei 后端），连接成功前 `input_injection` 保持 false；
/// - `None`：当前会话没有可用的注入后端。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputInjectionBackend {
    Native,
    Eis,
    None,
}

/// 平台中立的鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 平台中立的按键 token。
///
/// `os_control` 只负责解析字符串；具体映射到 enigo 0.2（X11/Windows）或
/// enigo 0.6 libei（Wayland）由平台层完成，避免工具层出现两套按键枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyToken {
    Control,
    Alt,
    Shift,
    Meta,
    Return,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    UpArrow,
    DownArrow,
    LeftArrow,
    RightArrow,
    Home,
    End,
    PageUp,
    PageDown,
    Unicode(char),
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
    /// 悬浮窗定位后端；`layer_shell` 时由运行时初始化结果动态决定是否可用
    pub window_positioning_backend: WindowPositioningBackend,
    pub always_on_top: bool,
    /// 当前是否已经有一个“可用”的全局快捷键后端（门户绑定成功后才为 true）
    pub global_shortcut: bool,
    /// 全局快捷键后端类型；前端据此选择插件注册或等待后端事件
    pub global_shortcut_backend: GlobalShortcutBackend,
    pub screen_capture: bool,
    /// 截图可用，但只用于“看”，不能注入键鼠（Wayland 实验模式）
    pub screen_capture_only: bool,
    pub input_injection: bool,
    /// 键鼠注入后端；`eis` 时由异步门户连接结果动态决定是否可用
    pub input_injection_backend: InputInjectionBackend,
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

/// Wayland 门户快捷键是否已经绑定成功。
///
/// 只有 Linux 会写入这个状态；其它平台恒为 false。门户绑定是异步的，
/// 因此不能把它塞进 `detect()` 的一次性结果里。
#[cfg(target_os = "linux")]
static PORTAL_SHORTCUT_READY: AtomicBool = AtomicBool::new(false);

/// 门户快捷键最近一次成功/失败的说明，供设置页展示。
#[cfg(target_os = "linux")]
static PORTAL_SHORTCUT_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Wayland layer-shell 是否已经成功初始化。
///
/// `detect()` 运行时只能知道“用户请求了 layer-shell”，真正的
/// `gtk_layer_is_supported()` + `gtk_layer_init_for_window()` 必须等 GTK
/// 初始化之后才能执行，因此单独用运行时状态叠加。
#[cfg(target_os = "linux")]
static LAYER_SHELL_READY: AtomicBool = AtomicBool::new(false);

/// layer-shell 最近一次成功/失败的说明。
#[cfg(target_os = "linux")]
static LAYER_SHELL_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Wayland EIS 输入服务是否已经连接成功。
#[cfg(target_os = "linux")]
static EIS_INPUT_READY: AtomicBool = AtomicBool::new(false);

/// EIS 输入服务最近一次成功/失败的说明。
#[cfg(target_os = "linux")]
static EIS_INPUT_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// 更新 EIS 输入服务的运行时状态（仅 Linux 输入线程调用）。
#[cfg(target_os = "linux")]
pub(crate) fn set_eis_input_status(ready: bool, note: Option<String>) {
    EIS_INPUT_READY.store(ready, Ordering::Release);
    if let Ok(mut slot) = EIS_INPUT_NOTE.lock() {
        *slot = note;
    }
}

/// 更新 layer-shell 的运行时状态（仅 Linux 窗口初始化代码调用）。
#[cfg(target_os = "linux")]
pub(crate) fn set_layer_shell_status(ready: bool, note: Option<String>) {
    LAYER_SHELL_READY.store(ready, Ordering::Release);
    if let Ok(mut slot) = LAYER_SHELL_NOTE.lock() {
        *slot = note;
    }
}

/// 更新门户快捷键的运行时状态（仅 Linux 门户服务调用）。
#[cfg(target_os = "linux")]
pub(crate) fn set_portal_shortcut_status(ready: bool, note: Option<String>) {
    PORTAL_SHORTCUT_READY.store(ready, Ordering::Relaxed);
    if let Ok(mut slot) = PORTAL_SHORTCUT_NOTE.lock() {
        *slot = note;
    }
}

/// 返回“此刻”的平台能力。
///
/// 静态部分来自 `capabilities()`；Wayland 门户快捷键是异步绑定的，
/// 这里叠加最新的运行时状态，避免前端拿到启动瞬间的旧值。
pub fn current_capabilities() -> PlatformCapabilities {
    let caps = capabilities().clone();

    // 非 Linux 构建没有动态门户状态，保持 detect() 的结果即可。
    #[cfg(target_os = "linux")]
    let caps = {
        let mut caps = caps;
        if caps.global_shortcut_backend == GlobalShortcutBackend::Portal {
            caps.global_shortcut = PORTAL_SHORTCUT_READY.load(Ordering::Relaxed);
            if let Ok(note) = PORTAL_SHORTCUT_NOTE.lock() {
                if let Some(note) = note.as_ref() {
                    caps.notes.push(note.clone());
                }
            }
        }
        if caps.window_positioning_backend == WindowPositioningBackend::LayerShell {
            caps.window_positioning = LAYER_SHELL_READY.load(Ordering::Acquire);
            // layer-shell 的 Top 层本身就是“置顶”的实现；初始化失败时一起回退。
            caps.always_on_top = caps.window_positioning;
            if let Ok(note) = LAYER_SHELL_NOTE.lock() {
                if let Some(note) = note.as_ref() {
                    caps.notes.push(note.clone());
                }
            }
        }
        if caps.input_injection_backend == InputInjectionBackend::Eis {
            caps.input_injection = EIS_INPUT_READY.load(Ordering::Acquire);
            if let Ok(note) = EIS_INPUT_NOTE.lock() {
                if let Some(note) = note.as_ref() {
                    caps.notes.push(note.clone());
                }
            }
        }
        caps
    };

    caps
}

/// GTK 初始化后，尝试为悬浮窗启用 layer-shell。
///
/// 必须在窗口首次显示之前调用；非 Linux 平台为空实现。
pub fn init_float_window_layer_shell(window: &tauri::WebviewWindow) {
    imp::init_float_window_layer_shell(window);
}

/// 用 layer-shell 的锚点/边距把悬浮窗定位到指定角落。
///
/// 返回 false 表示当前平台或初始化状态不支持。
pub fn position_float_window_layer_shell(position: &str) -> bool {
    imp::position_float_window_layer_shell(position)
}

/// 按逻辑像素增量移动 layer-shell 悬浮窗。
///
/// 只应由前端拖拽路径在 `window_positioning_backend == layer_shell` 时调用。
pub fn move_float_window_by(dx: i32, dy: i32) -> Result<()> {
    imp::move_float_window_by(dx, dy)
}

/// 启动 Wayland EIS 输入服务（显式 opt-in 时由 setup 调用）。
///
/// Windows / X11 为空实现；Wayland 下会在独立线程中请求门户授权并保持
/// EIS 会话，失败结果通过 `input-injection-status` 事件回报。
pub fn start_input_service(app: tauri::AppHandle) {
    imp::start_input_service(app);
}

/// 把鼠标移动到屏幕绝对坐标。
pub fn input_move_abs(x: i32, y: i32) -> Result<()> {
    imp::input_move_abs(x, y)
}

/// 鼠标点击（可选双击）。
pub fn input_click(button: MouseButton, double: bool) -> Result<()> {
    imp::input_click(button, double)
}

/// 输入一段文本。
pub fn input_type_text(text: &str) -> Result<()> {
    imp::input_type_text(text)
}

/// 按顺序按下修饰键、点一下主键、再逆序松开。
pub fn input_key_combo(keys: &[KeyToken]) -> Result<()> {
    imp::input_key_combo(keys)
}

/// 启动需要平台后端的全局快捷键服务。
///
/// Windows / X11 由前端插件注册，这里不做事；Wayland 门户由 Linux 实现
/// 在后台线程中创建会话并监听 `Activated`。
pub fn start_global_shortcut_service(app: tauri::AppHandle) {
    imp::start_global_shortcut_service(app);
}

/// 在 Tauri/GTK 初始化之前设置必要的环境变量。
pub fn apply_startup_env() {
    imp::apply_startup_env();
}

/// 当前鼠标在虚拟桌面中的绝对坐标（物理像素）。
pub fn cursor_position() -> Option<(i32, i32)> {
    imp::cursor_position()
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
