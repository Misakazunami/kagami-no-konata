import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import type {
  PlanItem,
  RestoreReportView,
  SessionSnapshotStreamView,
  SnapshotInfoView,
} from "../../types/events";
import type { ToolInvocation } from "../../types/tools";
import { copyText } from "../../utils/clipboard";

/**
 * 改动与产物面板
 *
 * 写类工具（写入 / 编辑 / 删除 / 移动 / 复制）在动手前都会把原文件备份下来：
 * - 顶部横幅是"刚刚这一轮"的回滚入口；
 * - 展开后是**整个会话**的改动记录（按轮次分组），历史轮次也能查看文件清单并回滚；
 * - 「复制任务报告」把计划进度、改动文件与执行过的验证命令整理成 Markdown，
 *   供用户粘贴到 issue / PR / 笔记里。
 *
 * 历史记录由本组件按会话懒加载（不再依赖"生成结束后才能读一次"的 store 快照，
 * 因此换会话回来仍能看到记录）。
 */
export function ChangesPanel() {
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const snapshot = useChatStore((s) => s.snapshot);
  const notice = useChatStore((s) => s.snapshotNotice);
  const restoreSnapshot = useChatStore((s) => s.restoreSnapshot);
  const dismissSnapshot = useChatStore((s) => s.dismissSnapshot);
  const plan = useChatStore((s) => s.plan);

  const [history, setHistory] = useState<SessionSnapshotStreamView[]>([]);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [expandedStream, setExpandedStream] = useState<string | null>(null);
  const [restoringId, setRestoringId] = useState<string | null>(null);
  const [historyNotice, setHistoryNotice] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const session = sessions.find((s) => s.id === currentSessionId);

  const loadHistory = useCallback(async () => {
    const sessionId = currentSessionId;
    if (!sessionId) return;
    try {
      const list = await invoke<SessionSnapshotStreamView[]>(
        "list_session_snapshots",
        { sessionId }
      );
      // 回读期间换了会话就丢弃结果
      if (useChatStore.getState().currentSessionId !== sessionId) return;
      setHistory(list ?? []);
    } catch (e) {
      console.error("Failed to load snapshot history:", e);
    }
  }, [currentSessionId]);

  // 换会话：清空本地视图并按新会话重新加载
  useEffect(() => {
    setHistory([]);
    setHistoryOpen(false);
    setExpandedStream(null);
    setHistoryNotice(null);
    void loadHistory();
  }, [loadHistory]);

  // 新一轮生成留下备份后刷新记录（store 的 snapshot 只在"刚结束的一轮"变化）
  useEffect(() => {
    if (snapshot) void loadHistory();
  }, [snapshot?.stream_id, loadHistory]);

  const restoreStream = async (streamId: string) => {
    if (!currentSessionId || restoringId) return;
    setRestoringId(streamId);
    setHistoryNotice(null);
    try {
      const report = await invoke<RestoreReportView>("restore_snapshot", {
        sessionId: currentSessionId,
        streamId,
      });
      const parts = [`已还原 ${report.restored} 个文件`];
      if (report.missing > 0) parts.push(`${report.missing} 个备份已失效`);
      if (report.errors.length > 0) {
        parts.push(`${report.errors.length} 个失败：${report.errors[0]}`);
      }
      setHistoryNotice(parts.join("，"));
    } catch (e) {
      setHistoryNotice(`回滚失败：${toMessage(e)}`);
    } finally {
      setRestoringId(null);
    }
  };

  const copyReport = async () => {
    if (!currentSessionId) return;
    try {
      const invocations = await invoke<ToolInvocation[]>("get_tool_invocations", {
        sessionId: currentSessionId,
      });
      const text = buildTaskReport({
        title: session?.title ?? currentSessionId,
        plan,
        history,
        invocations,
      });
      await copyText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      setHistoryNotice(`生成报告失败：${toMessage(e)}`);
    }
  };

  const hasAnything =
    !!snapshot || !!notice || history.length > 0 || (plan?.length ?? 0) > 0;
  if (!currentSessionId || !hasAnything) return null;

  return (
    <div className="changes-panel">
      {snapshot && (
        <SnapshotBannerView
          snapshot={snapshot}
          notice={notice}
          onRestore={restoreSnapshot}
          onDismiss={dismissSnapshot}
        />
      )}

      {/* 回退对话时若清掉了快照横幅，「文件已还原」的结果仍要有地方如实展示 */}
      {!snapshot && notice && (
        <div className="snapshot-banner" role="status">
          <span className="snapshot-icon" aria-hidden="true">
            🧷
          </span>
          <span className="snapshot-text">{notice}</span>
          <button
            className="snapshot-dismiss-btn"
            onClick={dismissSnapshot}
            title="知道了"
          >
            ✕
          </button>
        </div>
      )}

      <div className="changes-toolbar">
        <button
          className="changes-toggle"
          onClick={() => setHistoryOpen((prev) => !prev)}
          aria-expanded={historyOpen}
          title={historyOpen ? "收起改动记录" : "查看整个会话的改动记录"}
        >
          🧷 改动记录{history.length > 0 ? ` (${history.length})` : ""}
          <span className={`plan-arrow ${historyOpen ? "expanded" : ""}`}>▶</span>
        </button>
        <button
          className="changes-report-btn"
          onClick={copyReport}
          title="把计划进度、改动文件与执行过的验证命令整理成 Markdown 复制"
        >
          {copied ? "已复制 ✓" : "复制任务报告"}
        </button>
      </div>

      {historyOpen && (
        <div className="changes-body">
          {historyNotice && <div className="changes-notice">{historyNotice}</div>}
          {history.length === 0 && (
            <div className="changes-empty">还没有文件改动记录</div>
          )}
          {history.map((stream, index) => (
            <div key={stream.stream_id} className="changes-stream">
              <button
                className="changes-stream-head"
                onClick={() =>
                  setExpandedStream(
                    expandedStream === stream.stream_id ? null : stream.stream_id
                  )
                }
                aria-expanded={expandedStream === stream.stream_id}
              >
                <span className="changes-round">
                  {index === 0 ? "最近一轮" : `更早第 ${history.length - index} 轮`}
                </span>
                <span className="changes-time">
                  {new Date(stream.created_at).toLocaleString("zh-CN", {
                    month: "2-digit",
                    day: "2-digit",
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                </span>
                <span className="changes-summary">
                  {stream.files.length} 个文件 · {formatBackupSize(stream.total_bytes)}
                </span>
                <span
                  className={`plan-arrow ${expandedStream === stream.stream_id ? "expanded" : ""}`}
                >
                  ▶
                </span>
              </button>
              {expandedStream === stream.stream_id && (
                <div className="changes-files">
                  {stream.files.map((file) => (
                    <div
                      key={`${file.root_id}:${file.rel_path}:${file.created_at}`}
                      className="changes-file"
                    >
                      <span
                        className="changes-file-path"
                        title={`${file.root_id}:${file.rel_path}`}
                      >
                        {file.root_id}:{file.rel_path}
                      </span>
                      <span className="changes-file-size">
                        {formatBackupSize(file.bytes)}
                      </span>
                    </div>
                  ))}
                </div>
              )}
              <div className="changes-stream-actions">
                <button
                  className="changes-restore-btn"
                  onClick={() => restoreStream(stream.stream_id)}
                  disabled={restoringId !== null}
                  title="把这一轮改动过的文件还原回改动前的内容（可重复执行）"
                >
                  {restoringId === stream.stream_id ? "回滚中…" : "回滚这一轮"}
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** 备份体积的人类可读形式（只有界面用，与后端的 `human_bytes` 各自独立） */
export function formatBackupSize(bytes: number): string {
  const kb = bytes / 1024;
  if (kb >= 1024) return `${(kb / 1024).toFixed(1)} MB`;
  if (kb >= 1) return `${kb.toFixed(1)} KB`;
  return `${bytes} B`;
}

const toMessage = (error: unknown): string =>
  typeof error === "string"
    ? error
    : error instanceof Error
      ? error.message
      : String(error);

/**
 * 任务交付摘要（Markdown）
 *
 * 数据来源全部是既有记录：计划（session_plans）、改动文件（workspace_snapshots）
 * 与工具轨迹（tool_invocations）。验证命令只取 `run_command` 的最近几次，
 * 保留状态与预览首行，不把完整输出抄进报告。
 */
export function buildTaskReport({
  title,
  plan,
  history,
  invocations,
}: {
  title: string;
  plan: PlanItem[] | null;
  history: SessionSnapshotStreamView[];
  invocations: ToolInvocation[];
}): string {
  const lines: string[] = [];
  lines.push(`# 任务报告：${title}`);
  lines.push("");

  if (plan && plan.length > 0) {
    const done = plan.filter((item) => item.status === "done").length;
    lines.push(`## 计划进度（${done}/${plan.length}）`);
    for (const item of plan) {
      lines.push(`- [${planStatusMark(item.status)}] ${item.title}`);
    }
    lines.push("");
  }

  if (history.length > 0) {
    const totalFiles = history.reduce((sum, s) => sum + s.files.length, 0);
    lines.push(`## 改动文件（${totalFiles} 个，共 ${history.length} 轮）`);
    for (const stream of history) {
      lines.push("");
      lines.push(`### ${new Date(stream.created_at).toLocaleString("zh-CN")}`);
      for (const file of stream.files) {
        lines.push(`- \`${file.root_id}:${file.rel_path}\`（${formatBackupSize(file.bytes)}）`);
      }
    }
    lines.push("");
  }

  const commands = invocations
    .filter((inv) => inv.tool_name === "run_command")
    .slice(-10);
  if (commands.length > 0) {
    lines.push(`## 执行过的命令（最近 ${commands.length} 条）`);
    for (const inv of commands) {
      const status = inv.status === "ok" ? "✓" : `✗ ${inv.status}`;
      const preview = (inv.result_preview ?? inv.error ?? "").split("\n")[0].slice(0, 120);
      lines.push(`- ${status} ${preview || inv.tool_label}`);
    }
    lines.push("");
  }

  return lines.join("\n");
}

function planStatusMark(status: string): string {
  switch (status) {
    case "done":
      return "x";
    case "doing":
      return ">";
    case "blocked":
      return "!";
    default:
      return " ";
  }
}

/**
 * 最近一轮改动的回滚横幅（原来的 SnapshotBanner 行为保持不变）
 */
export function SnapshotBannerView({
  snapshot,
  notice,
  onRestore,
  onDismiss,
}: {
  snapshot: SnapshotInfoView | null;
  notice: string | null;
  onRestore: () => void | Promise<void>;
  onDismiss: () => void;
}) {
  // 回滚是不可重入的：快速双击会发出两次 restore_snapshot，
  // 产生重复还原与互相覆盖的提示文案
  const [restoring, setRestoring] = useState(false);
  if (!snapshot) return null;

  const size = formatBackupSize(snapshot.bytes);

  const handleRestore = async () => {
    if (restoring) return;
    setRestoring(true);
    try {
      await onRestore();
    } finally {
      setRestoring(false);
    }
  };

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
        onClick={handleRestore}
        disabled={restoring}
        title="把这一轮生成改动的文件还原回改动前的内容"
      >
        {restoring ? "回滚中…" : "回滚"}
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
