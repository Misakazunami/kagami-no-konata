import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  STREAM_EVENT,
  TOOL_EVENT,
  isSameStream,
  type MessageStatsData,
  type StreamErrorData,
  type StreamEventData,
  type ToolApprovalRequestData,
  type ToolApprovalResolvedData,
  type ToolCallResultData,
  type ToolCallStartData,
  type ToolDecision,
} from "../types/events";
import {
  toToolInvocation,
  type ToolCallView,
  type ToolInvocation,
} from "../types/tools";

export interface Message {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: string;
  session_id: string;
  token_count: number;
  thinking_ms: number;
  thinking?: string;
}

export interface Session {
  id: string;
  title: string;
  persona_id: string;
  created_at: string;
  updated_at: string;
}

interface ChatState {
  // 状态
  sessions: Session[];
  currentSessionId: string | null;
  messages: Message[];
  isStreaming: boolean;
  /** 当前进行中生成的唯一标识（用于流式事件过滤与取消） */
  activeStreamId: string | null;
  /** 最近一次错误（供界面展示，避免只打 console 用户无感） */
  errorMessage: string | null;
  initialized: boolean;

  /**
   * 本次生成正在发生 / 刚结束的工具调用
   *
   * 只在主窗口维护（悬浮窗不参与工具调用）。新一轮生成开始时清空；
   * `stream-end` / `stream-error` 之后**保留**，让气泡继续展示已完成的工具记录。
   */
  liveToolCalls: ToolCallView[];
  /** 历史工具调用记录，按 `message_id` 归组（按会话惰性加载） */
  toolsByMessage: Record<string, ToolInvocation[]>;
  /** 待用户确认的工具审批（主窗口同一时刻只保留一个） */
  pendingApproval: ToolApprovalRequestData | null;

  // 页面
  currentPage: "chat" | "settings" | "persona" | "onboarding";

  // 操作
  setCurrentPage: (page: "chat" | "settings" | "persona" | "onboarding") => void;
  clearError: () => void;
  initSession: () => Promise<void>;
  refreshCurrentSession: () => Promise<void>;
  loadSessions: () => Promise<void>;
  createSession: (title?: string, personaId?: string) => Promise<string>;
  switchSession: (sessionId: string) => Promise<void>;
  deleteSession: (sessionId: string) => Promise<void>;
  renameSession: (sessionId: string, title: string) => Promise<void>;
  sendMessage: (content: string) => Promise<void>;
  stopGeneration: () => Promise<void>;
  updateSessionTitle: (sessionId: string, title: string) => void;
  loadToolInvocations: (sessionId: string) => Promise<void>;
  resolveApproval: (approvalId: string, decision: ToolDecision) => Promise<void>;
}

const toMessage = (error: unknown): string =>
  typeof error === "string" ? error : error instanceof Error ? error.message : String(error);

/**
 * 当前窗口是否为主窗口
 *
 * 悬浮窗是纯聊天桌宠：工具事件一律不订阅、不渲染，
 * 否则桌宠会弹出审批框、并在气泡里出现工具卡片。
 * 非 Tauri 环境（纯浏览器调试）拿不到窗口信息，按"非主窗口"处理。
 */
const isMainWindow = (): boolean => {
  try {
    return getCurrentWebviewWindow().label !== "float";
  } catch {
    return false;
  }
};

export const useChatStore = create<ChatState>((set, get) => ({
  sessions: [],
  currentSessionId: null,
  messages: [],
  isStreaming: false,
  activeStreamId: null,
  errorMessage: null,
  liveToolCalls: [],
  toolsByMessage: {},
  pendingApproval: null,
  currentPage: "chat",
  initialized: false,

  setCurrentPage: (page) => set({ currentPage: page }),

  clearError: () => set({ errorMessage: null }),

  loadSessions: async () => {
    try {
      const sessions = await invoke<Session[]>("get_sessions");
      set({ sessions });
    } catch (e) {
      console.error("Failed to load sessions:", e);
      set({ errorMessage: `加载会话列表失败：${toMessage(e)}` });
    }
  },

  /** 首次初始化：只执行一次，查找或创建今日会话 */
  initSession: async () => {
    if (get().initialized) return;
    set({ initialized: true });

    try {
      const session = await invoke<Session>("find_or_create_today_session");
      await get().loadSessions();
      const messages = await invoke<Message[]>("get_messages", {
        sessionId: session.id,
      });
      set({ currentSessionId: session.id, messages });
      // 历史工具记录按会话惰性加载：失败也不影响正常对话
      void get().loadToolInvocations(session.id);
    } catch (e) {
      console.error("Failed to init session:", e);
      // 失败时必须允许重试：否则 initialized 守卫会让界面永久停在"正在准备今日会话..."
      set({ initialized: false, errorMessage: `初始化会话失败：${toMessage(e)}` });
    }
  },

  /** 刷新当前会话状态（从设置页返回、跨窗口同步时使用，不创建新会话） */
  refreshCurrentSession: async () => {
    await get().loadSessions();
    const { currentSessionId, sessions } = get();
    // 如果当前会话仍存在，刷新其消息
    if (currentSessionId && sessions.some((s) => s.id === currentSessionId)) {
      try {
        const messages = await invoke<Message[]>("get_messages", {
          sessionId: currentSessionId,
        });
        set({ messages });
        void get().loadToolInvocations(currentSessionId);
      } catch (e) {
        console.error("Failed to refresh session:", e);
      }
    } else if (sessions.length > 0) {
      // 当前会话已不存在（例如被删除），切换到最新的会话
      const latest = sessions[0];
      try {
        const messages = await invoke<Message[]>("get_messages", {
          sessionId: latest.id,
        });
        set({ currentSessionId: latest.id, messages });
        void get().loadToolInvocations(latest.id);
      } catch (e) {
        console.error("Failed to switch session:", e);
      }
    } else {
      set({ currentSessionId: null, messages: [], toolsByMessage: {} });
    }
  },

  createSession: async (title, personaId) => {
    const sessionId = await invoke<string>("create_session", {
      title: title ?? null,
      personaId: personaId ?? null,
    }).catch((e) => {
      set({ errorMessage: `创建会话失败：${toMessage(e)}` });
      throw e;
    });
    await get().loadSessions();
    set({ currentSessionId: sessionId, messages: [] });
    return sessionId;
  },

  switchSession: async (sessionId) => {
    try {
      const messages = await invoke<Message[]>("get_messages", { sessionId });
      // 切换会话时不能沿用上一个会话的流式状态与工具状态
      set({
        currentSessionId: sessionId,
        messages,
        isStreaming: false,
        activeStreamId: null,
        liveToolCalls: [],
        pendingApproval: null,
      });
      void get().loadToolInvocations(sessionId);
    } catch (e) {
      console.error("Failed to switch session:", e);
      set({ errorMessage: `切换会话失败：${toMessage(e)}` });
    }
  },

  deleteSession: async (sessionId) => {
    try {
      await invoke("delete_session", { sessionId });
      if (get().currentSessionId === sessionId) {
        set({
          currentSessionId: null,
          messages: [],
          isStreaming: false,
          activeStreamId: null,
          liveToolCalls: [],
          toolsByMessage: {},
          pendingApproval: null,
        });
      }
      // 删除当前会话后必须重新挑选一个会话，否则主区会永久停在"正在准备今日会话..."
      await get().refreshCurrentSession();
    } catch (e) {
      console.error("Failed to delete session:", e);
      set({ errorMessage: `删除会话失败：${toMessage(e)}` });
    }
  },

  renameSession: async (sessionId, title) => {
    try {
      await invoke("update_session_title", { sessionId, title });
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, title } : s
        ),
      }));
    } catch (e) {
      console.error("Failed to rename session:", e);
      set({ errorMessage: `重命名失败：${toMessage(e)}` });
    }
  },

  updateSessionTitle: (sessionId, title) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, title } : s
      ),
    }));
  },

  /**
   * 发送消息
   *
   * 流式内容由 StreamingText 组件自行订阅事件渲染，不经过 store，
   * 避免每个 chunk 触发全列表重渲染；本方法只管理 isStreaming 与最终消息落位。
   *
   * 关键点：
   * - 每次生成带唯一 `streamId`，事件只按它过滤（两个窗口可能共用同一个会话）
   * - 状态复位放在 `finally`，**不能**依赖事件到达（事件丢失会让输入框永久禁用）
   */
  sendMessage: async (content) => {
    const { currentSessionId, isStreaming } = get();
    if (!currentSessionId || !content.trim()) return;
    // 跨窗口无法互斥，但至少挡住本窗口的重复提交
    if (isStreaming) return;

    const streamId = crypto.randomUUID();
    const isCurrentStream = () => get().activeStreamId === streamId;

    const userMsg: Message = {
      id: crypto.randomUUID(),
      role: "user",
      content: content.trim(),
      timestamp: new Date().toISOString(),
      session_id: currentSessionId,
      token_count: 0,
      thinking_ms: 0,
    };
    set((state) => ({
      messages: [...state.messages, userMsg],
      isStreaming: true,
      activeStreamId: streamId,
      errorMessage: null,
      // 新一轮生成开始：清空上一轮的工具调用与残留审批
      liveToolCalls: [],
      pendingApproval: null,
    }));

    // 接收统计信息（后端在 stream-end 之前发出）
    let pendingStats = { token_count: 0, thinking_ms: 0 };
    let finished = false;

    const unlisteners: Array<() => void> = [];
    // 悬浮窗绝不订阅工具事件（纯聊天桌宠，不出现任何工具 UI）
    const withToolEvents = isMainWindow();

    try {
      const unlistenStats = await listen<MessageStatsData>(
        STREAM_EVENT.stats,
        (event) => {
          if (!isSameStream(event.payload, streamId)) return;
          pendingStats = {
            token_count: event.payload.token_count,
            thinking_ms: event.payload.thinking_ms,
          };
        }
      );
      unlisteners.push(unlistenStats);

      const unlistenEnd = await listen<StreamEventData>(
        STREAM_EVENT.end,
        (event) => {
          if (!isSameStream(event.payload, streamId)) return;
          finished = true;
          const assistantMsg: Message = {
            id: crypto.randomUUID(),
            role: "assistant",
            content: event.payload.data,
            timestamp: new Date().toISOString(),
            session_id: event.payload.session_id,
            token_count: pendingStats.token_count,
            thinking_ms: pendingStats.thinking_ms,
          };
          set((state) => ({
            messages: [...state.messages, assistantMsg],
            /*
             * 生成结束：保留 liveToolCalls（气泡要继续展示已完成的工具记录），
             * 同时把它们就地挂到这条消息 id 下——落库记录（带 message_id）只有回读
             * 才拿得到，而回读用的是数据库消息 id，与这里即时生成的消息 id 并不相同。
             * 不这样做的话，下一条消息开始生成（liveToolCalls 被清空）时卡片会凭空消失。
             */
            toolsByMessage:
              state.liveToolCalls.length > 0
                ? {
                    ...state.toolsByMessage,
                    [assistantMsg.id]: state.liveToolCalls.map((call) =>
                      toToolInvocation(call, {
                        sessionId: event.payload.session_id,
                        streamId,
                        messageId: assistantMsg.id,
                      })
                    ),
                  }
                : state.toolsByMessage,
            // 审批弹窗不能继续挂着，否则没人能处理它
            pendingApproval: null,
            ...(state.activeStreamId === streamId
              ? { isStreaming: false, activeStreamId: null }
              : {}),
          }));
        }
      );
      unlisteners.push(unlistenEnd);

      const unlistenError = await listen<StreamErrorData>(
        STREAM_EVENT.error,
        (event) => {
          if (!isSameStream(event.payload, streamId)) return;
          finished = true;
          set((state) => ({
            errorMessage: event.payload.message,
            // 失败同样保留已有的工具调用，方便用户判断是哪一步出的问题
            pendingApproval: null,
            ...(state.activeStreamId === streamId
              ? { isStreaming: false, activeStreamId: null }
              : {}),
          }));
        }
      );
      unlisteners.push(unlistenError);

      if (withToolEvents) {
        const unlistenToolStart = await listen<ToolCallStartData>(
          TOOL_EVENT.start,
          (event) => {
            if (!isSameStream(event.payload, streamId)) return;
            const {
              call_id,
              tool,
              tool_label,
              args_preview,
              permission,
              step,
            } = event.payload;
            set((state) => ({
              liveToolCalls: [
                // 同一个 call_id 重复到达时以最新一次为准，不产生重复卡片
                ...state.liveToolCalls.filter((c) => c.call_id !== call_id),
                {
                  call_id,
                  tool,
                  tool_label,
                  args_preview,
                  permission,
                  step,
                  status: "running",
                  preview: null,
                  duration_ms: 0,
                  truncated: false,
                },
              ],
            }));
          }
        );
        unlisteners.push(unlistenToolStart);

        const unlistenToolResult = await listen<ToolCallResultData>(
          TOOL_EVENT.result,
          (event) => {
            if (!isSameStream(event.payload, streamId)) return;
            const result = event.payload;
            set((state) => ({
              liveToolCalls: state.liveToolCalls.map((call) =>
                call.call_id === result.call_id
                  ? {
                      ...call,
                      status: result.status,
                      preview: result.preview,
                      duration_ms: result.duration_ms,
                      truncated: result.truncated,
                      error: result.error,
                    }
                  : call
              ),
            }));
          }
        );
        unlisteners.push(unlistenToolResult);

        const unlistenApprovalRequest = await listen<ToolApprovalRequestData>(
          TOOL_EVENT.approvalRequest,
          (event) => {
            if (!isSameStream(event.payload, streamId)) return;
            set({ pendingApproval: event.payload });
          }
        );
        unlisteners.push(unlistenApprovalRequest);

        const unlistenApprovalResolved = await listen<ToolApprovalResolvedData>(
          TOOL_EVENT.approvalResolved,
          (event) => {
            if (!isSameStream(event.payload, streamId)) return;
            const current = get().pendingApproval;
            // 只关闭同一个审批：避免迟到的旧事件误关新弹窗
            if (!current || current.approval_id === event.payload.approval_id) {
              set({ pendingApproval: null });
            }
          }
        );
        unlisteners.push(unlistenApprovalResolved);
      }

      await invoke("send_message", {
        sessionId: currentSessionId,
        content: content.trim(),
        streamId,
      });
    } catch (e) {
      console.error("Failed to send message:", e);
      set({ errorMessage: `发送失败：${toMessage(e)}` });
    } finally {
      unlisteners.forEach((fn) => fn());

      // 兜底复位：即使事件全部丢失，也必须让输入框可用
      if (isCurrentStream()) {
        set({ isStreaming: false, activeStreamId: null });
      }

      // 本次生成已经结束（无论正常与否），属于它的审批弹窗不能继续挂着
      const pending = get().pendingApproval;
      if (pending && pending.stream_id === streamId) {
        set({ pendingApproval: null });
      }

      // 没有收到流结束信号时（异常/取消），从数据库回读一次以对齐真实状态
      if (!finished) {
        void get().refreshCurrentSession();
      }
    }
  },

  /** 停止当前会话正在进行的生成（已生成的部分内容会被保留） */
  stopGeneration: async () => {
    const { currentSessionId, activeStreamId, isStreaming } = get();
    if (!isStreaming) return;
    try {
      await invoke<boolean>("stop_generation", {
        streamId: activeStreamId ?? null,
        sessionId: currentSessionId ?? null,
      });
    } catch (e) {
      console.error("Failed to stop generation:", e);
    } finally {
      // 无论后端是否成功接收到取消指令，都先恢复界面可交互状态
      set({ isStreaming: false, activeStreamId: null });
    }
  },

  /**
   * 加载某个会话的历史工具调用记录
   *
   * 记录按 `message_id` 归组，供消息气泡渲染；`message_id` 为空的记录
   * （尚未绑定到消息）直接跳过。整体替换而非合并：本方法只服务于当前会话，
   * 合并会让上一个会话的记录残留下来。
   */
  loadToolInvocations: async (sessionId) => {
    try {
      const rows = await invoke<ToolInvocation[]>("get_tool_invocations", {
        sessionId,
      });
      const grouped: Record<string, ToolInvocation[]> = {};
      for (const row of rows) {
        if (!row.message_id) continue;
        const list = grouped[row.message_id];
        if (list) list.push(row);
        else grouped[row.message_id] = [row];
      }
      set({ toolsByMessage: grouped });
    } catch (e) {
      // 拿不到工具记录不应影响正常对话：只记录，不弹全局错误
      console.error("Failed to load tool invocations:", e);
    }
  },

  /**
   * 提交工具审批结果
   *
   * 无论后端是否接受，都必须关闭弹窗：审批可能已经超时/被另一个窗口处理，
   * 把用户困在模态框里是更糟的结果。
   */
  resolveApproval: async (approvalId, decision) => {
    try {
      await invoke("resolve_tool_approval", { approvalId, decision });
    } catch (e) {
      console.error("Failed to resolve tool approval:", e);
      set({ errorMessage: `工具审批提交失败：${toMessage(e)}` });
    } finally {
      const current = get().pendingApproval;
      if (!current || current.approval_id === approvalId) {
        set({ pendingApproval: null });
      }
    }
  },
}));
