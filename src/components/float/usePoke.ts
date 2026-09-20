import { useRef, useCallback, useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { STREAM_EVENT, isSameStream, type StreamEventData } from "../../types/events";

/** 台词来源：由人格 YAML 提供（后端 `get_persona_summary` 下发） */
export interface PokePersona {
  /** 短名，用于替代气泡里的"此方"等自称占位 */
  shortName: string;
  /** 预置台词；为空时用下面的中性兜底 */
  lines: string[];
  /** 连续戳到"生气"时的台词 */
  angryLine: string;
}

/**
 * 最后兜底（中性，不含任何角色名）
 *
 * 正常路径下台词来自人格 YAML；这里只在后端不可用/人格未配置时兜底，
 * 因此绝不能写死某个角色的台词，否则换人格后桌宠还会说上一个角色的话。
 */
const FALLBACK_LINES = [
  "呀！别戳啦～",
  "嗯？有什么事吗？",
  "好啦好啦，看过来啦～",
];
const FALLBACK_ANGRY = "不要再戳啦……要生气啦！";

/** 连续点击上限 */
const MAX_CONSECUTIVE = 10;
/** 冷却时间（毫秒） */
const COOLDOWN_MS = 10_000;
/** 气泡自动隐藏时间（毫秒） */
const BUBBLE_HIDE_MS = 5_000;
/** LLM 反应的最大长度（系统提示要求 30 字，这里做一次硬截断） */
const MAX_REACTION_CHARS = 30;

interface PokeConfig {
  enabled: boolean;
  probability: number;
  llmChance: number;
  /** 当前角色的台词（随人格变化） */
  persona: PokePersona;
}

/** 从人格台词里随机取一条 */
function pickLine(persona: PokePersona): string {
  const lines = persona.lines.length > 0 ? persona.lines : FALLBACK_LINES;
  return lines[Math.floor(Math.random() * lines.length)];
}

/** "生气"台词 */
function angryLine(persona: PokePersona): string {
  return persona.angryLine?.trim() ? persona.angryLine : FALLBACK_ANGRY;
}

/**
 * 戳一下 hook
 *
 * 返回：
 * - handlePoke: 调用以触发戳一下（由点击覆盖层调用）
 * - pokeBubble: 当前气泡内容，null 表示不显示
 */
export function usePoke(config: PokeConfig) {
  const [pokeBubble, setPokeBubble] = useState<string | null>(null);
  const clickCountRef = useRef(0);
  const cooldownRef = useRef(false);
  const hideTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cooldownTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const isFetchingLlmRef = useRef(false);

  /** 隐藏气泡 */
  const hideBubble = useCallback(() => {
    setPokeBubble(null);
  }, []);

  // 卸载时清理定时器，避免回调对已卸载组件 setState（内存泄漏 + 状态错乱）
  useEffect(
    () => () => {
      if (hideTimerRef.current) clearTimeout(hideTimerRef.current);
      if (cooldownTimerRef.current) clearTimeout(cooldownTimerRef.current);
    },
    [],
  );

  /** 显示气泡并设定自动隐藏 */
  const showBubble = useCallback((text: string) => {
    if (hideTimerRef.current) clearTimeout(hideTimerRef.current);
    setPokeBubble(text);
    hideTimerRef.current = setTimeout(hideBubble, BUBBLE_HIDE_MS);
  }, [hideBubble]);

  /** 调用 LLM 获取反应 */
  const fetchLlmReaction = useCallback(async () => {
    if (isFetchingLlmRef.current) return;
    isFetchingLlmRef.current = true;

    let unlisteners: Array<() => void> = [];

    try {
      const session = await invoke<{ id: string }>("find_or_create_today_session");
      const sid = session.id;
      const streamId = crypto.randomUUID();

      // 收集流式响应：按 stream_id 过滤，避免与主窗口的生成互相串流
      let response = "";
      unlisteners = await Promise.all([
        listen<StreamEventData>(STREAM_EVENT.chunk, (event) => {
          if (!isSameStream(event.payload, streamId)) return;
          response += event.payload.data;
        }),
        listen<StreamEventData>(STREAM_EVENT.end, (event) => {
          if (!isSameStream(event.payload, streamId)) return;
          response = event.payload.data;
        }),
      ]);

      await invoke("send_message", {
        sessionId: sid,
        content: "（用户戳了戳你）",
        systemHint:
          "【系统提示：用户刚刚戳了你一下，请保持你当前角色的语气回应，回复不得超过30字，不要使用表情符号，直接说台词即可】",
        streamId,
        // 一次性反应：不落库、不入聊天记录、不触发标题生成与记忆提取
        persist: false,
      });

      // 使用截断的响应
      const text = response.trim();
      if (text) {
        showBubble(
          text.length > MAX_REACTION_CHARS ? text.slice(0, MAX_REACTION_CHARS) : text,
        );
      } else {
        // 后端没有返回内容时也要有反馈，避免气泡停在占位符
        showBubble(pickLine(config.persona));
      }
    } catch (e) {
      console.error("[usePoke] LLM reaction failed:", e);
      // LLM 失败时回退到预置词条，避免气泡停留在占位符
      showBubble(pickLine(config.persona));
    } finally {
      unlisteners.forEach((fn) => fn());
      isFetchingLlmRef.current = false;
    }
  }, [showBubble, config.persona]);

  /** 处理戳一下 */
  const handlePoke = useCallback(() => {
    if (!config.enabled) return;

    // 冷却中
    if (cooldownRef.current) return;

    clickCountRef.current += 1;

    // 超过连续上限 → 生气 + 冷却
    if (clickCountRef.current >= MAX_CONSECUTIVE) {
      showBubble(angryLine(config.persona));
      cooldownRef.current = true;
      clickCountRef.current = 0;
      if (cooldownTimerRef.current) clearTimeout(cooldownTimerRef.current);
      cooldownTimerRef.current = setTimeout(() => {
        cooldownRef.current = false;
      }, COOLDOWN_MS);
      return;
    }

    // 概率判定是否触发
    if (Math.random() > config.probability) return;

    // 重置连续计数（触发后归零）
    clickCountRef.current = 0;

    // 概率判定是 LLM 反应还是预置词条
    if (Math.random() < config.llmChance) {
      // 先显示占位，LLM 响应后替换
      showBubble("…");
      void fetchLlmReaction();
    } else {
      showBubble(pickLine(config.persona));
    }
  }, [config, showBubble, fetchLlmReaction]);

  return { handlePoke, pokeBubble };
}
