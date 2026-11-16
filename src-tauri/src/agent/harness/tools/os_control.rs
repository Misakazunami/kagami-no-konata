use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::harness::screen;
use crate::agent::harness::tools::args;
use crate::agent::harness::traits::{Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput};
use crate::platform::{self, InputInjectionBackend, KeyToken, MouseButton};

/// 移动鼠标后留给系统更新位置的时间：`SetCursorPos` 之后立刻发点击，
/// 偶尔会点在旧位置上
const CURSOR_SETTLE: Duration = Duration::from_millis(40);
/// 切前台后等系统真正画出来
const ACTIVATE_SETTLE: Duration = Duration::from_millis(120);

/// 可用动作清单（提示词用）
///
/// Wayland EIS 只支持输入，不支持窗口激活；提示词必须与真实能力一致，
/// 否则模型会调用一个必然报错的 `activate_window`。
pub fn action_list() -> &'static str {
    if platform::capabilities().activate_window {
        "mouse_click / keyboard_type / key_press / activate_window"
    } else {
        "mouse_click / keyboard_type / key_press"
    }
}

pub struct OsControlTool;

#[async_trait]
impl Tool for OsControlTool {
    fn descriptor(&self) -> ToolDescriptor {
        let actions = if platform::capabilities().activate_window {
            json!([
                "mouse_click",
                "keyboard_type",
                "key_press",
                "activate_window"
            ])
        } else {
            json!(["mouse_click", "keyboard_type", "key_press"])
        };
        let description = if platform::capabilities().os_control {
            "直接操作这台电脑：鼠标点击、键盘输入、组合键、把窗口切到前台。\
             每一轮系统都会自动注入当前显示器的截图，**不需要**自己调用截图。\
             坐标约定：mouse_click 的 x/y 直接使用截图上看到的像素坐标，工具会按截图缩放比换算成屏幕坐标。\
             注意：被最小化或不在前台的窗口不会出现在截图里、也收不到键盘输入，\
             操作它们之前必须先用 activate_window 切到前台。"
                .to_string()
        } else if platform::capabilities().screen_capture {
            "通过 Wayland EIS 直接输入：鼠标点击、键盘输入、组合键。\
             需要看屏幕时调用 look_at_screen；**没有可用截图时鼠标点击会被拒绝**。\
             坐标约定：mouse_click 的 x/y 直接使用截图上看到的像素坐标。\
             当前会话不能通过本工具切换窗口，也不要声称已经激活窗口。"
                .to_string()
        } else {
            "通过 Wayland EIS 直接输入：**当前没有可用的只读截图**，鼠标点击会被拒绝；\
             只能使用 keyboard_type 和 key_press。\
             当前会话不能通过本工具切换窗口，也不要声称已经激活窗口。"
                .to_string()
        };
        ToolDescriptor::new(
            "os_control",
            "操作系统控制",
            description,
            Permission::OsControl,
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": actions,
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
        platform::current_capabilities().input_injection
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

                // P3.4 安全策略：没有可用的屏幕截图就不允许盲点。
                if !platform::current_capabilities().screen_capture {
                    return Err(anyhow!(
                        "Wayland 下鼠标点击需要先启用只读截图（KONATA_WAYLAND_SCREENSHOT=1）\
                         并成功截过屏；没有画面时不允许盲点。键盘输入仍可使用。"
                    ));
                }

                // 截图坐标 -> 屏幕物理坐标：换算失败（越界/还没截过屏）必须
                // 明确回灌，而不是"点一下试试"
                let (screen_x, screen_y) = screen::image_to_screen(x, y)?;
                platform::input_move_abs(screen_x, screen_y)?;
                tokio::time::sleep(CURSOR_SETTLE).await;
                platform::input_click(button, double)?;

                let text = format!(
                    "已在截图像素 ({x}, {y}) 处{}点击（屏幕坐标 ({screen_x}, {screen_y})）",
                    if double { "双击" } else { "单击" }
                );
                Ok(ToolOutput::text(text.clone()).with_preview(text))
            }
            "keyboard_type" => {
                let text = args::required_str(&args, "text")?;
                platform::input_type_text(&text)?;
                let summary = format!("已输入 {} 个字符", text.chars().count());
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            "key_press" => {
                let spec = args::required_str(&args, "keys")?;
                let keys = parse_combo(&spec)?;
                platform::input_key_combo(&keys)?;
                let summary = format!("已按键：{spec}");
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            "activate_window" => {
                if !platform::capabilities().activate_window {
                    return Err(anyhow!(
                        "当前会话不支持通过 os_control 切换窗口；Wayland 下请让用户手动切换，\
                         不要声称已经激活"
                    ));
                }
                let title = args::required_str(&args, "title")?;
                let summary = platform::activate_window(&title, ACTIVATE_SETTLE)?;
                Ok(ToolOutput::text(summary.clone()).with_preview(summary))
            }
            other => Err(anyhow!(
                "不支持的动作「{other}」。可用动作：{}；\
                 截图由系统每轮自动注入，不需要调用",
                action_list()
            )),
        }
    }
}

fn parse_button(raw: Option<&str>) -> Result<MouseButton> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("left") => Ok(MouseButton::Left),
        Some("right") => Ok(MouseButton::Right),
        Some("middle") => Ok(MouseButton::Middle),
        Some(other) => Err(anyhow!(
            "button 只能是 left / right / middle，收到「{other}」"
        )),
    }
}

/// `alt+tab` / `ctrl+shift+t` / `enter` -> 按键序列
///
/// 只支持单字符作为主键（字母、数字、符号），功能键走固定别名表；
/// 认不出来的键直接报错，避免"悄悄按了个无关的键"。
fn parse_combo(spec: &str) -> Result<Vec<KeyToken>> {
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

fn key_from_token(token: &str) -> Result<KeyToken> {
    let key = match token {
        "ctrl" | "control" => KeyToken::Control,
        "alt" | "option" => KeyToken::Alt,
        "shift" => KeyToken::Shift,
        "win" | "meta" | "super" | "cmd" | "command" => KeyToken::Meta,
        "enter" | "return" => KeyToken::Return,
        "tab" => KeyToken::Tab,
        "esc" | "escape" => KeyToken::Escape,
        "space" => KeyToken::Space,
        "backspace" => KeyToken::Backspace,
        "delete" | "del" => KeyToken::Delete,
        "up" | "arrowup" => KeyToken::UpArrow,
        "down" | "arrowdown" => KeyToken::DownArrow,
        "left" | "arrowleft" => KeyToken::LeftArrow,
        "right" | "arrowright" => KeyToken::RightArrow,
        "home" => KeyToken::Home,
        "end" => KeyToken::End,
        "pageup" | "pgup" => KeyToken::PageUp,
        "pagedown" | "pgdn" => KeyToken::PageDown,
        _ => {
            let mut chars = token.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => KeyToken::Unicode(ch),
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

/// 当前环境为什么不能用桌面控制；可用时返回 `None`。
///
/// 注册表用它填 `ToolInfo.unavailable_reason`，调用入口用它兜底拒绝。
fn os_control_unavailable_reason() -> Option<String> {
    let capabilities = crate::platform::current_capabilities();
    if capabilities.input_injection {
        return None;
    }

    Some(match capabilities.session_type {
        crate::platform::SessionType::Wayland => {
            if capabilities.input_injection_backend == InputInjectionBackend::Eis {
                "Wayland EIS 键鼠输入尚未就绪：端口服务可能仍在等待授权、被用户拒绝或已断开。\
                 请查看设置页平台说明；需要完整桌面控制请改用 X11/Xorg 会话"
                    .to_string()
            } else {
                "Wayland 键鼠注入默认关闭；设置 KONATA_WAYLAND_INPUT=1 并重启后，\
                 应用会通过 XDG RemoteDesktop + libei/EIS 请求授权（实验）。\
                 鼠标点击还要求先启用只读截图；窗口激活始终不可用"
                    .to_string()
            }
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
            vec![KeyToken::Control, KeyToken::Shift, KeyToken::Unicode('t')]
        );
        assert_eq!(parse_combo("enter").unwrap(), vec![KeyToken::Return]);
        assert_eq!(
            parse_combo("win+d").unwrap(),
            vec![KeyToken::Meta, KeyToken::Unicode('d')]
        );
        // 大小写与空白都要容错
        assert_eq!(
            parse_combo(" Alt + Tab ").unwrap(),
            vec![KeyToken::Alt, KeyToken::Tab]
        );
    }

    #[test]
    fn parse_combo_rejects_unknown_keys() {
        assert!(parse_combo("ctrl+").is_err());
        assert!(parse_combo("ctrl+hyper+z").is_err());
    }

    #[test]
    fn parse_button_defaults_to_left() {
        assert_eq!(parse_button(None).unwrap(), MouseButton::Left);
        assert_eq!(parse_button(Some("")).unwrap(), MouseButton::Left);
        assert_eq!(parse_button(Some("Right")).unwrap(), MouseButton::Right);
        assert!(parse_button(Some("thumb")).is_err());
    }
}
