//! Wayland 只读截图工具
//!
//! 只在 `screen_capture_only` 能力下可见（当前即 Wayland 显式 opt-in）。
//! 它解决的是"看"：模型可以按需截一张当前屏幕，图片由 runner 在下一轮
//! LLM 请求前注入。键鼠注入、窗口激活仍然完全不可用。
//!
//! 与 `os_control` 的每轮自动截图不同，这里**按需**截图：Wayland 的门户
//! 截图可能弹出系统授权框，每轮都截会把用户烦死，也可能让工具循环卡住。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::harness::screen;
use crate::agent::harness::traits::{
    Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput, ToolServices,
};

/// 单次截图的时间上限。
///
/// `screenshots` 的 Wayland 后端会依次尝试 GNOME Shell DBus、xdg-desktop-portal
/// 和 wlr-screencopy；门户等待用户授权时可能阻塞到 60 秒。工具循环不能被它
/// 拖死，因此到点就明确失败，让模型/用户决定是否重试。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);

/// 同一时间只允许一个截图请求在途。
///
/// 阻塞调用即使被 `timeout` 放弃，底层线程仍会继续等门户响应；这时再发一次
/// 只会叠加授权框和 DBus 请求。用这个开关把后续调用明确挡回去。
static CAPTURE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub struct LookAtScreen;

/// 在途标记的 RAII 守卫：阻塞任务结束时才释放，而不是 `timeout` 触发时
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        CAPTURE_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn begin_capture() -> Result<InFlightGuard> {
    if CAPTURE_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(anyhow!(
            "上一次截图仍在等待系统响应（可能正显示授权框），请稍后重试"
        ));
    }
    Ok(InFlightGuard)
}

/// 当前环境为什么不能用只读截图；可用时返回 `None`。
fn look_at_screen_unavailable_reason() -> Option<String> {
    let capabilities = crate::platform::capabilities();
    if capabilities.screen_capture && capabilities.screen_capture_only {
        return None;
    }

    Some(match capabilities.session_type {
        crate::platform::SessionType::Wayland => {
            "Wayland 只读截图未启用：请设置 KONATA_WAYLAND_SCREENSHOT=1 并重启，\
             同时保持 XWayland（DISPLAY）与 DBus 会话可用；它只能看屏幕，不能控制键鼠"
                .to_string()
        }
        crate::platform::SessionType::X11 => {
            "X11 会话由 os_control 每轮自动注入截图，不需要单独调用 look_at_screen".to_string()
        }
        crate::platform::SessionType::Unknown => {
            "未检测到可用的图形显示（DISPLAY/WAYLAND_DISPLAY），截图不可用".to_string()
        }
        _ => "当前平台由 os_control 自动提供截图，不需要单独调用 look_at_screen".to_string(),
    })
}

/// 在阻塞线程里截图，并给整个调用加上限时。
async fn capture_current_screen(max_dim: u32) -> Result<screen::Frame> {
    let guard = begin_capture()?;

    // `Screen::capture()` 内部是同步 DBus/xcb 调用，不能直接放在 async 任务里。
    // 守卫随阻塞任务一起移动：即使外层 timeout 先返回，它也会在门户真正
    // 返回后才释放，避免并发叠加截图请求。
    let task = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        screen::capture_for_model(max_dim)
    });

    match tokio::time::timeout(CAPTURE_TIMEOUT, task).await {
        Ok(joined) => joined.map_err(|e| anyhow!("截图任务异常退出：{e}"))?,
        Err(_) => Err(anyhow!(
            "截图超时（{} 秒）：系统可能正在等待截图授权，或合成器没有响应。\
             请先在桌面弹出的授权框中允许截图后重试；若持续失败，请改用 X11/Xorg 会话",
            CAPTURE_TIMEOUT.as_secs()
        )),
    }
}

#[async_trait]
impl Tool for LookAtScreen {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "look_at_screen",
            "查看屏幕（只读）",
            "截取当前显示器画面供你查看。仅在 Wayland 只读模式下可用：\
             调用后图片会在下一轮对话开始前自动加入上下文，不需要也不能指定坐标。\
             这个工具只能看，不能点击、输入或切换窗口；不要声称已经操作界面。",
            Permission::Read,
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    fn enabled(&self) -> bool {
        let capabilities = crate::platform::capabilities();
        capabilities.screen_capture && capabilities.screen_capture_only
    }

    fn unavailable_reason(&self) -> Option<String> {
        look_at_screen_unavailable_reason()
    }

    fn timeout_budget(&self, _services: &ToolServices) -> Option<Duration> {
        // 内层 20 秒超时必须先于 runner 的外层兜底触发，
        // 否则用户只会看到笼统的“工具超时”，拿不到授权提示。
        Some(CAPTURE_TIMEOUT)
    }

    async fn call(&self, _args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // 注册表已经按能力过滤；这里再兜一层，防止旧 schema/直接调用
        // 在未 opt-in 的 Wayland 会话里“看起来成功”。
        if let Some(reason) = look_at_screen_unavailable_reason() {
            return Err(anyhow!(reason));
        }
        cx.ensure_not_cancelled()?;

        let frame = capture_current_screen(cx.services.screenshot_max_dim).await?;

        // 用户可能在等待截图授权时点了“停止”：这张图不能再塞进下一轮
        if cx.cancelled() {
            return Err(anyhow!("已被用户停止"));
        }

        screen::stash_pending_frame(cx.session_id, cx.stream_id, frame);

        let text = "已截取当前显示器画面；图片会在下一轮对话开始前自动加入上下文。\
                    请以收到的截图为准，不要猜测画面内容。";
        Ok(ToolOutput::text(text.to_string()).with_preview(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_read_only() {
        let descriptor = LookAtScreen.descriptor();
        assert_eq!(descriptor.name, "look_at_screen");
        assert_eq!(descriptor.permission, Permission::Read);
    }

    /// 在途守卫：上一次截图还没结束时必须明确拒绝，而不是排队叠加请求
    #[test]
    fn in_flight_guard_rejects_overlapping_captures() {
        let first = begin_capture().expect("第一次应能开始截图");
        assert!(begin_capture().is_err(), "上一次截图未结束时不能再次开始");
        drop(first);
        assert!(begin_capture().is_ok(), "上一次截图结束后应能再次开始");
    }
}
