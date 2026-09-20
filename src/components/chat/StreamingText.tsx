import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useChatStore } from "../../stores/chatStore";
import { STREAM_EVENT, isSameStream, type StreamEventData } from "../../types/events";
import { MarkdownContent } from "./MarkdownContent";
import { splitStableBlocks } from "./streamBlocks";

interface Props {
  /** 流式结束回调（可选） */
  onFinished?: (content: string, thinking: string) => void;
}

/**
 * 流式文本组件 —— 自包含订阅 stream-chunk / stream-thinking-chunk 事件
 *
 * chunk 先累积到 ref，再通过 requestAnimationFrame 合帧刷新，
 * 避免每个 token 都触发上层 React 重渲染。
 * 只处理**本次生成**（stream_id）的事件：主窗口与悬浮窗可能共用同一个会话，
 * 仅按 session_id 过滤无法阻止跨窗口串流。
 */
export function StreamingText({ onFinished }: Props) {
  const activeStreamId = useChatStore((s) => s.activeStreamId);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const [display, setDisplay] = useState({ content: "", thinking: "" });
  const bufRef = useRef({ content: "", thinking: "", dirty: false });
  const finishedRef = useRef(onFinished);
  finishedRef.current = onFinished;

  useEffect(() => {
    let raf = 0;
    let disposed = false;
    const unlistens: Array<() => void> = [];

    // 切换会话/重新发起生成时必须清空旧缓冲，否则上一个会话的内容会残留在新气泡里
    bufRef.current = { content: "", thinking: "", dirty: false };
    setDisplay({ content: "", thinking: "" });

    const flush = () => {
      raf = 0;
      const buf = bufRef.current;
      if (!buf.dirty) return;
      buf.dirty = false;
      setDisplay({ content: buf.content, thinking: buf.thinking });
    };

    const schedule = () => {
      if (!raf) raf = requestAnimationFrame(flush);
    };

    // 本次生成既要是当前 activeStreamId，也必须仍属于当前会话：
    // 换会话后旧流的事件不能再画进新会话的气泡里
    const matches = (payload: StreamEventData) =>
      isSameStream(payload, activeStreamId) &&
      payload.session_id === currentSessionId;

    // 注意：所有 listen 都是异步的，必须等 Promise 全部落地后再登记注销函数，
    // 否则 React 严格模式下的"挂载→清理→再挂载"会留下重复监听。
    void (async () => {
      // allSettled：单个订阅失败不能带走其它监听
      // （Promise.all 失败时已成功返回的 unlisten 会全部泄漏）
      const results = await Promise.allSettled([
        listen<StreamEventData>(STREAM_EVENT.chunk, (event) => {
          if (!matches(event.payload)) return;
          bufRef.current.content += event.payload.data;
          bufRef.current.dirty = true;
          schedule();
        }),
        listen<StreamEventData>(STREAM_EVENT.thinking, (event) => {
          if (!matches(event.payload)) return;
          bufRef.current.thinking += event.payload.data;
          bufRef.current.dirty = true;
          schedule();
        }),
        // 后端在流结束后才落库并发 stream-end；本组件随后被卸载，
        // 这里监听 end 仅用于把最终内容回传给需要方
        listen<StreamEventData>(STREAM_EVENT.end, (event) => {
          if (!matches(event.payload)) return;
          finishedRef.current?.(event.payload.data, bufRef.current.thinking);
        }),
      ]);

      if (disposed) {
        results.forEach((result) => {
          if (result.status === "fulfilled") result.value();
        });
        return;
      }
      for (const result of results) {
        if (result.status === "fulfilled") {
          unlistens.push(result.value);
        } else {
          console.error("Failed to subscribe stream events:", result.reason);
        }
      }
    })();

    return () => {
      disposed = true;
      if (raf) cancelAnimationFrame(raf);
      unlistens.forEach((fn) => fn());
    };
  }, [activeStreamId, currentSessionId]);

  const hasThinking = display.thinking.trim().length > 0;
  const hasContent = display.content.trim().length > 0;

  // 无内容且无思考时显示思考指示器（等待首个 chunk）
  if (!hasContent && !hasThinking) {
    return (
      <div className="message-row assistant">
        <div className="message-bubble streaming thinking">
          <div className="thinking-dots">
            <span className="dot">.</span>
            <span className="dot">.</span>
            <span className="dot">.</span>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="message-row assistant">
      <div className="message-bubble streaming">
        {/* 思考内容（可折叠） */}
        {hasThinking && (
          <ThinkingSection thinking={display.thinking} />
        )}
        {/* 正文内容：实时渲染 Markdown（稳定块 memo、尾部每帧解析），
            流式结束后由落库消息的完整气泡接管 */}
        {hasContent ? (
          <StreamingMarkdown content={display.content} />
        ) : (
          // 有思考但尚无正文时，显示等待指示
          <div className="thinking-dots">
            <span className="dot">.</span>
            <span className="dot">.</span>
            <span className="dot">.</span>
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * 流式正文：稳定块逐块 memo 渲染 + 尾部每帧重解析
 *
 * - 稳定块（围栏闭合、以空行结束）内容不会再变，memo 命中后跳过重解析，
 *   并开启代码高亮；
 * - 尾部仍在追加，每帧解析一次且暂不高亮（半截代码高亮会每帧抖动），
 *   光标作为文本附加在尾部末尾，跟随最后一个段落内联显示。
 */
function StreamingMarkdown({ content }: { content: string }) {
  const { stable, tail } = useMemo(() => splitStableBlocks(content), [content]);

  return (
    <div className="message-content">
      {stable.map((block, index) => (
        <MarkdownContent key={index} content={block} />
      ))}
      {tail.length > 0 ? (
        <MarkdownContent content={`${tail}▊`} highlight={false} />
      ) : (
        <span className="cursor-blink">▊</span>
      )}
    </div>
  );
}

/** 思考过程折叠区（独立组件，避免展开状态受父级重渲染影响） */
function ThinkingSection({ thinking }: { thinking: string }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className="thinking-section">
      <button
        className="thinking-toggle"
        onClick={() => setExpanded(!expanded)}
        aria-expanded={expanded}
      >
        <span className="thinking-icon">🧠</span>
        <span>思考过程</span>
        <span className={`thinking-arrow ${expanded ? "expanded" : ""}`}>▶</span>
      </button>
      {expanded && <div className="thinking-content">{thinking}</div>}
    </div>
  );
}
