import { memo, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore, type Message } from "../../stores/chatStore";
import { toToolCallView, type ToolCallView } from "../../types/tools";
import { ToolCallList } from "./ToolCallCard";
import { MarkdownContent } from "./MarkdownContent";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { useUiStore } from "../../stores/uiStore";
import { extractThinkTags, visibleAssistantContent } from "../../utils/messageText";
import { copyText } from "../../utils/clipboard";
import {
  IconBrain,
  IconCheck,
  IconChevronRight,
  IconCopy,
  IconPencil,
  IconRetry,
  IconRewind,
} from "../icons";

/**
 * 计算"重试 / 编辑"会连带**永久删除**的后续消息数
 *
 * 与 `chatStore.startGeneration` 的乐观截断保持同一套语义：目标消息本身
 * （用户提问或待重试的回复）会被保留/重新生成，它之后的全部消息不可恢复地消失。
 * 返回 0 表示截断不丢任何既有内容，无需打扰用户确认。
 */
function lostMessageCount(messageId: string): number {
  const { messages } = useChatStore.getState();
  const index = messages.findIndex((m) => m.id === messageId);
  if (index < 0) return 0;
  return Math.max(0, messages.length - index - 1);
}

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
  onRetry,
  onRewind,
}: {
  message: Message;
  isUser: boolean;
  canRetry: boolean;
  editing: boolean;
  onEdit: () => void;
  onRetry: () => void;
  onRewind?: (message: Message) => void;
}) {
  const isStreaming = useChatStore((s) => s.isStreaming);
  const disabled = isStreaming || editing;
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    // 助手消息复制干净正文（有 thinking 字段时正文已剥离思考；旧数据降级解析）
    const text = isUser ? message.content : visibleAssistantContent(message);
    if (!text.trim()) return;
    const ok = await copyText(text);
    if (!ok) {
      useUiStore.getState().pushToast("复制失败：剪贴板不可用", "error");
      return;
    }
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
        {copied ? <IconCheck /> : <IconCopy />}
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
          <IconPencil />
        </button>
      )}
      {canRetry && (
        <button
          type="button"
          className="message-action-btn"
          disabled={disabled}
          onClick={onRetry}
          title="重试：删除这条回复及其后内容，用原提问重新生成"
          aria-label="重试"
        >
          <IconRetry />
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
        <IconRewind />
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
  /*
   * 破坏性操作的二次确认
   *
   * 重试 / 编辑会截断目标之后的全部消息（不可恢复）。既有内容会被删掉时
   * 先弹统一确认框（与"回退到此处"同一标准）；不丢内容则直接执行，
   * 不为"重新生成最后一条"这种日常操作加摩擦。
   */
  const [pendingAction, setPendingAction] = useState<
    { kind: "retry" } | { kind: "edit"; content: string } | null
  >(null);

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
    if (lostMessageCount(message.id) > 0) {
      setPendingAction({ kind: "edit", content: next });
      return;
    }
    // 事件回调里用 getState()：气泡本体不订阅 store，避免整列表跟着重渲染
    void useChatStore.getState().editMessage(message.id, next);
  };

  const handleRetry = () => {
    if (lostMessageCount(message.id) > 0) {
      setPendingAction({ kind: "retry" });
      return;
    }
    void useChatStore.getState().retryMessage(message.id);
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

  // 确认框的执行与取消（取消"编辑"确认时回到编辑态，不丢用户刚敲的内容）
  const confirmedRef = useRef(false);
  const runPending = async () => {
    const action = pendingAction;
    if (!action) return;
    confirmedRef.current = true;
    if (action.kind === "retry") {
      await useChatStore.getState().retryMessage(message.id);
    } else {
      await useChatStore.getState().editMessage(message.id, action.content);
    }
  };
  const closePending = () => {
    const action = pendingAction;
    const confirmed = confirmedRef.current;
    confirmedRef.current = false;
    setPendingAction(null);
    if (!confirmed && action?.kind === "edit") {
      setDraft(action.content);
      setEditing(true);
    }
  };

  const lost = pendingAction ? lostMessageCount(message.id) : 0;

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
              <span className="thinking-icon">
                <IconBrain />
              </span>
              <span>思考过程</span>
              <span className={`thinking-arrow ${thinkingExpanded ? "expanded" : ""}`}>
                <IconChevronRight />
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
        onRetry={handleRetry}
        onRewind={onRewind}
      />
      {/* 重试/编辑会删除后续消息：与"回退"同一套确认标准（不丢内容时不打扰） */}
      {pendingAction && (
        <ConfirmDialog
          title={pendingAction.kind === "retry" ? "确认重试" : "确认编辑并重新生成"}
          description={
            pendingAction.kind === "retry" ? (
              <>
                将重新生成这条回复，并删除其后的 <strong>{lost}</strong> 条消息，且无法恢复。
              </>
            ) : (
              <>
                将用编辑后的内容重新生成回复，并删除这条提问之后的{" "}
                <strong>{lost}</strong> 条消息，且无法恢复。
              </>
            )
          }
          confirmLabel={pendingAction.kind === "retry" ? "确认重试" : "保存并重新生成"}
          danger
          onConfirm={runPending}
          onClose={closePending}
        />
      )}
    </div>
  );
}

/** memo 化：流式输出期间历史气泡不再随父级重渲染（跳过 markdown/highlight 重复解析） */
export const MessageBubble = memo(MessageBubbleImpl);
