/**
 * 工具（tool harness）相关的前端类型
 *
 * 这里只放**界面侧**的类型与小型映射表：
 * - `ToolInfo` / `ToolInvocation` / `WorkspaceView` 对应后端 4 个命令的返回值形状；
 * - `ToolCallView` 是把「开始 + 结果」两个事件合并后的渲染模型，供实时气泡使用。
 *
 * 事件载荷与 `ToolStatus` / `ToolPermission` / `ToolDecision` 枚举的唯一来源是
 * `types/events.ts`，本文件只做引用，不重复声明。
 */

import type { ToolPermission, ToolStatus } from "./events";

/** `list_tools()` 返回的单个工具信息 */
export interface ToolInfo {
  name: string;
  label: string;
  description: string;
  permission: string;
  read_only: boolean;
  enabled: boolean;
}

/** `get_tool_invocations({ sessionId })` 返回的一条工具调用记录 */
export interface ToolInvocation {
  id: string;
  session_id: string;
  message_id: string | null;
  stream_id: string;
  step: number;
  tool_name: string;
  tool_label: string;
  arguments_json: string;
  /** 后端 wire 值，使用前请经 `normalizeToolStatus` 兜底 */
  status: string;
  result_preview: string | null;
  error: string | null;
  truncated: boolean;
  duration_ms: number;
  approval: string | null;
  created_at: string;
}

/** `list_workspaces()` 返回的一个工作区根目录 */
export interface WorkspaceView {
  id: string;
  label: string;
  /** 已解析的绝对路径（默认工作区的哨兵由后端展开） */
  path: string;
  writable: boolean;
  /** 路径是否仍然存在可用 */
  available: boolean;
  /** 默认工作区不可删除 */
  is_default: boolean;
}

/**
 * 工具调用的渲染模型
 *
 * 由 `tool-call-start`（`status: "running"`）创建，`tool-call-result`
 * 按 `call_id` 回填结果字段；历史消息则由 `ToolInvocation` 转换而来。
 */
export interface ToolCallView {
  call_id: string;
  tool: string;
  tool_label: string;
  args_preview: string;
  permission: ToolPermission;
  step: number;
  status: ToolStatus;
  preview: string | null;
  duration_ms: number;
  truncated: boolean;
  error?: string;
}

/** 状态 → 中文文案（卡片上的状态胶囊） */
export const TOOL_STATUS_LABEL: Record<ToolStatus, string> = {
  running: "执行中",
  ok: "完成",
  error: "失败",
  denied: "已拒绝",
  cancelled: "已取消",
  timeout: "已超时",
};

/** 权限 → 中文文案（设置页里的工具清单） */
export const TOOL_PERMISSION_LABEL: Record<ToolPermission, string> = {
  read: "只读",
  write_app: "应用内写入",
  write_fs: "文件写入",
  execute: "执行命令",
  network: "网络访问",
};

/** 权限 → 风险等级（审批弹窗据此提示用户） */
export const TOOL_PERMISSION_RISK: Record<
  ToolPermission,
  { label: string; level: "low" | "medium" | "high" }
> = {
  read: { label: "低风险", level: "low" },
  write_app: { label: "中风险", level: "medium" },
  write_fs: { label: "中风险", level: "medium" },
  execute: { label: "高风险", level: "high" },
  network: { label: "高风险", level: "high" },
};

/** 权限模式 → 中文文案（配置里的 `tools.mode`） */
export const TOOL_MODE_LABEL: Record<string, string> = {
  read_only: "只读",
  standard: "标准",
  full: "完整",
};

/**
 * 归一化后端状态值
 *
 * 数据库里存的是字符串；遇到未知状态（例如后端新增了枚举）时按失败展示，
 * 避免界面上出现"看起来一切正常"的假象。
 */
export function normalizeToolStatus(raw: string): ToolStatus {
  switch (raw) {
    case "running":
    case "ok":
    case "error":
    case "denied":
    case "cancelled":
    case "timeout":
      return raw;
    default:
      return "error";
  }
}

/** 归一化权限值（`list_tools()` 的 permission 是字符串） */
export function normalizePermission(raw: string): ToolPermission {
  switch (raw) {
    case "read":
    case "write_app":
    case "write_fs":
    case "execute":
    case "network":
      return raw;
    default:
      return "read";
  }
}

/**
 * 历史记录 → 渲染模型
 *
 * 持久化记录里没有权限字段（权限只决定执行前是否需要审批），
 * 因此这里按 `read` 兜底；卡片本身不展示权限徽章，不会造成误读。
 */
export function toToolCallView(inv: ToolInvocation): ToolCallView {
  return {
    call_id: inv.id,
    tool: inv.tool_name,
    tool_label: inv.tool_label || inv.tool_name,
    args_preview: inv.arguments_json,
    permission: "read",
    step: inv.step,
    status: normalizeToolStatus(inv.status),
    preview: inv.result_preview,
    duration_ms: inv.duration_ms,
    truncated: inv.truncated,
    error: inv.error ?? undefined,
  };
}

/**
 * 渲染模型 → 历史记录
 *
 * 生成结束的那一刻，落库记录还没回读，而回读用的数据库消息 id 与前端即时生成的
 * 消息 id 并不相同。为了让工具卡片在本次会话里始终挂在正确的气泡上，
 * 这里把内存中的调用记录就地转成记录形状，挂到刚生成的消息 id 下。
 * 字段以"够用即可"为准（参数用后端给的预览，审批结果未知）。
 */
export function toToolInvocation(
  view: ToolCallView,
  context: { sessionId: string; streamId: string; messageId: string },
): ToolInvocation {
  return {
    id: view.call_id,
    session_id: context.sessionId,
    message_id: context.messageId,
    stream_id: context.streamId,
    step: view.step,
    tool_name: view.tool,
    tool_label: view.tool_label,
    arguments_json: view.args_preview,
    status: view.status,
    result_preview: view.preview,
    error: view.error ?? null,
    truncated: view.truncated,
    duration_ms: view.duration_ms,
    approval: null,
    created_at: new Date().toISOString(),
  };
}

/** 耗时展示：不足 1 秒显示毫秒，否则保留一位小数（如 `1.2s`） */
export function formatToolDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}
