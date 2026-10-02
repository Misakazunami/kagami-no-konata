import { useEffect, useRef, useState, type ReactNode } from "react";
import { useFocusTrap } from "../../hooks/useFocusTrap";

interface Props {
  title: string;
  /** 主说明文案（纯文本） */
  description: ReactNode;
  /** 额外的补充内容（引用、提示等） */
  children?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  /** 破坏性操作：确认按钮标红 */
  danger?: boolean;
  /** 确认回调；抛错时弹窗保持打开并展示错误 */
  onConfirm: () => void | Promise<void>;
  /** 取消回调（取消按钮 / Esc / 点遮罩）；抛错时弹窗保持打开并展示错误 */
  onCancel?: () => void | Promise<void>;
  onClose: () => void;
}

/**
 * 统一的确认对话框（替换全仓库散落的原生 `confirm()`）
 *
 * - `role="dialog"` + `aria-modal` + 焦点陷阱（Tab 循环）+ 关闭归还焦点；
 * - Esc / 点击遮罩 = 取消（与回退弹窗同一约定）；
 * - `onConfirm` 返回 Promise：等待期间禁用按钮并显示"处理中…"，失败展示错误且不关闭。
 */
export function ConfirmDialog({
  title,
  description,
  children,
  confirmLabel = "确认",
  cancelLabel = "取消",
  danger,
  onConfirm,
  onCancel,
  onClose,
}: Props) {
  const cardRef = useRef<HTMLDivElement>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useFocusTrap(cardRef, true, { initialFocusSelector: ".modal-cancel-btn" });

  const runAction = async (action?: () => void | Promise<void>) => {
    setSubmitting(true);
    setError(null);
    try {
      await action?.();
      onClose();
    } catch (e) {
      setError(typeof e === "string" ? e : e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const handleConfirm = () => (submitting ? undefined : runAction(onConfirm));
  const handleCancel = () => (submitting ? undefined : runAction(onCancel));

  // Esc 关闭（遮罩点击由外层处理，提交中一律忽略）
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting) {
        event.preventDefault();
        void handleCancel();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onCancel, onClose, submitting]);

  return (
    <div className="modal-overlay" onClick={() => void handleCancel()}>
      <div
        className="modal-card"
        ref={cardRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
      >
        <h3>{title}</h3>
        <p className="rewind-desc">{description}</p>
        {children}
        {error && <p className="rewind-desc rewind-error">{error}</p>}
        <div className="modal-actions">
          <button className="modal-cancel-btn" onClick={() => void handleCancel()} disabled={submitting}>
            {cancelLabel}
          </button>
          <button
            className={`modal-confirm-btn${danger ? " danger" : ""}`}
            onClick={() => void handleConfirm()}
            disabled={submitting}
          >
            {submitting ? "处理中…" : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
