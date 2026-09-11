import { useState } from "react";
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

/**
 * 单条工具调用卡片
 *
 * - 折叠状态只显示：图标 + 中文名 + 状态胶囊 + 耗时；
 * - 展开后显示参数与结果；
 * - 结果 `preview` 是**不可信的文件/命令输出**，因此用 `<pre>` 纯文本渲染，
 *   绝不走 markdown（否则输出里的内容会被当成指令/链接解析）。
 */
export function ToolCallCard({ call }: Props) {
  const [expanded, setExpanded] = useState(false);
  const duration = formatToolDuration(call.duration_ms);
  const running = call.status === "running";
  const body = call.error ?? call.preview;

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
        <span className={`tool-status-chip ${call.status}`}>
          {TOOL_STATUS_LABEL[call.status]}
        </span>
        {duration && <span className="tool-call-duration">{duration}</span>}
        {call.truncated && <span className="tool-call-truncated">已截断</span>}
        <span className={`tool-call-arrow ${expanded ? "expanded" : ""}`}>▶</span>
      </button>

      {expanded && (
        <div className="tool-call-body">
          <div className="tool-call-section">
            <span className="tool-call-section-title">参数</span>
            <pre className="tool-call-pre">
              {call.args_preview || "（无参数）"}
            </pre>
          </div>
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
