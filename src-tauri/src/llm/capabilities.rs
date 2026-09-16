//! 模型能力探测（目前只有一项：是否支持深度思考）
//!
//! **为什么需要它**：对话界面上的"深度思考"开关必须"有该能力才显示"，
//! 而对任意 OpenAI 兼容端点我们无法探测能力——只能按模型名启发式判断，
//! 再让用户在设置里对具体模型做**显式声明**覆盖（[`LlmProvider::thinking_models`]）。
//!
//! **启发式只覆盖"显式开关型"模型**（Qwen3 / GLM / DeepSeek-V3.1 等混合推理
//! 家族，端点按 `enable_thinking` 开关决定是否思考）。原生推理模型
//! （OpenAI o1/o3/gpt-5、DeepSeek-R1、Claude/Gemini 的原生思考模式）不接收这个
//! 字段，严格端点会直接 400；它们要发该字段必须由用户在设置里显式声明。
//!
//! 为什么默认**不**把 `enable_thinking` 发给所有模型：严格端点会对未知字段
//! 直接 400。因此判定为"不支持"时一律不发该字段，行为与接入该功能前一致。
//!
//! [`LlmProvider::thinking_models`]: crate::config::types::LlmProvider::thinking_models

use crate::config::types::LlmProvider;

/// 子串匹配的标记：这些名字里出现即视为支持 `enable_thinking` 开关
///
/// 只放"足够长、不会被别的模型名误包含"的词。
const THINKING_MARKERS: &[&str] = &[
    "qwen3",
    "glm-z",
    "deepseek-v3.1",
    "kimi-k2-thinking",
    "magistral",
];

/// 按模型名启发式判断是否支持深度思考
///
/// 纯函数：不访问网络、不读配置，便于单测与在命令层直接复用。
pub fn supports_thinking(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    if lower.trim().is_empty() {
        return false;
    }
    THINKING_MARKERS.iter().any(|m| lower.contains(m))
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
    fn detects_switchable_reasoning_models_by_name() {
        // 只认"端点用 enable_thinking 开关控制是否思考"的混合推理家族
        for model in [
            "qwen3-235b-a22b",
            "qwen3-coder",
            "glm-z1-air",
            "deepseek-v3.1",
            "kimi-k2-thinking",
            "magistral-small",
        ] {
            assert!(supports_thinking(model), "{model} 应被判定为支持深度思考开关");
        }
    }

    /// 原生推理模型与普通模型都不该收到 `enable_thinking`
    ///
    /// 历史缺陷：`gpt-5` / `o1` / `o3` / `deepseek-reasoner` 被判定为"支持开关"，
    /// 会话里一开思考就会给 OpenAI 官方端点发 `enable_thinking` → 400。
    /// 这类模型要发该字段必须由用户在设置里显式声明。
    #[test]
    fn native_reasoners_and_plain_models_do_not_claim_the_switch() {
        for model in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-3.5-turbo",
            "gpt-5",
            "gpt-5-mini",
            "o1",
            "o1-mini",
            "o3",
            "deepseek-reasoner",
            "deepseek-r1",
            "deepseek-r1-distill-qwen-32b",
            "qwq-32b",
            "gemini-2.5-pro",
            "claude-4-sonnet",
            "deepseek-chat",
            "text-embedding-3-small",
            "claude-3-5-sonnet",
            "",
        ] {
            assert!(!supports_thinking(model), "{model} 不应被判定为支持思考开关");
        }
    }

    #[test]
    fn explicit_declaration_overrides_detection() {
        let mut provider = LlmProvider::new("测试", "https://example.com/v1", "k");
        assert!(!provider_supports_thinking(&provider, "my-private-model"));
        // 原生推理模型也可以通过显式声明打开（自建网关确实支持该字段时）
        assert!(!provider_supports_thinking(&provider, "deepseek-r1"));

        provider.thinking_models.push("my-private-model".to_string());
        provider.thinking_models.push("deepseek-r1".to_string());
        assert!(
            provider_supports_thinking(&provider, "my-private-model"),
            "用户声明必须生效"
        );
        assert!(provider_supports_thinking(&provider, "deepseek-r1"));
        // 声明只影响被点名的模型
        assert!(!provider_supports_thinking(&provider, "another-private-model"));
    }
}
