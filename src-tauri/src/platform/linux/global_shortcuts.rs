//! Wayland 全局快捷键：`org.freedesktop.portal.GlobalShortcuts` 后台服务。
//!
//! 只有显式设置 `KONATA_WAYLAND_GLOBAL_SHORTCUTS=1`、当前是 Wayland 会话且
//! 存在 DBus 会话总线时，`platform::linux::start_global_shortcut_service` 才会
//! 调用本模块。
//!
//! 设计约束：
//! - 门户绑定是异步的，且首次通常需要用户在系统弹窗中确认；**绑定成功前不能**
//!   把 `PlatformCapabilities::global_shortcut` 标为可用。
//! - 门户可能被用户取消、被桌面环境拒绝，或会话随时关闭。任何失败都要写回
//!   能力状态并通知前端，不能静默假装成功。
//! - 绑定成功后由本模块直接切换主窗口，不再让前端 `tauri-plugin-global-shortcut`
//!   重复注册（Wayland 下该插件也不可用）。

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use ashpd::zbus;
use futures::{pin_mut, StreamExt};
use serde::Serialize;
use tauri::Emitter;

use crate::platform::{set_portal_shortcut_status, GlobalShortcutBackend};

/// 门户中使用的快捷键 ID（与托盘菜单的 `toggle-main` 保持一致）
const SHORTCUT_ID: &str = "toggle-main";
/// XDG shortcuts 规范格式：修饰键大写、keysym 小写
const SHORTCUT_TRIGGER: &str = "CTRL+SHIFT+k";
const SHORTCUT_LABEL: &str = "Ctrl+Shift+K";
const SHORTCUT_DESCRIPTION: &str = "显示或隐藏 Kagami no Konata 主窗口";

/// 发给前端的门户状态事件名（与 `src/types/events.ts` 的 `PLATFORM_EVENT` 对应）
const STATUS_EVENT: &str = "global-shortcut-status";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutStatusPayload {
    available: bool,
    backend: GlobalShortcutBackend,
    shortcut: &'static str,
    /// 桌面环境实际分配的触发方式（用户可能在系统设置里改过）
    trigger: Option<String>,
    message: String,
}

/// 启动后台线程。线程内部自建 current-thread tokio runtime，避免依赖 Tauri
/// 主线程的 async runtime，也避免在 GTK 主循环里阻塞等待门户弹窗。
pub fn start(app: tauri::AppHandle) {
    if let Err(err) = std::thread::Builder::new()
        .name("konata-portal-shortcuts".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    publish(
                        &app,
                        false,
                        None,
                        format!("无法创建 Wayland 全局快捷键运行时：{err}"),
                    );
                    return;
                }
            };
            runtime.block_on(run(app));
        })
    {
        eprintln!("[portal-shortcuts] 无法启动后台线程：{err}");
    }
}

async fn run(app: tauri::AppHandle) {
    if let Err(err) = run_inner(&app).await {
        publish(
            &app,
            false,
            None,
            format!(
                "Wayland 全局快捷键不可用：{err:#}。可以继续使用托盘菜单；\
                 需要完整快捷键支持请改用 X11/Xorg 会话。"
            ),
        );
    }
}

async fn run_inner(app: &tauri::AppHandle) -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("无法连接 DBus 会话总线")?;

    // 非沙箱应用需要先向 host registry 声明身份；xdg-desktop-portal 1.21+
    // 的 GlobalShortcuts 会拒绝没有 app id 的 CreateSession。
    //
    // 注册失败不立即放弃：旧版门户可能不需要注册，KDE 等后端也可能接受
    // 其他身份来源。真正的结论以 CreateSession/BindShortcuts 为准。
    register_host_app(app, &connection).await;

    let portal = GlobalShortcuts::with_connection(connection).await.context(
        "当前桌面没有实现 org.freedesktop.portal.GlobalShortcuts，\
             或 xdg-desktop-portal 服务未运行",
    )?;

    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .context("创建全局快捷键门户会话失败（桌面环境可能不支持该门户）")?;

    // 先订阅再绑定：绑定完成到开始监听之间可能已经有 Activated/Closed。
    let activated = portal
        .receive_activated()
        .await
        .context("订阅门户 Activated 信号失败")?;
    let closed = session
        .receive_closed()
        .await
        .context("订阅门户 Session.Closed 信号失败")?;
    pin_mut!(activated, closed);

    let request = portal
        .bind_shortcuts(
            &session,
            &[NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION)
                .preferred_trigger(SHORTCUT_TRIGGER)],
            None,
            BindShortcutsOptions::default(),
        )
        .await
        .context("请求绑定全局快捷键失败")?;

    // `bind_shortcuts` 返回时门户已经给出 Response，因此这里同步取值是安全的。
    let response = request
        .response()
        .context("门户没有确认快捷键绑定（可能被用户取消或被桌面拒绝）")?;
    let bound = response
        .shortcuts()
        .iter()
        .find(|shortcut| shortcut.id() == SHORTCUT_ID)
        .ok_or_else(|| anyhow!("门户没有返回预期的快捷键 ID `{SHORTCUT_ID}`"))?;

    let trigger = bound.trigger_description().trim().to_string();
    // 部分后端不返回触发描述；此时退回应用请求的按键，避免设置页显示空值。
    let trigger = if trigger.is_empty() {
        SHORTCUT_LABEL.to_string()
    } else {
        trigger
    };
    publish(
        app,
        true,
        Some(trigger.clone()),
        format!(
            "Wayland 全局快捷键已通过系统门户绑定：{trigger}（实际按键由桌面环境管理，\
             可在系统快捷键设置中修改）"
        ),
    );

    loop {
        tokio::select! {
            event = activated.next() => {
                match event {
                    Some(event) => {
                        if event.shortcut_id() != SHORTCUT_ID {
                            continue;
                        }
                        if let Err(err) =
                            crate::commands::window::toggle_main_window_now(app)
                        {
                            eprintln!("[portal-shortcuts] 切换主窗口失败：{err}");
                        }
                    }
                    None => {
                        publish(
                            app,
                            false,
                            None,
                            "系统门户的 Activated 信号流已结束，全局快捷键不再可用。".to_string(),
                        );
                        return Ok(());
                    }
                }
            }
            event = closed.next() => {
                match event {
                    Some(_) => {
                        publish(
                            app,
                            false,
                            None,
                            "系统门户已关闭全局快捷键会话；如需恢复请重启应用并重新确认。"
                                .to_string(),
                        );
                    }
                    None => {
                        publish(
                            app,
                            false,
                            None,
                            "系统门户会话已断开，全局快捷键不再可用。".to_string(),
                        );
                    }
                }
                return Ok(());
            }
        }
    }
}

/// best-effort 注册宿主应用 ID。
///
/// 这里直接用原始 zbus 调用，而不是 `ashpd::register_host_app`：ashpd 的
/// `AppID` 校验不允许中间段包含 `-`，而 Tauri 的 identifier
/// `com.konata-mirror.main` 是合法 DBus/桌面文件名，GNOME 也能通过
/// `com.konata-mirror.main.desktop` 解析它。
async fn register_host_app(app: &tauri::AppHandle, connection: &zbus::Connection) {
    let app_id = app.config().identifier.clone();
    let result = async {
        let proxy = zbus::Proxy::new(
            connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.host.portal.Registry",
        )
        .await?;
        let options: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
        proxy
            .call_method("Register", &(app_id.as_str(), options))
            .await?;
        Ok::<(), zbus::Error>(())
    }
    .await;

    match result {
        Ok(()) => eprintln!("[portal-shortcuts] 已注册宿主应用 ID：{app_id}"),
        Err(err) => eprintln!(
            "[portal-shortcuts] 宿主应用注册失败（继续尝试门户绑定）：{err}；\
             若桌面为 GNOME 50+/xdg-desktop-portal 1.21+，请确认已安装 \
             /usr/share/applications/{app_id}.desktop"
        ),
    }
}

/// 写回动态能力并广播给前端。`trigger` 只在绑定成功时才有值。
fn publish(app: &tauri::AppHandle, available: bool, trigger: Option<String>, message: String) {
    set_portal_shortcut_status(available, Some(message.clone()));

    let payload = ShortcutStatusPayload {
        available,
        backend: GlobalShortcutBackend::Portal,
        shortcut: SHORTCUT_LABEL,
        trigger,
        message: message.clone(),
    };

    if let Err(err) = app.emit(STATUS_EVENT, payload) {
        eprintln!("[portal-shortcuts] 无法广播全局快捷键状态：{err}");
    }
    eprintln!("[portal-shortcuts] {message}");
}
