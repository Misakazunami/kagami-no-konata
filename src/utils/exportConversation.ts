import type { Message, Session } from "../stores/chatStore";
import { visibleAssistantContent } from "./messageText";

const formatTimestamp = (iso: string): string => {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
};

const roleLabel = (role: Message["role"]): string => {
  if (role === "user") return "用户";
  if (role === "system") return "系统";
  return "助手";
};

/**
 * 把整个会话导出为 Markdown
 *
 * - 助手正文走 `visibleAssistantContent`：有 `thinking` 字段时正文已是干净的，
 *   旧数据则剥掉 `<think>` 标签，不把思考过程混进导出内容；
 * - 保留代码块围栏（正文原样输出），时间用本地时区。
 */
export function buildConversationMarkdown(
  session: Session | undefined,
  messages: Message[]
): string {
  const lines: string[] = [];
  lines.push(`# ${session?.title?.trim() || "对话记录"}`);
  lines.push("");
  lines.push(
    `> 导出时间：${formatTimestamp(new Date().toISOString())} · 共 ${messages.length} 条消息`
  );
  lines.push("");

  for (const message of messages) {
    const meta = [roleLabel(message.role), formatTimestamp(message.timestamp)];
    if (message.model) meta.push(message.model);
    lines.push(`**${meta.join(" · ")}**`);
    lines.push("");
    const body =
      message.role === "assistant" ? visibleAssistantContent(message) : message.content;
    lines.push(body.trim());
    lines.push("");
    lines.push("---");
    lines.push("");
  }

  return lines.join("\n").trimEnd() + "\n";
}
