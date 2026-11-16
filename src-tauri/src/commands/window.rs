use crate::platform;
use crate::AppState;
use tauri::{Emitter, Manager, State};

/// 根据配置将悬浮窗定位到屏幕指定角落。
///
/// 返回 `false` 表示当前平台/会话不支持绝对定位（例如 Wayland），
/// 调用方应提示用户，而不是假装成功。
pub fn apply_float_position(window: &tauri::WebviewWindow, position: &str) -> bool {
    if !platform::capabilities().window_positioning {
        return false;
    }

    let margin = 20.0f64;
    let Some(monitor) = window.primary_monitor().ok().flatten() else {
        return false;
    };
    let size = monitor.size();
    let scale = monitor.scale_factor();
    let sw = size.width as f64 / scale;
    let sh = size.height as f64 / scale;

    // 用实际窗口尺寸，避免以后改 tauri.conf.json 的宽高后定位漂移；
    // outer_size 是物理像素，需要按同一 scale 转成逻辑坐标。
    let (win_w, win_h) = window
        .outer_size()
        .ok()
        .map(|size| (size.width as f64 / scale, size.height as f64 / scale))
        .unwrap_or((220.0, 340.0));
    let win_w = win_w.min((sw - margin * 2.0).max(1.0));
    let win_h = win_h.min((sh - margin * 2.0).max(1.0));

    let (x, y) = match position {
        "top-left" => (margin, margin),
        "top-center" => ((sw - win_w) / 2.0, margin),
        "top-right" => (sw - win_w - margin, margin),
        "bottom-left" => (margin, sh - win_h - margin),
        "bottom-center" => ((sw - win_w) / 2.0, sh - win_h - margin),
        _ => (sw - win_w - margin, sh - win_h - margin), // bottom-right 默认
    };

    window
        .set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)))
        .is_ok()
}

/// 定位悬浮窗；不支持时发一次前端事件，让 UI 给出准确提示。
pub fn position_float_window(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    position: &str,
) {
    if !apply_float_position(window, position) {
        let _ = app.emit("float-position-unsupported", ());
    }
}

/// 显示主窗口
#[tauri::command]
pub async fn show_main_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 隐藏主窗口
#[tauri::command]
pub async fn hide_main_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 切换主窗口显示/隐藏
#[tauri::command]
pub async fn toggle_main_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            window.hide().map_err(|e| e.to_string())?;
        } else {
            window.show().map_err(|e| e.to_string())?;
            window.set_focus().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 显示悬浮窗
#[tauri::command]
pub async fn show_float_window(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("float") {
        let _ = window.set_shadow(false);
        let position = state
            .config
            .lock()
            .map(|c| c.ui.float_position.clone())
            .unwrap_or_else(|_| "bottom-right".to_string());
        position_float_window(&app, &window, &position);
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        let _ = app.emit("float-visibility-changed", true);
    }
    Ok(())
}

/// 隐藏悬浮窗
#[tauri::command]
pub async fn hide_float_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("float") {
        window.hide().map_err(|e| e.to_string())?;
    }
    let _ = app.emit("float-visibility-changed", false);
    Ok(())
}

/// 切换悬浮窗显示/隐藏
#[tauri::command]
pub async fn toggle_float_window(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("float") {
        if window.is_visible().unwrap_or(false) {
            window.hide().map_err(|e| e.to_string())?;
            let _ = app.emit("float-visibility-changed", false);
        } else {
            let _ = window.set_shadow(false);
            let position = state
                .config
                .lock()
                .map(|c| c.ui.float_position.clone())
                .unwrap_or_else(|_| "bottom-right".to_string());
            position_float_window(&app, &window, &position);
            window.show().map_err(|e| e.to_string())?;
            window.set_focus().map_err(|e| e.to_string())?;
            let _ = app.emit("float-visibility-changed", true);
        }
    }
    Ok(())
}

/// 查询悬浮窗是否可见
#[tauri::command]
pub async fn is_float_visible(app: tauri::AppHandle) -> Result<bool, String> {
    if let Some(window) = app.get_webview_window("float") {
        Ok(window.is_visible().unwrap_or(false))
    } else {
        Ok(false)
    }
}
