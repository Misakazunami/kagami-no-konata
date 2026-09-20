import { useEffect } from "react";

interface AutoApproveConfirmProps {
  open: boolean;
  /** 当前任务模式（Plan 下只有联网类调用需要审批） */
  mode: "plan" | "work";
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * 开启会话级 AUTO 前的确认弹窗
 *
 * AUTO 会跳过全部审批弹窗，属于"高风险、但用户显式选择"的能力：
 * 必须在开启前把覆盖范围与仍然生效的硬边界讲清楚，而不是一个静默开关。
 * Esc / 点击遮罩都视为取消（与工具审批弹窗同一约定）。
 */
export function AutoApproveConfirm({
  open,
  mode,
  onConfirm,
  onCancel,
}: AutoApproveConfirmProps) {
  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onCancel();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, onCancel]);

  if (!open) return null;

  return (
    <div
      className="modal-overlay tool-approval-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="开启 AUTO 确认"
      onClick={onCancel}
    >
      <div
        className="modal-card auto-approve-card"
        onClick={(event) => event.stopPropagation()}
      >
        <h3>⚡ 开启本会话 AUTO</h3>
        <p className="auto-approve-lead">
          开启后，本任务会话内
          <strong>所有需要审批的工具调用将自动放行</strong>，不再弹窗：
        </p>
        <ul className="auto-approve-list">
          <li>文件写入 / 编辑 / 删除 / 移动 / 复制（写前快照仍可回滚）</li>
          <li>run_command 执行命令（命令硬黑名单与参数审查仍然生效）</li>
          <li>联网抓取 / 检索（域名白名单仍然生效）</li>
          <li>MCP 执行类工具</li>
        </ul>
        {mode === "plan" && (
          <p className="auto-approve-note">
            当前是 Plan 模式：只会影响联网类调用的审批。
          </p>
        )}
        <p className="auto-approve-note">
          硬性安全边界（路径监狱、敏感文件清单、写前快照、回收站）不会放宽；
          AUTO 只作用于当前会话，可随时在状态条上关闭。
        </p>
        <div className="modal-actions">
          {/* 焦点默认落在「取消」：与工具审批弹窗"最安全的动作先聚焦"保持一致 */}
          <button className="modal-cancel-btn" onClick={onCancel} autoFocus>
            取消
          </button>
          <button className="modal-confirm-btn danger" onClick={onConfirm}>
            开启 AUTO
          </button>
        </div>
      </div>
    </div>
  );
}
