/**
 * 后端↔前端事件契约（唯一来源）
 *
 * 所有流式事件都由后端 `app.emit` 全局广播，主窗口与悬浮窗都会收到同一份载荷，
 * 因此**必须**同时携带 `session_id` 与 `stream_id`：
 * - `session_id` 用于跨会话隔离
 * - `stream_id` 用于跨窗口 / 跨请求隔离（两个窗口常常共用同一个"今日会话"）
 *
 * 历史问题：后端发的是裸字符串，而各组件按对象解包，过滤条件恒为 false，
 * 导致流式正文完全不渲染、`isStreaming` 永远无法复位。
 *
 * 工具调用（tool harness）的 4 个事件同样遵循上述契约：载荷类型与枚举字面量
 * 一律只在本文件声明，组件与 store 必须从这里导入，禁止各自重复声明。
 */

/** 流式正文 / 思考 / 结束事件载荷 */
export interface StreamEventData {
  session_id: string;
  stream_id: string;
  data: string;
}

/** `message-stats` 事件载荷 */
export interface MessageStatsData {
  session_id: string;
  stream_id: string;
  token_count: number;
  thinking_ms: number;
  /**
   * 本轮实际使用的模型（带主/子槽位说明）
   *
   * 自动选择下主轮次与子代理用的模型不同，界面必须能如实展示"这条是谁答的"。
   */
  model?: string;
}

/** `stream-error` 事件载荷（后端生成失败时发出） */
export interface StreamErrorData {
  session_id: string;
  stream_id: string;
  message: string;
}

/**
 * 工具执行状态（后端 wire 值，不要改动字面量）
 *
 * - `running` 正在执行
 * - `ok` 成功
 * - `error` 执行失败
 * - `denied` 用户拒绝 / 策略拒绝
 * - `cancelled` 生成被取消
 * - `timeout` 审批超时或执行超时
 */
export type ToolStatus =
  | "running"
  | "ok"
  | "error"
  | "denied"
  | "cancelled"
  | "timeout";

/**
 * 工具权限等级（决定是否需要审批与风险级别）
 *
 * `read` 只读；`write_app` 应用内写入；`write_fs` 文件系统写入；
 * `execute` 执行命令；`network` 访问网络。
 */
export type ToolPermission =
  | "read"
  | "write_app"
  | "write_fs"
  | "execute"
  | "network";

/** 审批决定（后端 wire 值） */
export type ToolDecision = "allow_once" | "allow_session" | "deny";

/**
 * 子代理事件附加字段
 *
 * 只读子代理的工具事件与父级共用 `session_id` / `stream_id`（否则按会话/流过滤的
 * 规则会直接把它们丢掉），额外的两个字段让界面把它们算在「子代理卡片」名下：
 * `parent_call_id` 是父级 `spawn_subagents` 那次调用的 id，`depth` 恒为 1。
 */
export interface NestedToolEventFields {
  parent_call_id?: string;
  depth?: number;
}

/** `tool-call-start` 事件载荷：一次工具调用开始执行 */
export interface ToolCallStartData extends NestedToolEventFields {
  session_id: string;
  stream_id: string;
  /** 同一次生成内唯一，`tool-call-result` 靠它配对 */
  call_id: string;
  /** 工具名（英文，用于图标与调试） */
  tool: string;
  /** 工具的中文展示名 */
  tool_label: string;
  /** 参数预览（已由后端截断，仅用于展示） */
  args_preview: string;
  permission: ToolPermission;
  /** 本次生成内的第几步（从 1 开始） */
  step: number;
}

/** `tool-call-result` 事件载荷：一次工具调用结束（成功或失败） */
export interface ToolCallResultData extends NestedToolEventFields {
  session_id: string;
  stream_id: string;
  call_id: string;
  status: ToolStatus;
  /** 结果预览，**不可信内容**，只能按纯文本渲染 */
  preview: string;
  duration_ms: number;
  /** 结果是否被后端截断 */
  truncated: boolean;
  /** 失败原因（`status === "error"` 时通常存在） */
  error?: string;
}

/**
 * `tool-output-chunk` 事件载荷：执行中的增量输出
 *
 * **仅供界面展示**：这些片段不会进入 LLM 上下文（回灌给模型的只有工具最终结果）。
 * 命令输出可能长达上百 MB，后端按 4 KB / 200 ms 合帧后推送，前端只保留末尾一段。
 */
export interface ToolOutputChunkData extends NestedToolEventFields {
  session_id: string;
  stream_id: string;
  call_id: string;
  tool: string;
  /** 输出流：stdout / stderr */
  stream: "stdout" | "stderr" | string;
  /** 本次生成内的第几步 */
  step: number;
  /** 增量文本片段（不可信内容，按纯文本渲染） */
  data: string;
}

/** `tool-approval-request` 事件载荷：需要用户确认才能继续 */
export interface ToolApprovalRequestData {
  session_id: string;
  stream_id: string;
  /** 审批标识，回传 `resolve_tool_approval` 用 */
  approval_id: string;
  call_id: string;
  tool: string;
  tool_label: string;
  /** 工具参数（原始 JSON 值，展示前需自行格式化） */
  args: unknown;
  /** 工具自算的人类可读摘要（例如多步命令的步骤清单），可能为 null */
  summary?: string | null;
  permission: ToolPermission;
  /** RFC3339 字符串；过期后后端视为拒绝，前端据此倒计时 */
  expires_at: string;
}

/** `tool-approval-resolved` 事件载荷：审批已被处理（可能由另一个窗口触发） */
export interface ToolApprovalResolvedData {
  session_id: string;
  stream_id: string;
  approval_id: string;
  decision: ToolDecision;
}

/**
 * 任务计划项状态（后端 wire 值，不要改动字面量）
 *
 * - `pending` 待办；`doing` 进行中；`done` 已完成；`blocked` 做不下去
 */
export type PlanStatus = "pending" | "doing" | "done" | "blocked";

/** 一个计划项（`update_plan` 工具写入、界面展示） */
export interface PlanItem {
  title: string;
  status: PlanStatus;
}

/**
 * `plan-updated` 事件载荷：会话级任务计划被整体覆盖写入
 *
 * 与其他流式事件不同，计划是**会话级**状态而不是某一次生成的状态，
 * 因此前端按 `session_id` 过滤（没有 `stream_id`）。
 */
export interface PlanUpdatedData {
  session_id: string;
  items: PlanItem[];
  note?: string | null;
}

/** `get_plan` 命令的返回值 */
export interface PlanView extends PlanUpdatedData {
  session_id: string;
  updated_at: string;
}

/**
 * 一条工作记忆（`save_note` 工具写、`get_notes` 读）
 *
 * 内容来自当时的工具输出，注入提示词时统一带 `<untrusted>` 标记。
 */
export interface SessionNote {
  id: string;
  title?: string | null;
  content: string;
  bytes: number;
  created_at: string;
}

/** `subagent-status` 事件载荷：子代理单个任务的生命周期状态 */
export interface SubagentStatusData {
  session_id: string;
  stream_id: string;
  parent_call_id: string;
  task_id: string;
  goal_preview: string;
  status: "queued" | "running" | "done" | "error" | "cancelled" | "skipped";
  duration_ms?: number;
  /** 自动选择时这条子任务实际用的子模型（未配置子模型时为空） */
  model?: string;
}

/** `notes-updated` 事件载荷：只带计数，正文由界面按需回读 */
export interface NotesUpdatedData {
  session_id: string;
  count: number;
  removed?: number;
}

/**
 * `get_snapshot` 命令的返回值：某一次生成造成的文件改动备份概况
 *
 * 只要 `files > 0`，界面上就会出现"回滚"入口——这是让模型动手改文件的前提。
 */
export interface SnapshotInfoView {
  session_id: string;
  stream_id: string;
  files: number;
  bytes: number;
}

/** `restore_snapshot` 命令的返回值 */
export interface RestoreReportView {
  restored: number;
  /** 备份文件已不存在（被清理过）而跳过的条目 */
  missing: number;
  errors: string[];
}

export const STREAM_EVENT = {
  chunk: "stream-chunk",
  thinking: "stream-thinking-chunk",
  end: "stream-end",
  error: "stream-error",
  stats: "message-stats",
} as const;

/** 工具调用事件名（与后端 `app.emit` 的字面量一一对应） */
export const TOOL_EVENT = {
  start: "tool-call-start",
  result: "tool-call-result",
  /** 执行中的增量输出（仅 UI，不进 LLM 上下文） */
  outputChunk: "tool-output-chunk",
  /** 会话级任务计划被更新（按 session_id 过滤，无 stream_id） */
  planUpdated: "plan-updated",
  /** 工作记忆发生变化（会话级事件，只带计数） */
  notesUpdated: "notes-updated",
  /** 子代理执行进度状态变更 */
  subagentStatus: "subagent-status",
  approvalRequest: "tool-approval-request",
  approvalResolved: "tool-approval-resolved",
} as const;

/** 载荷是否属于指定的一次生成 */
export function isSameStream(
  payload: { stream_id?: string } | undefined | null,
  streamId: string | null,
): boolean {
  if (!payload || !streamId) return false;
  return payload.stream_id === streamId;
}
