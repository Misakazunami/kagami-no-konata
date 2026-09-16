import { useCallback, useEffect, useRef, useState } from "react";
import { useChatStore } from "../../stores/chatStore";
import type { ToolDecision } from "../../types/events";
import {
  TOOL_PERMISSION_LABEL,
  TOOL_PERMISSION_RISK,
} from "../../types/tools";

/**
 * 把工具参数格式化成可读文本
 *
 * 后端可能下发对象，也可能下发已经序列化过的字符串；解析失败时按原样展示，
 * 保证任何输入都能看到内容（而不是一个报错占位）。
 */
function formatArgs(args: unknown): string {
  if (args === null || args === undefined) return "（无参数）";
  if (typeof args === "string") {
    try {
      return JSON.stringify(JSON.parse(args), null, 2);
    } catch {
      return args;
    }
  }
  try {
    return JSON.stringify(args, null, 2) ?? String(args);
  } catch {
    return String(args);
  }
}

/**
 * 工具审批弹窗
 *
 * 只在主窗口渲染（悬浮窗不订阅工具事件，`pendingApproval` 永远是 null）。
 *
 * 设计要点：
 * - 倒计时归零即调用拒绝：后端已超时，弹窗不能继续占着屏幕；
 * - Esc 与点击遮罩都等于「拒绝」——绝不能把用户困在模态框里；
 * - 参数用 `<pre>` 展示（不可信内容，不做任何解析）。
 */
export function ToolApprovalDialog() {
  const pendingApproval = useChatStore((s) => s.pendingApproval);
  const resolveApproval = useChatStore((s) => s.resolveApproval);
  const [remainingMs, setRemainingMs] = useState<number | null>(null);
  // 倒计时总时长（首次 tick 时的剩余量），仅用于进度条比例
  const totalRef = useRef(0);
  // 保证同一次审批只提交一次决定（倒计时与点击可能同时触发）
  const decidedRef = useRef(false);

  const decide = useCallback(
    (decision: ToolDecision) => {
      if (!pendingApproval || decidedRef.current) return;
      decidedRef.current = true;
      void resolveApproval(pendingApproval.approval_id, decision);
    },
    [pendingApproval, resolveApproval]
  );

  // 供定时器/Esc 回调使用，避免闭包拿到过期的 decide
  const decideRef = useRef(decide);
  decideRef.current = decide;

  // 倒计时
  useEffect(() => {
    decidedRef.current = false;
    totalRef.current = 0;
    if (!pendingApproval) {
      setRemainingMs(null);
      return;
    }

    const deadline = Date.parse(pendingApproval.expires_at);
    if (!Number.isFinite(deadline)) {
      // 时间戳异常时不倒计时也不自动拒绝，交给后端超时兜底
      setRemainingMs(null);
      return;
    }

    const tick = () => {
      const left = Math.max(0, deadline - Date.now());
      totalRef.current = Math.max(totalRef.current, left);
      setRemainingMs(left);
      if (left <= 0) {
        decideRef.current("deny");
      }
    };
    tick();
    const timer = setInterval(tick, 200);
    return () => clearInterval(timer);
  }, [pendingApproval]);

  // Esc = 拒绝
  useEffect(() => {
    if (!pendingApproval) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        decideRef.current("deny");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [pendingApproval]);

  if (!pendingApproval) return null;

  const risk = TOOL_PERMISSION_RISK[pendingApproval.permission];
  const seconds = remainingMs === null ? null : Math.ceil(remainingMs / 1000);
  const urgent = seconds !== null && seconds <= 15;
  const ratio =
    remainingMs === null || totalRef.current <= 0
      ? 1
      : Math.max(0, Math.min(1, remainingMs / totalRef.current));

  return (
    <div
      className="modal-overlay tool-approval-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="工具调用审批"
      onClick={() => decide("deny")}
    >
      <div className="tool-approval-card" onClick={(e) => e.stopPropagation()}>
        <div className="tool-approval-title">
          <span className="tool-approval-icon" aria-hidden="true">
            🔐
          </span>
          <span>请求调用工具</span>
        </div>

        <div className="tool-approval-meta">
          <span className="tool-approval-tool">
            {pendingApproval.tool_label || pendingApproval.tool}
          </span>
          <span className={`tool-approval-risk ${risk.level}`}>{risk.label}</span>
          <span className="tool-approval-permission">
            {TOOL_PERMISSION_LABEL[pendingApproval.permission]}
          </span>
        </div>

        {/* 工具自算的摘要（例如多步命令的步骤清单）优先展示：
            参数 JSON 适合机器看，人需要的是"到底要跑哪几条" */}
        {pendingApproval.summary && (
          <div className="tool-approval-summary-wrap">
            <span className="tool-approval-summary-title">将要执行</span>
            <pre className="tool-approval-summary">
              {pendingApproval.summary}
            </pre>
          </div>
        )}

        <div className="tool-approval-args-wrap">
          <span className="tool-approval-args-title">参数</span>
          <pre className="tool-approval-args">
            {formatArgs(pendingApproval.args)}
          </pre>
        </div>

        {seconds !== null && (
          <div className={`tool-approval-countdown ${urgent ? "urgent" : ""}`}>
            <div className="tool-approval-countdown-bar">
              <div
                className="tool-approval-countdown-fill"
                style={{ width: `${ratio * 100}%` }}
              />
            </div>
            <span className="tool-approval-countdown-text">
              ⏳ 剩余 {seconds} 秒，超时将自动拒绝
            </span>
          </div>
        )}

        <div className="tool-approval-actions">
          <button
            className="tool-approval-btn allow-once"
            onClick={() => decide("allow_once")}
          >
            仅本次允许
          </button>
          <button
            className="tool-approval-btn allow-session"
            onClick={() => decide("allow_session")}
          >
            本会话允许
          </button>
          <button
            className="tool-approval-btn deny"
            onClick={() => decide("deny")}
          >
            拒绝
          </button>
        </div>

        <div className="tool-approval-hint">按 Esc 或点击空白处视为拒绝</div>
      </div>
    </div>
  );
}
