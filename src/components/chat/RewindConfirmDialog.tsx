import { useEffect, useRef, useState } from "react";
import { useChatStore, type Message } from "../../stores/chatStore";
import type { RewindPreviewView } from "../../types/events";

interface Props {
  message: Message;
  onClose: () => void;
}

/**
 * 回退确认弹窗
 *
 * 打开时先向后端要一份只读预览（删多少条、涉及哪些可撤销的文件轮次），
 * 用户确认后才真正删除。勾选"同时撤销文件改动"时按预览给出的顺序
 * （新 → 旧）逐轮恢复快照。
 */
export function RewindConfirmDialog({ message, onClose }: Props) {
  const previewRewind = useChatStore((s) => s.previewRewind);
  const rewindTo = useChatStore((s) => s.rewindTo);
  const [preview, setPreview] = useState<RewindPreviewView | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [restoreFiles, setRestoreFiles] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setLoadError(null);
    previewRewind(message.id)
      .then((result) => {
        if (!cancelled) setPreview(result);
      })
      .catch((e) => {
        if (!cancelled) setLoadError(typeof e === "string" ? e : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [message.id, previewRewind]);

  // 焦点进入弹窗：Esc / 点击遮罩关闭才对键盘用户可用
  useEffect(() => {
    dialogRef.current?.focus();
  }, []);

  const totalFiles =
    preview?.affected_streams.reduce((sum, stream) => sum + stream.files, 0) ?? 0;

  const handleConfirm = async () => {
    if (submitting || loading || loadError) return;
    setSubmitting(true);
    try {
      await rewindTo(
        message.id,
        restoreFiles
          ? (preview?.affected_streams.map((stream) => stream.stream_id) ?? [])
          : []
      );
      onClose();
    } finally {
      setSubmitting(false);
    }
  };

  const roleLabel = message.role === "user" ? "用户消息" : "助手回复";
  const previewText = message.content.replace(/\s+/g, " ").slice(0, 40) || "（空）";

  return (
    <div className="modal-overlay" onClick={() => !submitting && onClose()}>
      <div
        className="modal-card rewind-modal"
        ref={dialogRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="回退到此处"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape" && !submitting) onClose();
        }}
      >
        <h3>⤺ 回退到此处</h3>
        {loading ? (
          <p className="rewind-desc">正在计算影响范围…</p>
        ) : loadError ? (
          <p className="rewind-desc rewind-error">无法回退：{loadError}</p>
        ) : (
          <>
            <p className="rewind-desc">
              将删除这条{roleLabel}及其之后的 <strong>{preview?.removed ?? 0}</strong>{" "}
              条消息，且无法恢复。
            </p>
            <p className="rewind-quote">“{previewText}”</p>
            {totalFiles > 0 && (
              <label className="rewind-restore">
                <input
                  type="checkbox"
                  checked={restoreFiles}
                  disabled={submitting}
                  onChange={(e) => setRestoreFiles(e.target.checked)}
                />
                <span>
                  同时撤销这些轮次的文件改动（{preview?.affected_streams.length ?? 0} 轮 ·{" "}
                  {totalFiles} 个文件）
                </span>
              </label>
            )}
            {message.role === "user" && (
              <p className="rewind-hint">回退后原文会填回输入框，方便修改后重新发送。</p>
            )}
          </>
        )}
        <div className="modal-actions">
          <button className="modal-cancel-btn" onClick={onClose} disabled={submitting}>
            取消
          </button>
          <button
            className="modal-confirm-btn danger"
            onClick={handleConfirm}
            disabled={loading || !!loadError || submitting}
          >
            {submitting ? "回退中…" : "确认回退"}
          </button>
        </div>
      </div>
    </div>
  );
}
