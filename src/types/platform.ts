/**
 * 后端 `get_platform_capabilities` 的返回类型
 *
 * 与 `src-tauri/src/platform/mod.rs` 的 `PlatformCapabilities` 一一对应。
 * 前端据此禁用/提示平台相关设置，而不是写死“Windows 有托盘、Linux 没有”。
 */

/** 当前图形会话类型（后端 `SessionType`，serde 为 snake_case） */
export type SessionType = "x11" | "wayland" | "native" | "unknown";

/**
 * 全局快捷键后端（后端 `GlobalShortcutBackend`，serde 为 snake_case）
 *
 * - `native`：Windows / X11，由前端 `tauri-plugin-global-shortcut` 注册；
 * - `portal`：Wayland XDG 门户，由后端异步绑定，前端不能再注册插件；
 * - `none`：当前会话没有可用后端。
 */
export type GlobalShortcutBackend = "native" | "portal" | "none";

/**
 * 悬浮窗定位后端（后端 `WindowPositioningBackend`，serde 为 snake_case）
 *
 * - `native`：Windows / X11，使用 set_position + 置顶标志；
 * - `layer_shell`：Wayland wlr-layer-shell，初始化成功后支持自由拖拽；
 * - `none`：当前会话没有可用的绝对定位后端。
 */
export type WindowPositioningBackend = "native" | "layer_shell" | "none";

/**
 * 键鼠注入后端（后端 `InputInjectionBackend`，serde 为 snake_case）
 *
 * - `native`：Windows / X11，使用现有 enigo；
 * - `eis`：Wayland XDG RemoteDesktop + libei/EIS，连接成功前不可用；
 * - `none`：当前会话没有可用的注入后端。
 */
export type InputInjectionBackend = "native" | "eis" | "none";

export interface PlatformCapabilities {
  /** `windows` / `linux` / `macos` / ... */
  os: string;
  sessionType: SessionType;
  /** Linux AppIndicator 通常不触发 TrayIconEvent */
  trayClickEvents: boolean;
  /** 是否提供托盘菜单（即使没有左键事件） */
  trayMenu: boolean;
  /** 托盘是否可靠可见（GNOME 默认没有 AppIndicator 扩展） */
  trayReliable: boolean;
  windowPositioning: boolean;
  /** 悬浮窗定位后端；`layer_shell` 时由运行时初始化结果决定是否可用 */
  windowPositioningBackend: WindowPositioningBackend;
  alwaysOnTop: boolean;
  globalShortcut: boolean;
  /** 全局快捷键后端；`portal` 时由后端门户直接处理按键 */
  globalShortcutBackend: GlobalShortcutBackend;
  screenCapture: boolean;
  /** 截图可用但只读（Wayland 实验模式：不能注入键鼠） */
  screenCaptureOnly: boolean;
  inputInjection: boolean;
  /** 键鼠注入后端；`eis` 时由异步门户连接结果决定是否可用 */
  inputInjectionBackend: InputInjectionBackend;
  activateWindow: boolean;
  /** 截图 + 键鼠 + 窗口激活整体是否可用 */
  osControl: boolean;
  /** 新安装时“关闭主窗口”的默认行为 */
  closeActionDefault: string;
  /** 人类可读说明（Wayland/无显示等） */
  notes: string[];
}
