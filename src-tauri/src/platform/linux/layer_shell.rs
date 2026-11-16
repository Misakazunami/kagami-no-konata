//! Wayland 悬浮窗定位：`wlr-layer-shell` 的运行时 dlopen 绑定。
//!
//! 为什么不是直接链接 `libgtk-layer-shell`：
//!
//! - P3.3 是实验能力，默认关闭；如果默认二进制硬链接该库，没装库的用户
//!   会在动态链接阶段直接启动失败，连聊天都用不了。
//! - 这里用 `libloading` 在真正 opt-in 且 GTK 初始化后按需加载；库缺失、
//!   合成器不支持、窗口已经 realize 都只影响悬浮窗定位能力，不影响启动。
//!
//! 线程约束：`gtk_layer_init_for_window` 与所有 GTK 调用只能在主线程执行。
//! 初始化发生在 Tauri setup（主线程）；拖拽更新通过 `glib::idle_add_once`
//! 回到默认主循环，避免从 Tauri 命令线程直接碰 GTK。
//!
//! 自由拖拽：layer-shell 表面不能像 xdg_toplevel 那样由合成器移动，位置由
//! 锚点 + 边距决定。前端把指针增量发过来，这里更新 left/top 边距；因此拖拽
//! 的平滑度取决于 WebKit 在 Wayland 下能否持续提供指针增量。

use std::ffi::{c_char, c_int, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;
use libloading::Library;

// gtk-layer-shell 的 C 枚举值（见 gtk-layer-shell/gtk-layer-shell.h）
const EDGE_LEFT: c_int = 0;
const EDGE_RIGHT: c_int = 1;
const EDGE_TOP: c_int = 2;
const EDGE_BOTTOM: c_int = 3;
const LAYER_TOP: c_int = 2;
const KEYBOARD_MODE_ON_DEMAND: c_int = 2;

/// 默认边距（逻辑像素），与 X11/Windows 的悬浮窗保持一致。
const MARGIN: i32 = 20;

type GtkLayerIsSupported = unsafe extern "C" fn() -> c_int;
type GtkLayerInitForWindow = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow);
type GtkLayerSetNamespace = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, *const c_char);
type GtkLayerSetLayer = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, c_int);
type GtkLayerSetAnchor = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, c_int, c_int);
type GtkLayerSetMargin = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, c_int, c_int);
type GtkLayerSetMonitor =
    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, *mut gtk::gdk::ffi::GdkMonitor);
type GtkLayerSetExclusiveZone = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, c_int);
type GtkLayerSetKeyboardMode = unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, c_int);

struct LayerShellLib {
    /// 函数指针指向库内代码，必须让 Library 活到进程结束。
    _library: Library,
    is_supported: GtkLayerIsSupported,
    init_for_window: GtkLayerInitForWindow,
    set_namespace: GtkLayerSetNamespace,
    set_layer: GtkLayerSetLayer,
    set_anchor: GtkLayerSetAnchor,
    set_margin: GtkLayerSetMargin,
    set_monitor: GtkLayerSetMonitor,
    set_exclusive_zone: GtkLayerSetExclusiveZone,
    set_keyboard_mode: GtkLayerSetKeyboardMode,
}

// 安全前提：所有 GTK 符号只在主线程调用；Library 本身在初始化后只读。
unsafe impl Send for LayerShellLib {}
unsafe impl Sync for LayerShellLib {}

/// 是否已经成功初始化并持有窗口指针。
static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// GtkWindow 指针（只作为 usize 保存，实际解引用只发生在主线程）。
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static MONITOR_W: AtomicI32 = AtomicI32::new(0);
static MONITOR_H: AtomicI32 = AtomicI32::new(0);
static WIN_W: AtomicI32 = AtomicI32::new(220);
static WIN_H: AtomicI32 = AtomicI32::new(340);
/// 当前 left/top 边距（layer-shell 的“窗口位置”）。
static POS_X: AtomicI32 = AtomicI32::new(0);
static POS_Y: AtomicI32 = AtomicI32::new(0);
static UPDATE_SCHEDULED: AtomicBool = AtomicBool::new(false);

fn library() -> Option<&'static LayerShellLib> {
    static LIB: OnceLock<Option<LayerShellLib>> = OnceLock::new();
    LIB.get_or_init(|| unsafe { load_library().ok() }).as_ref()
}

unsafe fn load_library() -> Result<LayerShellLib> {
    // 常见发行版安装的是带 soname 的 .so.0；源码构建环境可能只有 .so。
    let mut last_error = None;
    for name in ["libgtk-layer-shell.so.0", "libgtk-layer-shell.so"] {
        match Library::new(name) {
            Ok(library) => {
                let is_supported = *library
                    .get::<GtkLayerIsSupported>(b"gtk_layer_is_supported\0")
                    .context("库缺少 gtk_layer_is_supported")?;
                let init_for_window = *library
                    .get::<GtkLayerInitForWindow>(b"gtk_layer_init_for_window\0")
                    .context("库缺少 gtk_layer_init_for_window")?;
                let set_namespace = *library
                    .get::<GtkLayerSetNamespace>(b"gtk_layer_set_namespace\0")
                    .context("库缺少 gtk_layer_set_namespace")?;
                let set_layer = *library
                    .get::<GtkLayerSetLayer>(b"gtk_layer_set_layer\0")
                    .context("库缺少 gtk_layer_set_layer")?;
                let set_anchor = *library
                    .get::<GtkLayerSetAnchor>(b"gtk_layer_set_anchor\0")
                    .context("库缺少 gtk_layer_set_anchor")?;
                let set_margin = *library
                    .get::<GtkLayerSetMargin>(b"gtk_layer_set_margin\0")
                    .context("库缺少 gtk_layer_set_margin")?;
                let set_monitor = *library
                    .get::<GtkLayerSetMonitor>(b"gtk_layer_set_monitor\0")
                    .context("库缺少 gtk_layer_set_monitor")?;
                let set_exclusive_zone = *library
                    .get::<GtkLayerSetExclusiveZone>(b"gtk_layer_set_exclusive_zone\0")
                    .context("库缺少 gtk_layer_set_exclusive_zone")?;
                let set_keyboard_mode = *library
                    .get::<GtkLayerSetKeyboardMode>(b"gtk_layer_set_keyboard_mode\0")
                    .context("库缺少 gtk_layer_set_keyboard_mode")?;

                return Ok(LayerShellLib {
                    _library: library,
                    is_supported,
                    init_for_window,
                    set_namespace,
                    set_layer,
                    set_anchor,
                    set_margin,
                    set_monitor,
                    set_exclusive_zone,
                    set_keyboard_mode,
                });
            }
            Err(err) => last_error = Some(err),
        }
    }

    Err(anyhow!(
        "无法加载 libgtk-layer-shell：{}",
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    ))
}

/// 当前是否已经成功初始化。前端/命令层据此决定是否走 layer-shell 路径。
pub fn is_ready() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// 初始化悬浮窗。必须在窗口首次 `show()` 且尚未 realize 时调用。
pub fn init(window: &gtk::ApplicationWindow) -> Result<()> {
    if is_ready() {
        return Ok(());
    }
    if window.is_realized() {
        bail!("窗口已经 realize，无法再切换为 layer-shell；需要在首次显示前初始化");
    }

    let lib = library().ok_or_else(|| {
        anyhow!(
            "未找到 libgtk-layer-shell.so.0；请安装 gtk-layer-shell 运行库\
             （Debian/Ubuntu: libgtk-layer-shell0，Fedora: gtk-layer-shell）"
        )
    })?;

    // gtk_layer_is_supported 必须在 GTK 初始化后调用；setup 阶段满足条件。
    if unsafe { (lib.is_supported)() } == 0 {
        bail!("当前合成器不支持 wlr-layer-shell（GNOME/Mutter 通常不支持）");
    }

    let display = gtk::gdk::Display::default().ok_or_else(|| anyhow!("没有可用的 GDK 显示"))?;
    let monitor = display
        .primary_monitor()
        .or_else(|| display.monitor(0))
        .ok_or_else(|| anyhow!("没有可用的显示器"))?;
    let geometry = monitor.geometry();
    let (monitor_w, monitor_h) = (geometry.width(), geometry.height());
    if monitor_w <= 0 || monitor_h <= 0 {
        bail!("显示器尺寸无效：{monitor_w}×{monitor_h}");
    }

    let (mut win_w, mut win_h) = window.default_size();
    if win_w <= 1 || win_h <= 1 {
        let (allocated_w, allocated_h) = window.size();
        if allocated_w > 1 && allocated_h > 1 {
            win_w = allocated_w;
            win_h = allocated_h;
        }
    }
    if win_w <= 1 || win_h <= 1 {
        // 与 tauri.conf.json 的 float 窗口默认尺寸保持一致。
        win_w = 220;
        win_h = 340;
    }

    let ptr = window.upcast_ref::<gtk::Window>().to_glib_none().0;
    let namespace = CString::new("konata-float").context("layer-shell namespace 含 NUL")?;

    unsafe {
        (lib.init_for_window)(ptr);
        (lib.set_namespace)(ptr, namespace.as_ptr());
        (lib.set_layer)(ptr, LAYER_TOP);
        // 只锚定左上：位置统一由 left/top 边距表达，拖拽也只需要改这两个值。
        (lib.set_anchor)(ptr, EDGE_LEFT, 1);
        (lib.set_anchor)(ptr, EDGE_TOP, 1);
        (lib.set_anchor)(ptr, EDGE_RIGHT, 0);
        (lib.set_anchor)(ptr, EDGE_BOTTOM, 0);
        (lib.set_margin)(ptr, EDGE_LEFT, POS_X.load(Ordering::Relaxed));
        (lib.set_margin)(ptr, EDGE_TOP, POS_Y.load(Ordering::Relaxed));
        // -1 = 不预留工作区，桌宠不该把面板/其他窗口挤走。
        (lib.set_exclusive_zone)(ptr, -1);
        // OnDemand：需要输入时（点击输入框）能获得键盘焦点，但不强制抢占。
        (lib.set_keyboard_mode)(ptr, KEYBOARD_MODE_ON_DEMAND);
        (lib.set_monitor)(ptr, monitor.to_glib_none().0);
    }

    MONITOR_W.store(monitor_w, Ordering::Release);
    MONITOR_H.store(monitor_h, Ordering::Release);
    WIN_W.store(win_w, Ordering::Release);
    WIN_H.store(win_h, Ordering::Release);
    WINDOW.store(ptr as usize, Ordering::Release);
    INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

/// 把窗口放到指定角落。返回 false 表示 layer-shell 尚未初始化。
pub fn position(position: &str) -> bool {
    if !is_ready() {
        return false;
    }
    let monitor_w = MONITOR_W.load(Ordering::Acquire);
    let monitor_h = MONITOR_H.load(Ordering::Acquire);
    let win_w = WIN_W.load(Ordering::Acquire);
    let win_h = WIN_H.load(Ordering::Acquire);

    let (x, y) = match position {
        "top-left" => (MARGIN, MARGIN),
        "top-center" => ((monitor_w - win_w) / 2, MARGIN),
        "top-right" => (monitor_w - win_w - MARGIN, MARGIN),
        "bottom-left" => (MARGIN, monitor_h - win_h - MARGIN),
        "bottom-center" => ((monitor_w - win_w) / 2, monitor_h - win_h - MARGIN),
        _ => (monitor_w - win_w - MARGIN, monitor_h - win_h - MARGIN), // bottom-right
    };
    set_position(x, y);
    true
}

/// 按逻辑像素增量移动窗口。返回错误时前端应停止拖拽并提示。
pub fn move_by(dx: i32, dy: i32) -> Result<()> {
    if !is_ready() {
        bail!("layer-shell 尚未初始化，无法拖拽悬浮窗");
    }
    let x = POS_X.load(Ordering::Acquire).saturating_add(dx);
    let y = POS_Y.load(Ordering::Acquire).saturating_add(dy);
    set_position(x, y);
    Ok(())
}

/// 当前边距（仅测试/诊断用）。
pub fn current_position() -> Option<(i32, i32)> {
    if !is_ready() {
        return None;
    }
    Some((POS_X.load(Ordering::Acquire), POS_Y.load(Ordering::Acquire)))
}

fn set_position(x: i32, y: i32) {
    let max_x = (MONITOR_W.load(Ordering::Acquire) - WIN_W.load(Ordering::Acquire)).max(0);
    let max_y = (MONITOR_H.load(Ordering::Acquire) - WIN_H.load(Ordering::Acquire)).max(0);
    let x = x.clamp(0, max_x);
    let y = y.clamp(0, max_y);
    POS_X.store(x, Ordering::Release);
    POS_Y.store(y, Ordering::Release);
    schedule_update();
}

/// 合并高频拖拽请求：只保留最新位置，主循环空闲时统一写一次边距。
fn schedule_update() {
    if UPDATE_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    gtk::glib::idle_add_once(|| {
        UPDATE_SCHEDULED.store(false, Ordering::Release);
        apply_pending();
    });
}

fn apply_pending() {
    let Some(lib) = library() else {
        return;
    };
    let ptr = WINDOW.load(Ordering::Acquire) as *mut gtk::ffi::GtkWindow;
    if ptr.is_null() {
        return;
    }
    let x = POS_X.load(Ordering::Acquire);
    let y = POS_Y.load(Ordering::Acquire);
    unsafe {
        (lib.set_margin)(ptr, EDGE_LEFT, x);
        (lib.set_margin)(ptr, EDGE_TOP, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_presets_stay_inside_monitor() {
        // 纯计算逻辑通过临时设置静态状态验证；测试进程内不会真的调用 GTK。
        MONITOR_W.store(1920, Ordering::Release);
        MONITOR_H.store(1080, Ordering::Release);
        WIN_W.store(220, Ordering::Release);
        WIN_H.store(340, Ordering::Release);
        INITIALIZED.store(true, Ordering::Release);

        for preset in [
            "top-left",
            "top-center",
            "top-right",
            "bottom-left",
            "bottom-center",
            "bottom-right",
        ] {
            assert!(position(preset), "{preset}");
            let (x, y) = current_position().expect("应有位置");
            assert!((0..=1700).contains(&x), "{preset}: x={x}");
            assert!((0..=740).contains(&y), "{preset}: y={y}");
        }

        INITIALIZED.store(false, Ordering::Release);
    }
}
