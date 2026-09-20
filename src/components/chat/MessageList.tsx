import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore, type Message } from "../../stores/chatStore";
import { MessageBubble } from "./MessageBubble";
import { MessageTimeline } from "./MessageTimeline";
import { RewindConfirmDialog } from "./RewindConfirmDialog";
import { StreamingText } from "./StreamingText";
import { ToolCallList } from "./ToolCallCard";

/** 距底部小于该值时视为"位于底部"，才启用自动滚动跟随 */
const NEAR_BOTTOM_THRESHOLD = 80;

/**
 * 消息列表
 *
 * `personaShortName` 由 ChatWindow 传入（来自人格 YAML 的 `short_name`），
 * 用于让空状态文案随当前角色变化。
 */
export function MessageList({ personaShortName }: { personaShortName?: string }) {
  const messages = useChatStore((s) => s.messages);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const liveToolCalls = useChatStore((s) => s.liveToolCalls);
  const toolsByMessage = useChatStore((s) => s.toolsByMessage);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const sessions = useChatStore((s) => s.sessions);
  const isTaskSession =
    sessions.find((s) => s.id === currentSessionId)?.session_type === "task";
  const bottomRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [showStats, setShowStats] = useState(false);
  /** 回退确认的目标消息（null = 不显示弹窗） */
  const [rewindTarget, setRewindTarget] = useState<Message | null>(null);

  /*
   * 工具调用的落位规则
   *
   * - 生成中：作为独立区块显示在流式气泡上方（`liveToolCalls` 属于本轮生成）；
   * - 生成正常结束：store 已把本轮记录挂到新生成的回复消息上，由气泡自行渲染；
   * - 生成失败/被取消（没有产生回复消息）：`liveToolCalls` 无处可挂，
   *   作为独立区块显示在列表末尾，保证用户看得到到底执行过什么。
   */
  const tailAssistantId = useMemo(() => {
    const tail = messages[messages.length - 1];
    return tail && tail.role === "assistant" ? tail.id : null;
  }, [messages]);
  const tailHasToolRecords =
    tailAssistantId !== null && (toolsByMessage[tailAssistantId]?.length ?? 0) > 0;

  // 尾部那条回复还没拿到工具记录（例如回读尚未完成）时，先把内存里的记录挂上去
  const attachToTailAssistant =
    !isStreaming &&
    liveToolCalls.length > 0 &&
    tailAssistantId !== null &&
    !tailHasToolRecords;

  // 本轮没有产生回复消息 → 记录只能作为独立区块展示
  //
  // 额外要求 messages 非空：孤儿工具记录必然伴随着一条用户消息（工具调用由用户输入触发），
  // 所以"空会话 + 工具卡片"只可能是换会话时没清干净的残留。store 已经在换会话时复位
  // `liveToolCalls`，这里再挡一道，任何将来新增的会话切换路径都不会把卡片漏进新会话。
  const orphanToolCalls =
    !isStreaming &&
    messages.length > 0 &&
    liveToolCalls.length > 0 &&
    tailAssistantId === null &&
    !tailHasToolRecords;

  // 读取统计显示配置
  useEffect(() => {
    invoke<{ ui: { show_message_stats?: boolean } }>("get_config")
      .then((config) => setShowStats(config.ui.show_message_stats ?? false))
      .catch(() => {});
  }, []);

  // 监听配置变更事件
  useEffect(() => {
    const unlisten = listen("config-updated", () => {
      invoke<{ ui: { show_message_stats?: boolean } }>("get_config")
        .then((config) => setShowStats(config.ui.show_message_stats ?? false))
        .catch(() => {});
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  const isNearBottom = () => {
    const el = containerRef.current;
    if (!el) return true;
    return el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_THRESHOLD;
  };

  // 切换会话 / 首次挂载：无条件滚到最新消息
  //
  // 不能复用 `isNearBottom()`：切换瞬间 scrollTop 还是旧值（新会话刚挂载时是 0），
  // 从短会话切到 200 条消息的长会话必然判定"不在底部"，用户只能看到最旧一条。
  const prevSessionRef = useRef<string | null>(null);
  useEffect(() => {
    if (prevSessionRef.current === currentSessionId) return;
    prevSessionRef.current = currentSessionId;
    requestAnimationFrame(() => {
      bottomRef.current?.scrollIntoView({ block: "end" });
    });
  }, [currentSessionId, messages]);

  // 新消息到达时：仅当用户位于底部附近才自动滚动（上翻阅读历史时不打扰）
  useEffect(() => {
    if (isNearBottom()) {
      bottomRef.current?.scrollIntoView({ block: "end" });
    }
  }, [messages, liveToolCalls.length]);

  // 流式期间：低频跟随滚动
  //
  // 历史实现只要 isStreaming 为真就以 60fps 无限循环，且每帧读取
  // scrollHeight/scrollTop/clientHeight（强制同步布局）+ 每帧写入滚动位置，
  // 是典型的 layout thrashing；而绝大多数帧并没有新内容。
  // 现在改为 100ms 一次的定时器：流式文本本身也是按块到达的，观感足够。
  useEffect(() => {
    if (!isStreaming) return;
    const timer = setInterval(() => {
      if (isNearBottom()) {
        bottomRef.current?.scrollIntoView({ block: "end" });
      }
    }, 100);
    return () => clearInterval(timer);
  }, [isStreaming]);

  return (
    <div className="message-list-wrap">
      <div className="message-list" ref={containerRef}>
        {messages.length === 0 && !isStreaming && (
          <div className="empty-state">
            <div className="empty-icon">✦</div>
            {isTaskSession ? (
              <>
                <p>描述你的目标，Plan 阶段会先只读调查并给出计划</p>
                <p className="empty-state-hint">
                  例如「分析这个项目的构建流程并给出优化步骤」；
                  批准计划后会自动切到 Work 逐步执行，改动可随时回滚
                </p>
              </>
            ) : (
              <p>开始和{personaShortName ?? "角色"}聊天吧～</p>
            )}
          </div>
        )}
        {messages.map((msg, index) => (
          <MessageBubble
            key={msg.id}
            message={msg}
            showStats={showStats}
            anchorId={`msg-${msg.id}`}
            isLast={index === messages.length - 1}
            onRewind={setRewindTarget}
            // 尾部回复还没拿到落库记录时，用内存中的记录兜底渲染（避免卡片闪一下就没）
            liveToolCalls={
              attachToTailAssistant && msg.id === tailAssistantId
                ? liveToolCalls
                : undefined
            }
          />
        ))}
        {/* 进行中的工具调用：显示在流式气泡上方（悬浮窗不会收到工具事件） */}
        {isStreaming && liveToolCalls.length > 0 && (
          <div className="message-row assistant">
            <div className="message-bubble streaming tool-live-bubble">
              <ToolCallList calls={liveToolCalls} />
            </div>
          </div>
        )}
        {isStreaming && <StreamingText />}
        {/* 没有对应回复的工具记录（生成失败/被取消） */}
        {orphanToolCalls && (
          <div className="message-row assistant">
            <div className="message-bubble tool-live-bubble">
              <ToolCallList calls={liveToolCalls} />
            </div>
          </div>
        )}
        <div ref={bottomRef} />
      </div>
      {/* 右侧消息时间轴：按消息实际位置打点，点击跳转、滚动高亮 */}
      <MessageTimeline messages={messages} containerRef={containerRef} />
      {rewindTarget && (
        <RewindConfirmDialog
          message={rewindTarget}
          onClose={() => setRewindTarget(null)}
        />
      )}
    </div>
  );
}
