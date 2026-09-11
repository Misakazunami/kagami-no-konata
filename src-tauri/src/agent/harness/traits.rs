use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::config::types::{LlmProvider, ToolMode};
use crate::persona::engine::PersonaEngine;
use crate::store::chat_store::ChatStore;
use crate::store::memory_store::MemoryStore;

use super::jail::WorkspaceSet;

/// 工具风险等级
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// 只读：读文件、搜索、查时间、查状态
    Read,
    /// 只写应用数据目录（记忆、人设），不需要审批
    WriteApp,
    /// 写工作区文件，需要审批
    WriteFs,
    /// 执行外部命令，需要审批（且只在 `Full` 模式下可见）
    Execute,
    /// 访问网络，需要审批 + 域名白名单
    Network,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::WriteApp => "write_app",
            Permission::WriteFs => "write_fs",
            Permission::Execute => "execute",
            Permission::Network => "network",
        }
    }

    /// 中文风险描述（审批弹窗用）
    pub fn risk_label(self) -> &'static str {
        match self {
            Permission::Read => "低风险（只读）",
            Permission::WriteApp => "低风险（仅应用数据）",
            Permission::WriteFs => "中风险（写入文件）",
            Permission::Execute => "高风险（执行命令）",
            Permission::Network => "高风险（访问网络）",
        }
    }

    pub fn is_read_only(self) -> bool {
        matches!(self, Permission::Read)
    }

    /// 是否需要用户审批
    pub fn requires_approval(self) -> bool {
        matches!(
            self,
            Permission::WriteFs | Permission::Execute | Permission::Network
        )
    }

    /// 该等级的工具在指定模式下是否可见
    pub fn visible_in(self, mode: ToolMode) -> bool {
        match mode {
            // 只读模式：写入类与执行类工具直接从工具表移除
            ToolMode::ReadOnly => matches!(self, Permission::Read),
            // 标准模式：文件写入与联网需要审批，命令执行不可见
            ToolMode::Standard => !matches!(self, Permission::Execute),
            // 完整模式：全部可见（敏感命令仍然被硬拦截）
            ToolMode::Full => true,
        }
    }
}

/// 暴露给前端与模型的工具元信息
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub label: String,
    pub description: String,
    pub permission: Permission,
    pub read_only: bool,
    pub enabled: bool,
}

/// 工具的静态描述
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// JSON Schema（对象根）
    pub parameters: Value,
    pub permission: Permission,
}

impl ToolDescriptor {
    pub fn new(
        name: &'static str,
        label: &'static str,
        description: &'static str,
        permission: Permission,
        parameters: Value,
    ) -> Self {
        Self {
            name,
            label,
            description,
            parameters,
            permission,
        }
    }
}

/// 一次工具执行的结果
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 回灌给模型的文本（已按上限截断）
    pub content: String,
    /// 给 UI 卡片的短预览（不进 LLM 上下文）
    pub preview: Option<String>,
    pub truncated: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            preview: None,
            truncated: false,
        }
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

/// 工具可直接使用的外部服务句柄
///
/// 这里刻意不出现任何 Tauri 类型：`EventSink` 与 `Approver` 两个 trait
/// 已经把"发事件"和"请求审批"两处耦合抽象掉了，整条工具链因此可以在
/// 无 GUI 的单元测试里跑通。
#[derive(Clone)]
pub struct ToolServices {
    pub app_data_dir: PathBuf,
    pub workspaces: WorkspaceSet,
    pub mode: ToolMode,
    /// 当前活跃的 LLM 提供商（`search_memory` 需要 embedding）
    pub llm_provider: Option<LlmProvider>,
    pub memory: Option<Arc<Mutex<MemoryStore>>>,
    pub personas: Option<Arc<RwLock<PersonaEngine>>>,
    pub chat_store: Option<Arc<Mutex<ChatStore>>>,
    /// `web_fetch` 允许访问的域名
    pub web_domains: Vec<String>,
    /// `run_command` 的允许程序列表（敏感命令仍受硬黑名单约束）
    pub command_allowlist: Vec<String>,
    /// 打开本地文件/链接（可选，缺省时该工具不可用）
    pub opener: Option<Arc<dyn SystemOpener>>,
}

impl std::fmt::Debug for ToolServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolServices")
            .field("mode", &self.mode)
            .field("roots", &self.workspaces.list().len())
            .field("web_domains", &self.web_domains.len())
            .finish_non_exhaustive()
    }
}

impl ToolServices {
    /// 最小可用集合（单元测试用：不接数据库与人格引擎）
    pub fn minimal(app_data_dir: PathBuf, workspaces: WorkspaceSet, mode: ToolMode) -> Self {
        Self {
            app_data_dir,
            workspaces,
            mode,
            llm_provider: None,
            memory: None,
            personas: None,
            chat_store: None,
            web_domains: Vec::new(),
            command_allowlist: crate::config::types::DEFAULT_COMMAND_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
            opener: None,
        }
    }

    /// 默认工作区根目录的绝对路径（错误信息里用）
    pub fn default_root(&self) -> PathBuf {
        self.workspaces.default_path()
    }
}

/// 调用系统默认程序打开文件/链接
pub trait SystemOpener: Send + Sync {
    fn open(&self, target: &str) -> Result<()>;
}

/// 每次工具调用的上下文
pub struct ToolCtx<'a> {
    pub session_id: &'a str,
    pub stream_id: &'a str,
    pub step: usize,
    pub cancel: Arc<AtomicBool>,
    pub services: &'a ToolServices,
    pub limits: ToolLimits,
    pub emit: Arc<dyn EventSink>,
    pub approver: Arc<dyn Approver>,
}

impl ToolCtx<'_> {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 统一的取消检查：被取消时返回明确错误（由 runner 记成 cancelled 状态）
    pub fn ensure_not_cancelled(&self) -> Result<()> {
        if self.cancelled() {
            anyhow::bail!("已被用户停止");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToolLimits {
    pub max_output_bytes: usize,
    /// 单次工具执行超时
    pub call_timeout: Duration,
    /// 审批等待超时（超时按拒绝处理）
    pub approval_timeout: Duration,
}

/// 事件出口（生产环境为 Tauri 广播，测试里为记录器）
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
}

/// 丢弃所有事件的实现（无 GUI 场景 / 测试）
#[allow(dead_code)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &str, _payload: Value) {}
}

/// 用户的审批决定
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

impl ToolDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolDecision::AllowOnce => "allow_once",
            ToolDecision::AllowSession => "allow_session",
            ToolDecision::Deny => "deny",
        }
    }

    /// 从前后端约定的字符串解析（无法识别一律按拒绝处理）
    pub fn parse(raw: &str) -> ToolDecision {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allow_once" => ToolDecision::AllowOnce,
            "allow_session" => ToolDecision::AllowSession,
            _ => ToolDecision::Deny,
        }
    }
}

/// 审批请求
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub stream_id: String,
    pub call_id: String,
    pub tool: String,
    pub tool_label: String,
    pub args: Value,
    pub permission: Permission,
    pub timeout: Duration,
}

/// 审批通道
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn request(&self, req: ApprovalRequest) -> ToolDecision;
}

/// 一律拒绝（无审批 UI 时的 fail-closed 默认实现）
#[allow(dead_code)]
pub struct DenyAllApprover;

#[async_trait::async_trait]
impl Approver for DenyAllApprover {
    async fn request(&self, _req: ApprovalRequest) -> ToolDecision {
        ToolDecision::Deny
    }
}

/// 一律允许（单元测试用）
#[allow(dead_code)]
pub struct AllowAllApprover;

#[async_trait::async_trait]
impl Approver for AllowAllApprover {
    async fn request(&self, _req: ApprovalRequest) -> ToolDecision {
        ToolDecision::AllowOnce
    }
}

/// Agent 可调用的工具
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    /// `args` 已由 runner 解析为 JSON 对象；工具内部仍需自行校验每个字段
    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput>;
}

// ─── 文本截断工具 ────────────────────────────────────────

/// 找到不超过 `max` 的最大字符边界
pub fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut idx = max;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 找到不小于 `min` 的最小字符边界
pub fn ceil_char_boundary(text: &str, min: usize) -> usize {
    if min >= text.len() {
        return text.len();
    }
    let mut idx = min;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// 按字节上限截断文本，保留头尾并在中间标注丢弃量
///
/// 头尾都保留是因为：命令报错在尾部、文件结构在头部，只留头部会丢掉最关键的失败原因。
pub fn truncate_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let keep_head = max_bytes * 2 / 3;
    let keep_tail = max_bytes.saturating_sub(keep_head);
    let head_end = floor_char_boundary(text, keep_head);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(keep_tail));
    let dropped = tail_start.saturating_sub(head_end);
    let mut out = String::with_capacity(max_bytes + 64);
    out.push_str(&text[..head_end]);
    out.push_str(&format!("\n…[已截断 {} 字节]…\n", dropped));
    out.push_str(&text[tail_start..]);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_visibility_follows_mode() {
        assert!(Permission::Read.visible_in(ToolMode::ReadOnly));
        assert!(!Permission::WriteFs.visible_in(ToolMode::ReadOnly));
        assert!(!Permission::WriteApp.visible_in(ToolMode::ReadOnly));

        assert!(Permission::WriteFs.visible_in(ToolMode::Standard));
        assert!(!Permission::Execute.visible_in(ToolMode::Standard));

        assert!(Permission::Execute.visible_in(ToolMode::Full));
    }

    #[test]
    fn only_side_effecting_permissions_need_approval() {
        assert!(!Permission::Read.requires_approval());
        assert!(!Permission::WriteApp.requires_approval());
        assert!(Permission::WriteFs.requires_approval());
        assert!(Permission::Execute.requires_approval());
        assert!(Permission::Network.requires_approval());
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = "A".repeat(100) + &"B".repeat(100);
        let (out, truncated) = truncate_text(&text, 60);
        assert!(truncated);
        assert!(out.starts_with("AAA"));
        assert!(out.ends_with("BBB"));
        assert!(out.contains("已截断"));
    }

    #[test]
    fn truncate_is_multibyte_safe() {
        // 3 字节一个汉字：若按字节硬切会 panic
        let text = "此".repeat(50);
        let (out, truncated) = truncate_text(&text, 40);
        assert!(truncated);
        assert!(out.contains('此'));
    }

    #[test]
    fn truncate_leaves_short_text_untouched() {
        let (out, truncated) = truncate_text("短", 64);
        assert_eq!(out, "短");
        assert!(!truncated);
    }

    #[test]
    fn decision_parse_is_fail_closed() {
        assert_eq!(ToolDecision::parse("allow_once"), ToolDecision::AllowOnce);
        assert_eq!(
            ToolDecision::parse("allow_session"),
            ToolDecision::AllowSession
        );
        assert_eq!(ToolDecision::parse("deny"), ToolDecision::Deny);
        // 无法识别的输入必须按拒绝处理，不能默认放行
        assert_eq!(ToolDecision::parse("yes"), ToolDecision::Deny);
        assert_eq!(ToolDecision::parse(""), ToolDecision::Deny);
    }
}
