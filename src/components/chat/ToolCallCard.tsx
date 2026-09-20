import { useEffect, useRef, useState } from "react";
import {
  TOOL_STATUS_LABEL,
  formatToolDuration,
  type ToolCallView,
} from "../../types/tools";

/**
 * 工具图标
 *
 * 纯装饰：按工具名关键字猜一个 emoji，猜不到就用通用图标，
 * 不依赖后端返回任何图标字段。
 */
const ICON_RULES: Array<[RegExp, string]> = [
  [/read|read_file|cat|open/, "📄"],
  [/write|edit|patch|create|mkdir/, "✍"],
  [/list|glob|search|grep|find/, "🔍"],
  [/run|exec|shell|command|bash|terminal/, "⌨"],
  [/http|fetch|web|url|download/, "🌐"],
  [/memory|recall|remember/, "🧠"],
  [/delete|remove|rm/, "🗑"],
];

function toolIcon(tool: string): string {
  const name = tool.toLowerCase();
  for (const [pattern, icon] of ICON_RULES) {
    if (pattern.test(name)) return icon;
  }
  return "🔧";
}

interface Props {
  call: ToolCallView;
}

/** 折叠状态下展示的"最新一行输出"（太长的行截断，避免卡片被撑开） */
function lastOutputLine(output: string): string {
  const lines = output.split("\n").filter((line) => line.trim().length > 0);
  const last = lines[lines.length - 1] ?? "";
  return last.length > 48 ? `${last.slice(0, 48)}…` : last;
}

/**
 * 单条工具调用卡片
 *
 * - 折叠状态只显示：图标 + 中文名 + 状态胶囊 + 耗时（执行中额外显示最新一行输出）；
 * - 展开后显示参数、**实时输出**与结果；
 * - 结果 `preview` 与实时输出都是**不可信的文件/命令输出**，因此用 `<pre>` 纯文本渲染，
 *   绝不走 markdown（否则输出里的内容会被当成指令/链接解析）。
 */
export function ToolCallCard({ call }: Props) {
  const [expanded, setExpanded] = useState(false);
  const liveRef = useRef<HTMLPreElement>(null);
  const duration = formatToolDuration(call.duration_ms);
  const running = call.status === "running";
  const body = call.error ?? call.preview;
  const liveOutput = call.output ?? "";

  // 执行中跟随滚动到底部；用户主动上翻查看历史输出时不打扰，
  // 结束后不再滚动（此时用户可能正在往上翻）
  useEffect(() => {
    if (!running) return;
    const el = liveRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  }, [liveOutput, running]);

  return (
    <div className={`tool-call-card ${call.status}`}>
      <button
        className="tool-call-head"
        onClick={() => setExpanded((prev) => !prev)}
        aria-expanded={expanded}
        title={expanded ? "收起工具详情" : "展开工具详情"}
      >
        <span className="tool-call-icon" aria-hidden="true">
          {toolIcon(call.tool)}
        </span>
        <span className="tool-call-label">{call.tool_label || call.tool}</span>
        {call.step > 0 && (
          <span
            className="tool-call-step"
            title={
              call.maxSteps
                ? `本次生成的第 ${call.step} 步（共 ${call.maxSteps} 步工具预算）`
                : `本次生成的第 ${call.step} 步`
            }
          >
            {call.maxSteps ? `${call.step}/${call.maxSteps}` : `#${call.step}`}
          </span>
        )}
        <span className={`tool-status-chip ${call.status}`}>
          {TOOL_STATUS_LABEL[call.status]}
        </span>
        {call.subagentTasks && call.subagentTasks.length > 0 ? (
          <span
            className="tool-call-subagents"
            title="只读子代理任务进度"
          >
            子任务 {call.subagentTasks.filter((t) => t.status === "done").length}/{call.subagentTasks.length}
          </span>
        ) : call.subagentCalls ? (
          <span className="tool-call-subagents" title="这个工具派出的只读子代理已执行的调用次数">
            子代理 ×{call.subagentCalls}
          </span>
        ) : (
          running &&
          liveOutput && (
            <span className="tool-call-live-tail" title="最新一行输出（展开看完整输出）">
              {lastOutputLine(liveOutput)}
            </span>
          )
        )}
        {duration && <span className="tool-call-duration">{duration}</span>}
        {call.truncated && <span className="tool-call-truncated">已截断</span>}
        <span className={`tool-call-arrow ${expanded ? "expanded" : ""}`}>▶</span>
      </button>

      {expanded && (
        <div className="tool-call-body">
          {call.subagentTasks && call.subagentTasks.length > 0 && (
            <div className="tool-call-section">
              <span className="tool-call-section-title">子代理并行状态</span>
              <div style={{ display: "flex", flexDirection: "column", gap: "4px", marginTop: "4px" }}>
                {call.subagentTasks.map((task) => {
                  const statusMap: Record<string, { label: string; color: string }> = {
                    queued: { label: "排队中", color: "var(--text-muted, #888)" },
                    running: { label: "● 运行中", color: "var(--accent, #3b82f6)" },
                    done: { label: "✓ 已完成", color: "#10b981" },
                    error: { label: "✗ 失败", color: "#ef4444" },
                    cancelled: { label: "已取消", color: "#f59e0b" },
                    skipped: { label: "超额跳过", color: "#6b7280" },
                  };
                  const meta = statusMap[task.status] ?? { label: task.status, color: "#888" };
                  // 跳过原因（名额不足 / 时间预算不足）比笼统的"超额跳过"更准确
                  const label =
                    task.status === "skipped" && task.reason ? task.reason : meta.label;
                  return (
                    <div
                      key={task.taskId}
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        fontSize: "0.82rem",
                        padding: "4px 8px",
                        background: "rgba(0,0,0,0.15)",
                        borderRadius: "4px",
                      }}
                    >
                      <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: "70%" }}>
                        {task.goalPreview}
                      </span>
                      <span style={{ color: meta.color, fontSize: "0.75rem", fontWeight: 500 }}>
                        {/* 自动选择时如实标注这条子任务用的子模型 */}
                        {task.model ? `${task.model} · ` : ""}
                        {label}
                        {task.durationMs ? ` (${(task.durationMs / 1000).toFixed(1)}s)` : ""}
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          <div className="tool-call-section">
            <span className="tool-call-section-title">参数</span>
            <pre className="tool-call-pre">
              {call.args_preview || "（无参数）"}
            </pre>
          </div>
          {/* 实时输出：命令每产生一行就推一次，长任务不必等结束才有反馈 */}
          {liveOutput && (
            <div className="tool-call-section">
              <span className="tool-call-section-title">
                实时输出{running ? "（进行中）" : ""}
              </span>
              <pre className="tool-call-pre tool-call-live" ref={liveRef}>
                {liveOutput}
              </pre>
            </div>
          )}
          <div className="tool-call-section">
            <span className="tool-call-section-title">
              {running ? "执行中…" : "结果"}
            </span>
            <pre className="tool-call-pre">
              {body || (running ? "等待工具返回…" : "（无输出）")}
            </pre>
            {call.truncated && (
              <span className="tool-call-truncated-hint">
                输出过长已被截断，仅显示前面部分
              </span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/** 一组工具调用卡片（气泡上方统一排布） */
export function ToolCallList({ calls }: { calls: ToolCallView[] }) {
  if (calls.length === 0) return null;
  return (
    <div className="tool-call-list">
      {calls.map((call) => (
        <ToolCallCard key={call.call_id} call={call} />
      ))}
    </div>
  );
}
