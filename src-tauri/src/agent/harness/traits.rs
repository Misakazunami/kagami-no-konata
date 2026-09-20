use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

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
    /// 只写**会话级状态**（任务计划、工作记忆），不需要审批；只读模式下也可用
    ///
    /// 单独一档的原因：Plan 模式要求模型调用 `update_plan` / `save_note` 维护进度，
    /// 但它们写的是应用自己的会话数据、不碰工作区、不外发。若沿用 `WriteApp`
    /// 会被只读模式过滤掉（提示词与工具可见性自相矛盾）；若放宽 `WriteApp`
    /// 则连长期记忆写入也会在只读模式暴露。
    WriteSession,
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
            Permission::WriteSession => "write_session",
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
            Permission::WriteSession => "低风险（仅会话数据）",
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
            // 只读模式：写入类与执行类工具直接从工具表移除；
            // 会话级状态（计划/笔记）例外——它们是模型维护进度的载体
            ToolMode::ReadOnly => matches!(self, Permission::Read | Permission::WriteSession),
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
    /// 工具描述（`String` 而不是 `&'static str`：像工作记忆这种描述里要带
    /// 上/下限数字的工具，写死字面量迟早与常量漂移）
    pub description: String,
    /// JSON Schema（对象根）
    pub parameters: Value,
    pub permission: Permission,
}

impl ToolDescriptor {
    pub fn new(
        name: &'static str,
        label: &'static str,
        description: impl Into<String>,
        permission: Permission,
        parameters: Value,
    ) -> Self {
        Self {
            name,
            label,
            description: description.into(),
            parameters,
            permission,
        }
    }
}

/// 工具调用事件名（唯一来源）
///
/// 三个事件都由 runner / 工具通过 [`EventSink`] 发出，载荷里**必须**同时带
/// `session_id` 与 `stream_id`（前端按二者过滤），另外都带 `call_id` 用于配对。
/// `runner` 只做 `pub use` 转出，避免两处各写一遍字面量。
pub const EVENT_TOOL_START: &str = "tool-call-start";
pub const EVENT_TOOL_RESULT: &str = "tool-call-result";
/// 执行中的增量输出（仅 UI：**绝不进 LLM 上下文**）
pub const EVENT_TOOL_OUTPUT: &str = "tool-output-chunk";
/// 任务计划更新（`update_plan` 工具写库后广播，主窗口据此渲染进度面板）
pub const EVENT_PLAN_UPDATED: &str = "plan-updated";
/// 工作记忆变化（`save_note` / `forget_note` 写库后广播；只带计数，内容由命令回读）
pub const EVENT_NOTES_UPDATED: &str = "notes-updated";
/// 子代理生命周期状态更新（展示每个子任务的粗粒度进度：queued / running / done / error）
pub const EVENT_SUBAGENT_STATUS: &str = "subagent-status";

/// 单条输出流的截断上限（头部 1/3 + 尾部 2/3，与 [`truncate_text`] 同语义）
pub const OUTPUT_HEAD_TAIL_BYTES: usize = 48 * 1024;

/// 工具对本次调用状态的自我判定
///
/// 默认是 `Ok`（runner 只在工具自行收手时才需要它）：命令非零退出、被超时打断、
/// 用户中途停止，都属于"内容仍然有效但结果不是成功"的情形——此时工具必须如实
/// 上报，否则多步命令里"第一步构建失败、后面被跳过"会被界面显示成"完成"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    /// 执行完成但结果失败（例如命令退出码非零）
    Error,
    /// 被超时打断，`content` 是超时前的部分输出
    Timeout,
    /// 用户停止了本轮生成，`content` 是停止前的部分输出
    Cancelled,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Timeout => "timeout",
            ToolStatus::Cancelled => "cancelled",
        }
    }

    /// 非成功状态在记录里要附带一句人类可读的说明
    pub fn error_note(self) -> Option<&'static str> {
        match self {
            ToolStatus::Ok => None,
            ToolStatus::Error => Some("命令以非零退出码结束"),
            ToolStatus::Timeout => Some("工具执行超时，已返回超时前的部分输出"),
            ToolStatus::Cancelled => Some("用户已停止本轮生成，已返回停止前的部分输出"),
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
    /// 本次调用的状态（默认 `Ok`）
    pub status: ToolStatus,
    /// 这次调用**额外**消耗的 token 估算（子代理自己发起过生成时才有值）
    ///
    /// 不是精确计费，只是为了让"隐藏开销"对用户可见：子代理的用量会累加进
    /// 本轮的 `message-stats`，否则用户会看到"我只问了一句话怎么涨这么多"。
    pub extra_tokens: usize,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            preview: None,
            truncated: false,
            status: ToolStatus::Ok,
            extra_tokens: 0,
        }
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }

    /// 覆盖状态（部分输出 + 超时/取消/失败）
    pub fn with_status(mut self, status: ToolStatus) -> Self {
        self.status = status;
        self
    }

    /// 声明这次调用额外消耗的 token 估算（会累加进本轮统计）
    pub fn with_extra_tokens(mut self, tokens: usize) -> Self {
        self.extra_tokens = tokens;
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
    /// 写类工具改动前的文件快照（可选；缺省表示这次改动不可回滚）
    pub snapshots: Option<Arc<super::snapshot::SnapshotStore>>,
    /// 只读子代理运行时（可选；`None` 时 `spawn_subagents` 不可用——子代理自身也拿不到它，
    /// 因此"子代理不能再生子代理"是由构造方式保证的，而不是靠检查参数）
    pub subagent: Option<Arc<super::subagent::AgentRuntime>>,
    /// 工作记忆是否启用（关闭时 `save_note` / `forget_note` 明确报错，提示词也不再注入）
    pub working_memory: bool,
    /// 已解析的联网检索设置（`None` 表示未启用/未配好，`web_search` 会明确报错）
    pub search: Option<crate::config::types::ResolvedSearch>,
    /// 规划模式（Plan）的文件只读、但允许**联网只读**（仍逐次审批）：
    /// 调查阶段需要查版本/报错资料。子代理与普通只读会话恒为 false
    pub plan_network: bool,
    /// `spawn_subagents` 的单次整体时间预算
    ///
    /// 它把整个子代理批次包在一次工具调用里，普通工具的 `call_timeout`
    /// 根本不够用；带软截止的批次会在到点前返回已完成的部分结论
    pub subagent_timeout: Duration,
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
            snapshots: None,
            subagent: None,
            working_memory: false,
            search: None,
            plan_network: false,
            subagent_timeout: Duration::from_secs(
                super::subagent::DEFAULT_SUBAGENT_TIMEOUT_SECS,
            ),
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
    /// 本次调用的 id：工具自己发增量事件时要带上，前端靠它配对
    pub call_id: &'a str,
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
    /// 工具自算的人类可读摘要（例如多步命令的步骤清单）
    ///
    /// 参数 JSON 适合机器看、不适合快速判断，审批弹窗会优先展示这段文字。
    pub summary: Option<String>,
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

    /// 工具当前是否可用（注册表据此从"可见/可调用"集合中剔除）
    ///
    /// 绝大多数工具是静态的，默认恒为 true；动态来源（MCP 服务器）需要覆写它，
    /// 让用户在设置里取消信任/停用服务器后**立即**生效，而不是等到重启。
    fn enabled(&self) -> bool {
        true
    }

    /// `args` 已由 runner 解析为 JSON 对象；工具内部仍需自行校验每个字段
    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput>;

    /// 本次调用的时间预算（`None` = 沿用 `ToolLimits.call_timeout`）
    ///
    /// 只有"一次调用内部要跑完整段子流程"的工具需要覆盖它：目前只有
    /// `spawn_subagents`（它包住整个子代理批次，普通工具的 60 秒会把批次掐死）。
    fn timeout_budget(&self, _services: &ToolServices) -> Option<Duration> {
        None
    }

    /// 审批弹窗里展示的人类可读摘要（`None` 时前端只显示参数 JSON）
    ///
    /// 默认不提供：绝大多数工具的参数本身就够清楚，只有"一眼看不出要干什么"的
    /// 工具才需要自己算一份（多步命令的步骤清单、删除操作的规模统计等）。
    ///
    /// **实现约定**：只允许做**只读**检查（stat/遍历/解析），
    /// 绝不允许产生副作用——用户还没批准这次调用。
    fn approval_summary(&self, _args: &Value, _cx: &ToolCtx<'_>) -> Option<String> {
        None
    }
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

/// 粗略估算文本的 token 数
///
/// 中英分别加权：ASCII 约 4 字符/token，CJK 约 1.4 字符/token。
/// 这里刻意不用 tiktoken 之类：统计只用于界面展示与子代理开销提示，
/// 引入分词器会把"聊天"变成"下载模型"。
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut wide = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    let tokens = ascii / 4 + (wide * 10).div_ceil(14);
    tokens.max(1)
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

/// 增量版的头尾保留缓冲
///
/// 流式收集命令输出时**不能**先全存进内存再截断：一条 `cargo build` 可以吐出
/// 上百 MB。这里以行为单位增量维护"头部 1/3 + 尾部 2/3"，超过上限的部分边收边丢，
/// 内存占用恒定为 `max_bytes` 量级，语义与 [`truncate_text`] 一致（含省略标记）。
///
/// `Clone` 是为了让读取任务与调用方共享（`Arc<Mutex<..>>`）时能取一份快照收尾：
/// 进程被杀掉后读取任务可能还卡在管道上，此时不能等它把所有权还回来。
#[derive(Clone)]
pub struct HeadTailBuffer {
    head: String,
    tail: VecDeque<String>,
    head_limit: usize,
    tail_limit: usize,
    tail_bytes: usize,
    dropped: usize,
    /// 头部是否已关闭（有内容进过尾部）；关闭后所有行都只能进尾部
    head_closed: bool,
}

impl HeadTailBuffer {
    pub fn new(max_bytes: usize) -> Self {
        let head_limit = max_bytes / 3;
        Self {
            head: String::new(),
            tail: VecDeque::new(),
            head_limit,
            tail_limit: max_bytes.saturating_sub(head_limit),
            tail_bytes: 0,
            dropped: 0,
            head_closed: false,
        }
    }

    /// 压入一行（不含换行符）；超长单行会被截成尾部，保证状态有界
    pub fn push_line(&mut self, line: &str) {
        let mut rest = line;
        // 单行就可能超过尾部预算（例如一段压缩后的 JSON）：先取它的尾段，
        // 并且必须按字符边界切，否则后续 `&str` 操作会 panic
        if rest.len() > self.tail_limit && self.tail_limit > 0 {
            let start = ceil_char_boundary(rest, rest.len() - self.tail_limit);
            self.dropped += start;
            rest = &rest[start..];
        }

        let entry_len = rest.len() + 1;
        // 头部一旦关闭（有内容进了尾部）就不能再往头部追加：
        // 否则"长行之后又来了短行"会插到长行之前，输出顺序与执行顺序不一致
        if !self.head_closed && self.head.len() + entry_len <= self.head_limit {
            self.head.push_str(rest);
            self.head.push('\n');
            return;
        }
        self.head_closed = true;

        let entry = format!("{}\n", rest);
        self.tail_bytes += entry.len();
        self.tail.push_back(entry);
        while self.tail_bytes > self.tail_limit {
            match self.tail.pop_front() {
                Some(front) => {
                    self.tail_bytes -= front.len();
                    self.dropped += front.len();
                }
                None => break,
            }
        }
    }

    /// 取出最终文本与"是否截断"
    pub fn finish(self) -> (String, bool) {
        let truncated = self.dropped > 0;
        let mut out = self.head;
        if truncated {
            out.push_str(&format!("…[已省略 {} 字节]…\n", self.dropped));
        }
        for entry in self.tail {
            out.push_str(&entry);
        }
        (out, truncated)
    }

    /// 只保留最后 `max_lines` 行（用于 `cargo`/`git` 这类噪声很大的命令）
    ///
    /// 与字节上限相互独立：先按行裁剪，再交给 [`HeadTailBuffer`] 按字节兜底。
    pub fn tail_lines(text: &str, max_lines: usize) -> (String, bool) {
        let lines: Vec<&str> = text.lines().collect();
        if max_lines == 0 || lines.len() <= max_lines {
            return (text.to_string(), false);
        }
        let skipped = lines.len() - max_lines;
        let mut out = format!("…[已省略前 {} 行]…\n", skipped);
        out.push_str(&lines[skipped..].join("\n"));
        (out, true)
    }
}

/// 把执行中的增量输出发给 UI 的通道
///
/// 三重约束：
/// 1. **只进 UI**：这些片段绝不进入 LLM 上下文——回灌给模型的仍然只有
///    [`ToolOutput::content`]（runner 的不变式 1）；
/// 2. 合帧：不足 4 KB 且距上次刷新不足 200 ms 时只进缓冲，避免每个 token
///    都发一次事件（前端每个 chunk 都会触发一次 store 更新）；
/// 3. 载荷带全 `session_id` / `stream_id` / `call_id`，前端才能按会话与调用配对。
pub struct ToolStream {
    emit: Arc<dyn EventSink>,
    session_id: String,
    stream_id: String,
    call_id: String,
    tool: String,
    stream: &'static str,
    step: usize,
    pending: String,
    last_flush: Instant,
}

/// 触发一次刷新的字节阈值
const TOOL_STREAM_FLUSH_BYTES: usize = 4 * 1024;
/// 触发一次刷新的时间阈值
const TOOL_STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(200);
/// 单次事件的载荷上限（防止一行巨长输出把事件撑爆）
const TOOL_STREAM_CHUNK_BYTES: usize = 16 * 1024;

impl ToolStream {
    pub fn new(cx: &ToolCtx<'_>, tool: &str, stream: &'static str) -> Self {
        Self {
            emit: cx.emit.clone(),
            session_id: cx.session_id.to_string(),
            stream_id: cx.stream_id.to_string(),
            call_id: cx.call_id.to_string(),
            tool: tool.to_string(),
            stream,
            step: cx.step,
            pending: String::new(),
            last_flush: Instant::now(),
        }
    }

    pub fn push(&mut self, chunk: &str) {
        self.pending.push_str(chunk);
        if self.pending.len() >= TOOL_STREAM_FLUSH_BYTES
            || self.last_flush.elapsed() >= TOOL_STREAM_FLUSH_INTERVAL
        {
            self.flush();
        }
    }

    /// 立即把缓冲里的内容发出去（正常结束、超时、取消时都要调用）
    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.pending);
        self.last_flush = Instant::now();
        let mut rest = data.as_str();
        while !rest.is_empty() {
            let end = floor_char_boundary(rest, TOOL_STREAM_CHUNK_BYTES.min(rest.len()));
            let end = if end == 0 { rest.len() } else { end };
            self.emit.emit(
                EVENT_TOOL_OUTPUT,
                json!({
                    "session_id": self.session_id,
                    "stream_id": self.stream_id,
                    "call_id": self.call_id,
                    "tool": self.tool,
                    "stream": self.stream,
                    "step": self.step,
                    "data": &rest[..end],
                }),
            );
            rest = &rest[end..];
        }
    }
}

impl Drop for ToolStream {
    /// 兜底：任何提前返回的路径（`?` 报错、取消）都不会把已收到的输出吞掉
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_visibility_follows_mode() {
        assert!(Permission::Read.visible_in(ToolMode::ReadOnly));
        // 会话级写入（计划/工作记忆）在只读模式下必须可用，否则 Plan 阶段的
        // "必须调用 update_plan" 提示词与工具可见性互相矛盾
        assert!(Permission::WriteSession.visible_in(ToolMode::ReadOnly));
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
        assert!(!Permission::WriteSession.requires_approval());
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

    // ─── 增量头尾缓冲（流式命令输出用） ───

    /// 未超上限时必须**逐字保留**：不能因为分块收集就改变内容
    #[test]
    fn head_tail_buffer_keeps_short_input_intact() {
        let mut buffer = HeadTailBuffer::new(4096);
        for line in ["第一行", "second line", "第三行"] {
            buffer.push_line(line);
        }
        let (out, truncated) = buffer.finish();
        assert_eq!(out, "第一行\nsecond line\n第三行\n");
        assert!(!truncated);
    }

    /// 超出上限时丢中间、留头尾，并给出省略量
    #[test]
    fn head_tail_buffer_drops_the_middle() {
        let mut buffer = HeadTailBuffer::new(512);
        for i in 0..500 {
            buffer.push_line(&format!("line-{:04}", i));
        }
        let (out, truncated) = buffer.finish();
        assert!(truncated);
        assert!(out.starts_with("line-0000"), "头部必须保留");
        assert!(out.trim_end().ends_with("line-0499"), "尾部必须保留");
        assert!(out.contains("已省略"), "{out}");
        // 内存有界：512 字节上限 + 省略标记 + 至多一行溢出
        assert!(out.len() < 700, "实际 {} 字节", out.len());
    }

    /// 单行超长（无换行的巨型输出）也必须按字符边界安全切分
    #[test]
    fn head_tail_buffer_handles_oversized_single_line() {
        let mut buffer = HeadTailBuffer::new(64);
        let line = "此".repeat(100); // 300 字节，远超 64 字节上限
        buffer.push_line(&line);
        let (out, truncated) = buffer.finish();
        assert!(truncated);
        assert!(out.contains('此'));
        assert!(out.len() <= 64 + "已省略 字节\n".len() + 8, "实际 {} 字节", out.len());
    }

    /// 头部关闭后，短行不能再插到已进尾部的长行之前（输出必须保持执行顺序）
    ///
    /// 修复前：长行进尾部后头部仍未关闭，后续短行会继续追加进头部，
    /// 于是输出变成 "aaaa, cc, 省略标记, bbbb…"——因果顺序被调换。
    #[test]
    fn head_tail_buffer_never_reorders_lines() {
        // 上限 4096：head_limit=1365、tail=2731
        let mut buffer = HeadTailBuffer::new(4096);
        buffer.push_line("aaaa"); // 进头部
        let long = "b".repeat(2000); // 超头部预算 → 进尾部（并关闭头部）
        buffer.push_line(&long);
        buffer.push_line("cc"); // 头部已关闭 → 必须进尾部

        let (out, _truncated) = buffer.finish();
        let a = out.find("aaaa").expect("第一行");
        let b = out.find("bbbb").expect("第二行");
        let c = out.find("cc").expect("第三行");
        assert!(a < b && b < c, "行序必须保持执行顺序：{out}");
    }

    #[test]
    fn tail_lines_keeps_the_last_n() {
        let text = (0..100)
            .map(|i| format!("row-{:03}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, trimmed) = HeadTailBuffer::tail_lines(&text, 10);
        assert!(trimmed);
        assert!(out.contains("已省略前 90 行"), "{out}");
        assert!(out.contains("row-099"));
        assert!(!out.contains("row-089"));
        // 不裁剪时原样返回
        let (same, trimmed) = HeadTailBuffer::tail_lines("a\nb", 10);
        assert_eq!(same, "a\nb");
        assert!(!trimmed);
    }
}
