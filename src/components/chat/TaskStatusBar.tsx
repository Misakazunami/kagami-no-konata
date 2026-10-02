import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import type { WorkspaceView } from "../../types/tools";
import { AutoApproveConfirm } from "./AutoApproveConfirm";
import {
  IconAlert,
  IconCheck,
  IconClock,
  IconClipboardList,
  IconCoins,
  IconFolder,
  IconMore,
  IconUnlock,
  IconWrench,
  IconX,
  IconZap,
} from "../icons";

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
 * 顶栏只保留"一眼要看的四件事"，其余全部收进「详情」弹层：
 * 1. 当前模式（Plan 只读调查 / Work 可写执行）；
 * 2. 计划进度（`已完成/总数`，与底部计划面板同一份数据）；
 * 3. AUTO 是否已开——这是本会话唯一"放宽审批"的开关，开着时必须一直可见；
 * 4. 需要用户动手的信号（等待审批 / 步数用尽）与主操作按钮。
 *
 * 工作区选择、工具步数、token 用量、本会话已授权的工具属于"想知道才看"的信息，
 * 且授权条数不封顶——留在顶栏会把状态条撑爆（每个工具一个 chip），因此放进弹层。
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
  const [detailsOpen, setDetailsOpen] = useState(false);
  const detailsRef = useRef<HTMLDivElement>(null);

  const session = sessions.find((s) => s.id === currentSessionId);
  const isTask = session?.session_type === "task";
  const autoOn = session?.auto_approve_all ?? false;

  useEffect(() => {
    if (!isTask) return;
    invoke<WorkspaceView[]>("list_workspaces")
      .then(setWorkspaces)
      .catch((e) => console.error("Failed to load workspaces:", e));
  }, [isTask]);

  // 换会话 / 开始生成时收起弹层与确认弹窗：它们描述的是"上一轮/上一个会话"的状态
  useEffect(() => {
    setAutoConfirmOpen(false);
    setDetailsOpen(false);
  }, [currentSessionId, isStreaming]);

  // 详情弹层：点击别处或 Esc 关闭（不拦截 AUTO 确认弹窗自己的 Esc）
  useEffect(() => {
    if (!detailsOpen) return;
    const onPointerDown = (event: MouseEvent) => {
      if (!detailsRef.current?.contains(event.target as Node)) setDetailsOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setDetailsOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    window.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [detailsOpen]);

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
        <div className="task-status-chips">
          {/* 模式：Plan=只读调查，Work=可写执行，两者的工具可见性完全不同 */}
          <span className={`task-status-mode ${mode}`} title="任务运行模式">
            {mode === "plan" ? (
              <>
                <IconClipboardList /> Plan
              </>
            ) : (
              <>
                <IconZap /> Work
              </>
            )}
          </span>

          {/* 计划进度：只报数字，明细在输入框上方的计划面板里 */}
          {total > 0 && (
            <span
              className="task-status-chip"
              title={`任务计划进度：已完成 ${done}/${total}`}
            >
              计划 {done}/{total}
            </span>
          )}

          <button
            type="button"
            className={`task-status-chip auto ${autoOn ? "on" : ""}`}
            disabled={isStreaming}
            onClick={() => {
              if (autoOn) {
                void setSessionAutoApprove(session.id, false);
                return;
              }
              // 确认弹窗独占键盘（Esc=取消）：先收起详情，避免一次 Esc 关掉两层
              setDetailsOpen(false);
              setAutoConfirmOpen(true);
            }}
            title={
              autoOn
                ? "AUTO 已开：本会话所有需要审批的工具调用自动放行（点击关闭，下一条消息生效）"
                : "开启 AUTO：本会话自动允许全部审批类工具调用（需确认；硬安全边界不变）"
            }
          >
            <IconZap /> {autoOn ? "AUTO 已开" : "AUTO"}
          </button>

          {pendingApproval && (
            <span className="task-status-chip warn" title="有工具调用正在等你审批">
              <IconClock /> 等待审批
            </span>
          )}

          {!isStreaming && stepLimitHit && (
            <span
              className="task-status-chip warn"
              title="本轮已到工具步数上限，模型被强制收尾；点右侧「继续任务」接着做"
            >
              <IconAlert /> 步数用尽
              {toolSteps > 0 ? `（${toolSteps} 步）` : ""}中断
            </span>
          )}

          {isStreaming && (
            <span className="task-status-chip" title="本轮生成已用时">
              <IconClock /> {formatElapsed(elapsed)}
            </span>
          )}
        </div>

        <div className="task-status-actions">
          {!isStreaming && mode === "plan" && unfinished > 0 && (
            <button
              type="button"
              className="task-status-action approve"
              onClick={approvePlan}
              title="切换到执行模式，并按这份计划开始执行"
            >
              <IconCheck /> 批准并执行
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
      </div>

      {/* 详情：工作区 / 步数 / token / 已授权工具。挂在状态条之外：
          状态条是 flex 行，弹层放进去会变成一个占位的 flex 子项（而不是浮层） */}
      <div className="task-status-more" ref={detailsRef}>
        <button
          type="button"
          className={`task-status-chip task-status-more-btn${detailsOpen ? " open" : ""}`}
          aria-expanded={detailsOpen}
          aria-haspopup="true"
          onClick={() => setDetailsOpen((prev) => !prev)}
          title="更多状态：执行工作区、工具步数、token 用量、本会话已授权的工具"
        >
          <IconMore /> 详情
          {sessionGrants.length > 0 && (
            <span className="task-status-more-count" title={`本会话已授权 ${sessionGrants.length} 个工具`}>
              {sessionGrants.length}
            </span>
          )}
        </button>

        {detailsOpen && (
          <div className="task-status-panel" role="group" aria-label="任务会话详情">
            <div className="task-status-row">
              <span className="task-status-row-label">
                <IconFolder /> 工作区
              </span>
              {workspaces.length > 0 ? (
                <select
                  className="task-status-row-control"
                  value={session.workspace_id ?? ""}
                  disabled={isStreaming}
                  onChange={(e) => setSessionWorkspace(session.id, e.target.value || null)}
                  aria-label="任务执行工作区"
                  title={
                    isStreaming
                      ? "生成中不可切换工作区"
                      : workspace
                        ? `执行目录：${workspace.path}`
                        : "未绑定工作区：使用默认沙箱"
                  }
                >
                  <option value="">默认沙箱</option>
                  {workspaces.map((w) => (
                    <option key={w.id} value={w.id}>
                      {w.label}
                      {w.available ? "" : "（不可用）"}
                    </option>
                  ))}
                </select>
              ) : (
                <span className="task-status-row-value">默认沙箱</span>
              )}
            </div>

            <div className="task-status-row">
              <span className="task-status-row-label">
                <IconWrench /> 工具步数
              </span>
              <span className="task-status-row-value">
                {step > 0
                  ? `第 ${step}${maxStep ? `/${maxStep}` : ""} 步`
                  : maxStep
                    ? `本轮预算 ${maxStep} 步`
                    : "本轮尚未调用工具"}
              </span>
            </div>

            <div className="task-status-row">
              <span className="task-status-row-label">
                <IconCoins /> Token
              </span>
              <span
                className="task-status-row-value"
                title={
                  usage
                    ? `消息 ${usage.message_tokens} tokens${
                        usage.tool_extra_tokens > 0
                          ? ` · 子代理等隐藏开销 ${usage.tool_extra_tokens} tokens`
                          : ""
                      }（本会话累计）`
                    : "尚未产生用量统计"
                }
              >
                {usage && usage.total_tokens > 0
                  ? formatTokens(usage.total_tokens)
                  : "暂无"}
              </span>
            </div>

            <div className="task-status-row">
              <span className="task-status-row-label">
                <IconUnlock /> 本会话已允许
              </span>
              <span className="task-status-row-value">
                {sessionGrants.length === 0 ? "无" : `${sessionGrants.length} 项`}
              </span>
            </div>

            {sessionGrants.length > 0 && (
              <div className="task-status-grants">
                {sessionGrants.map((tool) => (
                  <button
                    key={tool}
                    type="button"
                    className="task-status-chip grant"
                    onClick={() => revokeSessionGrant(tool)}
                    title={`已允许本会话使用「${tool}」，点击撤销（撤销后下次调用重新弹审批）`}
                  >
                    <IconUnlock /> {tool} <IconX />
                  </button>
                ))}
              </div>
            )}

            <div className="task-status-panel-hint">
              工作区与 AUTO 的改动只对下一轮生成生效。
            </div>
          </div>
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
