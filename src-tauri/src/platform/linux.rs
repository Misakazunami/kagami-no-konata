//! Linux 平台实现。
//!
//! - X11：通过 x11rb 做光标定位、绝对移动和 EWMH 窗口激活。
//! - Wayland：默认不做“假装支持”的尝试；只有显式 opt-in 的只读截图例外。

use std::time::Duration;

use anyhow::{anyhow, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt, EventMask, Window,
};

mod global_shortcuts;
mod input;
mod layer_shell;

use super::{
    capabilities, set_layer_shell_status, GlobalShortcutBackend, InputInjectionBackend, KeyToken,
    MouseButton, PlatformCapabilities, SessionType, WindowPositioningBackend,
};

const CURRENT_TIME: u32 = 0;

/// Wayland 只读截图的显式 opt-in 开关
const WAYLAND_SCREENSHOT_ENV: &str = "KONATA_WAYLAND_SCREENSHOT";

/// Wayland 全局快捷键（XDG 门户）的显式 opt-in 开关
const WAYLAND_GLOBAL_SHORTCUTS_ENV: &str = "KONATA_WAYLAND_GLOBAL_SHORTCUTS";

/// Wayland 悬浮窗 layer-shell 定位的显式 opt-in 开关
const WAYLAND_LAYER_SHELL_ENV: &str = "KONATA_WAYLAND_LAYER_SHELL";

/// Wayland 键鼠注入（libei/EIS）的显式 opt-in 开关
const WAYLAND_INPUT_ENV: &str = "KONATA_WAYLAND_INPUT";

/// 解析环境变量开关：空值与 "0" 视为关闭，其余非空值视为开启
fn env_flag(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .map(|value| {
            let text = value.to_string_lossy();
            let text = text.trim();
            !text.is_empty() && text != "0"
        })
        .unwrap_or(false)
}

fn wayland_screenshot_opt_in() -> bool {
    env_flag(std::env::var_os(WAYLAND_SCREENSHOT_ENV).as_deref())
}

fn wayland_global_shortcuts_opt_in() -> bool {
    env_flag(std::env::var_os(WAYLAND_GLOBAL_SHORTCUTS_ENV).as_deref())
}

fn wayland_layer_shell_opt_in() -> bool {
    env_flag(std::env::var_os(WAYLAND_LAYER_SHELL_ENV).as_deref())
}

fn wayland_input_opt_in() -> bool {
    env_flag(std::env::var_os(WAYLAND_INPUT_ENV).as_deref())
}

/// 纯逻辑：根据会话类型与运行时条件选择全局快捷键后端。
///
/// X11 始终走原生插件；Wayland 只有在显式 opt-in 且存在 DBus 会话时
/// 才尝试门户。门户接口本身是否存在、用户是否授权，要到异步绑定阶段
/// 才能知道，因此这里返回 `Portal` 只代表“可以尝试”，不代表可用。
fn shortcut_backend(
    session: SessionType,
    opt_in: bool,
    has_dbus_session: bool,
) -> GlobalShortcutBackend {
    match session {
        SessionType::X11 => GlobalShortcutBackend::Native,
        SessionType::Wayland if opt_in && has_dbus_session => GlobalShortcutBackend::Portal,
        _ => GlobalShortcutBackend::None,
    }
}

/// Wayland 只读截图是否具备运行条件。
///
/// `screenshots` 的 Wayland 后端仍需要 XWayland（`DISPLAY`）枚举显示器，
/// 并通过 DBus 访问 GNOME Shell / xdg-desktop-portal；缺一不可。纯 Wayland
/// 且关闭 XWayland 时明确不可用，而不是尝试后返回一张错图。
fn wayland_screenshot_available() -> bool {
    wayland_screenshot_opt_in()
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

pub fn detect() -> PlatformCapabilities {
    let mut session = current_session();
    let mut notes = Vec::new();

    // 用户显式要求 XWayland（KONATA_FORCE_X11=1 会设置 GDK_BACKEND=x11，
    // 也可以自行设置）：窗口由 XWayland 提供，X11 桌面能力可用。
    // 注意：这只覆盖本应用窗口；其它原生 Wayland 窗口仍无法被截图/注入。
    let forced_x11 = session == SessionType::Wayland
        && std::env::var("GDK_BACKEND")
            .map(|value| value.eq_ignore_ascii_case("x11"))
            .unwrap_or(false)
        && std::env::var_os("DISPLAY").is_some();
    if forced_x11 {
        session = SessionType::X11;
        notes.push(
            "已启用 XWayland（GDK_BACKEND=x11）：本应用的窗口定位/截图/桌面控制走 X11 后端；\
             原生 Wayland 窗口仍无法被截图或注入输入。"
                .to_string(),
        );
    }

    let x11 = session == SessionType::X11;
    let wayland_screenshot = session == SessionType::Wayland && wayland_screenshot_available();
    let has_dbus_session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    let global_shortcut_backend =
        shortcut_backend(session, wayland_global_shortcuts_opt_in(), has_dbus_session);
    let wayland_shortcuts = global_shortcut_backend == GlobalShortcutBackend::Portal;

    // layer-shell 需要 GTK 初始化后才能探测；这里只记录“用户请求了”，
    // 真正的可用性由 init_float_window_layer_shell() 写回动态状态。
    let window_positioning_backend = if x11 {
        WindowPositioningBackend::Native
    } else if session == SessionType::Wayland && wayland_layer_shell_opt_in() {
        WindowPositioningBackend::LayerShell
    } else {
        WindowPositioningBackend::None
    };

    // EIS 需要门户会话；真正的连接结果由后台输入线程写回动态状态。
    let input_injection_backend = if x11 {
        InputInjectionBackend::Native
    } else if session == SessionType::Wayland && wayland_input_opt_in() && has_dbus_session {
        InputInjectionBackend::Eis
    } else {
        InputInjectionBackend::None
    };

    match session {
        SessionType::X11 if !forced_x11 => {
            notes.push("X11 会话：窗口定位、置顶、截图和桌面控制可用。".to_string());
        }
        SessionType::Wayland => {
            notes.push(
                "Wayland 会话：窗口绝对定位、置顶和桌面控制不可用；\
                 全局快捷键只有在显式启用 XDG 门户实验后才可能可用；\
                 需要完整桌面能力请改用 X11/Xorg 会话。"
                    .to_string(),
            );
        }
        SessionType::Unknown => {
            notes.push("未检测到 DISPLAY/WAYLAND_DISPLAY：桌面能力不可用。".to_string());
        }
        _ => {}
    }

    if session == SessionType::Wayland {
        if wayland_shortcuts {
            notes.push(
                "Wayland 全局快捷键已请求（实验）：将通过 XDG 门户绑定 Ctrl+Shift+K；\
                 首次可能要求系统确认，绑定成功前不视为可用。实际按键可能由桌面调整。"
                    .to_string(),
            );
        } else if wayland_global_shortcuts_opt_in() {
            notes.push(
                "已设置 KONATA_WAYLAND_GLOBAL_SHORTCUTS=1，但缺少 \
                 DBUS_SESSION_BUS_ADDRESS：Wayland 全局快捷键依赖 XDG 桌面门户，当前不可用。"
                    .to_string(),
            );
        } else {
            notes.push(
                "Wayland 全局快捷键默认关闭；设置 KONATA_WAYLAND_GLOBAL_SHORTCUTS=1 并重启\
                 可尝试通过 XDG 门户启用（实验，需要桌面环境支持并经过用户确认）。"
                    .to_string(),
            );
        }
    }

    if session == SessionType::Wayland {
        if input_injection_backend == InputInjectionBackend::Eis {
            notes.push(
                "Wayland EIS 键鼠注入已请求（实验）：启动时会请求系统授权；\
                 连接成功前桌面控制不可用。没有只读截图时鼠标点击会被拒绝，\
                 窗口激活始终不可用。"
                    .to_string(),
            );
        } else if wayland_input_opt_in() {
            notes.push(
                "已设置 KONATA_WAYLAND_INPUT=1，但缺少 DBUS_SESSION_BUS_ADDRESS：\
                 Wayland 键鼠注入依赖 XDG RemoteDesktop 门户，当前不可用。"
                    .to_string(),
            );
        } else {
            notes.push(
                "Wayland 键鼠注入默认关闭；设置 KONATA_WAYLAND_INPUT=1 并重启\
                 可尝试通过 XDG RemoteDesktop + libei/EIS 启用（实验，需要桌面环境支持\
                 并经过用户授权；鼠标点击还要求先启用只读截图）。"
                    .to_string(),
            );
        }
    }

    if session == SessionType::Wayland {
        if window_positioning_backend == WindowPositioningBackend::LayerShell {
            notes.push(
                "Wayland 悬浮窗定位已请求（实验）：GTK 初始化后将探测 wlr-layer-shell；\
                 仅 Sway/Hyprland 等支持的合成器可用，失败时回退为合成器默认摆放。"
                    .to_string(),
            );
        } else if wayland_layer_shell_opt_in() {
            notes.push(
                "已设置 KONATA_WAYLAND_LAYER_SHELL=1，但当前不是可尝试 layer-shell 的 \
                 Wayland 会话，悬浮窗定位保持不可用。"
                    .to_string(),
            );
        } else {
            notes.push(
                "Wayland 悬浮窗定位默认关闭；设置 KONATA_WAYLAND_LAYER_SHELL=1 并重启\
                 可尝试 layer-shell（实验，GNOME/Mutter 通常不支持）。"
                    .to_string(),
            );
        }
    }

    if session == SessionType::Wayland {
        if wayland_screenshot {
            notes.push(
                "Wayland 只读截图已启用（实验）：只能查看屏幕，不能点击/输入/切换窗口；\
                 首次截图可能弹出系统授权，多屏或分数缩放下画面可能不准确。"
                    .to_string(),
            );
        } else if wayland_screenshot_opt_in() {
            notes.push(
                "已设置 KONATA_WAYLAND_SCREENSHOT=1，但缺少 DISPLAY 或 \
                 DBUS_SESSION_BUS_ADDRESS：Wayland 截图需要 XWayland 枚举显示器并依赖 DBus 会话，\
                 当前不可用。"
                    .to_string(),
            );
        } else {
            notes.push(
                "Wayland 只读截图默认关闭；设置 KONATA_WAYLAND_SCREENSHOT=1 并重启可启用\
                 （实验，需要 XWayland 与系统授权；仍然不能控制键鼠）。"
                    .to_string(),
            );
        }
    }

    PlatformCapabilities {
        os: "linux",
        session_type: session,
        // AppIndicator 通常只弹菜单，不产生 TrayIconEvent。
        tray_click_events: false,
        tray_menu: true,
        // GNOME 默认不显示 AppIndicator；其它桌面通常有 StatusNotifier 宿主。
        tray_reliable: tray_reliable(),
        window_positioning: x11,
        window_positioning_backend,
        always_on_top: x11,
        // 门户路径要等异步绑定成功后才由 current_capabilities() 置为 true。
        global_shortcut: x11,
        global_shortcut_backend,
        screen_capture: x11 || wayland_screenshot,
        screen_capture_only: wayland_screenshot,
        input_injection: x11,
        input_injection_backend,
        activate_window: x11,
        os_control: x11,
        close_action_default: "hide_and_float",
        notes,
    }
}

/// 托盘是否大概率可见。
///
/// GNOME 需要额外安装 AppIndicator 扩展；无法从进程内可靠探测扩展是否启用，
/// 因此对 GNOME 保守返回 false。其它桌面（KDE/XFCE/MATE 等）通常自带
/// StatusNotifierWatcher。
fn tray_reliable() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    !desktop.is_empty() && !desktop.contains("gnome")
}

pub fn apply_startup_env() {
    let session = current_session();

    // 高级 opt-in：Wayland 用户如果更在意窗口定位/键鼠，可以强制走 XWayland。
    // 不默认开启，避免 HiDPI 模糊和原生 Wayland 应用截图失败。
    if session == SessionType::Wayland
        && std::env::var_os("KONATA_FORCE_X11").is_some()
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("GDK_BACKEND").is_none()
    {
        std::env::set_var("GDK_BACKEND", "x11");
        eprintln!("[platform] KONATA_FORCE_X11=1：使用 XWayland（GDK_BACKEND=x11）");
    }

    if session == SessionType::Wayland && wayland_screenshot_opt_in() {
        eprintln!("[platform] KONATA_WAYLAND_SCREENSHOT=1：请求启用 Wayland 只读截图（实验）");
    }

    if session == SessionType::Wayland && wayland_global_shortcuts_opt_in() {
        eprintln!(
            "[platform] KONATA_WAYLAND_GLOBAL_SHORTCUTS=1：请求启用 Wayland 全局快捷键（XDG 门户，实验）"
        );
    }

    if session == SessionType::Wayland && wayland_layer_shell_opt_in() {
        eprintln!(
            "[platform] KONATA_WAYLAND_LAYER_SHELL=1：请求启用 Wayland layer-shell 悬浮窗定位（实验）"
        );
    }

    if session == SessionType::Wayland && wayland_input_opt_in() {
        eprintln!(
            "[platform] KONATA_WAYLAND_INPUT=1：请求启用 Wayland EIS 键鼠注入（实验，需要系统授权）"
        );
    }

    let safe_mode = std::env::var_os("KONATA_WEBKIT_SAFE").is_some();
    let nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists();

    if safe_mode {
        set_env_if_absent("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        set_env_if_absent("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        set_env_if_absent("__NV_DISABLE_EXPLICIT_SYNC", "1");
        eprintln!("[platform] KONATA_WEBKIT_SAFE=1：已启用 WebKitGTK 兼容环境变量");
    } else if nvidia {
        // Tauri 官方对 NVIDIA + WebKitGTK 的首选 workaround。
        // 只设置这一项，保留 compositing 性能；仍异常时用户可再开 KONATA_WEBKIT_SAFE。
        set_env_if_absent("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        eprintln!("[platform] 检测到 NVIDIA 驱动，已设置 WEBKIT_DISABLE_DMABUF_RENDERER=1");
    }
}

fn set_env_if_absent(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, value);
    }
}

pub fn start_global_shortcut_service(app: tauri::AppHandle) {
    if capabilities().global_shortcut_backend != GlobalShortcutBackend::Portal {
        return;
    }
    global_shortcuts::start(app);
}

/// GTK 初始化后尝试为悬浮窗启用 layer-shell（主线程调用）。
pub fn init_float_window_layer_shell(window: &tauri::WebviewWindow) {
    if capabilities().window_positioning_backend != WindowPositioningBackend::LayerShell {
        return;
    }

    let result = window
        .gtk_window()
        .map_err(|err| anyhow!("无法取得悬浮窗的 GTK 句柄：{err}"))
        .and_then(|gtk_window| layer_shell::init(&gtk_window));

    match result {
        Ok(()) => {
            let note = "Wayland layer-shell 悬浮窗定位已启用（实验）：支持自由拖拽与置顶，\
                        位置由合成器的 layer-shell 协议管理；GNOME/Mutter 不支持该协议。"
                .to_string();
            set_layer_shell_status(true, Some(note.clone()));
            eprintln!("[platform] {note}");
        }
        Err(err) => {
            let note = format!(
                "Wayland layer-shell 不可用：{err}。悬浮窗位置由合成器决定，\
                 应用不会假装定位成功。"
            );
            set_layer_shell_status(false, Some(note.clone()));
            eprintln!("[platform] {note}");
        }
    }
}

/// 用 layer-shell 边距把悬浮窗放到指定角落；未初始化时返回 false。
pub fn position_float_window_layer_shell(position: &str) -> bool {
    layer_shell::position(position)
}

/// 按逻辑像素增量移动 layer-shell 悬浮窗（自由拖拽）。
pub fn move_float_window_by(dx: i32, dy: i32) -> Result<()> {
    layer_shell::move_by(dx, dy)
}

/// 启动 Wayland EIS 输入服务（仅显式 opt-in 时）。
pub fn start_input_service(app: tauri::AppHandle) {
    if capabilities().input_injection_backend != InputInjectionBackend::Eis {
        return;
    }
    input::start(app);
}

pub fn input_move_abs(x: i32, y: i32) -> Result<()> {
    if capabilities().input_injection_backend == InputInjectionBackend::Eis {
        input::move_abs(x, y)
    } else {
        move_cursor(x, y)
    }
}

pub fn input_click(button: MouseButton, double: bool) -> Result<()> {
    if capabilities().input_injection_backend == InputInjectionBackend::Eis {
        input::click(button, double)
    } else {
        super::native_input::click(button, double)
    }
}

pub fn input_type_text(text: &str) -> Result<()> {
    if capabilities().input_injection_backend == InputInjectionBackend::Eis {
        input::type_text(text)
    } else {
        super::native_input::type_text(text)
    }
}

pub fn input_key_combo(keys: &[KeyToken]) -> Result<()> {
    if capabilities().input_injection_backend == InputInjectionBackend::Eis {
        input::key_combo(keys)
    } else {
        super::native_input::key_combo(keys)
    }
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    let reply = conn.query_pointer(root).ok()?.reply().ok()?;
    Some((reply.root_x as i32, reply.root_y as i32))
}

pub fn move_cursor(x: i32, y: i32) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| anyhow!("连接 X11 失败：{e}"))?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| anyhow!("X11 screen {screen_num} 不存在"))?
        .root;

    // 绝对坐标走 WarpPointer，而不是 enigo 的 Coordinate::Abs：
    // enigo 0.2 只按主屏归一化，副屏在左侧时的负坐标会点错。
    conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x as i16, y as i16)
        .map_err(|e| anyhow!("把鼠标移到 ({x}, {y}) 失败：{e}"))?;
    conn.flush()
        .map_err(|e| anyhow!("刷新 X11 请求失败：{e}"))?;
    Ok(())
}

pub fn activate_window(title: &str, settle: Duration) -> Result<String> {
    let needle = title.trim().to_lowercase();
    if needle.is_empty() {
        return Err(anyhow!("窗口标题不能为空"));
    }

    let (conn, screen_num) = x11rb::connect(None).map_err(|e| anyhow!("连接 X11 失败：{e}"))?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| anyhow!("X11 screen {screen_num} 不存在"))?
        .root;

    let client_list = intern_atom(&conn, b"_NET_CLIENT_LIST")?;
    let net_wm_name = intern_atom(&conn, b"_NET_WM_NAME")?;
    let utf8_string = intern_atom(&conn, b"UTF8_STRING")?;
    let net_active_window = intern_atom(&conn, b"_NET_ACTIVE_WINDOW")?;
    let net_wm_state = intern_atom(&conn, b"_NET_WM_STATE")?;
    let net_wm_state_hidden = intern_atom(&conn, b"_NET_WM_STATE_HIDDEN")?;

    let reply = conn
        .get_property(false, root, client_list, AtomEnum::WINDOW, 0, u32::MAX)
        .map_err(|e| anyhow!("读取 _NET_CLIENT_LIST 失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("读取 _NET_CLIENT_LIST 失败：{e}"))?;
    let windows: Vec<Window> = reply
        .value32()
        .map(|values| values.collect())
        .unwrap_or_default();

    if windows.is_empty() {
        return Err(anyhow!(
            "当前窗口管理器没有提供 _NET_CLIENT_LIST，无法按标题激活窗口"
        ));
    }

    let mut exact = None;
    let mut partial = None;
    for window in windows {
        let Some(name) = window_title(&conn, window, net_wm_name, utf8_string)? else {
            continue;
        };
        let lower = name.to_lowercase();
        if lower == needle {
            exact = Some((window, name));
            break;
        }
        if partial.is_none() && lower.contains(&needle) {
            partial = Some((window, name));
        }
    }

    let (window, name) = exact.or(partial).ok_or_else(|| {
        anyhow!("没有找到标题包含「{title}」的窗口。若它最小化在托盘里，需要先让用户从托盘唤出")
    })?;

    // 先从最小化状态恢复。_NET_ACTIVE_WINDOW 本身也应让 WM 取消最小化，
    // 但部分 WM 需要显式移除 _NET_WM_STATE_HIDDEN。
    send_client_message(
        &conn,
        root,
        window,
        net_wm_state,
        [0, net_wm_state_hidden, 0, 2, 0],
    )?;
    send_client_message(
        &conn,
        root,
        window,
        net_active_window,
        [2, CURRENT_TIME, 0, 0, 0],
    )?;
    conn.flush()
        .map_err(|e| anyhow!("刷新 X11 请求失败：{e}"))?;

    std::thread::sleep(settle);
    Ok(format!("已把窗口「{name}」切到前台"))
}

fn intern_atom(conn: &impl Connection, name: &[u8]) -> Result<u32> {
    conn.intern_atom(false, name)
        .map_err(|e| anyhow!("创建 X11 atom 失败：{e}"))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|e| anyhow!("创建 X11 atom 失败：{e}"))
}

fn window_title(
    conn: &impl Connection,
    window: Window,
    net_wm_name: u32,
    utf8_string: u32,
) -> Result<Option<String>> {
    let reply = conn
        .get_property(false, window, net_wm_name, utf8_string, 0, 1024)
        .map_err(|e| anyhow!("读取窗口标题失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("读取窗口标题失败：{e}"))?;
    if !reply.value.is_empty() {
        return Ok(Some(String::from_utf8_lossy(&reply.value).to_string()));
    }

    let reply = conn
        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
        .map_err(|e| anyhow!("读取窗口标题失败：{e}"))?
        .reply()
        .map_err(|e| anyhow!("读取窗口标题失败：{e}"))?;
    if reply.value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(String::from_utf8_lossy(&reply.value).to_string()))
    }
}

fn send_client_message(
    conn: &impl Connection,
    root: Window,
    window: Window,
    type_: u32,
    data: [u32; 5],
) -> Result<()> {
    let event = ClientMessageEvent::new(32, window, type_, ClientMessageData::from(data));
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )
    .map_err(|e| anyhow!("发送 X11 客户端消息失败：{e}"))?;
    Ok(())
}

fn current_session() -> SessionType {
    let xdg = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").map(|_| ());
    let display = std::env::var_os("DISPLAY").map(|_| ());
    classify_session(xdg.as_deref(), wayland.is_some(), display.is_some())
}

fn classify_session(
    xdg_session_type: Option<&str>,
    has_wayland_display: bool,
    has_display: bool,
) -> SessionType {
    match xdg_session_type
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("wayland") => SessionType::Wayland,
        Some("x11") => SessionType::X11,
        _ if has_wayland_display => SessionType::Wayland,
        _ if has_display => SessionType::X11,
        _ => SessionType::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_session_prefers_explicit_xdg_type() {
        assert_eq!(
            classify_session(Some("wayland"), true, true),
            SessionType::Wayland
        );
        assert_eq!(classify_session(Some("x11"), true, true), SessionType::X11);
    }

    #[test]
    fn classify_session_falls_back_to_display_vars() {
        assert_eq!(classify_session(None, true, true), SessionType::Wayland);
        assert_eq!(classify_session(None, false, true), SessionType::X11);
        assert_eq!(classify_session(None, false, false), SessionType::Unknown);
    }

    #[test]
    fn classify_session_ignores_blank_xdg_type() {
        assert_eq!(classify_session(Some("  "), false, true), SessionType::X11);
    }

    /// 只读截图的 opt-in 开关：空值与 "0" 必须是关闭，避免
    /// `KONATA_WAYLAND_SCREENSHOT=0` 被误判成启用
    #[test]
    fn env_flag_treats_zero_and_empty_as_off() {
        use std::ffi::OsStr;

        assert!(env_flag(Some(OsStr::new("1"))));
        assert!(env_flag(Some(OsStr::new("true"))));
        assert!(!env_flag(Some(OsStr::new("0"))));
        assert!(!env_flag(Some(OsStr::new(""))));
        assert!(!env_flag(Some(OsStr::new("  "))));
        assert!(!env_flag(None));
    }

    /// X11 永远走原生插件；Wayland 只有 opt-in + DBus 才尝试门户。
    /// 缺少任一条件都必须返回 None，不能“看起来支持”。
    #[test]
    fn shortcut_backend_requires_wayland_opt_in_and_dbus() {
        assert_eq!(
            shortcut_backend(SessionType::X11, false, false),
            GlobalShortcutBackend::Native
        );
        assert_eq!(
            shortcut_backend(SessionType::X11, true, true),
            GlobalShortcutBackend::Native
        );
        assert_eq!(
            shortcut_backend(SessionType::Wayland, false, true),
            GlobalShortcutBackend::None
        );
        assert_eq!(
            shortcut_backend(SessionType::Wayland, true, false),
            GlobalShortcutBackend::None
        );
        assert_eq!(
            shortcut_backend(SessionType::Wayland, true, true),
            GlobalShortcutBackend::Portal
        );
        assert_eq!(
            shortcut_backend(SessionType::Unknown, true, true),
            GlobalShortcutBackend::None
        );
    }
}
