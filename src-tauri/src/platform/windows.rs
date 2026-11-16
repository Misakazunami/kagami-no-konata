//! Windows 平台实现。
//!
//! 这里保留原有 Win32 行为，避免平台抽象改变 Windows 上的光标/窗口激活语义。

use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetCursorPos, GetForegroundWindow, GetWindowTextW, IsIconic,
    IsWindowVisible, SetCursorPos, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

use super::{GlobalShortcutBackend, PlatformCapabilities, SessionType, WindowPositioningBackend};

pub fn detect() -> PlatformCapabilities {
    PlatformCapabilities {
        os: "windows",
        session_type: SessionType::Native,
        tray_click_events: true,
        tray_menu: true,
        tray_reliable: true,
        window_positioning: true,
        window_positioning_backend: WindowPositioningBackend::Native,
        always_on_top: true,
        global_shortcut: true,
        global_shortcut_backend: GlobalShortcutBackend::Native,
        screen_capture: true,
        screen_capture_only: false,
        input_injection: true,
        activate_window: true,
        os_control: true,
        close_action_default: "hide",
        notes: Vec::new(),
    }
}

pub fn apply_startup_env() {
    // Windows 不需要 GTK/WebKitGTK 的 Linux 兼容环境变量。
}

pub fn start_global_shortcut_service(_app: tauri::AppHandle) {
    // Windows 使用前端 `tauri-plugin-global-shortcut`，无需后端服务。
}

pub fn init_float_window_layer_shell(_window: &tauri::WebviewWindow) {}

pub fn position_float_window_layer_shell(_position: &str) -> bool {
    false
}

pub fn move_float_window_by(_dx: i32, _dy: i32) -> Result<()> {
    Err(anyhow!("layer-shell 仅支持 Linux Wayland 会话"))
}

pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some((point.x, point.y))
}

/// 用 SetCursorPos 而不是 enigo 的绝对坐标。
///
/// enigo 0.2 的 `Coordinate::Abs` 走 `MOUSEEVENTF_ABSOLUTE`，按主屏尺寸
/// 归一化且不带 `MOUSEEVENTF_VIRTUALDESK`：副屏的负坐标表达不了，落点会被
/// 钳回主屏。SetCursorPos 用的是虚拟桌面坐标，左/上方的显示器也能命中。
pub fn move_cursor(x: i32, y: i32) -> Result<()> {
    unsafe { SetCursorPos(x, y) }.map_err(|e| anyhow!("把鼠标移到 ({x}, {y}) 失败：{e}"))
}

struct Hit {
    hwnd: HWND,
    title: String,
    visible: bool,
    iconic: bool,
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam 是调用方传入的 Vec<Hit> 指针，生命周期覆盖整个枚举过程
    let hits = &mut *(lparam.0 as *mut Vec<Hit>);
    let mut buffer = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buffer);
    if len > 0 {
        hits.push(Hit {
            hwnd,
            title: String::from_utf16_lossy(&buffer[..len as usize]),
            visible: IsWindowVisible(hwnd).as_bool(),
            iconic: IsIconic(hwnd).as_bool(),
        });
    }
    BOOL::from(true)
}

pub fn activate_window(title: &str, settle: Duration) -> Result<String> {
    let needle = title.trim().to_lowercase();
    if needle.is_empty() {
        return Err(anyhow!("窗口标题不能为空"));
    }

    let mut hits: Vec<Hit> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), LPARAM(&mut hits as *mut _ as isize))
            .map_err(|e| anyhow!("枚举窗口失败：{e}"))?;
    }

    let mut exact = None;
    let mut partial = None;
    for hit in &hits {
        let lower = hit.title.to_lowercase();
        if lower == needle {
            exact = Some(hit);
            break;
        }
        if partial.is_none() && lower.contains(&needle) && (hit.visible || hit.iconic) {
            partial = Some(hit);
        }
    }
    let hit = exact.or(partial).ok_or_else(|| {
        anyhow!("没有找到标题包含「{title}」的窗口。若它最小化在托盘里，需要先让用户从托盘唤出")
    })?;

    unsafe {
        if hit.iconic {
            let _ = ShowWindow(hit.hwnd, SW_RESTORE);
        }
        let _ = ShowWindow(hit.hwnd, SW_SHOW);
        let _ = BringWindowToTop(hit.hwnd);
        let _ = SetForegroundWindow(hit.hwnd);
    }
    std::thread::sleep(settle);

    if unsafe { GetForegroundWindow() } == hit.hwnd {
        Ok(format!("已把窗口「{}」切到前台", hit.title))
    } else {
        Ok(format!(
            "已尝试激活窗口「{}」，但系统没有把前台交给它（可能被弹窗或其他程序抢占）。\
             请先看最新截图确认当前画面，再决定下一步",
            hit.title
        ))
    }
}
