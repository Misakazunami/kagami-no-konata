use serde::{Deserialize, Serialize};

/// 内置默认人格 ID（后端统一引用，避免魔法字符串散落）
pub const DEFAULT_PERSONA_ID: &str = "konata-default";

/// 没有配置戳一戳台词时的**中性**兜底
///
/// 刻意不含任何具体角色名：换成人格 B 之后，兜底台词也必须仍然成立，
/// 否则桌宠会说出上一个角色的台词。
pub const FALLBACK_POKE_LINES: &[&str] = &[
    "呀！别戳啦～",
    "呜呜…在忙着呢",
    "嘻嘻，好痒～",
    "诶？怎么啦？",
    "唔…被发现了",
    "戳一下是想我了吗？",
    "哇！突然袭击！",
    "…在想事情啦，别打扰",
    "嗯？有什么事吗？",
    "好啦好啦，看过来啦～",
];

pub const FALLBACK_POKE_ANGRY_LINE: &str = "不要再戳啦……要生气啦！";

/// 人格配置（对应 YAML 文件）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub system_prompt: String,
    #[serde(default)]
    pub personality: Personality,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// UI 用短名（侧栏按钮、空状态文案等）。缺省时从 `name` 派生
    #[serde(default)]
    pub short_name: Option<String>,
    /// 戳一戳的预置台词（随机抽取）。缺省时回退到中性兜底
    #[serde(default)]
    pub poke_lines: Vec<String>,
    /// 连续戳一戳到"生气"时的台词
    #[serde(default)]
    pub poke_angry_line: Option<String>,
}

impl PersonaConfig {
    /// 按钮/空状态用的短名
    ///
    /// 优先级：显式 `short_name` → `name` 去掉括号注释（"此方（こなた）" → "此方"）→ `name`
    pub fn display_short_name(&self) -> String {
        if let Some(short) = self.short_name.as_deref() {
            let short = short.trim();
            if !short.is_empty() {
                return short.to_string();
            }
        }
        let name = self.name.trim();
        let cut = name
            .char_indices()
            .find(|(_, c)| *c == '（' || *c == '(')
            .map(|(i, _)| i);
        match cut {
            Some(index) if index > 0 => name[..index].trim().to_string(),
            _ => name.to_string(),
        }
    }

    /// 实际生效的戳一戳台词（过滤空串；为空则用中性兜底）
    pub fn effective_poke_lines(&self) -> Vec<String> {
        let lines: Vec<String> = self
            .poke_lines
            .iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            FALLBACK_POKE_LINES.iter().map(|s| s.to_string()).collect()
        } else {
            lines
        }
    }

    /// 实际生效的"生气"台词
    pub fn effective_poke_angry_line(&self) -> String {
        match self.poke_angry_line.as_deref() {
            Some(line) if !line.trim().is_empty() => line.trim().to_string(),
            _ => FALLBACK_POKE_ANGRY_LINE.to_string(),
        }
    }
}

fn default_version() -> String {
    "1.0.0".to_string()
}

/// 性格特征
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Personality {
    #[serde(default)]
    pub traits: Vec<String>,
    #[serde(default)]
    pub speech_style: String,
    #[serde(default)]
    pub interests: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> PersonaConfig {
        serde_yaml::from_str(yaml).expect("parse yaml")
    }

    /// 精简 YAML（缺省 personality / constraints / version）也应可解析
    #[test]
    fn minimal_yaml_parses_with_defaults() {
        let p = parse(
            r#"
id: "test-char"
name: "测试角色"
system_prompt: |
  你是测试角色。
"#,
        );
        assert_eq!(p.id, "test-char");
        assert_eq!(p.version, "1.0.0");
        assert!(p.personality.traits.is_empty());
        assert!(p.constraints.is_empty());
        // 新增字段全部可选：旧人格文件不会因此解析失败
        assert!(p.short_name.is_none());
        assert!(p.poke_lines.is_empty());
        assert!(p.poke_angry_line.is_none());
    }

    #[test]
    fn poke_visuals_parse_from_yaml() {
        let p = parse(
            r#"
id: "sakura"
name: "小樱（さくら）"
short_name: "小樱"
system_prompt: |
  你是小樱。
poke_lines:
  - "呀！"
  - "  留白会被裁掉  "
poke_angry_line: "再戳就封印你哦！"
"#,
        );
        assert_eq!(p.display_short_name(), "小樱");
        assert_eq!(p.effective_poke_lines(), vec!["呀！", "留白会被裁掉"]);
        assert_eq!(p.effective_poke_angry_line(), "再戳就封印你哦！");
    }

    #[test]
    fn short_name_derives_from_name_when_absent() {
        let mut p = parse("id: a\nname: \"此方（こなた）\"\nsystem_prompt: x\n");
        assert_eq!(p.display_short_name(), "此方");

        p.name = "小樱 (Sakura)".to_string();
        assert_eq!(p.display_short_name(), "小樱");

        p.name = "Solo".to_string();
        assert_eq!(p.display_short_name(), "Solo");

        // 显式短名为空串时也走派生，不会被空串覆盖
        p.name = "此方（こなた）".to_string();
        p.short_name = Some("   ".to_string());
        assert_eq!(p.display_short_name(), "此方");
    }

    /// 关键不变式：没有配置台词的任意人格，兜底台词里绝不能出现别的角色名
    #[test]
    fn fallback_poke_lines_are_persona_agnostic() {
        let p = parse("id: other\nname: 别的角色\nsystem_prompt: x\n");
        let lines = p.effective_poke_lines();
        assert_eq!(lines.len(), FALLBACK_POKE_LINES.len());
        for line in &lines {
            for name in ["此方", "こなた", "泉此方"] {
                assert!(!line.contains(name), "兜底台词不得含角色名：{line}");
            }
        }
        assert!(!p.effective_poke_angry_line().contains("此方"));
    }

    #[test]
    fn blank_poke_lines_fall_back() {
        let mut p = parse("id: a\nname: a\nsystem_prompt: x\n");
        p.poke_lines = vec!["   ".to_string(), "".to_string()];
        assert_eq!(p.effective_poke_lines().len(), FALLBACK_POKE_LINES.len());
        p.poke_angry_line = Some("  ".to_string());
        assert_eq!(p.effective_poke_angry_line(), FALLBACK_POKE_ANGRY_LINE);
    }

    #[test]
    fn builtin_persona_declares_its_own_visuals() {
        let p = parse(include_str!("../../personas/default.yaml"));
        assert_eq!(p.id, DEFAULT_PERSONA_ID);
        assert_eq!(p.display_short_name(), "此方");
        assert!(p.poke_lines.len() >= 10, "内置人格应当自带成套台词");
        assert!(p.poke_angry_line.is_some());
        assert!(p.poke_lines.iter().any(|l| l.contains("此方")));
    }
}
