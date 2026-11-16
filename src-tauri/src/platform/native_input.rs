//! Windows / X11 的原生键鼠注入（enigo 0.2）。
//!
//! Wayland 的 EIS 路径在 `linux/input.rs`，两者通过 `platform` 的中立类型
//! 隔离：工具层不再直接依赖 enigo 的具体版本。

use std::time::Duration;

use anyhow::{anyhow, Result};
use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};

use super::{KeyToken, MouseButton};

/// 双击两下之间的间隔
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);

fn open() -> Result<Enigo> {
    Enigo::new(&Settings::default()).map_err(|err| anyhow!("初始化键鼠设备失败：{err}"))
}

pub fn click(button: MouseButton, double: bool) -> Result<()> {
    let mut enigo = open()?;
    enigo.button(map_button(button), Direction::Click)?;
    if double {
        std::thread::sleep(DOUBLE_CLICK_GAP);
        enigo.button(map_button(button), Direction::Click)?;
    }
    Ok(())
}

pub fn type_text(text: &str) -> Result<()> {
    let mut enigo = open()?;
    enigo.text(text)?;
    Ok(())
}

pub fn key_combo(keys: &[KeyToken]) -> Result<()> {
    let (main, modifiers) = keys.split_last().ok_or_else(|| anyhow!("组合键不能为空"))?;
    let mut enigo = open()?;
    for key in modifiers {
        enigo.key(map_key(*key), Direction::Press)?;
    }
    enigo.key(map_key(*main), Direction::Click)?;
    for key in modifiers.iter().rev() {
        enigo.key(map_key(*key), Direction::Release)?;
    }
    Ok(())
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
