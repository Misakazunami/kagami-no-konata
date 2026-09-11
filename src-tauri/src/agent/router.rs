use regex::Regex;
use serde::{Deserialize, Serialize};

use super::traits::AgentManifest;

/// 路由判决结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    /// 目标 Agent ID
    pub target_agent: String,
    /// 置信度 [0.0, 1.0]
    pub confidence: f32,
    /// 路由判定原因/方式（如 "slash_command", "regex", "keyword", "fallback"）
    pub reason: String,
    /// 是否需要用当前人设语气包装结果
    pub wrap_in_persona: bool,
    /// 清理掉触发前缀后的核心输入内容
    pub clean_input: String,
}

/// 编译后的 Agent 清单（预编译正则，避免运行期重复编译）
#[derive(Clone)]
struct CompiledManifest {
    manifest: AgentManifest,
    regexes: Vec<Regex>,
}

impl CompiledManifest {
    fn from_manifest(manifest: AgentManifest) -> Self {
        let regexes = manifest
            .regex_patterns
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();
        Self { manifest, regexes }
    }
}

/// 意图路由器
pub struct IntentRouter {
    compiled_manifests: Vec<CompiledManifest>,
}

impl IntentRouter {
    pub fn new(manifests: Vec<AgentManifest>) -> Self {
        let compiled = manifests
            .into_iter()
            .map(CompiledManifest::from_manifest)
            .collect();
        Self {
            compiled_manifests: compiled,
        }
    }

    /// 注册或更新清单（自动预编译所有正则）
    pub fn update_manifests(&mut self, manifests: Vec<AgentManifest>) {
        self.compiled_manifests = manifests
            .into_iter()
            .map(CompiledManifest::from_manifest)
            .collect();
    }

    /// 核心路由规则判定流水线（多级策略）
    pub fn route(&self, input: &str) -> RouteDecision {
        let trimmed = input.trim();

        // ─── Level 1: 显式前缀 / Slash Command 判定（0ms，精确词边界）───
        for cm in &self.compiled_manifests {
            let m = &cm.manifest;
            for cmd in &m.prefix_commands {
                // 要求全词相等，或紧跟空格/标点，防止 "/sys" 误匹配 "/systemic"
                if trimmed == cmd {
                    return RouteDecision {
                        target_agent: m.id.clone(),
                        confidence: 1.0,
                        reason: format!("slash_command: {}", cmd),
                        wrap_in_persona: m.wrap_in_persona,
                        clean_input: String::new(),
                    };
                } else if trimmed.starts_with(cmd) {
                    let rest = &trimmed[cmd.len()..];
                    if rest.starts_with(' ') || rest.starts_with('\t') || rest.starts_with(':') || rest.starts_with('：') {
                        let clean = rest.trim().to_string();
                        return RouteDecision {
                            target_agent: m.id.clone(),
                            confidence: 1.0,
                            reason: format!("slash_command: {}", cmd),
                            wrap_in_persona: m.wrap_in_persona,
                            clean_input: clean,
                        };
                    }
                }
            }
        }

        // ─── Level 2: 预编译正则模式匹配 ───
        for cm in &self.compiled_manifests {
            let m = &cm.manifest;
            for re in &cm.regexes {
                if re.is_match(trimmed) {
                    return RouteDecision {
                        target_agent: m.id.clone(),
                        confidence: 0.95,
                        reason: format!("regex: {}", re.as_str()),
                        wrap_in_persona: m.wrap_in_persona,
                        clean_input: trimmed.to_string(),
                    };
                }
            }
        }

        // ─── Level 3: 启发式关键词匹配与加权打分 ───
        let mut best_agent: Option<&AgentManifest> = None;
        let mut best_score = 0.0f32;

        for cm in &self.compiled_manifests {
            let m = &cm.manifest;
            if m.id == "chat" {
                continue; // chat 作为兜底，不参与关键词争夺
            }

            let mut matched_count = 0;
            for kw in &m.trigger_keywords {
                if trimmed.contains(kw) {
                    matched_count += 1;
                }
            }

            if matched_count > 0 {
                // 改进打分逻辑：结合绝对命中词数（饱和阈值3个）与相对占比，防止关键词越多的 Agent 得分反而被惩罚
                let count_factor = (matched_count as f32).min(3.0) / 3.0; // [0.33, 1.0]
                let ratio_factor = (matched_count as f32) / (m.trigger_keywords.len().max(1) as f32);
                let score = count_factor * 0.7 + ratio_factor * 0.3;

                if score > best_score {
                    best_score = score;
                    best_agent = Some(m);
                }
            }
        }

        if let Some(m) = best_agent {
            if best_score >= 0.3 {
                return RouteDecision {
                    target_agent: m.id.clone(),
                    confidence: best_score,
                    reason: "keyword_heuristic".to_string(),
                    wrap_in_persona: m.wrap_in_persona,
                    clean_input: trimmed.to_string(),
                };
            }
        }

        // ─── Level 4: 默认回退到 ChatAgent ───
        RouteDecision {
            target_agent: "chat".to_string(),
            confidence: 0.0,
            reason: "fallback_default".to_string(),
            wrap_in_persona: false,
            clean_input: trimmed.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_manifests() -> Vec<AgentManifest> {
        vec![
            AgentManifest {
                id: "system".to_string(),
                name: "System Agent".to_string(),
                description: "Handles system tasks".to_string(),
                prefix_commands: vec!["/sys".to_string(), "/open".to_string()],
                trigger_keywords: vec!["系统状态".to_string(), "时间".to_string()],
                regex_patterns: vec![r"^打开.+".to_string()],
                wrap_in_persona: true,
            },
            AgentManifest {
                id: "chat".to_string(),
                name: "Chat Agent".to_string(),
                description: "Chatbot".to_string(),
                prefix_commands: vec![],
                trigger_keywords: vec![],
                regex_patterns: vec![],
                wrap_in_persona: false,
            },
        ]
    }

    #[test]
    fn test_slash_command() {
        let router = IntentRouter::new(mock_manifests());
        let decision = router.route("/open 记事本");
        assert_eq!(decision.target_agent, "system");
        assert_eq!(decision.clean_input, "记事本");
        assert_eq!(decision.confidence, 1.0);
    }

    #[test]
    fn test_slash_command_boundary_safety() {
        let router = IntentRouter::new(mock_manifests());
        // "/openmind" 不应该误触发 "/open"
        let decision = router.route("/openmind 是一个很棒的项目");
        assert_eq!(decision.target_agent, "chat");
    }

    #[test]
    fn test_regex_match() {
        let router = IntentRouter::new(mock_manifests());
        let decision = router.route("打开网易云音乐");
        assert_eq!(decision.target_agent, "system");
        assert_eq!(decision.clean_input, "打开网易云音乐");
        assert_eq!(decision.confidence, 0.95);
    }

    #[test]
    fn test_fallback_chat() {
        let router = IntentRouter::new(mock_manifests());
        let decision = router.route("今天吃什么好呢？");
        assert_eq!(decision.target_agent, "chat");
        assert_eq!(decision.confidence, 0.0);
    }
}
