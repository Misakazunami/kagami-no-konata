import { useEffect, useMemo, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Live2DCanvas } from "./Live2DCanvas";
import { FloatingClock } from "./FloatingClock";
import { usePoke, type PokePersona } from "./usePoke";
import type { PersonaSummary } from "../../types/persona";
import { STREAM_EVENT, isSameStream, type StreamEventData } from "../../types/events";

interface FloatConfig {
  show_float_clock?: boolean;
  poke_enabled?: boolean;
  poke_probability?: number;
  poke_llm_chance?: number;
  bubble_auto_hide_secs?: number;
}

export function FloatingWidget() {
  const [useLive2D, setUseLive2D] = useState(true);
  const [, setLive2dError] = useState<string | null>(null);
  const [input, setInput] = useState("");
  const [isSending, setIsSending] = useState(false);
  const [bubbleContent, setBubbleContent] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const [showBubble, setShowBubble] = useState(false);
  const [showClock, setShowClock] = useState(false);

  // 戳一下配置 + 气泡自动隐藏时间
  const [pokeConfig, setPokeConfig] = useState({ enabled: true, probability: 0.3, llmChance: 0.15 });
  const autoHideSecsRef = useRef(20);

  const isStreamingRef = useRef(false);
  const inputFocusedRef = useRef(false);
  const autoHideTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** 失焦后的延时判断（150ms）：需要能取消，且不能读到陈旧的 bubbleContent */
  const blurTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const bubbleContentRef = useRef("");
  bubbleContentRef.current = bubbleContent;
  /** 本窗口发起的生成标识：只有它的事件才会渲染到气泡里 */
  const activeStreamIdRef = useRef<string | null>(null);

  // 清除自动隐藏计时器
  const clearAutoHide = () => {
    if (autoHideTimerRef.current) {
      clearTimeout(autoHideTimerRef.current);
      autoHideTimerRef.current = null;
    }
  };

  // 启动自动隐藏计时器
  const startAutoHide = () => {
    clearAutoHide();
    const secs = autoHideSecsRef.current;
    if (secs <= 0) return; // 0 表示不自动隐藏
    autoHideTimerRef.current = setTimeout(() => {
      setShowBubble(false);
    }, secs * 1000);
  };

  /*
   * 戳一戳台词来自当前人格（后端 `get_persona_summary` 解析"今日会话"的人格），
   * 悬浮窗常驻不会重新挂载，因此挂载时拉一次；人格由主窗口切换，
   * 下次悬浮窗重新显示时（visibility 变化）会再拉一次。
   */
  const [personaPoke, setPersonaPoke] = useState<PokePersona>({
    shortName: "角色",
    lines: [],
    angryLine: "",
  });

  const loadPersonaPoke = () => {
    invoke<PersonaSummary>("get_persona_summary", { personaId: null })
      .then((p) =>
        setPersonaPoke({
          shortName: p.short_name,
          lines: p.poke_lines ?? [],
          angryLine: p.poke_angry_line ?? "",
        }),
      )
      .catch((e) => console.error("[float] 读取人格台词失败:", e));
  };

  // 合并后的配置（useMemo 保证 handlePoke 的 useCallback 依赖稳定）
  const pokeConfigWithPersona = useMemo(
    () => ({ ...pokeConfig, persona: personaPoke }),
    [pokeConfig, personaPoke],
  );

  // 戳一下 hook
  const { handlePoke, pokeBubble } = usePoke(pokeConfigWithPersona);

  /** 读取配置（挂载时 + 配置变更时都要刷新：悬浮窗常驻，不会重新挂载） */
  const loadConfig = () => {
    invoke<{ ui: FloatConfig }>("get_config")
      .then((cfg) => {
        setShowClock(cfg.ui.show_float_clock ?? false);
        setPokeConfig({
          enabled: cfg.ui.poke_enabled ?? true,
          probability: cfg.ui.poke_probability ?? 0.3,
          llmChance: cfg.ui.poke_llm_chance ?? 0.15,
        });
        autoHideSecsRef.current = cfg.ui.bubble_auto_hide_secs ?? 20;
      })
      .catch(() => {});
  };

  useEffect(() => {
    loadConfig();
    loadPersonaPoke();
    // 设置页修改后需要立即生效，否则用户会以为"设置没保存"
    const unlistenConfig = listen("config-updated", loadConfig);
    // 主窗口切换会话人格后，悬浮窗需要重新取台词（桌宠常驻不重新挂载）
    const unlistenSession = listen("session-updated", loadPersonaPoke);
    return () => {
      unlistenConfig.then((fn) => fn()).catch(() => {});
      unlistenSession.then((fn) => fn()).catch(() => {});
    };
    // 仅在挂载时绑定：两个回调都是稳定的一次性拉取
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 卸载时清理自动隐藏与失焦计时器
  useEffect(
    () => () => {
      if (autoHideTimerRef.current) clearTimeout(autoHideTimerRef.current);
      if (blurTimerRef.current) clearTimeout(blurTimerRef.current);
    },
    []
  );

  // 流式事件：只渲染本窗口发起的那一次生成
  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    const matches = (payload: StreamEventData) =>
      isSameStream(payload, activeStreamIdRef.current);

    void (async () => {
      // allSettled：单个订阅失败不能带走其它监听
      const results = await Promise.allSettled([
        listen<StreamEventData>(STREAM_EVENT.chunk, (event) => {
          if (!matches(event.payload)) return;
          setBubbleContent((prev) => prev + event.payload.data);
        }),
        listen<StreamEventData>(STREAM_EVENT.end, (event) => {
          if (!matches(event.payload)) return;
          activeStreamIdRef.current = null;
          setBubbleContent(event.payload.data);
          setIsStreaming(false);
          isStreamingRef.current = false;
          setShowBubble(true);
          // 回复完毕后自动隐藏气泡
          if (!inputFocusedRef.current) {
            startAutoHide();
          }
        }),
        listen<{ stream_id?: string; message?: string }>(STREAM_EVENT.error, (event) => {
          if (!isSameStream(event.payload, activeStreamIdRef.current)) return;
          activeStreamIdRef.current = null;
          setIsStreaming(false);
          isStreamingRef.current = false;
          setBubbleContent(event.payload.message ?? "生成失败了…");
          setShowBubble(true);
          if (!inputFocusedRef.current) {
            startAutoHide();
          }
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
          unlisteners.push(result.value);
        } else {
          console.error("[float] 订阅流式事件失败:", result.reason);
        }
      }
    })();

    return () => {
      disposed = true;
      unlisteners.forEach((fn) => fn());
    };
  }, []);

  // 发送消息
  const handleSend = async () => {
    if (!input.trim() || isSending) return;
    const content = input.trim();
    setInput("");
    setIsSending(true);

    const streamId = crypto.randomUUID();
    activeStreamIdRef.current = streamId;
    clearAutoHide();
    setBubbleContent("");
    setIsStreaming(true);
    isStreamingRef.current = true;
    setShowBubble(true);

    try {
      const session = await invoke<{ id: string }>("find_or_create_today_session");
      await invoke("send_message", {
        sessionId: session.id,
        content,
        systemHint: "【请用简短的文字回复，控制在100字以内】",
        streamId,
      });
    } catch (e) {
      console.error("Failed to send:", e);
      activeStreamIdRef.current = null;
      setIsStreaming(false);
      isStreamingRef.current = false;
      setBubbleContent(`发送失败：${typeof e === "string" ? e : String(e)}`);
      setShowBubble(true);
      if (!inputFocusedRef.current) startAutoHide();
    } finally {
      setIsSending(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    // 与主窗口输入框同一规则：输入法组合期间的回车是选词，不是发送
    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const displayContent = isStreaming && !bubbleContent
    ? "···"
    : bubbleContent
      ? (bubbleContent.length > 120 ? bubbleContent.slice(0, 120) + "..." : bubbleContent)
      : "";

  return (
    <div className="desktop-pet">
      {/* 气泡（内嵌，有内容时显示） */}
      {(showBubble && displayContent) || pokeBubble ? (
        <div
          className="pet-inline-bubble"
          // 手动关闭：把"自动隐藏"设为 0 时气泡不会自己消失，
          // 没有这个入口它会永久盖住桌宠
          onClick={() => setShowBubble(false)}
          title="点击关闭气泡"
        >
          <div className="bubble-text">
            {pokeBubble ?? displayContent}
            {isStreaming && !pokeBubble && <span className="cursor-blink">▊</span>}
          </div>
        </div>
      ) : null}

      {/* 角色立绘 */}
      <div
        className="pet-image-wrap"
        onMouseDown={(e) => {
          const startX = e.clientX;
          const startY = e.clientY;
          let isDrag = false;

          const handleMove = (ev: MouseEvent) => {
            const dx = Math.abs(ev.clientX - startX);
            const dy = Math.abs(ev.clientY - startY);
            // 移动超过 5px 判定为拖拽，启动窗口拖拽
            if (!isDrag && (dx >= 5 || dy >= 5)) {
              isDrag = true;
              document.removeEventListener("mousemove", handleMove);
              document.removeEventListener("mouseup", handleUp);
              getCurrentWindow().startDragging().catch(() => {});
            }
          };

          const handleUp = () => {
            document.removeEventListener("mousemove", handleMove);
            document.removeEventListener("mouseup", handleUp);
            if (!isDrag) handlePoke();
          };

          document.addEventListener("mousemove", handleMove);
          document.addEventListener("mouseup", handleUp);
        }}
      >
        {showClock && <FloatingClock />}
        {useLive2D ? (
          <Live2DCanvas
            modelPath="/live2d/konata/konata.model3.json"
            width={200}
            height={200}
            onModelLoaded={() => console.log("[FloatingWidget] Live2D loaded")}
            onError={(err) => {
              console.warn("[FloatingWidget] Live2D error:", err.message);
              setLive2dError(err.message);
              setUseLive2D(false);
            }}
          />
        ) : (
          <img
            className="pet-image"
            src="/pet/default.png"
            alt={personaPoke.shortName}
            draggable={false}
          />
        )}
      </div>

      {/* 输入框 */}
      <div className="pet-input-bar">
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          onFocus={() => {
            inputFocusedRef.current = true;
            clearAutoHide();
            if (bubbleContent) setShowBubble(true);
          }}
          onBlur={() => {
            inputFocusedRef.current = false;
            if (blurTimerRef.current) clearTimeout(blurTimerRef.current);
            blurTimerRef.current = setTimeout(() => {
              blurTimerRef.current = null;
              if (!document.hasFocus()) {
                // 失焦时如果有内容，启动自动隐藏而非立即隐藏；
                // 这里读 ref 而不是闭包里的 bubbleContent：150ms 内可能刚收到
                // 第一个 chunk，用陈旧值会把正在流式的气泡直接藏掉
                if (bubbleContentRef.current && !isStreamingRef.current) {
                  startAutoHide();
                } else if (!isStreamingRef.current) {
                  setShowBubble(false);
                }
              }
            }, 150);
          }}
          placeholder="我在听哦..."
          disabled={isSending}
          rows={1}
        />
        <button onClick={handleSend} disabled={isSending || !input.trim()} style={{ color: '#0047AB' }}>
          ➤
        </button>
      </div>

      {/* 控制按钮 */}
      <div className="pet-controls-mini">
        <button className="pet-ctrl-btn" onClick={() => invoke("show_main_window")} title="展开主窗口">⧉</button>
        <button className="pet-ctrl-btn" onClick={() => invoke("hide_float_window")} title="隐藏桌宠">×</button>
      </div>
    </div>
  );
}
