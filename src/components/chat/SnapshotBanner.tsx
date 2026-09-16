import { useChatStore } from "../../stores/chatStore";
import type { SnapshotInfoView } from "../../types/events";

/**
 * 文件改动回滚条
 *
 * 写类工具（写入 / 编辑 / 删除 / 移动 / 复制）在动手前都会把原文件备份下来，
 * 这里给用户一个"撤销这一轮改动"的入口——这是敢让模型动代码的前提。
 *
 * 只在真的产生过备份时出现；回滚结果（成功/失败、还原了几个文件）也显示在这里，
 * 不占用全局错误横幅（那不是错误）。
 */
export function SnapshotBanner() {
  const snapshot = useChatStore((s) => s.snapshot);
  const notice = useChatStore((s) => s.snapshotNotice);
  const restoreSnapshot = useChatStore((s) => s.restoreSnapshot);
  const dismissSnapshot = useChatStore((s) => s.dismissSnapshot);

  return (
    <SnapshotBannerView
      snapshot={snapshot}
      notice={notice}
      onRestore={restoreSnapshot}
      onDismiss={dismissSnapshot}
    />
  );
}

/** 备份体积的人类可读形式（只有界面用，与后端的 `human_bytes` 各自独立） */
export function formatBackupSize(bytes: number): string {
  const kb = bytes / 1024;
  if (kb >= 1024) return `${(kb / 1024).toFixed(1)} MB`;
  if (kb >= 1) return `${kb.toFixed(1)} KB`;
  return `${bytes} B`;
}

/**
 * 纯展示层
 *
 * 与 store 解耦（只吃 props），可以在无 DOM 环境里做渲染测试。
 */
export function SnapshotBannerView({
  snapshot,
  notice,
  onRestore,
  onDismiss,
}: {
  snapshot: SnapshotInfoView | null;
  notice: string | null;
  onRestore: () => void;
  onDismiss: () => void;
}) {
  if (!snapshot) return null;

  const size = formatBackupSize(snapshot.bytes);

  return (
    <div className="snapshot-banner" role="status">
      <span className="snapshot-icon" aria-hidden="true">
        🧷
      </span>
      <span className="snapshot-text">
        {notice
          ? notice
          : `本轮改动了 ${snapshot.files} 个文件（${size}），已保留改动前的内容`}
      </span>
      <button
        className="snapshot-restore-btn"
        onClick={onRestore}
        title="把这一轮生成改动的文件还原回改动前的内容"
      >
        回滚
      </button>
      <button
        className="snapshot-dismiss-btn"
        onClick={onDismiss}
        title="保留这些改动，不再提示"
      >
        ✕
      </button>
    </div>
  );
}
