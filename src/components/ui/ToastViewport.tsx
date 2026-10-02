import { useUiStore, type ToastKind } from "../../stores/uiStore";

const KIND_ICON: Record<ToastKind, string> = {
  success: "✓",
  error: "✕",
  info: "ⓘ",
};

const KIND_LABEL: Record<ToastKind, string> = {
  success: "成功",
  error: "错误",
  info: "提示",
};

/**
 * 全局 toast 视口
 *
 * 挂在 App 根部（两个窗口各一份），`role="status"` + `aria-live`：
 * 成功/提示用 polite，错误用 assertive，读屏用户不会错过失败反馈。
 */
export function ToastViewport() {
  const toasts = useUiStore((s) => s.toasts);
  const dismissToast = useUiStore((s) => s.dismissToast);

  if (toasts.length === 0) return null;

  return (
    <div className="toast-viewport" aria-live="polite">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className={`toast-item toast-${toast.kind}`}
          role={toast.kind === "error" ? "alert" : "status"}
          aria-live={toast.kind === "error" ? "assertive" : "polite"}
        >
          <span className="toast-icon" aria-hidden="true">
            {KIND_ICON[toast.kind]}
          </span>
          <span className="toast-text">{toast.text}</span>
          <button
            type="button"
            className="toast-close"
            onClick={() => dismissToast(toast.id)}
            aria-label={`关闭${KIND_LABEL[toast.kind]}提示`}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
