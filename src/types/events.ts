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

/** `tool-call-start` 事件载荷：一次工具调用开始执行 */
export interface ToolCallStartData {
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
export interface ToolCallResultData {
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
