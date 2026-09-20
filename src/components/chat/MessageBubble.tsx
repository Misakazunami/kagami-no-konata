import { memo, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore, type Message } from "../../stores/chatStore";
import { toToolCallView, type ToolCallView } from "../../types/tools";
import { ToolCallList } from "./ToolCallCard";
import { MarkdownContent } from "./MarkdownContent";
import { extractThinkTags, visibleAssistantContent } from "../../utils/messageText";
import { copyText } from "../../utils/clipboard";

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
  /** 时间轴锚点 id（右侧轨道滚动跳转用） */
  anchorId?: string;
  /** 是否为本会话最后一条消息（决定用户消息上的"重试"入口） */
  isLast?: boolean;
  /** 点击"回退到此处"（由 MessageList 弹出确认框） */
  onRewind?: (message: Message) => void;
}

/**
 * 消息操作按钮（复制 / 编辑 / 重试 / 回退）
 *
 * 单独成组件并 memo：只有它订阅 `isStreaming`，生成开始/结束的重渲染
 * 不会波及气泡本身（否则整列表的 Markdown 会跟着重新解析）。
 */
const MessageActions = memo(function MessageActions({
  message,
  isUser,
  canRetry,
  editing,
  onEdit,
  onRewind,
}: {
  message: Message;
  isUser: boolean;
  canRetry: boolean;
  editing: boolean;
  onEdit: () => void;
  onRewind?: (message: Message) => void;
}) {
  const isStreaming = useChatStore((s) => s.isStreaming);
  const disabled = isStreaming || editing;
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    // 助手消息复制干净正文（有 thinking 字段时正文已剥离思考；旧数据降级解析）
    const text = isUser ? message.content : visibleAssistantContent(message);
    if (!text.trim()) return;
    await copyText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="message-actions" role="group" aria-label="消息操作">
      <button
        type="button"
        className="message-action-btn"
        disabled={editing}
        onClick={handleCopy}
        title={isUser ? "复制这条消息" : "复制回复正文（Markdown）"}
        aria-label="复制消息"
      >
        {copied ? "✓" : "⧉"}
      </button>
      {isUser && (
        <button
          type="button"
          className="message-action-btn"
          disabled={disabled}
          onClick={onEdit}
          title="编辑并重新生成"
          aria-label="编辑并重新生成"
        >
          ✎
        </button>
      )}
      {canRetry && (
        <button
          type="button"
          className="message-action-btn"
          disabled={disabled}
          onClick={() => void useChatStore.getState().retryMessage(message.id)}
          title="重试：删除这条回复及其后内容，用原提问重新生成"
          aria-label="重试"
        >
          ↻
        </button>
      )}
      <button
        type="button"
        className="message-action-btn"
        disabled={disabled}
        onClick={() => onRewind?.(message)}
        title="回退到此处：删除这条消息及其之后的全部消息"
        aria-label="回退到此处"
      >
        ⤺
      </button>
    </div>
  );
});

function MessageBubbleImpl({
  message,
  showStats,
  liveToolCalls,
  anchorId,
  isLast,
  onRewind,
}: Props) {
  const isUser = message.role === "user";
  const [enabled, setEnabled] = useState(showStats ?? false);
  const [thinkingExpanded, setThinkingExpanded] = useState(false);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");

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

  const startEdit = () => {
    setDraft(message.content);
    setEditing(true);
  };

  const commitEdit = () => {
    const next = draft.trim();
    setEditing(false);
    if (!next || next === message.content.trim()) return;
    // 事件回调里用 getState()：气泡本体不订阅 store，避免整列表跟着重渲染
    void useChatStore.getState().editMessage(message.id, next);
  };

  const handleEditKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // 输入法组合期间的回车是"确认候选词"，不是保存（与 InputBox 同一套保护）
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      commitEdit();
    }
    if (e.key === "Escape") {
      e.preventDefault();
      setEditing(false);
    }
  };

  // 重试入口：助手回复总是可以重试；用户消息只在"还没有回复"（最后一条）时出现
  const canRetry = !isUser || isLast === true;

  return (
    <div className={`message-row ${isUser ? "user" : "assistant"}`} id={anchorId}>
      <div className="message-bubble">
        {/* 思考内容（可折叠） */}
        {!isUser && thinking && (
          <div className="thinking-section">
            <button
              className="thinking-toggle"
              onClick={() => setThinkingExpanded(!thinkingExpanded)}
              aria-expanded={thinkingExpanded}
            >
              <span className="thinking-icon">🧠</span>
              <span>思考过程</span>
              <span className={`thinking-arrow ${thinkingExpanded ? "expanded" : ""}`}>
                ▶
              </span>
            </button>
            {thinkingExpanded && (
              <div className="thinking-content">
                <MarkdownContent content={thinking} />
              </div>
            )}
          </div>
        )}
        {/* 工具调用记录（正文上方，点击卡片可展开参数与结果） */}
        {toolCalls.length > 0 && <ToolCallList calls={toolCalls} />}
        {editing ? (
          <div className="message-edit">
            <textarea
              className="message-edit-textarea"
              value={draft}
              autoFocus
              rows={Math.min(12, Math.max(2, draft.split("\n").length))}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={handleEditKeyDown}
              aria-label="编辑消息内容"
            />
            <div className="message-edit-actions">
              <span className="message-edit-hint">
                Enter 保存并重新生成 · Esc 取消
              </span>
              <button
                type="button"
                className="message-edit-btn"
                onClick={() => setEditing(false)}
              >
                取消
              </button>
              <button
                type="button"
                className="message-edit-btn primary"
                disabled={!draft.trim()}
                onClick={commitEdit}
              >
                保存并重新生成
              </button>
            </div>
          </div>
        ) : (
          <div className="message-content">
            {isUser ? (
              message.content
            ) : (
              <MarkdownContent content={cleanContent} />
            )}
          </div>
        )}
        <div className="message-meta">
          <span className="message-time">{time}</span>
          {/*
            生成本条回复的模型：自动选择下主轮次/子代理会用到不同模型，
            这里是"这条到底是谁答的"的唯一如实来源（历史消息回读时不带该字段）
          */}
          {!isUser && message.model && (
            <span className="message-model" title="生成本条回复的模型">
              {message.model}
            </span>
          )}
          {enabled && hasStats && (
            <span className="message-stats">
              {message.token_count} tokens · {(message.thinking_ms / 1000).toFixed(1)}s
            </span>
          )}
        </div>
      </div>
      {/* 操作按钮位于气泡下方（默认低调常驻，hover / 聚焦时点亮） */}
      <MessageActions
        message={message}
        isUser={isUser}
        canRetry={canRetry}
        editing={editing}
        onEdit={startEdit}
        onRewind={onRewind}
      />
    </div>
  );
}

/** memo 化：流式输出期间历史气泡不再随父级重渲染（跳过 markdown/highlight 重复解析） */
export const MessageBubble = memo(MessageBubbleImpl);
