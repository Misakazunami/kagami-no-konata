import { memo, useEffect, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useChatStore, type Message } from "../../stores/chatStore";
import { toToolCallView, type ToolCallView } from "../../types/tools";
import { ToolCallList } from "./ToolCallCard";

interface Props {
  message: Message;
  showStats?: boolean;
  /**
   * 进行中 / 刚刚结束的生成里的工具调用
   *
   * 流式期间由 MessageList 直接渲染在流式气泡上；生成结束的那一刻，
   * 落库记录（带 message_id）还没回读到，此时由 MessageList 把它挂到最后一条
   * 回复上，保证工具记录不会在生成完成后突然消失。
   */
  liveToolCalls?: ToolCallView[];
}

/**
 * 外链渲染：交给系统默认浏览器打开
 *
 * 默认的 `<a>` 会让整个应用 webview 直接导航到外部页面（界面丢失，
 * 且外部页面会运行在注入了 IPC 的 webview 里）。这里拦截点击并调用
 * opener 插件，同时补上 rel="noreferrer"。
 */
function MarkdownLink({
  href,
  children,
  ...rest
}: React.AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a
      {...rest}
      href={href}
      rel="noreferrer noopener"
      onClick={(event) => {
        event.preventDefault();
        if (!href) return;
        if (!/^https?:\/\//i.test(href)) return; // 只放行 http(s)
        openUrl(href).catch((e) => console.error("打开链接失败:", e));
      }}
    >
      {children}
    </a>
  );
}

const MARKDOWN_COMPONENTS = { a: MarkdownLink } as const;

/** 从文本中提取 <think> 标签内容，返回 { thinking, content } */
function extractThinkTags(text: string): { thinking: string | null; content: string } {
  const thinkRegex = /<think>([\s\S]*?)<\/think>/g;
  const thinkParts: string[] = [];
  let cleaned = text;
  let match;
  while ((match = thinkRegex.exec(text)) !== null) {
    thinkParts.push(match[1].trim());
  }
  if (thinkParts.length > 0) {
    cleaned = text.replace(thinkRegex, "").trim();
  }
  return {
    thinking: thinkParts.length > 0 ? thinkParts.join("\n\n") : null,
    content: cleaned,
  };
}

function MessageBubbleImpl({ message, showStats, liveToolCalls }: Props) {
  const isUser = message.role === "user";
  const [enabled, setEnabled] = useState(showStats ?? false);
  const [thinkingExpanded, setThinkingExpanded] = useState(false);

  // 历史工具记录：按 message_id 归组，未命中时为 undefined（引用稳定，不会引起额外重渲染）
  const persistedCalls = useChatStore((s) => s.toolsByMessage[message.id]);
  const toolCalls: ToolCallView[] = isUser
    ? []
    : liveToolCalls && liveToolCalls.length > 0
      ? liveToolCalls
      : (persistedCalls ?? []).map(toToolCallView);

  // 思考内容：优先使用消息自带字段，降级到从内容中解析 <think> 标签（兼容旧数据）
  const thinking = isUser
    ? null
    : message.thinking ?? extractThinkTags(message.content).thinking;
  const cleanContent = isUser
    ? message.content
    : message.thinking
      ? message.content
      : extractThinkTags(message.content).content;

  // 如果没有通过 props 传入，则从配置读取
  useEffect(() => {
    if (showStats !== undefined) {
      setEnabled(showStats);
      return;
    }
    invoke<{ ui: { show_message_stats?: boolean } }>("get_config")
      .then((config) => setEnabled(config.ui.show_message_stats ?? false))
      .catch(() => {});
  }, [showStats]);

  const time = new Date(message.timestamp).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });

  const hasStats = !isUser && message.token_count > 0;

  return (
    <div className={`message-row ${isUser ? "user" : "assistant"}`}>
      <div className="message-bubble">
        {/* 思考内容（可折叠） */}
        {!isUser && thinking && (
          <div className="thinking-section">
            <button
              className="thinking-toggle"
              onClick={() => setThinkingExpanded(!thinkingExpanded)}
            >
              <span className="thinking-icon">🧠</span>
              <span>思考过程</span>
              <span className={`thinking-arrow ${thinkingExpanded ? "expanded" : ""}`}>
                ▶
              </span>
            </button>
            {thinkingExpanded && (
              <div className="thinking-content">
                <Markdown
                  remarkPlugins={[remarkGfm]}
                  rehypePlugins={[rehypeHighlight]}
                  components={MARKDOWN_COMPONENTS}
                >
                  {thinking}
                </Markdown>
              </div>
            )}
          </div>
        )}
        {/* 工具调用记录（正文上方，点击卡片可展开参数与结果） */}
        {toolCalls.length > 0 && <ToolCallList calls={toolCalls} />}
        <div className="message-content">
          {isUser ? (
            message.content
          ) : (
            <Markdown
              remarkPlugins={[remarkGfm]}
              rehypePlugins={[rehypeHighlight]}
              components={MARKDOWN_COMPONENTS}
            >
              {cleanContent}
            </Markdown>
          )}
        </div>
        <div className="message-meta">
          <span className="message-time">{time}</span>
          {enabled && hasStats && (
            <span className="message-stats">
              {message.token_count} tokens · {(message.thinking_ms / 1000).toFixed(1)}s
            </span>
          )}
        </div>
      </div>
    </div>
  );
}

/** memo 化：流式输出期间历史气泡不再随父级重渲染（跳过 markdown/highlight 重复解析） */
export const MessageBubble = memo(MessageBubbleImpl);
