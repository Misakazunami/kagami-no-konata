import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore } from "../../stores/chatStore";
import { MessageBubble } from "./MessageBubble";
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
  const bottomRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [showStats, setShowStats] = useState(false);

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
  const orphanToolCalls =
    !isStreaming &&
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
    <div className="message-list" ref={containerRef}>
      {messages.length === 0 && !isStreaming && (
        <div className="empty-state">
          <div className="empty-icon">✦</div>
          <p>开始和{personaShortName ?? "角色"}聊天吧～</p>
        </div>
      )}
      {messages.map((msg) => (
        <MessageBubble
          key={msg.id}
          message={msg}
          showStats={showStats}
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
  );
}
