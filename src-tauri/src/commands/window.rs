use tauri::{Emitter, Manager, State};
use crate::AppState;

/// 根据配置将悬浮窗定位到屏幕指定角落
pub fn apply_float_position(window: &tauri::WebviewWindow, position: &str) {
    let margin = 20.0f64;
    let win_w = 220.0f64;
    let win_h = 340.0f64;

    if let Some(monitor) = window.primary_monitor().ok().flatten() {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let sw = size.width as f64 / scale;
        let sh = size.height as f64 / scale;

        let (x, y) = match position {
            "top-left" => (margin, margin),
            "top-center" => ((sw - win_w) / 2.0, margin),
            "top-right" => (sw - win_w - margin, margin),
            "bottom-left" => (margin, sh - win_h - margin),
            "bottom-center" => ((sw - win_w) / 2.0, sh - win_h - margin),
            _ => (sw - win_w - margin, sh - win_h - margin), // bottom-right 默认
        };

        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)));
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
pub async fn show_float_window(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("float") {
        let _ = window.set_shadow(false);
        let position = state.config.lock().map(|c| c.ui.float_position.clone()).unwrap_or_else(|_| "bottom-right".to_string());
        apply_float_position(&window, &position);
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
pub async fn toggle_float_window(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("float") {
        if window.is_visible().unwrap_or(false) {
            window.hide().map_err(|e| e.to_string())?;
            let _ = app.emit("float-visibility-changed", false);
        } else {
            let _ = window.set_shadow(false);
            let position = state.config.lock().map(|c| c.ui.float_position.clone()).unwrap_or_else(|_| "bottom-right".to_string());
            apply_float_position(&window, &position);
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
