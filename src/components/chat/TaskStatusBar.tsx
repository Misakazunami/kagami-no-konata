import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import type { WorkspaceView } from "../../types/tools";
import { AutoApproveConfirm } from "./AutoApproveConfirm";

function formatElapsed(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = (seconds % 60).toString().padStart(2, "0");
  return `${minutes}m${rest}s`;
}

function formatTokens(tokens: number): string {
  if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(2)}M`;
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(1)}k`;
  return String(tokens);
}

interface SessionUsage {
  session_id: string;
  message_tokens: number;
  tool_extra_tokens: number;
  total_tokens: number;
}

/**
 * 任务状态条（会话头部，仅任务会话显示）
 *
 * 把原本散落的信息聚到一处：当前模式、绑定的工作区、计划进度、本轮的
 * 工具步数（第 N/M 步）、生成耗时、是否在等审批，以及两个恢复入口
 * （Plan 阶段的"批准并执行"、Work 阶段的"继续任务"）。
 *
 * 数据全部来自 store 的会话级状态；工作区名称按需拉一次 `list_workspaces`，
 * 失败只影响标签显示，不影响对话。
 */
export function TaskStatusBar() {
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const plan = useChatStore((s) => s.plan);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const liveToolCalls = useChatStore((s) => s.liveToolCalls);
  const pendingApproval = useChatStore((s) => s.pendingApproval);
  const approvePlan = useChatStore((s) => s.approvePlan);
  const continueTask = useChatStore((s) => s.continueTask);
  const stepLimitHit = useChatStore((s) => s.stepLimitHit);
  const toolSteps = useChatStore((s) => s.toolSteps);
  const sessionGrants = useChatStore((s) => s.sessionGrants);
  const revokeSessionGrant = useChatStore((s) => s.revokeSessionGrant);
  const setSessionWorkspace = useChatStore((s) => s.setSessionWorkspace);
  const setSessionAutoApprove = useChatStore((s) => s.setSessionAutoApprove);
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [elapsed, setElapsed] = useState(0);
  const [usage, setUsage] = useState<SessionUsage | null>(null);
  const [autoConfirmOpen, setAutoConfirmOpen] = useState(false);

  const session = sessions.find((s) => s.id === currentSessionId);
  const isTask = session?.session_type === "task";
  const autoOn = session?.auto_approve_all ?? false;

  useEffect(() => {
    if (!isTask) return;
    invoke<WorkspaceView[]>("list_workspaces")
      .then(setWorkspaces)
      .catch((e) => console.error("Failed to load workspaces:", e));
  }, [isTask]);

  // 换会话 / 开始生成时收起确认弹窗：否则弹窗背后的会话已经变了
  useEffect(() => {
    setAutoConfirmOpen(false);
  }, [currentSessionId, isStreaming]);

  // 生成期间计时；完成/停止后清零，下一轮重新开始
  useEffect(() => {
    if (!isStreaming) {
      setElapsed(0);
      return;
    }
    const started = Date.now();
    const timer = setInterval(
      () => setElapsed(Math.floor((Date.now() - started) / 1000)),
      1000
    );
    return () => clearInterval(timer);
  }, [isStreaming]);

  // 会话级 token 用量：换会话时回读，每轮生成结束后刷新
  useEffect(() => {
    if (!isTask || !currentSessionId) {
      setUsage(null);
      return;
    }
    let cancelled = false;
    invoke<SessionUsage>("get_session_usage", { sessionId: currentSessionId })
      .then((result) => {
        if (!cancelled) setUsage(result);
      })
      .catch((e) => console.error("Failed to load session usage:", e));
    return () => {
      cancelled = true;
    };
  }, [isTask, currentSessionId, isStreaming]);

  if (!isTask || !session) return null;

  const workspace = workspaces.find((w) => w.id === session.workspace_id);
  const total = plan?.length ?? 0;
  const done = plan?.filter((item) => item.status === "done").length ?? 0;
  const unfinished = plan?.filter((item) => item.status !== "done").length ?? 0;
  // 当前生成已经用掉的工具轮次（子代理的内部调用不会累加到父级 step）
  const step = liveToolCalls.reduce((max, call) => Math.max(max, call.step), 0);
  const maxStep = liveToolCalls.find((call) => call.maxSteps)?.maxSteps;
  const mode = session.task_mode ?? "plan";

  return (
    <>
      <div className="task-status-bar">
        <span className={`task-status-mode ${mode}`} title="任务运行模式">
          {mode === "plan" ? "📋 Plan" : "⚡ Work"}
        </span>
        <button
          type="button"
          className={`task-status-chip auto ${autoOn ? "on" : ""}`}
          disabled={isStreaming}
          onClick={() =>
            autoOn
              ? void setSessionAutoApprove(session.id, false)
              : setAutoConfirmOpen(true)
          }
          title={
            autoOn
              ? "AUTO 已开：本会话所有需要审批的工具调用自动放行（点击关闭，下一条消息生效）"
              : "开启 AUTO：本会话自动允许全部审批类工具调用（需确认；硬安全边界不变）"
          }
        >
          {autoOn ? "⚡ AUTO 已开" : "⚡ AUTO"}
        </button>
        {workspaces.length > 0 && (
          <select
            className="task-status-chip task-status-ws-select"
            value={session.workspace_id ?? ""}
            disabled={isStreaming}
            onChange={(e) => setSessionWorkspace(session.id, e.target.value || null)}
            title={
              workspace
                ? `执行目录：${workspace.path}（生成中不可切换）`
                : "未绑定工作区：使用默认沙箱"
            }
            aria-label="任务执行工作区"
          >
            <option value="">默认沙箱</option>
            {workspaces.map((w) => (
              <option key={w.id} value={w.id}>
                {w.label}
                {w.available ? "" : "（不可用）"}
              </option>
            ))}
          </select>
        )}
        {total > 0 && (
          <span className="task-status-chip" title="任务计划进度">
            计划 {done}/{total}
          </span>
        )}
        {step > 0 && (
          <span
            className="task-status-chip"
            title={maxStep ? `工具预算 ${maxStep} 步` : "本次生成的第几步"}
          >
            第 {step}
            {maxStep ? `/${maxStep}` : ""} 步
          </span>
        )}
        {isStreaming && (
          <span className="task-status-chip" title="本轮生成已用时">
            ⏱ {formatElapsed(elapsed)}
          </span>
        )}
        {usage && usage.total_tokens > 0 && (
          <span
            className="task-status-chip"
            title={`消息 ${usage.message_tokens} tokens${
              usage.tool_extra_tokens > 0
                ? ` · 子代理等隐藏开销 ${usage.tool_extra_tokens} tokens`
                : ""
            }`}
          >
            🪙 {formatTokens(usage.total_tokens)}
          </span>
        )}
        {pendingApproval && (
          <span className="task-status-chip warn">⏳ 等待审批</span>
        )}
        {!isStreaming && stepLimitHit && (
          <span
            className="task-status-chip warn"
            title="本轮已到工具步数上限，模型被强制收尾；点右侧「继续任务」接着做"
          >
            ⚠ 步数用尽
            {toolSteps > 0 ? `（${toolSteps} 步）` : ""}中断
          </span>
        )}
        {sessionGrants.map((tool) => (
          <button
            key={tool}
            type="button"
            className="task-status-chip grant"
            onClick={() => revokeSessionGrant(tool)}
            title={`已允许本会话使用「${tool}」，点击撤销（撤销后下次调用重新弹审批）`}
          >
            🔓 {tool} ×
          </button>
        ))}
        {!isStreaming && mode === "plan" && unfinished > 0 && (
          <button
            type="button"
            className="task-status-action approve"
            onClick={approvePlan}
            title="切换到执行模式，并按这份计划开始执行"
          >
            ✓ 批准并执行
          </button>
        )}
        {!isStreaming && mode === "work" && (unfinished > 0 || stepLimitHit) && (
          <button
            type="button"
            className={`task-status-action${stepLimitHit ? " approve" : ""}`}
            onClick={continueTask}
            title="从计划里第一个未完成项继续执行"
          >
            继续任务
          </button>
        )}
      </div>
      <AutoApproveConfirm
        open={autoConfirmOpen}
        mode={mode}
        onConfirm={() => {
          setAutoConfirmOpen(false);
          void setSessionAutoApprove(session.id, true);
        }}
        onCancel={() => setAutoConfirmOpen(false)}
      />
    </>
  );
}
