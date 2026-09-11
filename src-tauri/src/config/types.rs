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
    #[serde(default)]
    pub enable_thinking: bool,
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
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub tools: ToolConfig,
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
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/*.pem",
    "**/*.key",
    "**/*.pfx",
    "**/credentials*",
    "**/AppData/Roaming/**",
    "**/AppData/Local/**",
    "**/.git/config",
];

/// 默认允许执行的外部程序
///
/// 这只是"白名单"的一半语义：真正的黑名单（shell、计划任务、注册表、
/// 网络下载器等）硬编码在 `agent::harness::command_guard` 中且**优先级更高**，
/// 用户配置只能收窄、不能放宽。
pub const DEFAULT_COMMAND_ALLOWLIST: &[&str] = &[
    "git", "cargo", "rustc", "rustup", "node", "npm", "pnpm", "yarn", "npx", "deno", "bun",
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
    /// 额外允许执行的程序（不能覆盖内置黑名单）
    pub command_allowlist: Vec<String>,
    /// `web_fetch` 允许访问的域名（为空表示禁用联网工具）
    pub web_domain_allowlist: Vec<String>,
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
            command_allowlist: DEFAULT_COMMAND_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
            web_domain_allowlist: Vec::new(),
        }
    }
}

/// 词法层面的路径包含判断（不访问文件系统，供配置校验使用）
///
/// 两边都必须是绝对路径；Windows 下大小写不敏感。
fn lexical_starts_with(child: &std::path::Path, parent: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
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

        if self
            .deny_globs
            .iter()
            .any(|g| g.trim().is_empty())
        {
            return Err("拒绝访问模式不能为空字符串".to_string());
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
}
