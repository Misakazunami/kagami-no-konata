//! Wayland 键鼠注入：enigo 0.6 的 libei 后端（XDG RemoteDesktop + EIS）。
//!
//! 这条路径只在显式设置 `KONATA_WAYLAND_INPUT=1` 的 Wayland 会话启用。
//! `Enigo::new` 会通过 ashpd 创建 RemoteDesktop 门户会话、请求用户授权，
//! 然后用 reis 连接 `ConnectToEIS` 返回的 fd；整个过程可能阻塞到用户确认，
//! 因此放在独立线程里，绝不占用 UI 主线程或异步工具线程。
//!
//! 会话由本线程独占持有，工具层通过命令通道发送动作。会话关闭、授权失败
//! 或线程退出都会把能力写回 false，并广播明确原因。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use enigo_libei::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Serialize;
use tauri::Emitter;

use crate::platform::{set_eis_input_status, InputInjectionBackend, KeyToken, MouseButton};

const STATUS_EVENT: &str = "input-injection-status";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);

/// 同一时间只允许一个输入服务线程。
static STARTED: AtomicBool = AtomicBool::new(false);

type Reply = SyncSender<Result<(), String>>;

struct Request {
    /// 超过这个时刻就不再执行：调用方已经超时返回，不能再补一刀
    deadline: Instant,
    command: Command,
}

enum Command {
    Click {
        button: MouseButton,
        double: bool,
        reply: Reply,
    },
    TypeText {
        text: String,
        reply: Reply,
    },
    KeyCombo {
        keys: Vec<KeyToken>,
        reply: Reply,
    },
    MoveAbs {
        x: i32,
        y: i32,
        reply: Reply,
    },
}

impl Command {
    fn reply(&self) -> &Reply {
        match self {
            Command::Click { reply, .. }
            | Command::TypeText { reply, .. }
            | Command::KeyCombo { reply, .. }
            | Command::MoveAbs { reply, .. } => reply,
        }
    }
}

fn sender_slot() -> &'static Mutex<Option<Sender<Request>>> {
    static SENDER: OnceLock<Mutex<Option<Sender<Request>>>> = OnceLock::new();
    SENDER.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputStatusPayload {
    available: bool,
    backend: InputInjectionBackend,
    message: String,
}

/// 启动后台输入线程。重复调用无副作用。
pub fn start(app: tauri::AppHandle) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(err) = std::thread::Builder::new()
        .name("konata-eis-input".to_string())
        .spawn(move || run(app))
    {
        STARTED.store(false, Ordering::Release);
        eprintln!("[eis-input] 无法启动输入线程：{err}");
    }
}

fn run(app: tauri::AppHandle) {
    // 注意：这里不能在 tokio runtime 上下文中调用 Enigo::new；它的
    // libei_tokio 后端在检测到已有 runtime 时会退回 futures::executor，
    // 反而无法驱动门户的异步 IO。独立 std 线程是正确边界。
    match Enigo::new(&Settings::default()) {
        Ok(mut enigo) => {
            let (tx, rx) = mpsc::channel();
            if let Ok(mut slot) = sender_slot().lock() {
                *slot = Some(tx);
            }
            publish(
                &app,
                true,
                "Wayland EIS 键鼠输入已连接（实验）：每次 os_control 调用仍需审批；\
                 没有只读截图时鼠标点击会被拒绝。"
                    .to_string(),
            );

            while let Ok(request) = rx.recv() {
                if Instant::now() > request.deadline {
                    let _ = request
                        .command
                        .reply()
                        .send(Err("输入命令已超时，未执行".to_string()));
                    continue;
                }
                execute(&mut enigo, request.command);
            }

            if let Ok(mut slot) = sender_slot().lock() {
                *slot = None;
            }
            publish(
                &app,
                false,
                "Wayland EIS 输入服务已停止；如需恢复请重启应用并重新授权。".to_string(),
            );
        }
        Err(err) => {
            publish(
                &app,
                false,
                format!(
                    "Wayland EIS 键鼠输入不可用：{err}。可能是门户未实现 \
                     RemoteDesktop/ConnectToEIS、用户拒绝授权或会话被取消；\
                     需要完整桌面控制请改用 X11/Xorg 会话。"
                ),
            );
        }
    }
    STARTED.store(false, Ordering::Release);
}

fn execute(enigo: &mut Enigo, command: Command) {
    match command {
        Command::Click {
            button,
            double,
            reply,
        } => {
            let result = (|| -> Result<()> {
                enigo.button(map_button(button), Direction::Click)?;
                if double {
                    std::thread::sleep(DOUBLE_CLICK_GAP);
                    enigo.button(map_button(button), Direction::Click)?;
                }
                Ok(())
            })();
            let _ = reply.send(result.map_err(|err| err.to_string()));
        }
        Command::TypeText { text, reply } => {
            let result = enigo
                .text(&text)
                .map_err(|err| anyhow!("Wayland 文本输入失败：{err}"));
            let _ = reply.send(result.map_err(|err| err.to_string()));
        }
        Command::KeyCombo { keys, reply } => {
            let result = (|| -> Result<()> {
                let (main, modifiers) =
                    keys.split_last().ok_or_else(|| anyhow!("组合键不能为空"))?;
                for key in modifiers {
                    enigo.key(map_key(*key), Direction::Press)?;
                }
                enigo.key(map_key(*main), Direction::Click)?;
                for key in modifiers.iter().rev() {
                    enigo.key(map_key(*key), Direction::Release)?;
                }
                Ok(())
            })();
            let _ = reply.send(result.map_err(|err| err.to_string()));
        }
        Command::MoveAbs { x, y, reply } => {
            let result = enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|err| anyhow!("Wayland 鼠标绝对移动失败：{err}"));
            let _ = reply.send(result.map_err(|err| err.to_string()));
        }
    }
}

fn dispatch(build: impl FnOnce(Reply) -> Command) -> Result<()> {
    let sender = sender_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| {
            anyhow!(
                "Wayland EIS 输入服务尚未就绪：请确认已设置 KONATA_WAYLAND_INPUT=1、\
                 在系统弹窗中允许远程控制，并查看设置页的平台说明"
            )
        })?;

    let (tx, rx) = mpsc::sync_channel(1);
    sender
        .send(Request {
            deadline: Instant::now() + COMMAND_TIMEOUT,
            command: build(tx),
        })
        .map_err(|_| anyhow!("Wayland EIS 输入线程已停止；请重启应用重新授权"))?;

    match rx.recv_timeout(COMMAND_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(anyhow!("{err}")),
        Err(_) => Err(anyhow!(
            "Wayland EIS 输入命令超时（{} 秒）：合成器或门户没有响应",
            COMMAND_TIMEOUT.as_secs()
        )),
    }
}

pub fn click(button: MouseButton, double: bool) -> Result<()> {
    dispatch(|reply| Command::Click {
        button,
        double,
        reply,
    })
}

pub fn type_text(text: &str) -> Result<()> {
    dispatch(|reply| Command::TypeText {
        text: text.to_string(),
        reply,
    })
}

pub fn key_combo(keys: &[KeyToken]) -> Result<()> {
    dispatch(|reply| Command::KeyCombo {
        keys: keys.to_vec(),
        reply,
    })
}

pub fn move_abs(x: i32, y: i32) -> Result<()> {
    if x < 0 || y < 0 {
        return Err(anyhow!(
            "Wayland EIS 绝对坐标暂不支持负坐标（多显示器布局在主屏左侧/上方）；\
             请改用 X11/Xorg 会话或把目标显示器放在主屏右侧/下方"
        ));
    }
    dispatch(|reply| Command::MoveAbs { x, y, reply })
}

fn map_button(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

fn map_key(key: KeyToken) -> Key {
    match key {
        KeyToken::Control => Key::Control,
        KeyToken::Alt => Key::Alt,
        KeyToken::Shift => Key::Shift,
        KeyToken::Meta => Key::Meta,
        KeyToken::Return => Key::Return,
        KeyToken::Tab => Key::Tab,
        KeyToken::Escape => Key::Escape,
        KeyToken::Space => Key::Space,
        KeyToken::Backspace => Key::Backspace,
        KeyToken::Delete => Key::Delete,
        KeyToken::UpArrow => Key::UpArrow,
        KeyToken::DownArrow => Key::DownArrow,
        KeyToken::LeftArrow => Key::LeftArrow,
        KeyToken::RightArrow => Key::RightArrow,
        KeyToken::Home => Key::Home,
        KeyToken::End => Key::End,
        KeyToken::PageUp => Key::PageUp,
        KeyToken::PageDown => Key::PageDown,
        KeyToken::Unicode(ch) => Key::Unicode(ch),
    }
}

fn publish(app: &tauri::AppHandle, available: bool, message: String) {
    set_eis_input_status(available, Some(message.clone()));
    let payload = InputStatusPayload {
        available,
        backend: InputInjectionBackend::Eis,
        message: message.clone(),
    };
    if let Err(err) = app.emit(STATUS_EVENT, payload) {
        eprintln!("[eis-input] 无法广播输入状态：{err}");
    }
    eprintln!("[eis-input] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_absolute_coordinates_are_rejected_before_dispatch() {
        // 不依赖 EIS 服务：负坐标在发送命令前就必须明确失败。
        assert!(move_abs(-1, 0).is_err());
        assert!(move_abs(0, -1).is_err());
    }
}
