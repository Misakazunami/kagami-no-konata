//! 屏幕感知：选屏、截图缩放、坐标换算
//!
//! 多显示器 + 每显示器 DPI 缩放会让"截图里的坐标"和"系统里的坐标"不是同一个
//! 东西，这里把三件事收敛到唯一一处实现：
//!
//! 1. **选屏**：跟随鼠标所在显示器，退回主屏。绝不使用 `Screen::all()` 的
//!    枚举顺序——`EnumDisplayMonitors` 的顺序由系统决定，多屏机器上第一块
//!    完全可能是没有窗口的副屏（曾经因此把整块壁纸喂给模型）；
//! 2. **缩放**：送进模型的截图统一等比缩到长边 [`DEFAULT_MAX_IMAGE_DIM`]，避免高分屏
//!    产生几 MB 的 Base64；
//! 3. **换算**：模型给的是**截图里的像素坐标**，必须按同一缩放比还原、再加回
//!    显示器原点，才是 `SetCursorPos` 用的屏幕物理坐标。
//!
//! 缩放比在截图时记进 [`last_mapping`]，`os_control` 的鼠标动作据此换算，
//! 保证"模型看到的那张图"和"落点"始终是同一坐标系。

use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Result};
use screenshots::{image::RgbaImage, Screen};

/// 截图长边上限的默认值（像素）
///
/// 只是"默认"，实际值来自 `tools.screenshot_max_dim`：0 表示按原始分辨率下发。
/// 1024 是 token 与可读性的折中；需要看清聊天记录、菜单里的小字时必须调大。
pub const DEFAULT_MAX_IMAGE_DIM: u32 = 1024;

/// 截图 JPEG 质量的默认值
pub const DEFAULT_JPEG_QUALITY: u8 = 85;

/// 一次截图建立的坐标系
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    /// 显示器左上角在屏幕坐标里的原点（物理像素，可为负）
    pub origin_x: i32,
    pub origin_y: i32,
    /// 图像像素 / 屏幕物理像素（≤ 1）
    pub scale: f32,
    /// 缩放后的图像尺寸，用于越界检查
    pub image_width: u32,
    pub image_height: u32,
    /// 显示器物理尺寸
    pub screen_width: u32,
    pub screen_height: u32,
}

/// 一轮生成用的截图帧
pub struct Frame {
    /// 已缩放到长边 ≤ [`DEFAULT_MAX_IMAGE_DIM`] 的图像（或按配置不缩放）
    pub image: RgbaImage,
    /// 随图像一起注入上下文的说明文字（必须以 [`FRAME_MARKER`] 开头，
    /// runner 靠它清理历史快照）
    pub note: String,
}

/// 快照消息的前缀：runner 用它识别并清理上一轮的图，别改
pub const FRAME_MARKER: &str = "[系统感知] 当前屏幕快照";

static LAST_MAPPING: OnceLock<Mutex<Option<Mapping>>> = OnceLock::new();

/// 按需截图（`look_at_screen`）暂存区
///
/// 工具本身只能返回文本，图片要等 runner 在下一轮调用 LLM 前注入。
/// 带上 session/stream 是为了避免“用户已经停止生成，下一轮另一个会话
/// 却突然收到上一张屏幕图”的泄漏。
struct PendingFrame {
    session_id: String,
    stream_id: String,
    frame: Frame,
}

static PENDING_FRAME: OnceLock<Mutex<Option<PendingFrame>>> = OnceLock::new();

fn mapping_slot() -> &'static Mutex<Option<Mapping>> {
    LAST_MAPPING.get_or_init(|| Mutex::new(None))
}

fn pending_slot() -> &'static Mutex<Option<PendingFrame>> {
    PENDING_FRAME.get_or_init(|| Mutex::new(None))
}

/// 暂存一张按需截图，等待 runner 注入
pub fn stash_pending_frame(
    session_id: impl Into<String>,
    stream_id: impl Into<String>,
    frame: Frame,
) {
    if let Ok(mut slot) = pending_slot().lock() {
        *slot = Some(PendingFrame {
            session_id: session_id.into(),
            stream_id: stream_id.into(),
            frame,
        });
    }
}

/// 取出属于当前会话/流的按需截图；不匹配的旧帧不会被别的会话读到
pub fn take_pending_frame(session_id: &str, stream_id: &str) -> Option<Frame> {
    let mut slot = pending_slot().lock().ok()?;
    let matches = slot
        .as_ref()
        .is_some_and(|pending| pending.session_id == session_id && pending.stream_id == stream_id);
    if matches {
        slot.take().map(|pending| pending.frame)
    } else {
        None
    }
}

fn remember(mapping: Mapping) {
    if let Ok(mut slot) = mapping_slot().lock() {
        *slot = Some(mapping);
    }
}

/// 最近一次截图的坐标系（模型当前看到的画面就是这个）
pub fn last_mapping() -> Option<Mapping> {
    mapping_slot().lock().ok().and_then(|slot| *slot)
}

/// 等比缩放比：长边不超过 `max_dim`，小图不放大
///
/// `max_dim == 0` 表示不缩放（原始分辨率）。
pub fn fit_scale(width: u32, height: u32, max_dim: u32) -> f32 {
    if max_dim == 0 {
        return 1.0;
    }
    let longest = width.max(height).max(1) as f32;
    (max_dim as f32 / longest).min(1.0)
}

/// 当前应该操作的显示器
///
/// 优先鼠标所在的显示器（用户正在看的那个），取不到就退回主屏，再退回第一块。
/// 注意：不能用 `screens.first()`——枚举顺序与"主屏"无关。
pub fn active_screen() -> Result<Screen> {
    let screens = Screen::all().map_err(|e| anyhow!("无法枚举显示器：{e}"))?;
    let fallback = screens
        .iter()
        .find(|s| s.display_info.is_primary)
        .or_else(|| screens.first())
        .copied()
        .ok_or_else(|| anyhow!("没有检测到可用的显示器"))?;

    // 光标定位统一走平台层：Windows 是 GetCursorPos，X11 是 QueryPointer。
    // Wayland 下 XWayland 的光标位置可能停在原生 Wayland 窗口之外，选屏会
    // 选错；只读截图直接用主屏，避免把副屏画面当成主屏喂给模型。
    let use_cursor =
        crate::platform::capabilities().session_type != crate::platform::SessionType::Wayland;
    if use_cursor {
        if let Some((x, y)) = crate::platform::cursor_position() {
            if let Ok(screen) = Screen::from_point(x, y) {
                return Ok(screen);
            }
        }
    }

    Ok(fallback)
}

/// 截当前显示器并把坐标系记下来
///
/// `max_dim`：长边上限；0 表示按原始分辨率下发（小字更清楚，但更耗 token）。
pub fn capture_for_model(max_dim: u32) -> Result<Frame> {
    let capabilities = crate::platform::capabilities();
    // 不支持时明确失败，不返回黑屏/旧图。Wayland 的只读截图必须由用户
    // 显式 opt-in；默认路径仍然直接拒绝。
    if !capabilities.screen_capture {
        return Err(anyhow!(
            "当前图形会话不支持屏幕截图。Wayland 可设置 KONATA_WAYLAND_SCREENSHOT=1 \
             并重启后启用只读实验截图（需要 XWayland 与系统授权）；\
             需要完整桌面控制请改用 X11/Xorg 会话"
        ));
    }

    let screen = active_screen()?;
    let image = screen.capture().map_err(|e| anyhow!("屏幕截图失败：{e}"))?;

    let (width, height) = (image.width().max(1), image.height().max(1));
    let scale = fit_scale(width, height, max_dim);
    let image_width = ((width as f32) * scale).round().max(1.0) as u32;
    let image_height = ((height as f32) * scale).round().max(1.0) as u32;

    let image = if (image_width, image_height) == (width, height) {
        image
    } else {
        screenshots::image::imageops::resize(
            &image,
            image_width,
            image_height,
            screenshots::image::imageops::FilterType::Triangle,
        )
    };

    let mapping = Mapping {
        origin_x: screen.display_info.x,
        origin_y: screen.display_info.y,
        scale,
        image_width,
        image_height,
        screen_width: width,
        screen_height: height,
    };
    remember(mapping);

    let note = if capabilities.screen_capture_only {
        frame_note_read_only(screen.display_info.is_primary, mapping)
    } else {
        frame_note(screen.display_info.is_primary, mapping)
    };
    Ok(Frame { image, note })
}

/// 未缩放时别写"缩放比 1.000"：模型会以为画面被处理过，反而怀疑坐标不准
fn frame_size_note(mapping: Mapping) -> String {
    if (mapping.scale - 1.0).abs() < 1e-4 {
        format!(
            "原始分辨率 {}×{}，未缩放",
            mapping.image_width, mapping.image_height
        )
    } else {
        format!(
            "截图已缩放至 {}×{}，缩放比 {:.3}",
            mapping.image_width, mapping.image_height, mapping.scale
        )
    }
}

fn frame_note(is_primary: bool, mapping: Mapping) -> String {
    let role = if is_primary { "主屏" } else { "副屏" };
    let size = frame_size_note(mapping);
    format!(
        "{FRAME_MARKER}（{role}，屏幕 {}×{}；{size}）\n\
         坐标约定：mouse_click 的 x/y 直接填**这张截图里**的像素坐标（原点在图片左上角），\
         工具会自动换算成屏幕物理坐标。\
         截图只包含这一块显示器；目标窗口若被最小化或不在前台，它根本不会出现在画面里，\
         请先用 activate_window 把它切到前台。",
        mapping.screen_width, mapping.screen_height,
    )
}

/// 只读截图（Wayland opt-in）的说明
///
/// 不能复用 [`frame_note`]：那段文字会教模型用 `mouse_click` 换算坐标，
/// 而只读会话里根本没有鼠标工具，模型会误以为"看得到就点得到"。
fn frame_note_read_only(is_primary: bool, mapping: Mapping) -> String {
    let role = if is_primary { "主屏" } else { "副屏" };
    let size = frame_size_note(mapping);
    format!(
        "{FRAME_MARKER}（{role}，屏幕 {}×{}；{size}）\n\
         这是一张**只读**截图：你可以观察屏幕内容并据此回答用户，但当前 Wayland 会话\
         不支持鼠标点击、键盘输入或窗口切换，**不要声称已经操作界面**。\
         如果用户需要实际控制桌面，请说明需要改用 X11/Xorg 会话。",
        mapping.screen_width, mapping.screen_height,
    )
}

/// 把截图里的像素坐标换算成屏幕物理坐标
///
/// 用的是**最近一次**截图的坐标系，而不是"现在鼠标在哪块屏"——模型拿到的是
/// 上一轮的图，落点必须跟那张图对齐。
pub fn image_to_screen(x: i64, y: i64) -> Result<(i32, i32)> {
    let mapping = last_mapping().ok_or_else(|| {
        anyhow!("当前还没有可用的屏幕截图，无法换算坐标；请等待系统注入截图后再操作")
    })?;
    map_image_point(&mapping, x as f64, y as f64)
}

/// 纯函数版的坐标换算（单元测试直接测它）
pub fn map_image_point(mapping: &Mapping, x: f64, y: f64) -> Result<(i32, i32)> {
    if mapping.scale <= 0.0 || !mapping.scale.is_finite() {
        return Err(anyhow!("截图缩放比无效：{}", mapping.scale));
    }
    if x < 0.0 || y < 0.0 || x >= mapping.image_width as f64 || y >= mapping.image_height as f64 {
        return Err(anyhow!(
            "坐标 ({x:.0}, {y:.0}) 超出截图范围 {}×{}，请使用截图内的像素坐标",
            mapping.image_width,
            mapping.image_height
        ));
    }

    let screen_x = mapping.origin_x + (x / mapping.scale as f64).round() as i32;
    let screen_y = mapping.origin_y + (y / mapping.scale as f64).round() as i32;
    Ok((screen_x, screen_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(origin_x: i32, origin_y: i32, scale: f32) -> Mapping {
        Mapping {
            origin_x,
            origin_y,
            scale,
            image_width: 1024,
            image_height: 640,
            screen_width: 2560,
            screen_height: 1600,
        }
    }

    #[test]
    fn fit_scale_never_upscales_small_screens() {
        assert_eq!(fit_scale(1024, 768, DEFAULT_MAX_IMAGE_DIM), 1.0);
        assert_eq!(fit_scale(800, 600, DEFAULT_MAX_IMAGE_DIM), 1.0);
    }

    #[test]
    fn fit_scale_limits_the_long_edge() {
        let max = DEFAULT_MAX_IMAGE_DIM;
        assert!((fit_scale(2560, 1600, max) - 0.4).abs() < 1e-6);
        // 竖屏也按长边算
        assert!((fit_scale(1600, 2560, max) - 0.4).abs() < 1e-6);
        // 1080p 主屏
        assert!((fit_scale(1920, 1080, max) - 1024.0 / 1920.0).abs() < 1e-6);
    }

    /// 0 = 不缩放：原图下发时坐标就是屏幕物理像素，换算比例必须是 1
    #[test]
    fn zero_max_dim_keeps_native_resolution() {
        assert_eq!(fit_scale(2560, 1600, 0), 1.0);
        assert_eq!(fit_scale(3840, 2160, 0), 1.0);
    }

    /// 放宽上限时按新值缩放：2560 长边设成 2048 只缩到 0.8
    #[test]
    fn custom_max_dim_is_honoured() {
        assert!((fit_scale(2560, 1600, 2048) - 0.8).abs() < 1e-6);
        // 上限比图还大时不放大
        assert_eq!(fit_scale(1920, 1080, 4096), 1.0);
    }

    /// 副屏在左边时坐标原点是负的：换算必须加回原点，否则永远点在主屏上
    #[test]
    fn map_point_restores_origin_and_scale() {
        let m = mapping(-2560, 0, 0.4);
        assert_eq!(map_image_point(&m, 0.0, 0.0).unwrap(), (-2560, 0));
        assert_eq!(map_image_point(&m, 500.0, 300.0).unwrap(), (-1310, 750));
        // 右下角附近：不能溢出到显示器外
        let (x, y) = map_image_point(&m, 1023.0, 639.0).unwrap();
        assert!((-2560..-2560 + 2560).contains(&x));
        assert!((0..1600).contains(&y));
    }

    #[test]
    fn map_point_handles_primary_screen() {
        let m = Mapping {
            origin_x: 0,
            origin_y: 0,
            scale: 1024.0 / 1920.0,
            image_width: 1024,
            image_height: 576,
            screen_width: 1920,
            screen_height: 1080,
        };
        assert_eq!(map_image_point(&m, 0.0, 0.0).unwrap(), (0, 0));
        assert_eq!(map_image_point(&m, 512.0, 288.0).unwrap(), (960, 540));
    }

    /// 越界坐标必须报错而不是点到屏幕外——模型偶尔会给出 1080p 的坐标，
    /// 而它看到的其实是 1024×640 的图，这种错误要明确回灌
    #[test]
    fn map_point_rejects_out_of_bounds() {
        let m = mapping(0, 0, 1.0);
        assert!(map_image_point(&m, 1024.0, 10.0).is_err());
        assert!(map_image_point(&m, 10.0, 640.0).is_err());
        assert!(map_image_point(&m, -1.0, 10.0).is_err());
    }

    #[test]
    fn frame_note_starts_with_marker() {
        let m = Mapping {
            image_width: 1024,
            image_height: 576,
            ..mapping(0, 0, 1024.0 / 1920.0)
        };
        let note = frame_note(true, m);
        assert!(note.starts_with(FRAME_MARKER), "{note}");
        assert!(note.contains("主屏"), "{note}");
        assert!(note.contains("activate_window"), "{note}");
    }

    /// 原始分辨率下发时不能写"缩放比 1.000"：模型会以为画面被压缩过
    #[test]
    fn frame_note_reports_native_resolution_when_unscaled() {
        let m = Mapping {
            origin_x: 0,
            origin_y: 0,
            scale: 1.0,
            image_width: 1920,
            image_height: 1080,
            screen_width: 1920,
            screen_height: 1080,
        };
        let note = frame_note(true, m);
        assert!(note.contains("原始分辨率 1920×1080"), "{note}");
        assert!(!note.contains("缩放比"), "{note}");
    }

    /// 只读模式不能教模型换算坐标、切前台：这些工具根本不可用
    #[test]
    fn read_only_note_does_not_promise_desktop_control() {
        let m = Mapping {
            image_width: 1024,
            image_height: 576,
            ..mapping(0, 0, 1024.0 / 1920.0)
        };
        let note = frame_note_read_only(true, m);
        assert!(note.starts_with(FRAME_MARKER), "{note}");
        assert!(note.contains("只读"), "{note}");
        assert!(!note.contains("mouse_click"), "{note}");
        assert!(!note.contains("activate_window"), "{note}");
    }

    /// 按需截图必须按 session/stream 隔离：另一个会话不能读到这张图
    #[test]
    fn pending_frame_is_scoped_to_session_and_stream() {
        let frame = Frame {
            image: RgbaImage::new(2, 2),
            note: "test".to_string(),
        };
        stash_pending_frame("session-a", "stream-a", frame);

        assert!(take_pending_frame("session-b", "stream-a").is_none());
        assert!(take_pending_frame("session-a", "stream-b").is_none());

        let taken = take_pending_frame("session-a", "stream-a").expect("应取到本会话的帧");
        assert_eq!(taken.note, "test");
        assert!(take_pending_frame("session-a", "stream-a").is_none());
    }
}
