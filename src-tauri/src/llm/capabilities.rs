//! 模型能力探测（目前只有一项：是否支持深度思考）
//!
//! **为什么需要它**：对话界面上的"深度思考"开关必须"有该能力才显示"，
//! 而对任意 OpenAI 兼容端点我们无法探测能力——只能按模型名启发式判断，
//! 再让用户在设置里对具体模型做**显式声明**覆盖（[`LlmProvider::thinking_models`]）。
//!
//! 为什么默认**不**把 `enable_thinking` 发给所有模型：严格端点会对未知字段
//! 直接 400。因此判定为"不支持"时一律不发该字段，行为与接入该功能前一致。
//!
//! [`LlmProvider::thinking_models`]: crate::config::types::LlmProvider::thinking_models

use crate::config::types::LlmProvider;

/// 子串匹配的标记：这些名字里出现即视为支持思考
///
/// 只放"足够长、不会被别的模型名误包含"的词。
const THINKING_MARKERS: &[&str] = &[
    "reasoner",
    "thinking",
    "qwq",
    "deepseek-v3.1",
    "magistral",
    "glm-z",
    "qwen3",
    "gemini-2.5",
    "gemini-3",
    "claude-3-7",
    "claude-4",
    "kimi-k2-thinking",
    "ernie-x1",
    "hunyuan-a13b",
    "seed-oss",
    "gpt-5",
    "minimax-m1",
    "minimax-m2",
];

/// 整段匹配的标记（按非字母数字切分后的 token 相等）
///
/// 这些名字太短，用子串匹配会误伤（例如 `o1` 会命中 `gpt-4o1` 之类的变体名）。
const THINKING_TOKENS: &[&str] = &["o1", "o3", "o4", "r1"];

/// 把模型 id 按非字母数字切成 token（`deepseek-r1-distill` → ["deepseek","r1","distill"]）
fn tokens(model: &str) -> Vec<&str> {
    model
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect()
}

/// 按模型名启发式判断是否支持深度思考
///
/// 纯函数：不访问网络、不读配置，便于单测与在命令层直接复用。
pub fn supports_thinking(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    if lower.trim().is_empty() {
        return false;
    }
    if THINKING_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let toks = tokens(&lower);
    THINKING_TOKENS
        .iter()
        .any(|want| toks.iter().any(|t| t == want))
}

/// 结合用户显式声明判断提供商下某个模型是否支持深度思考
///
/// 声明优先于启发式：探测不准时用户可以在设置里勾选该模型，勾选后
/// 界面会出现"深度思考"开关，请求里也才会带 `enable_thinking`。
pub fn provider_supports_thinking(provider: &LlmProvider, model: &str) -> bool {
    if provider.thinking_models.iter().any(|m| m == model) {
        return true;
    }
    supports_thinking(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_reasoning_models_by_name() {
        for model in [
            "deepseek-reasoner",
            "deepseek-r1",
            "deepseek-r1-distill-qwen-32b",
            "qwq-32b",
            "qwen3-235b-a22b",
            "o1-mini",
            "o3",
            "gpt-5",
            "gpt-5-mini",
            "glm-z1-air",
            "gemini-2.5-pro",
            "kimi-k2-thinking",
        ] {
            assert!(supports_thinking(model), "{model} 应被判定为支持思考");
        }
    }

    #[test]
    fn plain_models_do_not_claim_thinking() {
        for model in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-3.5-turbo",
            "deepseek-chat",
            "text-embedding-3-small",
            "claude-3-5-sonnet",
            "",
        ] {
            assert!(!supports_thinking(model), "{model} 不应被判定为支持思考");
        }
    }

    /// 短标记必须整段匹配，否则 `gpt-4o1` 这类名字会被误判
    #[test]
    fn short_markers_match_whole_tokens_only() {
        assert!(!supports_thinking("gpt-4o1"));
        assert!(!supports_thinking("llama-3-o1x"));
        assert!(supports_thinking("o1"));
        assert!(supports_thinking("deepseek-r1"));
    }

    #[test]
    fn explicit_declaration_overrides_detection() {
        let mut provider = LlmProvider::new("测试", "https://example.com/v1", "k");
        assert!(!provider_supports_thinking(&provider, "my-private-model"));

        provider.thinking_models.push("my-private-model".to_string());
        assert!(
            provider_supports_thinking(&provider, "my-private-model"),
            "用户声明必须生效"
        );
        // 声明只影响被点名的模型
        assert!(!provider_supports_thinking(&provider, "another-private-model"));
    }
}
