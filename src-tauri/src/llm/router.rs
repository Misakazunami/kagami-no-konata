//! 会话级模型路由：把"这一轮用什么模型"解析成一个 [`ModelPlan`]
//!
//! 设计要点：
//! - **请求级**：解析只发生在 `send_message`，结果随 `AgentContext` 进入本轮生成。
//!   因此用户中途换模型不会污染正在跑的生成，两个窗口并发生成也互不影响。
//! - **主/子分离**：`ModelPlan` 为每个模型建一个请求级 backend；主轮次用 `main`，
//!   只读子代理按序号在 `subs` 上轮转（见 `agent::harness::subagent::AgentRuntime`）。
//! - **绝不失败**：引用的提供商/模型悬空时降级到全局活跃提供商并记录日志。
//!   模型选择是"锦上添花"，不能让它把用户的一条消息变成"发送失败"。
//! - **自动选择只对任务会话生效**：普通对话会话即使被写成 `auto` 也按 `Inherit`
//!   处理（这条规则同时由界面与后端守着）。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::types::{AppConfig, LlmProvider, ModelMode, ModelRef, MAX_SUB_MODELS};
use crate::llm::backend::ChatBackend;
use crate::llm::capabilities;
use crate::llm::proxy::{LlmProxy, ProviderOverrides};

/// 会话级模型偏好（持久化为 `sessions.model_pref` 里的一个 JSON 对象）
///
/// 单列 JSON 而不是多个列：这些字段从不参与查询/排序，且以后加字段
/// 只需 `#[serde(default)]`，不必再写迁移。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionModelPref {
    pub mode: ModelMode,
    /// `Manual` 时选定的提供商 id
    pub provider_id: Option<String>,
    /// `Manual` 时选定的模型 id
    pub model: Option<String>,
    /// 深度思考开关：`None` = 跟随提供商默认；`Some` = 本会话显式开关
    pub thinking: Option<bool>,
}

impl SessionModelPref {
    /// 手动选定某个模型的偏好
    pub fn manual(provider_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            mode: ModelMode::Manual,
            provider_id: Some(provider_id.into()),
            model: Some(model.into()),
            thinking: None,
        }
    }

    /// 自动选择（任务会话的主/子模型路由）
    pub fn auto() -> Self {
        Self {
            mode: ModelMode::Auto,
            provider_id: None,
            model: None,
            thinking: None,
        }
    }

    /// 是否存在任何实际内容（全空的偏好等同于"未设置"，不写库）
    pub fn is_empty(&self) -> bool {
        self.mode == ModelMode::Inherit && self.thinking.is_none()
    }

    /// 手动模式是否信息齐备（缺一即视为"未设置"，回退到跟随全局）
    fn manual_target(&self) -> Option<ModelRef> {
        match (self.provider_id.as_deref(), self.model.as_deref()) {
            (Some(p), Some(m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
                Some(ModelRef::new(p, m))
            }
            _ => None,
        }
    }
}

/// 本轮生成实际使用的模型方案
#[derive(Debug, Clone)]
pub struct ModelPlan {
    main: LlmProvider,
    /// 子代理用的模型池（可能为空：此时子代理与主轮次同模型）
    subs: Vec<LlmProvider>,
    /// 主槽位是否被子模型顶替（Plan 模式）——只用于展示与日志
    main_is_sub: bool,
    /// 会话级深度思考开关（下发前还会按模型能力过滤）
    thinking: Option<bool>,
    /// 是否发生过降级（展示与日志用）
    degraded: bool,
}

/// 子代理可用的一个模型：请求级后端 + 展示用模型名
///
/// 模型名会随子代理事件一起发给界面，让用户看得见"这条结论是哪个子模型跑的"。
#[derive(Clone)]
pub struct ChildModel {
    pub backend: Arc<dyn ChatBackend>,
    /// 展示名（模型 id；为空表示"与主轮次同模型"的默认后端）
    pub label: String,
}

impl ChildModel {
    pub fn new(backend: Arc<dyn ChatBackend>, label: impl Into<String>) -> Self {
        Self {
            backend,
            label: label.into(),
        }
    }
}

/// 为一个提供商构造请求级后端（带上按能力过滤过的深度思考覆盖）
fn backend_for(provider: &LlmProvider, thinking: Option<bool>) -> Arc<dyn ChatBackend> {
    let backend: Arc<dyn ChatBackend> = Arc::new(LlmProxy::with_overrides(
        provider,
        overrides_for(provider, thinking),
    ));
    backend
}

impl ModelPlan {
    /// 主轮次使用的提供商（含被顶替的情况）
    pub fn main_provider(&self) -> &LlmProvider {
        &self.main
    }

    /// 子代理模型池（为空表示"子代理与主轮次同模型"）
    pub fn sub_providers(&self) -> &[LlmProvider] {
        &self.subs
    }

    pub fn main_is_sub(&self) -> bool {
        self.main_is_sub
    }

    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// 主轮次的请求级 backend
    pub fn main_backend(&self) -> Arc<dyn ChatBackend> {
        backend_for(&self.main, self.thinking)
    }

    /// 子代理的请求级 backend 池（按配置顺序，调用方按序号轮转）
    pub fn sub_backends(&self) -> Vec<Arc<dyn ChatBackend>> {
        self.subs
            .iter()
            .map(|provider| backend_for(provider, self.thinking))
            .collect()
    }

    /// 子代理可用的模型池（未配置子模型时退化为"主轮次模型"）
    ///
    /// 这是交给 `AgentRuntime` 的形态：后端与展示名成对出现，
    /// 便于子代理卡片显示实际使用的模型。
    pub fn child_models(&self) -> Vec<ChildModel> {
        if self.subs.is_empty() {
            return vec![ChildModel::new(self.main_backend(), self.main.model.clone())];
        }
        self.subs
            .iter()
            .map(|provider| ChildModel::new(backend_for(provider, self.thinking), provider.model.clone()))
            .collect()
    }

    /// 人类可读标签（`message-stats` 事件与日志用）
    pub fn label(&self) -> String {
        let suffix = if self.degraded { "，已回退" } else { "" };
        if self.main_is_sub {
            format!(
                "{}（子模型 1/{}，Plan 模式{}）",
                self.main.model,
                self.subs.len().max(1),
                suffix
            )
        } else if self.subs.is_empty() {
            format!("{}（主模型{}）", self.main.model, suffix)
        } else {
            format!(
                "{}（主模型，子模型 {} 个{}）",
                self.main.model,
                self.subs.len(),
                suffix
            )
        }
    }
}

/// 按模型能力过滤后的单轮覆盖
///
/// **只有判定支持深度思考的模型才会收到 `enable_thinking`**：严格端点会对
/// 未知字段直接 400，而对本来就不思考的模型发这个字段没有任何收益。
/// 自动模式下多个模型共用同一个开关，这条过滤是必需的。
fn overrides_for(provider: &LlmProvider, thinking: Option<bool>) -> ProviderOverrides {
    let supported = capabilities::provider_supports_thinking(provider, &provider.model);
    ProviderOverrides {
        thinking: if supported { thinking } else { None },
    }
}

/// 按 id 找提供商（找不到返回 None，由调用方降级）
fn provider_by_id<'a>(cfg: &'a AppConfig, id: &str) -> Option<&'a LlmProvider> {
    cfg.llm.providers.iter().find(|p| p.id == id)
}

/// 克隆提供商并替换模型 id
fn with_model(provider: &LlmProvider, model: &str) -> LlmProvider {
    let mut clone = provider.clone();
    clone.model = model.to_string();
    clone
}

/// 解析本轮模型方案
///
/// 优先级：会话偏好 → 全局活跃提供商；`Auto` 只在任务会话生效。
/// 任何一步失败都降级而**不报错**（见模块头注释）。
pub fn resolve(
    cfg: &AppConfig,
    pref: Option<&SessionModelPref>,
    session_type: &str,
    task_mode: &str,
) -> ModelPlan {
    let thinking = pref.and_then(|p| p.thinking);
    let is_task = session_type == "task";
    let mode = pref.map(|p| p.mode).unwrap_or_default();

    // `auto` 在非任务会话里没有意义：普通会话没有主/子模型之分，
    // 悄悄按继承处理，并留下一条日志便于排查"为什么自动没生效"。
    let mode = if mode == ModelMode::Auto && !is_task {
        eprintln!(
            "[models] 会话类型为 {}，自动选择只对任务会话生效，本轮按「跟随全局提供商」处理",
            session_type
        );
        ModelMode::Inherit
    } else {
        mode
    };

    match mode {
        ModelMode::Manual => {
            let active = cfg.llm.active_provider();
            let Some(target) = pref.and_then(|p| p.manual_target()) else {
                // 手动模式但信息不全（前端状态异常）：按继承处理而不是报错
                eprintln!("[models] 手动模型选择缺少提供商或模型，回退全局提供商");
                return inherit_plan(active, thinking, true);
            };
            match provider_by_id(cfg, &target.provider_id) {
                Some(provider) => ModelPlan {
                    main: with_model(provider, &target.model),
                    subs: Vec::new(),
                    main_is_sub: false,
                    thinking,
                    degraded: false,
                },
                None => {
                    eprintln!(
                        "[models] 会话指定的提供商不存在（{}），已回退全局提供商",
                        target.provider_id
                    );
                    inherit_plan(active, thinking, true)
                }
            }
        }
        ModelMode::Auto => resolve_auto(cfg, thinking, task_mode),
        ModelMode::Inherit => inherit_plan(cfg.llm.active_provider(), thinking, false),
    }
}

/// 跟随全局活跃提供商（默认行为，与未引入本功能时逐字节一致）
fn inherit_plan(provider: &LlmProvider, thinking: Option<bool>, degraded: bool) -> ModelPlan {
    ModelPlan {
        main: provider.clone(),
        subs: Vec::new(),
        main_is_sub: false,
        thinking,
        degraded,
    }
}

/// 自动选择：主模型 + 子模型池，Plan 模式与子代理优先用子模型
fn resolve_auto(cfg: &AppConfig, thinking: Option<bool>, task_mode: &str) -> ModelPlan {
    let settings = &cfg.models;

    // 子模型池：逐个解析，悬空的单独丢掉（其余的仍然可用）
    let mut subs: Vec<LlmProvider> = Vec::new();
    let mut degraded = false;
    for sub in settings.subs.iter().take(MAX_SUB_MODELS) {
        match provider_by_id(cfg, &sub.provider_id) {
            Some(provider) => subs.push(with_model(provider, &sub.model)),
            None => {
                eprintln!(
                    "[models] 子模型 {} 引用的提供商不存在，已跳过",
                    sub.label()
                );
                degraded = true;
            }
        }
    }

    // 主模型：配置缺失或悬空 → 全局活跃提供商
    let main = match settings.main.as_ref() {
        Some(reference) => match provider_by_id(cfg, &reference.provider_id) {
            Some(provider) => with_model(provider, &reference.model),
            None => {
                eprintln!(
                    "[models] 主模型 {} 引用的提供商不存在，已回退全局提供商",
                    reference.label()
                );
                degraded = true;
                cfg.llm.active_provider().clone()
            }
        },
        None => cfg.llm.active_provider().clone(),
    };

    // Plan 模式（只读调查、产出计划）：整轮用**同一个**子模型，避免同一份计划
    // 里混进多个模型的文风；子代理仍可在子模型池上轮转。
    if task_mode == "plan" {
        if let Some(first) = subs.first() {
            return ModelPlan {
                main: first.clone(),
                subs,
                main_is_sub: true,
                thinking,
                degraded,
            };
        }
        // 没配子模型：Plan 也只能用主模型（配置不全不应让用户无法使用）
        eprintln!("[models] Plan 模式未配置可用子模型，本轮使用主模型");
    }

    ModelPlan {
        main,
        subs,
        main_is_sub: false,
        thinking,
        degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{AppConfig, LlmConfig, ModelRef};

    /// 两个提供商的配置：A（活跃）、B
    fn config_with_providers() -> (AppConfig, String, String) {
        let mut cfg = AppConfig::default();
        let mut a = LlmProvider::new("A", "https://a.example/v1", "sk-a");
        a.model = "a-model".to_string();
        let mut b = LlmProvider::new("B", "https://b.example/v1", "sk-b");
        b.model = "b-model".to_string();
        let (a_id, b_id) = (a.id.clone(), b.id.clone());
        cfg.llm = LlmConfig {
            providers: vec![a, b],
            active_provider_id: a_id.clone(),
        };
        (cfg, a_id, b_id)
    }

    #[test]
    fn inherit_uses_active_provider_and_no_sub_pool() {
        let (cfg, _, _) = config_with_providers();
        let plan = resolve(&cfg, None, "chat", "plan");
        assert_eq!(plan.main_provider().model, "a-model");
        assert!(plan.sub_providers().is_empty(), "默认不得引入子模型池");
        assert!(!plan.degraded());
    }

    #[test]
    fn manual_selection_wins_over_active_provider() {
        let (cfg, _, b_id) = config_with_providers();
        let pref = SessionModelPref::manual(b_id.clone(), "b-chat-32k");
        let plan = resolve(&cfg, Some(&pref), "chat", "plan");
        assert_eq!(plan.main_provider().model, "b-chat-32k");
        assert_eq!(plan.main_provider().id, b_id);
        // 手动模式下子代理与主轮次同模型（不引入用户没选的模型）
        assert!(plan.sub_backends().is_empty());
    }

    #[test]
    fn manual_selection_with_dangling_provider_degrades() {
        let (cfg, _, _) = config_with_providers();
        let pref = SessionModelPref::manual("ghost", "whatever");
        let plan = resolve(&cfg, Some(&pref), "chat", "plan");
        assert_eq!(plan.main_provider().model, "a-model", "必须回退活跃提供商");
        assert!(plan.degraded());
    }

    #[test]
    fn manual_selection_without_target_degrades_without_panic() {
        let (cfg, _, _) = config_with_providers();
        // 只给了模式，没给 provider/model
        let pref = SessionModelPref {
            mode: ModelMode::Manual,
            ..SessionModelPref::default()
        };
        let plan = resolve(&cfg, Some(&pref), "chat", "plan");
        assert_eq!(plan.main_provider().model, "a-model");
        assert!(plan.degraded());
    }

    fn auto_config() -> (AppConfig, String, String) {
        let (mut cfg, a_id, b_id) = config_with_providers();
        cfg.models.main = Some(ModelRef::new(a_id.clone(), "a-big"));
        cfg.models.subs = vec![
            ModelRef::new(b_id.clone(), "b-fast"),
            ModelRef::new(a_id.clone(), "a-fast"),
        ];
        (cfg, a_id, b_id)
    }

    #[test]
    fn auto_in_work_mode_prefers_main_model() {
        let (cfg, a_id, b_id) = auto_config();
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "work");
        assert_eq!(plan.main_provider().model, "a-big");
        assert_eq!(plan.main_provider().id, a_id);
        assert!(!plan.main_is_sub());
        // 子代理仍用子模型池
        assert_eq!(plan.sub_providers().len(), 2);
        assert_eq!(plan.sub_providers()[0].model, "b-fast");
        assert_eq!(plan.sub_providers()[1].id, a_id);
        let _ = b_id;
    }

    #[test]
    fn auto_in_plan_mode_prefers_sub_model() {
        let (cfg, _, b_id) = auto_config();
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "plan");
        assert_eq!(plan.main_provider().model, "b-fast", "Plan 模式优先子模型");
        assert_eq!(plan.main_provider().id, b_id);
        assert!(plan.main_is_sub());
        assert_eq!(plan.sub_providers().len(), 2);
    }

    #[test]
    fn auto_without_subs_falls_back_to_main_model() {
        let (mut cfg, _, _) = auto_config();
        cfg.models.subs.clear();
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "plan");
        assert_eq!(plan.main_provider().model, "a-big");
        assert!(!plan.main_is_sub(), "没有子模型时不应谎称用了子模型");
    }

    /// 自动选择只给任务会话：普通会话里写成 auto 也必须按继承处理
    #[test]
    fn auto_is_ignored_for_non_task_sessions() {
        let (cfg, _, _) = auto_config();
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "chat", "plan");
        assert_eq!(plan.main_provider().model, "a-model");
        assert!(plan.sub_providers().is_empty());
    }

    #[test]
    fn dangling_sub_models_are_skipped_individually() {
        let (mut cfg, a_id, b_id) = auto_config();
        cfg.models.subs = vec![
            ModelRef::new("ghost", "x"),
            ModelRef::new(b_id.clone(), "b-fast"),
        ];
        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "plan");
        assert_eq!(plan.sub_providers().len(), 1, "只有悬空的被丢掉");
        assert_eq!(plan.main_provider().model, "b-fast");
        assert!(plan.degraded());
        let _ = a_id;
    }

    /// 深度思考开关按模型能力过滤：不支持的模型**不发** enable_thinking
    #[test]
    fn thinking_override_is_filtered_by_model_capability() {
        let mut provider = LlmProvider::new("A", "https://a.example/v1", "sk-a");
        provider.model = "gpt-4o".to_string();
        assert_eq!(
            overrides_for(&provider, Some(true)).thinking,
            None,
            "不支持的模型不得收到 enable_thinking"
        );

        provider.model = "deepseek-reasoner".to_string();
        assert_eq!(overrides_for(&provider, Some(true)).thinking, Some(true));
        assert_eq!(overrides_for(&provider, Some(false)).thinking, Some(false));
        assert_eq!(
            overrides_for(&provider, None).thinking,
            None,
            "未设置时沿用提供商默认"
        );

        // 用户显式声明的模型同样生效
        provider.model = "my-private-model".to_string();
        provider.thinking_models.push("my-private-model".to_string());
        assert_eq!(overrides_for(&provider, Some(true)).thinking, Some(true));
    }

    #[test]
    fn label_mentions_which_slot_is_used() {
        let (cfg, _, _) = auto_config();
        let work = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "work");
        assert!(work.label().contains("a-big"), "{}", work.label());
        assert!(work.label().contains("主模型"), "{}", work.label());

        let plan = resolve(&cfg, Some(&SessionModelPref::auto()), "task", "plan");
        assert!(plan.label().contains("b-fast"), "{}", plan.label());
        assert!(plan.label().contains("子模型"), "{}", plan.label());
    }
}
