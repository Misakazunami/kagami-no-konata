use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;
use uuid::Uuid;

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

/// 兜底提供商：`providers` 意外为空时使用
///
/// 历史实现直接 `expect("No LLM provider configured")`，而 `providers` 为空会让
/// 命令在持有 `config` 锁时 panic：IPC 永不返回、锁被毒化，甚至把空配置写盘，
/// 导致下次启动在 `setup` 阶段崩溃后应用永久无法启动。
fn fallback_provider() -> &'static LlmProvider {
    static FALLBACK: OnceLock<LlmProvider> = OnceLock::new();
    FALLBACK.get_or_init(|| LlmProvider::new("未配置", "", ""))
}

// ─── 提供商配置 ────────────────────────────────────────

/// 单个 API 提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProvider {
    pub id: String,
    pub name: String,
    pub api_base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub enabled_models: Vec<String>,
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// 是否启用思考模式（需要模型支持，如 DeepSeek-R1、QwQ 等）
    ///
    /// 这是**提供商级默认值**：会话里没有单独开关时以它为准
    /// （见 `llm::proxy::ProviderOverrides`）。
    #[serde(default)]
    pub enable_thinking: bool,
    /// 显式声明支持深度思考的模型 id（覆盖名称启发式探测）
    ///
    /// 探测不准时（自建端点、新模型）用户可在设置里对具体模型勾选，
    /// 勾选后界面才会出现"深度思考"开关、请求里才会带 `enable_thinking`。
    #[serde(default)]
    pub thinking_models: Vec<String>,
}

fn default_max_tokens() -> u32 {
    2048
}

fn default_temperature() -> f32 {
    0.8
}

impl LlmProvider {
    pub fn new(name: &str, api_base_url: &str, api_key: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            api_base_url: api_base_url.to_string(),
            api_key: api_key.to_string(),
            model: String::new(),
            enabled_models: Vec::new(),
            embedding_model: "text-embedding-3-small".to_string(),
            max_tokens: 2048,
            temperature: 0.8,
            enable_thinking: false,
            thinking_models: Vec::new(),
        }
    }
}

/// LLM 配置（多提供商）
#[derive(Debug, Clone, Serialize)]
pub struct LlmConfig {
    pub providers: Vec<LlmProvider>,
    pub active_provider_id: String,
}

impl LlmConfig {
    /// 获取当前活跃提供商
    ///
    /// 不会 panic：`providers` 为空或活跃 id 不存在时回退到首个提供商，
    /// 仍为空则返回兜底提供商（调用方应通过 `AppConfig::validate` 或
    /// `ensure_non_empty` 保证配置合法）。
    pub fn active_provider(&self) -> &LlmProvider {
        self.providers
            .iter()
            .find(|p| p.id == self.active_provider_id)
            .or_else(|| self.providers.first())
            .unwrap_or_else(|| fallback_provider())
    }

    /// 获取当前活跃提供商的索引（空表返回 None）
    fn active_index(&self) -> Option<usize> {
        self.providers
            .iter()
            .position(|p| p.id == self.active_provider_id)
            .or_else(|| (!self.providers.is_empty()).then_some(0))
    }

    /// 获取当前活跃提供商（可变引用，空表返回 None）
    pub fn active_provider_mut(&mut self) -> Option<&mut LlmProvider> {
        let idx = self.active_index()?;
        self.providers.get_mut(idx)
    }

    /// 添加提供商
    pub fn add_provider(&mut self, provider: LlmProvider) {
        if self.providers.is_empty() {
            self.active_provider_id = provider.id.clone();
        }
        self.providers.push(provider);
    }

    /// 删除提供商
    ///
    /// 拒绝删除最后一个提供商：`providers` 为空会让所有 `active_provider()`
    /// 调用方失去配置来源（历史版本会 panic 并把空配置写盘）。
    pub fn remove_provider(&mut self, id: &str) -> Result<(), String> {
        if !self.providers.iter().any(|p| p.id == id) {
            return Err("提供商不存在".to_string());
        }
        if self.providers.len() <= 1 {
            return Err("至少需要保留一个提供商，无法删除最后一个".to_string());
        }
        self.providers.retain(|p| p.id != id);
        // 如果删除的是当前活跃提供商，切换到第一个
        if self.active_provider_id == id {
            self.active_provider_id = self
                .providers
                .first()
                .map(|p| p.id.clone())
                .unwrap_or_default();
        }
        Ok(())
    }

    /// 切换活跃提供商
    pub fn set_active(&mut self, id: &str) -> Result<(), String> {
        if self.providers.iter().any(|p| p.id == id) {
            self.active_provider_id = id.to_string();
            Ok(())
        } else {
            Err("提供商不存在".to_string())
        }
    }

    /// 保证至少存在一个提供商，返回是否发生了修复
    ///
    /// 用于加载历史坏配置（这些配置已落盘为空数组，只能就地修复）。
    pub fn ensure_non_empty(&mut self) -> bool {
        if self.providers.is_empty() {
            let provider = LlmProvider::new("OpenAI", "https://api.openai.com/v1", "");
            self.active_provider_id = provider.id.clone();
            self.providers.push(provider);
            true
        } else {
            false
        }
    }
}

/// 新格式的中间结构（避免 Deserialize 递归）
#[derive(Debug, Deserialize)]
struct NewLlmConfig {
    providers: Vec<LlmProvider>,
    active_provider_id: String,
}

/// 从旧格式迁移的辅助结构（兼容旧配置文件）
#[derive(Debug, Deserialize)]
struct LegacyLlmConfig {
    api_base_url: String,
    api_key: String,
    model: String,
    #[serde(default)]
    enabled_models: Vec<String>,
    max_tokens: u32,
    temperature: f32,
    #[serde(default = "default_embedding_model")]
    embedding_model: String,
}

impl<'de> Deserialize<'de> for LlmConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // 先尝试反序列化为 serde_json::Value 来判断格式
        let value = serde_json::Value::deserialize(deserializer)?;

        // 如果有 "providers" 字段，说明是新格式
        if value.get("providers").is_some() {
            let new: NewLlmConfig =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            return Ok(LlmConfig {
                providers: new.providers,
                active_provider_id: new.active_provider_id,
            });
        }

        // 否则尝试旧格式迁移
        let legacy: LegacyLlmConfig =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;

        let provider = LlmProvider {
            id: Uuid::new_v4().to_string(),
            name: "默认".to_string(),
            api_base_url: legacy.api_base_url,
            api_key: legacy.api_key,
            model: legacy.model,
            enabled_models: if legacy.enabled_models.is_empty() {
                Vec::new()
            } else {
                legacy.enabled_models
            },
            embedding_model: legacy.embedding_model,
            max_tokens: legacy.max_tokens,
            temperature: legacy.temperature,
            enable_thinking: false,
            thinking_models: Vec::new(),
        };

        Ok(LlmConfig {
            active_provider_id: provider.id.clone(),
            providers: vec![provider],
        })
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        let provider = LlmProvider::new("OpenAI", "https://api.openai.com/v1", "");
        LlmConfig {
            active_provider_id: provider.id.clone(),
            providers: vec![provider],
        }
    }
}

// ─── 应用全局配置 ────────────────────────────────────────

/// 应用全局配置
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub user: UserConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    /// 模型路由（会话级"自动选择"用的主/子模型池）
    #[serde(default)]
    pub models: ModelSettings,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub tools: ToolConfig,
}

// ─── 模型路由（自动选择） ────────────────────────────────

/// 子模型池上限（界面与轮转逻辑都要能承载）
pub const MAX_SUB_MODELS: usize = 8;

/// 指向"某个提供商的某个模型"的稳定引用
///
/// 只存 id 而不存地址/密钥：提供商配置变了（换地址、换 key）引用依然有效，
/// 提供商被删除时由 `llm::router::resolve` 降级到活跃提供商，而不是让整轮失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model: model.into(),
        }
    }

    /// 人类可读标签（错误信息、日志用）
    pub fn label(&self) -> String {
        format!("{}@{}", self.model, self.provider_id)
    }
}

/// 模型选择模式
///
/// - `Inherit`：跟随全局活跃提供商（默认；与未引入本功能时的行为一致）
/// - `Manual`：本会话手动选定一个模型
/// - `Auto`：主/子模型自动路由（**只对任务会话生效**）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    #[default]
    Inherit,
    Manual,
    Auto,
}

/// 模型路由配置（自动选择的主/子模型池）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    /// 新建**任务**会话是否默认开启"自动选择"
    pub auto_by_default: bool,
    /// 主模型：Work 模式与普通对话优先使用
    pub main: Option<ModelRef>,
    /// 子模型池：Plan 模式与子代理优先使用，按顺序轮转
    pub subs: Vec<ModelRef>,
}

impl ModelSettings {
    /// 校验模型池（悬空提供商必须报错：那是前端/配置状态错误，不是可降级的运行时状况）
    pub fn validate(&self, providers: &[LlmProvider]) -> Result<(), String> {
        if self.subs.len() > MAX_SUB_MODELS {
            return Err(format!("子模型数量不能超过 {} 个", MAX_SUB_MODELS));
        }

        let check = |item: &ModelRef, what: &str| -> Result<(), String> {
            if item.model.trim().is_empty() {
                return Err(format!("{}的模型不能为空", what));
            }
            if item.provider_id.trim().is_empty() {
                return Err(format!("{}的提供商不能为空", what));
            }
            if !providers.iter().any(|p| p.id == item.provider_id) {
                return Err(format!(
                    "{}引用的提供商不存在：{}（请先在设置里选择有效模型）",
                    what,
                    item.provider_id
                ));
            }
            Ok(())
        };

        if let Some(main) = &self.main {
            check(main, "主模型")?;
        }
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for sub in &self.subs {
            check(sub, "子模型")?;
            if !seen.insert((sub.provider_id.as_str(), sub.model.as_str())) {
                return Err(format!("子模型重复：{}", sub.label()));
            }
        }

        // 说明：主模型同时出现在子模型池里是允许的（用户可能只想给子代理固定用同一个模型），
        // 因此这里不做交叉去重。
        Ok(())
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            nickname: "Master".to_string(),
            pronouns: default_pronouns(),
            gender: String::new(),
            birthday: String::new(),
            bio: String::new(),
        }
    }
}

/// 单条字符串字段的长度上限（防止超长文本被注入 system prompt 或写爆配置）
const MAX_NICKNAME_CHARS: usize = 64;
const MAX_BIO_CHARS: usize = 2000;

impl AppConfig {
    /// 校验配置合法性（在落盘前调用，替代"写入自毁式配置"）
    ///
    /// 注意：`model` / `api_key` 允许为空，因为引导流程允许先保存地址后再拉取模型。
    pub fn validate(&self) -> Result<(), String> {
        if self.llm.providers.is_empty() {
            return Err("至少需要保留一个 LLM 提供商".to_string());
        }

        let mut seen: HashSet<&str> = HashSet::new();
        for p in &self.llm.providers {
            if p.id.trim().is_empty() {
                return Err("提供商 id 不能为空".to_string());
            }
            if !seen.insert(p.id.as_str()) {
                return Err(format!("提供商 id 重复：{}", p.id));
            }
            let name = if p.name.trim().is_empty() { p.id.as_str() } else { p.name.as_str() };
            if p.api_base_url.trim().is_empty() {
                return Err(format!("提供商「{}」的 API 地址不能为空", name));
            }
            if !(p.api_base_url.starts_with("http://") || p.api_base_url.starts_with("https://")) {
                return Err(format!("提供商「{}」的 API 地址必须以 http:// 或 https:// 开头", name));
            }
            if p.max_tokens == 0 || p.max_tokens > 1_000_000 {
                return Err(format!("提供商「{}」的 max_tokens 必须在 1 ~ 1000000 之间", name));
            }
            if !p.temperature.is_finite() || !(0.0..=2.0).contains(&p.temperature) {
                return Err(format!("提供商「{}」的 temperature 必须在 0.0 ~ 2.0 之间", name));
            }
        }

        if !self
            .llm
            .providers
            .iter()
            .any(|p| p.id == self.llm.active_provider_id)
        {
            return Err("活跃提供商 id 不存在".to_string());
        }

        self.models.validate(&self.llm.providers)?;

        if self.user.nickname.chars().count() > MAX_NICKNAME_CHARS {
            return Err(format!("昵称长度不能超过 {} 个字符", MAX_NICKNAME_CHARS));
        }
        if self.user.bio.chars().count() > MAX_BIO_CHARS {
            return Err(format!("简介长度不能超过 {} 个字符", MAX_BIO_CHARS));
        }
        if self.memory.max_context_memories > 100 {
            return Err("上下文记忆数不能超过 100".to_string());
        }
        if !(8..=72).contains(&self.ui.font_size) {
            return Err("字号必须在 8 ~ 72 之间".to_string());
        }
        if !self.ui.poke_probability.is_finite() || !(0.0..=1.0).contains(&self.ui.poke_probability) {
            return Err("戳一下触发概率必须在 0.0 ~ 1.0 之间".to_string());
        }
        if !self.ui.poke_llm_chance.is_finite() || !(0.0..=1.0).contains(&self.ui.poke_llm_chance) {
            return Err("戳一下 LLM 反应概率必须在 0.0 ~ 1.0 之间".to_string());
        }
        if self.ui.bubble_auto_hide_secs > 3600 {
            return Err("气泡自动隐藏时间不能超过 3600 秒".to_string());
        }

        self.tools.validate()?;

        Ok(())
    }
}

/// 用户个性化配置
///
/// 容器级 `#[serde(default)]`：任一字段缺失都回退到 `Default`，
/// 避免旧版本配置文件因为少一个字段就解析失败（进而导致启动崩溃）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    pub nickname: String,
    #[serde(default = "default_pronouns")]
    pub pronouns: String,
    #[serde(default)]
    pub gender: String,
    #[serde(default)]
    pub birthday: String,
    #[serde(default)]
    pub bio: String,
}

fn default_pronouns() -> String {
    "他".to_string()
}

/// 记忆系统配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub auto_extract: bool,
    pub max_context_memories: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_extract: true,
            max_context_memories: 5,
        }
    }
}

fn default_close_action() -> String {
    "hide".to_string()
}

fn default_float_position() -> String {
    "bottom-right".to_string()
}

fn default_poke_probability() -> f32 {
    0.3
}

fn default_poke_llm_chance() -> f32 {
    0.15
}

fn default_bubble_auto_hide_secs() -> u32 {
    20
}

/// UI 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub font_size: u32,
    #[serde(default)]
    pub show_message_stats: bool,
    #[serde(default = "default_close_action")]
    pub close_action: String,
    #[serde(default)]
    pub show_float_clock: bool,
    #[serde(default = "default_float_position")]
    pub float_position: String,
    /// 是否启用戳一下功能
    #[serde(default = "default_true")]
    pub poke_enabled: bool,
    /// 戳一下触发概率（0.0-1.0）
    #[serde(default = "default_poke_probability")]
    pub poke_probability: f32,
    /// 戳一下触发 LLM 反应的概率（0.0-1.0）
    #[serde(default = "default_poke_llm_chance")]
    pub poke_llm_chance: f32,
    /// 气泡自动隐藏时间（秒）
    #[serde(default = "default_bubble_auto_hide_secs")]
    pub bubble_auto_hide_secs: u32,
}

fn default_true() -> bool {
    true
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            font_size: 14,
            show_message_stats: false,
            close_action: default_close_action(),
            show_float_clock: false,
            float_position: default_float_position(),
            poke_enabled: true,
            poke_probability: default_poke_probability(),
            poke_llm_chance: default_poke_llm_chance(),
            bubble_auto_hide_secs: default_bubble_auto_hide_secs(),
        }
    }
}

// ─── 工具运行时配置 ──────────────────────────────────────

/// 工具模式
///
/// - `ReadOnly`：只读工具可见，写入类工具直接从工具表中移除
/// - `Standard`：默认；读 + 应用内写入自动放行，文件写入与命令执行需审批
/// - `Full`：在 `Standard` 基础上额外放开命令执行（仍然禁止敏感命令）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    ReadOnly,
    #[default]
    Standard,
    Full,
}

/// 工作区路径
///
/// 默认工作区用**哨兵**表示而不是绝对路径：`app_data_dir` 会随机器与用户变化，
/// 把 `C:\Users\x\AppData\...` 落盘会让配置换机即失效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WorkspacePath {
    /// 应用数据目录下的 `workspace/`
    AppDataWorkspace,
    /// 用户自定义的绝对路径
    Absolute(String),
}

/// 一个工作区根目录
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRoot {
    /// 稳定标识，工具参数里用 `id:相对路径` 寻址
    pub id: String,
    pub label: String,
    pub path: WorkspacePath,
    /// 是否允许写入（默认工作区 true，用户新增的根默认 false）
    #[serde(default)]
    pub writable: bool,
}

impl WorkspaceRoot {
    /// 解析为绝对路径（`app_data_dir` 用于展开哨兵）
    pub fn resolve(&self, app_data_dir: &std::path::Path) -> std::path::PathBuf {
        match &self.path {
            WorkspacePath::AppDataWorkspace => app_data_dir.join(DEFAULT_WORKSPACE_DIR_NAME),
            WorkspacePath::Absolute(p) => std::path::PathBuf::from(p),
        }
    }
}

impl Default for WorkspaceRoot {
    fn default() -> Self {
        Self {
            id: DEFAULT_WORKSPACE_ID.to_string(),
            label: "应用工作区".to_string(),
            path: WorkspacePath::AppDataWorkspace,
            writable: true,
        }
    }
}

pub const DEFAULT_WORKSPACE_ID: &str = "default";
pub const DEFAULT_WORKSPACE_DIR_NAME: &str = "workspace";
/// 工作区数量上限（UI 与工具描述都要能承载）
pub const MAX_WORKSPACE_ROOTS: usize = 8;

/// 工作区 id 允许的字符集（会被拼进工具参数与错误信息，必须严格）
pub fn is_valid_workspace_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// 内置拒绝访问的文件模式（用户不能移除，只能追加）
///
/// 只读工具同样受限：`config.json` 里存着 API Key，一旦被读进上下文，
/// 后续摘要与记忆提取会把它固化下来。
pub const DEFAULT_DENY_GLOBS: &[&str] = &[
    "**/config.json",
    "**/*.db",
    "**/*.db-wal",
    "**/*.db-shm",
    "**/*.sqlite",
    "**/*.sqlite3",
    "**/.env",
    "**/.env.*",
    "**/.ssh/**",
    "**/.aws/**",
    "**/.gnupg/**",
    "**/.kube/**",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/*.pem",
    "**/*.key",
    "**/*.pfx",
    "**/credentials*",
    // 凭据文件（command_guard 的敏感路径片段已把它们视为高危，这里保持同一标准）
    "**/.git-credentials",
    "**/.netrc",
    "**/.npmrc",
    "**/.pypirc",
    "**/.docker/config.json",
    "**/AppData/Roaming/**",
    "**/AppData/Local/**",
    "**/.git/config",
];

/// 默认允许执行的外部程序
///
/// 内置默认项**始终有效**：`CommandGuard::from_allowlist` 会无条件合并本列表，
/// 用户只能在默认项之外追加，不能通过删配置移除它们（想彻底禁用请关闭工具
/// 或改用只读模式）。真正的黑名单（shell、计划任务、注册表、网络下载器、
/// `npx`/`bunx` 这类"下载即执行"的包运行器）硬编码在
/// `agent::harness::command_guard` 中且**优先级更高**，用户配置无法放行；
/// 代执行类参数（`find -exec`、`git -c alias.x=!cmd`、`npm exec`/`pnpm dlx`）
/// 也在参数审查里一律拒绝。
pub const DEFAULT_COMMAND_ALLOWLIST: &[&str] = &[
    "git", "cargo", "rustc", "rustup", "node", "npm", "pnpm", "yarn", "deno", "bun",
    "python", "python3", "pip", "pip3", "uv", "go", "java", "javac", "mvn", "gradle", "dotnet",
    // 解释器允许执行脚本文件，但 `-c` / `-e` 等求值参数由命令守卫一律拒绝
    "perl", "ruby", "php", "lua",
    "tsc", "eslint", "prettier", "pytest", "make", "cmake", "ls", "cat", "head", "tail", "wc",
    "grep", "find", "echo", "pwd", "whoami",
];

fn default_max_steps() -> usize {
    8
}

fn default_max_output_bytes() -> usize {
    64 * 1024
}

fn default_approval_timeout_secs() -> u64 {
    120
}

/// 单次工具调用的超时（秒）
///
/// 这个值曾经写死为 60 秒，导致 `cargo build` 这类首次编译要几分钟的命令必然超时；
/// 现在可配，默认仍是 60 秒以免改变既有行为。
fn default_call_timeout_secs() -> u64 {
    60
}

/// 工具运行时配置（只作用于主窗口；悬浮窗恒不使用工具）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    /// 主开关
    pub enabled: bool,
    pub mode: ToolMode,
    /// 工作区根目录列表（空 → 校验时自动补默认根）
    pub workspaces: Vec<WorkspaceRoot>,
    /// 追加的拒绝模式（内置条目会被强制保留）
    pub deny_globs: Vec<String>,
    /// 免审批的工具名（当前会话内自动放行）
    pub auto_approve: Vec<String>,
    pub max_steps: usize,
    pub max_output_bytes: usize,
    pub approval_timeout_secs: u64,
    /// 单次工具调用的超时（`run_command` 会据此在超时前收手并保留部分输出）
    pub call_timeout_secs: u64,
    /// 工作记忆：模型主动记下的跨轮结论（默认开；关闭后相关工具明确报错且不再注入提示词）
    pub working_memory: bool,
    /// 额外允许执行的程序（不能覆盖内置黑名单）
    pub command_allowlist: Vec<String>,
    /// `web_fetch` 允许访问的域名（为空表示禁用联网工具）
    pub web_domain_allowlist: Vec<String>,
    /// 联网检索（`web_search`）
    pub search: SearchConfig,
    /// MCP 服务器（外部工具生态）
    pub mcp: McpConfig,
}

/// 联网检索配置
///
/// 为什么默认关闭：这是唯一会把**用户的问题文本**主动发给第三方的能力，
/// 必须由用户显式打开并填写自己的端点/密钥。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SearchConfig {
    pub enabled: bool,
    pub provider: SearchProvider,
    /// SearXNG 之类的自建实例地址；Tavily/Brave 用官方地址时留空
    pub endpoint: String,
    pub api_key: String,
    /// 默认返回条数（1~10）
    pub max_results: usize,
}

/// 检索后端
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchProvider {
    /// 自建 SearXNG（推荐：数据不出自己的机器）
    Searxng,
    Tavily,
    Brave,
}

impl SearchProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchProvider::Searxng => "searxng",
            SearchProvider::Tavily => "tavily",
            SearchProvider::Brave => "brave",
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: SearchProvider::Searxng,
            endpoint: String::new(),
            api_key: String::new(),
            max_results: 5,
        }
    }
}

impl SearchConfig {
    /// 直接可用的检索设置（端点与密钥齐备）
    pub fn resolved(&self) -> Option<ResolvedSearch> {
        if !self.enabled {
            return None;
        }
        let endpoint = self.endpoint.trim();
        let endpoint = if endpoint.is_empty() {
            match self.provider {
                SearchProvider::Tavily => "https://api.tavily.com/search",
                SearchProvider::Brave => "https://api.search.brave.com/res/v1/web/search",
                // 自建实例没有默认地址：没填就是没配好
                SearchProvider::Searxng => return None,
            }
        } else {
            endpoint
        };
        if matches!(self.provider, SearchProvider::Tavily | SearchProvider::Brave)
            && self.api_key.trim().is_empty()
        {
            return None;
        }
        if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
            return None;
        }
        Some(ResolvedSearch {
            provider: self.provider,
            endpoint: endpoint.to_string(),
            api_key: self.api_key.trim().to_string(),
            max_results: self.max_results.clamp(1, 10),
        })
    }
}

/// 已解析、可直接发起请求的检索设置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSearch {
    pub provider: SearchProvider,
    pub endpoint: String,
    pub api_key: String,
    pub max_results: usize,
}

/// MCP 配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

/// 一个 MCP 服务器
///
/// 安全默认值：`permission` 默认 `write`（即需要审批），`env` 默认为空
/// （**不继承应用的任何密钥**），`enabled` 默认 false —— 加配置不等于授权。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    /// 稳定标识（工具名会用到：`mcp:<id>:<tool>`）
    pub id: String,
    pub enabled: bool,
    /// 启动命令（必须是 PATH 中的程序）
    pub command: String,
    pub args: Vec<String>,
    /// 传给子进程的环境变量（只传这里写明的，不继承应用密钥）
    pub env: Vec<McpEnvVar>,
    /// 这些工具需要的权限（决定是否审批）
    pub permission: McpPermission,
    /// 用户是否确认过"这个服务器可信"（不勾选则完全不连接）
    pub trusted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct McpEnvVar {
    pub key: String,
    pub value: String,
}

/// MCP 工具的权限映射
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpPermission {
    /// 只读：不审批
    Read,
    /// 可能有副作用：审批（默认）
    #[default]
    Write,
    /// 执行外部动作：审批 + 只在完整模式可见
    Execute,
}

impl McpPermission {
    pub fn as_str(self) -> &'static str {
        match self {
            McpPermission::Read => "read",
            McpPermission::Write => "write",
            McpPermission::Execute => "execute",
        }
    }

    /// 映射到 harness 的权限等级
    pub fn to_permission(self) -> crate::agent::harness::Permission {
        match self {
            McpPermission::Read => crate::agent::harness::Permission::Read,
            McpPermission::Write => crate::agent::harness::Permission::WriteFs,
            McpPermission::Execute => crate::agent::harness::Permission::Execute,
        }
    }
}



impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ToolMode::default(),
            workspaces: vec![WorkspaceRoot::default()],
            deny_globs: DEFAULT_DENY_GLOBS.iter().map(|s| s.to_string()).collect(),
            auto_approve: Vec::new(),
            max_steps: default_max_steps(),
            max_output_bytes: default_max_output_bytes(),
            approval_timeout_secs: default_approval_timeout_secs(),
            call_timeout_secs: default_call_timeout_secs(),
            working_memory: true,
            command_allowlist: DEFAULT_COMMAND_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
            web_domain_allowlist: Vec::new(),
            search: SearchConfig::default(),
            mcp: McpConfig::default(),
        }
    }
}

/// 词法层面的路径包含判断（不访问文件系统，供配置校验使用）
///
/// 两边都必须是绝对路径；只有 Windows 的文件系统大小写不敏感，
/// Linux/macOS 上 `/Data` 与 `/data` 是两个互不嵌套的目录。
fn lexical_starts_with(child: &std::path::Path, parent: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let segment = c.as_os_str().to_string_lossy().to_string();
                if cfg!(windows) {
                    segment.to_ascii_lowercase()
                } else {
                    segment
                }
            })
            .collect()
    };
    let c = norm(child);
    let p = norm(parent);
    c.len() >= p.len() && c[..p.len()] == p[..]
}

impl ToolConfig {
    /// 校验工具配置（在落盘前调用）
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=32).contains(&self.max_steps) {
            return Err("工具步数上限必须在 1 ~ 32 之间".to_string());
        }
        if !(8 * 1024..=1024 * 1024).contains(&self.max_output_bytes) {
            return Err("工具输出上限必须在 8 KB ~ 1 MB 之间".to_string());
        }
        if !(5..=600).contains(&self.approval_timeout_secs) {
            return Err("审批超时必须在 5 ~ 600 秒之间".to_string());
        }
        // 上限 30 分钟：再长就该改用后台任务，而不是让一轮生成一直挂着
        if !(5..=1800).contains(&self.call_timeout_secs) {
            return Err("单次工具超时必须在 5 ~ 1800 秒之间".to_string());
        }
        if self.workspaces.len() > MAX_WORKSPACE_ROOTS {
            return Err(format!("工作区数量不能超过 {} 个", MAX_WORKSPACE_ROOTS));
        }

        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut seen_paths: Vec<std::path::PathBuf> = Vec::new();

        for root in &self.workspaces {
            if !is_valid_workspace_id(&root.id) {
                return Err(format!(
                    "工作区 id「{}」非法：只允许小写字母、数字、下划线与连字符，且需以字母或数字开头（≤32 字符）",
                    root.id
                ));
            }
            if !seen_ids.insert(root.id.as_str()) {
                return Err(format!("工作区 id 重复：{}", root.id));
            }
            if root.label.trim().is_empty() {
                return Err(format!("工作区「{}」的名称不能为空", root.id));
            }

            // 绝对路径可直接校验；哨兵路径在运行期展开，这里跳过
            if let WorkspacePath::Absolute(raw) = &root.path {
                let path = std::path::PathBuf::from(raw);
                if !path.is_absolute() {
                    return Err(format!("工作区「{}」必须使用绝对路径", root.id));
                }
                // 驱动器根 / 文件系统根一律拒绝
                if path.parent().is_none() {
                    return Err(format!("工作区「{}」不能是磁盘根目录", root.id));
                }
            }
        }

        // 重复路径与互相嵌套会让寻址与 deny_glob 语义变模糊
        for root in &self.workspaces {
            let path = match &root.path {
                WorkspacePath::Absolute(raw) => std::path::PathBuf::from(raw),
                WorkspacePath::AppDataWorkspace => continue,
            };
            for other in &seen_paths {
                if lexical_starts_with(&path, other) || lexical_starts_with(other, &path) {
                    return Err(format!(
                        "工作区路径与已有条目重复或互相嵌套：{}",
                        path.display()
                    ));
                }
            }
            seen_paths.push(path);
        }

        // MCP 服务器：id 会拼进工具名（`mcp:<id>:<tool>`），必须非空、唯一、
        // 且不含冒号等分隔符；重复 id 会让后者的工具在注册表里静默丢失
        let mut seen_mcp_ids: HashSet<&str> = HashSet::new();
        for server in &self.mcp.servers {
            if server.id.trim().is_empty() {
                return Err("MCP 服务器 id 不能为空".to_string());
            }
            if server.id.len() > 32
                || !server
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "MCP 服务器 id「{}」非法：只允许字母、数字、-、_（≤32 字符）",
                    server.id
                ));
            }
            if !seen_mcp_ids.insert(server.id.as_str()) {
                return Err(format!("MCP 服务器 id 重复：{}", server.id));
            }
            if server.enabled && server.command.trim().is_empty() {
                return Err(format!(
                    "MCP 服务器「{}」已启用但没有填写启动命令",
                    server.id
                ));
            }
        }

        if self
            .deny_globs
            .iter()
            .any(|g| g.trim().is_empty())
        {
            return Err("拒绝访问模式不能为空字符串".to_string());
        }

        // 内置拒绝条目**不可移除**：清空它意味着 config.json（API Key）、
        // *.db、.env、.ssh/** 重新可读。允许在末尾追加自定义模式。
        if let Some(missing) = self.missing_builtin_deny_globs().first() {
            return Err(format!(
                "拒绝访问模式缺少内置条目「{}」（内置条目不可移除，只能追加自定义模式）",
                missing
            ));
        }
        for pattern in &self.deny_globs {
            if globset::GlobBuilder::new(pattern).build().is_err() {
                return Err(format!("拒绝访问模式不是合法的 glob：{}", pattern));
            }
        }

        Ok(())
    }

    /// 内置拒绝模式是否完整保留（防止配置被改写后绕过）
    pub fn missing_builtin_deny_globs(&self) -> Vec<&'static str> {
        DEFAULT_DENY_GLOBS
            .iter()
            .filter(|g| !self.deny_globs.iter().any(|d| d == **g))
            .copied()
            .collect()
    }

    /// 构造只包含单个绝对路径根的配置（命令层与测试共用）
    pub fn with_single_root(path: &std::path::Path, writable: bool, label: &str) -> Self {
        Self {
            workspaces: vec![WorkspaceRoot {
                id: DEFAULT_WORKSPACE_ID.to_string(),
                label: label.to_string(),
                path: WorkspacePath::Absolute(path.display().to_string()),
                writable,
            }],
            ..Self::default()
        }
    }

    /// 构造包含指定一组根的配置（测试与多根场景复用）
    pub fn with_roots(roots: Vec<WorkspaceRoot>) -> Self {
        let cfg = Self {
            workspaces: roots,
            ..Self::default()
        };
        debug_assert!(cfg.validate().is_ok(), "测试配置必须合法");
        cfg
    }

    /// 补齐默认工作区（读取旧配置时使用，保证至少有一个可用根）
    pub fn ensure_workspaces(&mut self) {
        if self.workspaces.is_empty() {
            self.workspaces.push(WorkspaceRoot::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, url: &str) -> LlmProvider {
        LlmProvider::new(name, url, "sk-test")
    }

    #[test]
    fn active_provider_never_panics_on_empty_list() {
        let cfg = LlmConfig {
            providers: Vec::new(),
            active_provider_id: "missing".to_string(),
        };
        // 历史实现此处 expect 直接 panic
        assert_eq!(cfg.active_provider().id, fallback_provider().id);

        let mut cfg = cfg;
        assert!(cfg.active_provider_mut().is_none());
    }

    #[test]
    fn active_provider_falls_back_to_first_when_id_dangling() {
        let cfg = LlmConfig {
            providers: vec![provider("A", "https://a.example/v1")],
            active_provider_id: "not-exist".to_string(),
        };
        assert_eq!(cfg.active_provider().name, "A");
    }

    #[test]
    fn remove_provider_refuses_to_empty_the_list() {
        let mut cfg = LlmConfig {
            providers: vec![provider("only", "https://a.example/v1")],
            active_provider_id: String::new(),
        };
        cfg.active_provider_id = cfg.providers[0].id.clone();

        assert!(cfg.remove_provider(&cfg.providers[0].id.clone()).is_err());
        assert_eq!(cfg.providers.len(), 1, "最后一个提供商必须保留");
        assert!(cfg.remove_provider("不存在的 id").is_err());

        cfg.providers.push(provider("second", "https://b.example/v1"));
        let first_id = cfg.providers[0].id.clone();
        assert!(cfg.remove_provider(&first_id).is_ok());
        assert_eq!(cfg.providers.len(), 1);
        // 活跃 id 应切换到剩余提供商
        assert_eq!(cfg.active_provider().name, "second");
    }

    #[test]
    fn ensure_non_empty_repairs_legacy_broken_config() {
        let mut cfg = LlmConfig {
            providers: Vec::new(),
            active_provider_id: String::new(),
        };
        assert!(cfg.ensure_non_empty());
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.active_provider().id, cfg.active_provider_id);
        assert!(!cfg.ensure_non_empty(), "已修复时不应再次改动");
    }

    #[test]
    fn validate_rejects_self_destructive_config() {
        let mut cfg = AppConfig::default();
        assert!(cfg.validate().is_ok());

        // 空 providers：历史上会让下次启动直接崩溃
        cfg.llm.providers.clear();
        assert!(cfg.validate().is_err());
    }

    /// 内置敏感文件拒绝条目不可移除；非法 glob 必须在保存前被拒绝
    #[test]
    fn validate_guards_deny_globs() {
        let mut cfg = AppConfig::default();
        assert!(cfg.validate().is_ok());

        // 删掉任意一个内置条目（例如 config.json）→ 拒绝
        cfg.tools.deny_globs.retain(|g| !g.contains("config.json"));
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("内置条目"), "{err}");

        // 全清空 → 拒绝
        cfg.tools.deny_globs.clear();
        assert!(cfg.validate().is_err());

        // 非法 glob → 拒绝（而不是静默跳过、让用户以为生效了）
        let mut cfg = AppConfig::default();
        cfg.tools.deny_globs.push("[[[".to_string());
        assert!(cfg.validate().is_err());

        // 追加合法自定义模式 → 允许
        let mut cfg = AppConfig::default();
        cfg.tools.deny_globs.push("**/secrets/**".to_string());
        assert!(cfg.validate().is_ok());
    }

    /// MCP 服务器 id 必须非空、唯一、字符集合法，启用时必须填命令
    #[test]
    fn validate_guards_mcp_servers() {
        use crate::config::types::{McpPermission, McpServerConfig};

        let server = |id: &str| McpServerConfig {
            id: id.to_string(),
            permission: McpPermission::Read,
            ..Default::default()
        };

        let mut cfg = AppConfig::default();
        cfg.tools.mcp.servers = vec![server("")];
        assert!(cfg.validate().is_err(), "空 id 必须被拒绝");

        cfg.tools.mcp.servers = vec![server("dup"), server("dup")];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("重复"), "{err}");

        cfg.tools.mcp.servers = vec![server("bad:id")];
        assert!(cfg.validate().is_err(), "分隔符字符必须被拒绝");

        let mut enabled = server("ok");
        enabled.enabled = true;
        cfg.tools.mcp.servers = vec![enabled];
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("启动命令"), "{err}");

        // 合法配置通过
        let mut ok = server("my-server");
        ok.enabled = true;
        ok.command = "npx".to_string();
        cfg.tools.mcp.servers = vec![ok];
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
    }

    /// Linux 上大小写不同就是不同目录，不应判成"互相嵌套"
    #[cfg(target_os = "linux")]
    #[test]
    fn case_different_workspaces_are_not_nested_on_linux() {
        let base = std::env::temp_dir().join(format!("konata-case-{}", uuid::Uuid::new_v4()));
        let upper = base.join("Data");
        let lower = base.join("data");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::create_dir_all(&lower).unwrap();

        let mut cfg = AppConfig::default();
        cfg.tools.workspaces = vec![
            WorkspaceRoot {
                id: "upper".to_string(),
                label: "U".to_string(),
                path: WorkspacePath::Absolute(upper.display().to_string()),
                writable: true,
            },
            WorkspaceRoot {
                id: "lower".to_string(),
                label: "L".to_string(),
                path: WorkspacePath::Absolute(lower.display().to_string()),
                writable: true,
            },
        ];
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn validate_rejects_out_of_range_values() {
        let base = AppConfig::default();
        let id = base.llm.providers[0].id.clone();

        let mut cfg = base.clone();
        cfg.llm.providers[0].max_tokens = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = base.clone();
        cfg.llm.providers[0].temperature = 99.0;
        assert!(cfg.validate().is_err());

        let mut cfg = base.clone();
        cfg.llm.providers[0].api_base_url = String::new();
        assert!(cfg.validate().is_err());

        let mut cfg = base.clone();
        cfg.llm.providers[0].api_base_url = "ftp://example.com".to_string();
        assert!(cfg.validate().is_err());

        let mut cfg = base.clone();
        cfg.ui.font_size = 0;
        assert!(cfg.validate().is_err());

        // 活跃 id 悬空
        let mut cfg = base.clone();
        cfg.llm.active_provider_id = "ghost".to_string();
        assert!(cfg.validate().is_err());

        // 合法配置仍然通过（含 onboarding 阶段允许的空 model / 空 key）
        let mut cfg = base;
        cfg.llm.active_provider_id = id;
        assert!(cfg.validate().is_ok());
    }

    /// 单次工具超时可配：默认 60 秒，越界必须被拒（否则会写出"永远挂着"的配置）
    #[test]
    fn tool_call_timeout_is_configurable_with_bounds() {
        assert_eq!(ToolConfig::default().call_timeout_secs, 60);

        let mut cfg = AppConfig::default();
        cfg.tools.call_timeout_secs = 1800;
        assert!(cfg.validate().is_ok(), "30 分钟应当合法");

        cfg.tools.call_timeout_secs = 4;
        assert!(cfg.validate().is_err(), "低于 5 秒会误杀正常命令");

        cfg.tools.call_timeout_secs = 1801;
        assert!(cfg.validate().is_err(), "超过 30 分钟应当拒绝");

        // 旧配置里没有这个字段时必须落到默认值，而不是 0（0 会导致每次调用立即超时）
        let legacy: ToolConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.call_timeout_secs, 60);
    }

    // ─── 模型路由（自动选择） ───────────────────────

    /// 旧配置里没有 `models` 段时必须落到默认值（不自动路由、无悬空引用）
    #[test]
    fn missing_model_settings_fall_back_to_defaults() {
        let mut json = serde_json::to_value(AppConfig::default()).unwrap();
        json.as_object_mut().unwrap().remove("models");

        let cfg: AppConfig = serde_json::from_value(json).expect("旧配置必须能解析");
        assert!(!cfg.models.auto_by_default, "默认不自动路由");
        assert!(cfg.models.main.is_none());
        assert!(cfg.models.subs.is_empty());
        assert!(cfg.validate().is_ok(), "旧配置必须依然合法");
    }

    #[test]
    fn model_settings_reject_dangling_provider_references() {
        let mut cfg = AppConfig::default();
        let provider_id = cfg.llm.providers[0].id.clone();

        cfg.models.main = Some(ModelRef::new(provider_id.clone(), "m1"));
        cfg.models.subs = vec![ModelRef::new(provider_id.clone(), "m2")];
        assert!(cfg.validate().is_ok(), "引用真实提供商必须通过");

        // 主模型指向不存在的提供商
        cfg.models.main = Some(ModelRef::new("ghost", "m1"));
        assert!(cfg.validate().is_err());
        cfg.models.main = Some(ModelRef::new(provider_id.clone(), "m1"));

        // 子模型指向不存在的提供商
        cfg.models.subs = vec![ModelRef::new("ghost", "m2")];
        assert!(cfg.validate().is_err());

        // 空模型名
        cfg.models.subs = vec![ModelRef::new(provider_id.clone(), "  ")];
        assert!(cfg.validate().is_err());

        // 子模型重复
        cfg.models.subs = vec![
            ModelRef::new(provider_id.clone(), "m2"),
            ModelRef::new(provider_id.clone(), "m2"),
        ];
        assert!(cfg.validate().is_err(), "重复的子模型必须被拒绝");

        // 数量上限
        cfg.models.subs = (0..=MAX_SUB_MODELS)
            .map(|i| ModelRef::new(provider_id.clone(), format!("m{}", i)))
            .collect();
        assert!(cfg.validate().is_err());
    }

    /// 新增的 `thinking_models` 声明必须能被序列化往返（旧配置缺字段时为空）
    #[test]
    fn provider_thinking_models_round_trip() {
        let mut provider = LlmProvider::new("A", "https://a.example/v1", "sk");
        provider.thinking_models.push("my-model".to_string());

        let json = serde_json::to_string(&provider).unwrap();
        let back: LlmProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(back.thinking_models, vec!["my-model".to_string()]);

        // 旧配置没有该字段 → 空列表（而不是解析失败）
        let legacy: LlmProvider = serde_json::from_str(
            r#"{"id":"p","name":"n","api_base_url":"https://a/v1","api_key":"k","model":"m","max_tokens":100,"temperature":0.5}"#,
        )
        .unwrap();
        assert!(legacy.thinking_models.is_empty());
        assert!(!legacy.enable_thinking);
    }
}
