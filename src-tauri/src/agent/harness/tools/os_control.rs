use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde_json::{json, Value};

use crate::agent::harness::screen;
use crate::agent::harness::tools::args;
use crate::agent::harness::traits::{Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput};

/// 移动鼠标后留给系统更新位置的时间：`SetCursorPos` 之后立刻发点击，
/// 偶尔会点在旧位置上
const CURSOR_SETTLE: Duration = Duration::from_millis(40);
/// 双击两下之间的间隔
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);
/// 切前台后等系统把窗口真正画出来
const ACTIVATE_SETTLE: Duration = Duration::from_millis(120);

/// 可用动作清单（提示词用）
///
/// 与 [`OsControlTool::descriptor`] 里 `action` 的 enum 必须同步：模型判断
/// "有没有桌面工具"时，提示词里的这句往往比函数 schema 更早被它读到。
pub const ACTION_LIST: &str = "mouse_click / keyboard_type / key_press / activate_window";

pub struct OsControlTool;

#[async_trait]
impl Tool for OsControlTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "os_control",
            "操作系统控制",
            "直接操作这台电脑：鼠标点击、键盘输入、组合键、把窗口切到前台。\
             每一轮系统都会自动注入当前显示器的截图，**不需要**自己调用截图。\
             坐标约定：mouse_click 的 x/y 直接使用截图上看到的像素坐标，工具会按截图缩放比换算成屏幕坐标。\
             注意：被最小化或不在前台的窗口不会出现在截图里、也收不到键盘输入，\
             操作它们之前必须先用 activate_window 切到前台。",
            Permission::OsControl,
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["mouse_click", "keyboard_type", "key_press", "activate_window"],
                        "description": "要执行的动作"
                    },
                    "x": { "type": "integer", "description": "mouse_click：截图上的像素坐标 X" },
                    "y": { "type": "integer", "description": "mouse_click：截图上的像素坐标 Y" },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "mouse_click：按键，默认 left"
                    },
                    "double": { "type": "boolean", "description": "mouse_click：是否双击，默认 false" },
                    "text": { "type": "string", "description": "keyboard_type：要输入的文本" },
                    "keys": { "type": "string", "description": "key_press：组合键，例如 alt+tab、ctrl+s、enter、win+d" },
                    "title": { "type": "string", "description": "activate_window：目标窗口标题（包含匹配，不区分大小写），例如 QQ" }
                },
                "required": ["action"]
            }),
        )
    }

    fn enabled(&self) -> bool {
        crate::platform::capabilities().os_control
    }

    fn unavailable_reason(&self) -> Option<String> {
        os_control_unavailable_reason()
    }

    async fn call(&self, args: Value, _cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // 注册表已经按平台能力过滤；这里再兜一层，防止旧 schema/直接调用
        // 在 Wayland 上产生“看起来执行了”的假象。
        if let Some(reason) = os_control_unavailable_reason() {
            return Err(anyhow!(reason));
        }

        let action = args::required_str(&args, "action")?;

        match action.as_str() {
            "mouse_click" => {
                let x = args
                    .get("x")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("mouse_click 需要整数参数 x（截图上的像素坐标）"))?;
                let y = args
                    .get("y")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("mouse_click 需要整数参数 y（截图上的像素坐标）"))?;
                let button = parse_button(args::optional_str(&args, "button").as_deref())?;
                let double = args
                    .get("double")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                // 截图坐标 -> 屏幕物理坐标：换算失败（越界/还没截过屏）必须
                // 明确回灌，而不是"点一下试试"
                let (screen_x, screen_y) = screen::image_to_screen(x, y)?;
                move_cursor(screen_x, screen_y)?;
                tokio::time::sleep(CURSOR_SETTLE).await;
                click(button, double)?;

                let text = format!(
                    "已在截图像素 ({x}, {y}) 处{}点击（屏幕坐标 ({screen_x}, {screen_y})）",
                    if double { "双击" } else { "单击" }
                );
                Ok(ToolOutput::text(text.clone()).with_preview(text))
            }
            "keyboard_type" => {
                let text = args::required_str(&args, "text")?;
                let mut enigo = open_enigo()?;
                enigo.text(&text)?;
                let summary = format!("已输入 {} 个字符", text.chars().count());
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            "key_press" => {
                let spec = args::required_str(&args, "keys")?;
                let keys = parse_combo(&spec)?;
                press_combo(&keys)?;
                let summary = format!("已按键：{spec}");
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            "activate_window" => {
                let title = args::required_str(&args, "title")?;
                let summary = activate_window(&title)?;
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            other => Err(anyhow!(
                "不支持的动作「{other}」。可用动作：mouse_click / keyboard_type / key_press / activate_window；\
                 截图由系统每轮自动注入，不需要调用"
            )),
        }
    }
}

fn open_enigo() -> Result<Enigo> {
    Enigo::new(&Settings::default()).map_err(|e| anyhow!("初始化键鼠设备失败：{e}"))
}

fn parse_button(raw: Option<&str>) -> Result<Button> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("left") => Ok(Button::Left),
        Some("right") => Ok(Button::Right),
        Some("middle") => Ok(Button::Middle),
        Some(other) => Err(anyhow!(
            "button 只能是 left / right / middle，收到「{other}」"
        )),
    }
}

fn click(button: Button, double: bool) -> Result<()> {
    let mut enigo = open_enigo()?;
    enigo.button(button, Direction::Click)?;
    if double {
        std::thread::sleep(DOUBLE_CLICK_GAP);
        enigo.button(button, Direction::Click)?;
    }
    Ok(())
}

/// 按顺序按下修饰键、点一下主键、再逆序松开
fn press_combo(keys: &[Key]) -> Result<()> {
    let (main, modifiers) = keys.split_last().ok_or_else(|| anyhow!("组合键不能为空"))?;
    let mut enigo = open_enigo()?;
    for key in modifiers {
        enigo.key(*key, Direction::Press)?;
    }
    enigo.key(*main, Direction::Click)?;
    for key in modifiers.iter().rev() {
        enigo.key(*key, Direction::Release)?;
    }
    Ok(())
}

/// `alt+tab` / `ctrl+shift+t` / `enter` -> 按键序列
///
/// 只支持单字符作为主键（字母、数字、符号），功能键走固定别名表；
/// 认不出来的键直接报错，避免"悄悄按了个无关的键"。
fn parse_combo(spec: &str) -> Result<Vec<Key>> {
    let mut keys = Vec::new();
    for raw in spec.split('+') {
        let token = raw.trim().to_ascii_lowercase();
        if token.is_empty() {
            return Err(anyhow!("组合键「{spec}」里有空段"));
        }
        keys.push(key_from_token(&token)?);
    }
    if keys.is_empty() {
        return Err(anyhow!("组合键不能为空"));
    }
    Ok(keys)
}

fn key_from_token(token: &str) -> Result<Key> {
    let key = match token {
        "ctrl" | "control" => Key::Control,
        "alt" | "option" => Key::Alt,
        "shift" => Key::Shift,
        "win" | "meta" | "super" | "cmd" | "command" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" | "arrowup" => Key::UpArrow,
        "down" | "arrowdown" => Key::DownArrow,
        "left" | "arrowleft" => Key::LeftArrow,
        "right" | "arrowright" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        _ => {
            let mut chars = token.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Key::Unicode(ch),
                _ => {
                    return Err(anyhow!(
                        "无法识别的按键「{token}」：功能键请用别名（enter/tab/esc/up/down/...），字母数字直接写"
                    ))
                }
            }
        }
    };
    Ok(key)
}

fn move_cursor(x: i32, y: i32) -> Result<()> {
    crate::platform::move_cursor(x, y)
}

fn activate_window(title: &str) -> Result<String> {
    crate::platform::activate_window(title, ACTIVATE_SETTLE)
}

/// 当前环境为什么不能用桌面控制；可用时返回 `None`。
///
/// 注册表用它填 `ToolInfo.unavailable_reason`，调用入口用它兜底拒绝。
fn os_control_unavailable_reason() -> Option<String> {
    let capabilities = crate::platform::capabilities();
    if capabilities.os_control {
        return None;
    }

    Some(match capabilities.session_type {
        crate::platform::SessionType::Wayland => {
            "Wayland 会话不提供通用的键鼠注入/窗口激活接口，桌面控制已禁用；\
             只读截图可设置 KONATA_WAYLAND_SCREENSHOT=1 单独启用（不能控制键鼠）；\
             需要完整桌面能力请注销并选择 X11/Xorg 会话"
                .to_string()
        }
        crate::platform::SessionType::Unknown => {
            "未检测到可用的图形显示（DISPLAY/WAYLAND_DISPLAY），桌面控制已禁用".to_string()
        }
        _ => "当前平台不支持桌面控制".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_combo_reads_modifiers_and_main_key() {
        assert_eq!(
            parse_combo("ctrl+shift+t").unwrap(),
            vec![Key::Control, Key::Shift, Key::Unicode('t')]
        );
        assert_eq!(parse_combo("enter").unwrap(), vec![Key::Return]);
        assert_eq!(
            parse_combo("win+d").unwrap(),
            vec![Key::Meta, Key::Unicode('d')]
        );
        // 大小写与空白都要容错
        assert_eq!(
            parse_combo(" Alt + Tab ").unwrap(),
            vec![Key::Alt, Key::Tab]
        );
    }

    #[test]
    fn parse_combo_rejects_unknown_keys() {
        assert!(parse_combo("ctrl+").is_err());
        assert!(parse_combo("ctrl+hyper+z").is_err());
    }

    #[test]
    fn parse_button_defaults_to_left() {
        assert_eq!(parse_button(None).unwrap(), Button::Left);
        assert_eq!(parse_button(Some("")).unwrap(), Button::Left);
        assert_eq!(parse_button(Some("Right")).unwrap(), Button::Right);
        assert!(parse_button(Some("thumb")).is_err());
    }
}
