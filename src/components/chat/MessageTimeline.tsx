import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import type { Message } from "../../stores/chatStore";

/** 轨道最多渲染多少个刻度（超出后抽稀，避免长会话塞进几千个 DOM 节点） */
const MAX_TICKS = 400;
/** 悬停提示里的正文预览长度 */
const PREVIEW_LENGTH = 40;

interface Tick {
  id: string;
  role: Message["role"];
  title: string;
  /** 相对滚动内容的百分比位置 */
  top: number;
}

interface Props {
  messages: Message[];
  containerRef: RefObject<HTMLDivElement | null>;
}

const selectorFor = (messageId: string): string => `#msg-${CSS.escape(messageId)}`;

const formatTime = (iso: string): string => {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
};

/**
 * 消息时间轴（右侧竖向轨道）
 *
 * 按消息在滚动内容里的实际位置打点（`offsetTop / scrollHeight`），用户消息
 * 用强调色、助手回复用小灰点；点击跳转到对应消息，滚动时高亮当前阅读位置。
 * 位置在消息变化 / 容器尺寸变化时重新测量——流式内容只在 `stream-end` 追加
 * 消息，因此不会每帧重排。
 */
export function MessageTimeline({ messages, containerRef }: Props) {
  const [ticks, setTicks] = useState<Tick[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const rafRef = useRef(0);

  const measure = useCallback(() => {
    const container = containerRef.current;
    if (!container || messages.length === 0) {
      setTicks([]);
      return;
    }
    const scrollHeight = container.scrollHeight;
    if (scrollHeight <= 0) return;

    const all: Tick[] = [];
    for (const message of messages) {
      const el = container.querySelector<HTMLElement>(selectorFor(message.id));
      if (!el) continue;
      const label = message.role === "user" ? "用户" : message.role === "system" ? "系统" : "助手";
      all.push({
        id: message.id,
        role: message.role,
        title: `${label} · ${formatTime(message.timestamp)} · ${message.content
          .replace(/\s+/g, " ")
          .slice(0, PREVIEW_LENGTH)}`,
        top: ((el.offsetTop + el.offsetHeight / 2) / scrollHeight) * 100,
      });
    }

    // 抽稀：用户消息全保留，助手消息按步长采样
    let next = all;
    if (all.length > MAX_TICKS) {
      const step = Math.ceil(all.length / MAX_TICKS);
      next = all.filter((tick, index) => tick.role === "user" || index % step === 0);
    }
    setTicks(next);
  }, [containerRef, messages]);

  // 消息变化 / 面板缩放后重新测量
  useEffect(() => {
    measure();
    const container = containerRef.current;
    if (!container) return;
    const observer = new ResizeObserver(() => measure());
    observer.observe(container);
    const onResize = () => measure();
    window.addEventListener("resize", onResize);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", onResize);
    };
  }, [measure, containerRef]);

  // 当前阅读位置：视口上方 1/3 处最后一条跨过的消息
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const onScroll = () => {
      if (rafRef.current) return;
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = 0;
        const threshold = container.scrollTop + container.clientHeight / 3;
        let current: string | null = null;
        for (const message of messages) {
          const el = container.querySelector<HTMLElement>(selectorFor(message.id));
          if (el && el.offsetTop <= threshold) current = message.id;
        }
        setActiveId(current);
      });
    };
    onScroll();
    container.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      container.removeEventListener("scroll", onScroll);
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
    };
  }, [containerRef, messages]);

  const jumpTo = (messageId: string) => {
    const el = containerRef.current?.querySelector<HTMLElement>(selectorFor(messageId));
    if (!el) return;
    el.scrollIntoView({ block: "center", behavior: "smooth" });
  };

  if (ticks.length < 2) return null;

  return (
    <div className="message-timeline" role="navigation" aria-label="对话时间轴">
      {ticks.map((tick) => (
        <button
          key={tick.id}
          type="button"
          className={`timeline-tick ${tick.role}${tick.id === activeId ? " active" : ""}`}
          style={{ top: `${tick.top}%` }}
          title={tick.title}
          aria-label={tick.title}
          onClick={() => jumpTo(tick.id)}
        />
      ))}
    </div>
  );
}
