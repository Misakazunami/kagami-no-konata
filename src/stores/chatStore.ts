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
  type ToolOutputChunkData,
  type PlanItem,
  type PlanUpdatedData,
  type PlanView,
  type NotesUpdatedData,
  type RestoreReportView,
  type RewindOutcomeView,
  type RewindPreviewView,
  type SessionNote,
  type SessionUpdatedData,
  type SnapshotInfoView,
  type SubagentStatusData,
} from "../types/events";
import {
  toToolInvocation,
  type ToolCallView,
  type ToolInvocation,
} from "../types/tools";
import type { ModelCatalog, SessionModelPref } from "../types/models";

export interface Message {
  id: string;
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: string;
  session_id: string;
  token_count: number;
  thinking_ms: number;
  thinking?: string;
  /**
   * 生成这条回复的模型（来自 `message-stats` 事件；后端已把同一标签写入
   * `messages.model`，因此历史回读也带着它）
   *
   * 自动选择下主轮次与子代理用的模型不同，必须让用户看得见"这条是谁答的"。
   */
  model?: string;
}

export interface Session {
  id: string;
  title: string;
  persona_id: string;
  session_type: "chat" | "task";
  task_mode: "plan" | "work";
  workspace_id?: string | null;
  /** 会话级模型选择（null/缺省 = 跟随全局活跃提供商） */
  model_pref?: SessionModelPref | null;
  /**
   * 会话级 AUTO：自动允许所有需要审批的工具调用（仅任务会话可开）
   *
   * 只跳过审批弹窗，不改变工具可见性与任何硬安全边界。
   */
  auto_approve_all: boolean;
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
   * 回退时写入输入框的预填充文本
   *
   * 回退一条用户消息时把原文放回输入框方便修改重发；`seq` 保证同一段文本
   * 二次回退也能触发 `useEffect`。
   */
  composerPrefill: { text: string; seq: number } | null;

  /**
   * 本轮生成是否因工具步数用尽被强制收尾
   *
   * 后端在 `message-stats` 里如实上报；界面据此提示"中断，可继续"，
   * 避免用户以为模型已经答完（长任务被静默截断是真实报障）。
   */
  stepLimitHit: boolean;
  /** 本次生成实际执行的工具轮数（"中断于第 N 步"用） */
  toolSteps: number;

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
  /**
   * 当前会话的任务计划（`update_plan` 工具维护，界面与模型看同一份）
   *
   * 与 `liveToolCalls` 不同，它是**会话级**状态：换会话时不做"轮次复位"，
   * 而是按新会话重新回读（见 `loadPlan`）。
   */
  plan: PlanItem[] | null;
  /** 计划的一行备注（模型写的阻塞原因等） */
  planNote: string | null;
  /**
   * 最近一次生成留下的文件改动备份（有值时界面显示"回滚"）
   *
   * 与 `plan` 一样属于会话级状态，换会话时清空。
   */
  snapshot: SnapshotInfoView | null;
  /** 回滚结果提示（成功/失败都如实显示一句话） */
  snapshotNotice: string | null;
  /**
   * 当前会话的工作记忆（模型主动记下的跨轮结论）
   *
   * 会话级状态：换会话时清空并回读（与 `plan` 同一套规则）。
   */
  notes: SessionNote[];

  /**
   * 当前会话已持久授权的工具名（审批弹窗「本会话允许」）
   *
   * 会话级状态：后端会把这些工具并入之后每轮的免审批清单，
   * 界面据此展示并可逐项撤销。
   */
  sessionGrants: string[];

  /**
   * 可选模型目录（提供商 → 已启用模型 + 能力探测结果）
   *
   * 由 `loadModelCatalog()` 从后端拉取；界面不重复实现能力启发式。
   * 设置页改过配置后回到对话页会重新挂载，因此这里按需刷新即可。
   */
  modelCatalog: ModelCatalog | null;

  // 页面
  currentPage: "chat" | "settings" | "persona" | "onboarding";

  // 操作
  setCurrentPage: (page: "chat" | "settings" | "persona" | "onboarding") => void;
  clearError: () => void;
  initSession: () => Promise<void>;
  refreshCurrentSession: () => Promise<void>;
  loadSessions: () => Promise<void>;
  createSession: (
    title?: string,
    personaId?: string,
    sessionType?: "chat" | "task",
    workspaceId?: string,
    modelPref?: SessionModelPref | null,
    taskMode?: "plan" | "work"
  ) => Promise<string>;
  setTaskMode: (sessionId: string, taskMode: "plan" | "work") => Promise<void>;
  /** 开关会话级 AUTO（自动允许所有审批类工具；仅任务会话） */
  setSessionAutoApprove: (sessionId: string, enabled: boolean) => Promise<void>;
  /** 设置/清除会话绑定的工作区（null = 默认沙箱） */
  setSessionWorkspace: (sessionId: string, workspaceId: string | null) => Promise<void>;
  /**
   * 设置会话级模型选择（手动模型 / 自动选择 / 深度思考开关）
   *
   * `null` 表示清除选择、回到"跟随全局活跃提供商"。只影响**下一轮**生成：
   * 正在跑的那一轮用的是发送那一刻的快照。
   */
  setSessionModel: (sessionId: string, pref: SessionModelPref | null) => Promise<void>;
  /** 拉取可选模型目录（提供商 → 已启用模型 + 能力探测） */
  loadModelCatalog: () => Promise<void>;
  switchSession: (sessionId: string) => Promise<void>;
  deleteSession: (sessionId: string) => Promise<void>;
  renameSession: (sessionId: string, title: string) => Promise<void>;
  sendMessage: (content: string) => Promise<void>;
  /** 重试：删除原回复及其后内容，用原提问重新生成 */
  retryMessage: (messageId: string) => Promise<void>;
  /** 编辑用户消息并立即重新生成（仅用户消息可编辑） */
  editMessage: (messageId: string, content: string) => Promise<void>;
  /**
   * 把本窗口乐观消息的本地 id 换成数据库 id
   *
   * 主窗口收到自己那轮的 `session-updated` 时调用（那次广播在本轮落库之后发出，
   * 因此 id 一定已经存在）。别的窗口仍走整表回读。
   */
  adoptLocalMessageIds: (payload: SessionUpdatedData) => void;
  /** 预览"回退到某条消息"的影响范围（只读，确认弹窗用） */
  previewRewind: (messageId: string, inclusive?: boolean) => Promise<RewindPreviewView>;
  /** 回退：删除该条及其之后的全部消息；可选一并撤销这些轮次的文件改动 */
  rewindTo: (messageId: string, restoreStreamIds?: string[]) => Promise<void>;
  /** 消费回退时写入的预填充文本（InputBox 用） */
  consumeComposerPrefill: () => void;
  stopGeneration: () => Promise<void>;
  updateSessionTitle: (sessionId: string, title: string) => void;
  loadToolInvocations: (sessionId: string) => Promise<void>;
  /** 回读某个会话的任务计划（换会话 / 初始化时调用） */
  loadPlan: (sessionId: string) => Promise<void>;
  /** 用户手动清空当前会话的计划 */
  clearPlan: () => Promise<void>;
  /** 用户手动编辑当前会话的计划（勾选 / 改标题 / 增删） */
  updatePlanItems: (items: PlanItem[], note?: string | null) => Promise<void>;
  /** 批准计划并执行：切到 Work 模式并发出一条执行指令 */
  approvePlan: () => Promise<void>;
  /** 继续执行未完成的任务（从计划里的进行中/待办/受阻项接续） */
  continueTask: () => Promise<void>;
  /** 回读某次生成的文件改动备份概况 */
  loadSnapshot: (sessionId: string, streamId: string) => Promise<void>;
  /** 回滚该次生成造成的全部文件改动 */
  restoreSnapshot: () => Promise<void>;
  /** 关闭回滚提示条 */
  dismissSnapshot: () => void;
  /** 回读某个会话的工作记忆 */
  loadNotes: (sessionId: string) => Promise<void>;
  /** 清空当前会话的工作记忆（用户对跨轮记忆的最终控制权） */
  clearNotes: () => Promise<void>;
  /** 回读某个会话已持久授权的工具 */
  loadSessionGrants: (sessionId: string) => Promise<void>;
  /** 撤销某个工具的会话授权 */
  revokeSessionGrant: (tool: string) => Promise<void>;
  resolveApproval: (approvalId: string, decision: ToolDecision) => Promise<void>;
}

const toMessage = (error: unknown): string =>
  typeof error === "string" ? error : error instanceof Error ? error.message : String(error);

/**
 * 卡片里实时输出的保留上限（字符）
 *
 * 命令输出可以到上百 MB，而这里只服务于"现在在打印什么"：
 * 超出后只留末尾一段，并在开头标注被丢弃的量。完整结果始终在 `preview` 里。
 */
const TOOL_OUTPUT_LIMIT = 8 * 1024;

const appendToolOutput = (prev: string | undefined, chunk: string): string => {
  const next = (prev ?? "") + chunk;
  if (next.length <= TOOL_OUTPUT_LIMIT) return next;
  const dropped = next.length - TOOL_OUTPUT_LIMIT;
  return `…[前 ${dropped} 字符已省略]…\n${next.slice(-TOOL_OUTPUT_LIMIT)}`;
};

/**
 * 换会话时必须一并复位的一次性状态
 *
 * `liveToolCalls` / `pendingApproval` 描述的是「当前会话正在发生（或刚刚结束）的这一轮生成」，
 * 而且 `stream-end` 之后 `liveToolCalls` 是**刻意保留**的（尾部气泡要继续展示已完成的工具记录，
 * 见 `sendMessage`）。代价是它们本身没有会话归属：任何换会话的动作
 * （新建 / 切换 / 删除 / 当前会话消失）都必须显式清掉，否则上一个会话的工具卡片会留在新会话里，
 * 在新会话 messages 为空时被 MessageList 判成「孤儿工具记录」直接渲染出来。
 */
export const emptyGenerationState = (): Pick<
  ChatState,
  | "isStreaming"
  | "activeStreamId"
  | "liveToolCalls"
  | "toolsByMessage"
  | "pendingApproval"
  | "plan"
  | "planNote"
  | "snapshot"
  | "snapshotNotice"
  | "notes"
  | "sessionGrants"
  | "stepLimitHit"
  | "toolSteps"
  | "composerPrefill"
> => ({
  isStreaming: false,
  activeStreamId: null,
  liveToolCalls: [],
  toolsByMessage: {},
  pendingApproval: null,
  // 计划是会话级状态：换会话时必须清空，随后由 `loadPlan` 回读新会话的那一份
  plan: null,
  planNote: null,
  // 回滚入口只对"刚刚这一轮"有意义，换会话后不再保留
  snapshot: null,
  snapshotNotice: null,
  notes: [],
  sessionGrants: [],
  stepLimitHit: false,
  toolSteps: 0,
  // 回退产生的草稿属于原会话：换会话不带着走
  composerPrefill: null,
});

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

/**
 * 计划事件的全局订阅（会话级）
 *
 * 不同于工具调用事件：计划是**会话状态**，可能在任何一次生成里被更新，
 * 所以不能挂在某次 `sendMessage` 的监听器上（那些监听器随生成结束就注销了）。
 * 这里注册一次，按 `session_id` 过滤——只有当前会话的计划会写进 store。
 *
 * 悬浮窗不订阅：它既不显示计划面板，模型侧也拿不到工具。
 */
let planEventsBound = false;
const bindPlanEvents = (set: (partial: Partial<ChatState>) => void, get: () => ChatState) => {
  if (planEventsBound || !isMainWindow()) return;
  planEventsBound = true;
  void listen<PlanUpdatedData>(TOOL_EVENT.planUpdated, (event) => {
    const { session_id, items, note } = event.payload;
    // 会话归属过滤：别的会话的计划更新不能画到当前会话的面板上
    if (get().currentSessionId !== session_id) return;
    set({
      plan: items.length > 0 ? items : null,
      planNote: note?.trim() ? note : null,
    });
  }).catch((e) => {
    // 订阅失败不影响对话：换会话时仍会回读一次
    planEventsBound = false;
    console.error("Failed to subscribe plan events:", e);
  });

  // 工作记忆：事件只带计数（正文可能很大），因此收到后回读一次
  void listen<NotesUpdatedData>(TOOL_EVENT.notesUpdated, (event) => {
    const { session_id } = event.payload;
    if (get().currentSessionId !== session_id) return;
    void get().loadNotes(session_id);
  }).catch((e) => {
    console.error("Failed to subscribe notes events:", e);
  });
};

/**
 * 最近一次由**本窗口**发起的 stream_id
 *
 * `session-updated` 是全局广播：主窗口收到自己发起的那一轮时不需要回读
 * （本地消息已带统计与模型标签，回读只会闪烁）。store 之外用模块变量记录，
 * 避免为一个只读比较项引入可订阅的状态。
 */
let lastLocalStreamId: string | null = null;

/** 本窗口最近发起的 stream_id（供 App 的事件监听判断"是不是自己"） */
export const getLastLocalStreamId = (): string | null => lastLocalStreamId;

type StoreSet = (
  partial: Partial<ChatState> | ((state: ChatState) => Partial<ChatState>)
) => void;
type StoreGet = () => ChatState;

/** 本轮生成要做什么：发送新消息 / 重试旧回复 / 编辑后重发 */
type GenerationMode =
  | { kind: "send"; content: string }
  | { kind: "retry"; messageId: string }
  | { kind: "edit"; messageId: string; content: string };

const GENERATION_LABEL: Record<GenerationMode["kind"], string> = {
  send: "发送",
  retry: "重试",
  edit: "编辑",
};

/**
 * 本窗口乐观消息的本地 id（等待 `session-updated` 换成数据库 id）
 *
 * 乐观消息的 id（`crypto.randomUUID()`）与数据库 id 不同源；主窗口收到自己那轮
 * 的 `session-updated` 时由 `adoptLocalMessageIds` 就地替换。不替换的话，
 * 紧接着点「重试 / 编辑 / 回退」时后端按本地 id 查不到消息。
 */
const localTurnIds = new Map<string, { userId?: string; assistantId?: string }>();

/**
 * 统一的生成流程（发送 / 重试 / 编辑共用）
 *
 * 流式内容由 StreamingText 组件自行订阅事件渲染，不经过 store，
 * 避免每个 chunk 触发全列表重渲染；本方法只管理 isStreaming 与最终消息落位。
 *
 * 关键点：
 * - 每次生成带唯一 `streamId`，事件按它 **且** 按发起会话过滤
 *   （两个窗口可能共用同一个会话，用户也可能中途换会话）
 * - 状态复位放在 `finally`，**不能**依赖事件到达（事件丢失会让输入框永久禁用）
 */
async function startGeneration(
  set: StoreSet,
  get: StoreGet,
  mode: GenerationMode
): Promise<void> {
  const { currentSessionId, isStreaming } = get();
  if (!currentSessionId) return;
  if (mode.kind === "send" && !mode.content.trim()) return;
  // 跨窗口无法互斥，但至少挡住本窗口的重复提交
  if (isStreaming) return;

  const streamId = crypto.randomUUID();
  lastLocalStreamId = streamId;
  const isCurrentStream = () => get().activeStreamId === streamId;

  /*
   * 本轮生成属于发起时的这个会话
   *
   * 监听器是按 `streamId` 注册的，用户中途切换/新建会话并不会让它们注销。
   * 若只按 `streamId` 过滤，上一轮的回复正文与工具卡片会直接落进新会话
   * （切到一个空会话时，就表现为"凭空出现的工具调用记录"）。因此事件还必须
   * 确认「发起它的会话仍是当前会话」才被采纳：换会话后旧流的事件一律丢弃，
   * 内容已经落库，切回去时由回读补上。
   *
   * 三个条件都要满足：载荷自带的两个 id 必须和这次生成一致（防止 id 不配对的
   * 脏事件混进来），且我们仍然停在这个会话上。
   */
  const sessionId = currentSessionId;
  const isCurrentGeneration = (payload: {
    session_id: string;
    stream_id: string;
  }): boolean =>
    isSameStream(payload, streamId) &&
    payload.session_id === sessionId &&
    get().currentSessionId === sessionId;

  // ── 乐观状态：先把界面改成"这一轮结束后应有的样子" ──
  let optimisticUserId: string | undefined;
  if (mode.kind === "send") {
    const userMsg: Message = {
      id: crypto.randomUUID(),
      role: "user",
      content: mode.content.trim(),
      timestamp: new Date().toISOString(),
      session_id: currentSessionId,
      token_count: 0,
      thinking_ms: 0,
    };
    optimisticUserId = userMsg.id;
    set((state) => ({
      messages: [...state.messages, userMsg],
      isStreaming: true,
      activeStreamId: streamId,
      errorMessage: null,
      // 新一轮生成开始：清空上一轮的工具调用与残留审批，并复位中断标记
      liveToolCalls: [],
      pendingApproval: null,
      stepLimitHit: false,
      toolSteps: 0,
    }));
  } else {
    // 重试 / 编辑：本地立即截断，语义与后端 `rewind_messages` / `edit_user_message` 一致
    const state = get();
    const index = state.messages.findIndex((m) => m.id === mode.messageId);
    if (index < 0) return;
    const target = state.messages[index];
    if (mode.kind === "edit" && target.role !== "user") return;

    // 目标是"还没有回复的用户消息"（上一轮失败/被取消）：保留提问行；其余从目标开始删
    const keepTarget = mode.kind === "edit" || target.role === "user";
    const messages = keepTarget
      ? state.messages.slice(0, index + 1)
      : state.messages.slice(0, index);
    const removed = state.messages.slice(keepTarget ? index + 1 : index);
    if (mode.kind === "edit") {
      messages[index] = { ...messages[index], content: mode.content.trim() };
    }
    const toolsByMessage = { ...state.toolsByMessage };
    for (const message of removed) delete toolsByMessage[message.id];
    set({
      messages,
      toolsByMessage,
      isStreaming: true,
      activeStreamId: streamId,
      errorMessage: null,
      liveToolCalls: [],
      pendingApproval: null,
      stepLimitHit: false,
      toolSteps: 0,
    });
  }

  // 接收统计信息（后端在 stream-end 之前发出）
  let pendingStats: { token_count: number; thinking_ms: number; model?: string } = {
    token_count: 0,
    thinking_ms: 0,
  };
  let finished = false;

  const unlisteners: Array<() => void> = [];
  // 悬浮窗绝不订阅工具事件（纯聊天桌宠，不出现任何工具 UI）
  const withToolEvents = isMainWindow();

  try {
    const unlistenStats = await listen<MessageStatsData>(
      STREAM_EVENT.stats,
      (event) => {
        if (!isCurrentGeneration(event.payload)) return;
        pendingStats = {
          token_count: event.payload.token_count,
          thinking_ms: event.payload.thinking_ms,
          model: event.payload.model,
        };
        // 步数用尽不是错误，但必须让用户看见"这一轮被截断了"
        set({
          stepLimitHit: event.payload.step_limit_hit === true,
          toolSteps: event.payload.tool_steps ?? 0,
        });
      }
    );
    unlisteners.push(unlistenStats);

    const unlistenEnd = await listen<StreamEventData>(
      STREAM_EVENT.end,
      (event) => {
        if (!isCurrentGeneration(event.payload)) return;
        finished = true;
        const assistantMsg: Message = {
          id: crypto.randomUUID(),
          role: "assistant",
          content: event.payload.data,
          timestamp: new Date().toISOString(),
          session_id: event.payload.session_id,
          token_count: pendingStats.token_count,
          thinking_ms: pendingStats.thinking_ms,
          model: pendingStats.model,
        };
        // 记下这轮本地 id：session-updated 到达时换成数据库 id（重试/编辑/回退依赖）
        // 防御性清理：正常每轮都会被 adopt 删除，异常路径（如中途换会话）可能残留
        if (localTurnIds.size > 32) {
          const oldest = localTurnIds.keys().next().value;
          if (oldest) localTurnIds.delete(oldest);
        }
        localTurnIds.set(streamId, {
          userId: optimisticUserId,
          assistantId: assistantMsg.id,
        });
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
        if (!isCurrentGeneration(event.payload)) return;
        finished = true;
        set((state) => ({
          errorMessage: event.payload.message,
          // 失败同样保留已有的工具调用，方便用户判断是哪一步出的问题
          pendingApproval: null,
          ...(state.activeStreamId === streamId
            ? { isStreaming: false, activeStreamId: null }
            : {}),
        }));
        // 失败路径不会发 session-updated：用户消息已经落库，回读一次把本地乐观
        // id 对齐成数据库 id，否则紧接着点「重试」会因 id 对不上而失败
        void get().refreshCurrentSession();
      }
    );
    unlisteners.push(unlistenError);

    if (withToolEvents) {
      const unlistenToolStart = await listen<ToolCallStartData>(
        TOOL_EVENT.start,
        (event) => {
          if (!isCurrentGeneration(event.payload)) return;
          const {
            call_id,
            tool,
            tool_label,
            args_preview,
            permission,
            step,
            max_steps,
          } = event.payload;

          // 子代理（depth >= 1）的内部调用不单独成卡片：
          // 它们只累加到父级 `spawn_subagents` 卡片上的计数
          if (event.payload.depth) {
            const parentId = event.payload.parent_call_id;
            if (!parentId) return;
            set((state) => ({
              liveToolCalls: state.liveToolCalls.map((call) =>
                call.call_id === parentId
                  ? { ...call, subagentCalls: (call.subagentCalls ?? 0) + 1 }
                  : call
              ),
            }));
            return;
          }
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
                maxSteps: max_steps,
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
          if (!isCurrentGeneration(event.payload)) return;
          // 子代理内部调用的结果同样不进主卡片列表
          if (event.payload.depth) return;
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

      /*
       * 执行中的增量输出（仅界面）
       *
       * 后端按 4 KB / 200 ms 合帧，所以这里每个事件做一次 store 更新是安全的；
       * 事件只落在已存在的卡片上——找不到对应 call_id 就丢弃，避免凭空造卡片。
       */
      const unlistenToolOutput = await listen<ToolOutputChunkData>(
        TOOL_EVENT.outputChunk,
        (event) => {
          if (!isCurrentGeneration(event.payload)) return;
          const { call_id, data, stream } = event.payload;
          if (!data) return;
          // 子代理没有自己的卡片，它的实时输出直接丢弃（结论会在结果里给出）
          if (event.payload.depth) return;
          set((state) => {
            if (!state.liveToolCalls.some((call) => call.call_id === call_id)) {
              return state;
            }
            return {
              liveToolCalls: state.liveToolCalls.map((call) =>
                call.call_id === call_id
                  ? {
                      ...call,
                      output: appendToolOutput(
                        call.output,
                        // stderr 单独标注：它几乎总是失败原因所在
                        stream === "stderr" ? `[stderr] ${data}` : data
                      ),
                    }
                  : call
              ),
            };
          });
        }
      );
      unlisteners.push(unlistenToolOutput);

      const unlistenApprovalRequest = await listen<ToolApprovalRequestData>(
        TOOL_EVENT.approvalRequest,
        (event) => {
          if (!isCurrentGeneration(event.payload)) return;
          set({ pendingApproval: event.payload });
        }
      );
      unlisteners.push(unlistenApprovalRequest);

      const unlistenApprovalResolved = await listen<ToolApprovalResolvedData>(
        TOOL_EVENT.approvalResolved,
        (event) => {
          if (!isCurrentGeneration(event.payload)) return;
          const current = get().pendingApproval;
          // 只关闭同一个审批：避免迟到的旧事件误关新弹窗
          if (!current || current.approval_id === event.payload.approval_id) {
            set({ pendingApproval: null });
          }
        }
      );
      unlisteners.push(unlistenApprovalResolved);

      const unlistenSubagentStatus = await listen<SubagentStatusData>(
        TOOL_EVENT.subagentStatus,
        (event) => {
          if (!isCurrentGeneration(event.payload)) return;
          const {
            parent_call_id,
            task_id,
            goal_preview,
            status,
            duration_ms,
            model,
            reason,
          } = event.payload;
          set((state) => ({
            liveToolCalls: state.liveToolCalls.map((call) => {
              if (call.call_id !== parent_call_id) return call;
              const prevTasks = call.subagentTasks ?? [];
              const exists = prevTasks.some((t) => t.taskId === task_id);
              const updatedTasks = exists
                ? prevTasks.map((t) =>
                    t.taskId === task_id
                      ? {
                          ...t,
                          status,
                          durationMs: duration_ms ?? t.durationMs,
                          model: model ?? t.model,
                          reason: reason ?? t.reason,
                        }
                      : t
                  )
                : [
                    ...prevTasks,
                    {
                      taskId: task_id,
                      goalPreview: goal_preview,
                      status,
                      durationMs: duration_ms,
                      model,
                      reason,
                    },
                  ];
              return {
                ...call,
                subagentTasks: updatedTasks,
              };
            }),
          }));
        }
      );
      unlisteners.push(unlistenSubagentStatus);
    }

    if (mode.kind === "send") {
      await invoke("send_message", {
        sessionId: currentSessionId,
        content: mode.content.trim(),
        streamId,
      });
    } else if (mode.kind === "retry") {
      await invoke("regenerate_message", {
        sessionId: currentSessionId,
        messageId: mode.messageId,
        streamId,
      });
    } else {
      await invoke("edit_message", {
        sessionId: currentSessionId,
        messageId: mode.messageId,
        content: mode.content.trim(),
        streamId,
      });
    }
  } catch (e) {
    console.error("Failed to generate:", e);
    set({ errorMessage: `${GENERATION_LABEL[mode.kind]}失败：${toMessage(e)}` });
    // 重试/编辑在本地做过乐观截断：命令被拒绝（例如会话正忙）时由 finally 里的
    // 回读对齐数据库状态（`!finished` 分支），这里不重复回读
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

    // 本轮可能有写类工具留下的文件改动备份：回读一次，界面据此显示"回滚"
    if (withToolEvents) {
      void get().loadSnapshot(currentSessionId, streamId);
    }

    // 没有收到流结束信号时（异常/取消），从数据库回读一次以对齐真实状态
    if (!finished) {
      void get().refreshCurrentSession();
    }
  }
}

export const useChatStore = create<ChatState>((set, get) => ({
  sessions: [],
  currentSessionId: null,
  messages: [],
  isStreaming: false,
  activeStreamId: null,
  errorMessage: null,
  stepLimitHit: false,
  toolSteps: 0,
  liveToolCalls: [],
  toolsByMessage: {},
  pendingApproval: null,
  plan: null,
  planNote: null,
  snapshot: null,
  snapshotNotice: null,
  notes: [],
  sessionGrants: [],
  modelCatalog: null,
  currentPage: "chat",
  initialized: false,
  composerPrefill: null,

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
      bindPlanEvents(set, get);
      void get().loadPlan(session.id);
      void get().loadNotes(session.id);
      void get().loadSessionGrants(session.id);
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
        // 回读期间新的一轮生成已经开始（乐观消息已进列表）：旧快照不能覆盖它
        if (get().currentSessionId !== currentSessionId || get().isStreaming) return;
        set({ messages });
        void get().loadToolInvocations(currentSessionId);
        bindPlanEvents(set, get);
        void get().loadPlan(currentSessionId);
        void get().loadNotes(currentSessionId);
        void get().loadSessionGrants(currentSessionId);
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
        set({ currentSessionId: latest.id, messages, ...emptyGenerationState() });
        void get().loadToolInvocations(latest.id);
        void get().loadPlan(latest.id);
        void get().loadNotes(latest.id);
        void get().loadSessionGrants(latest.id);
      } catch (e) {
        console.error("Failed to switch session:", e);
      }
    } else {
      set({ currentSessionId: null, messages: [], ...emptyGenerationState() });
    }
  },

  createSession: async (
    title,
    personaId,
    sessionType = "chat",
    workspaceId,
    modelPref,
    taskMode
  ) => {
    const sessionId = await invoke<string>("create_session", {
      title: title ?? null,
      personaId: personaId ?? null,
      sessionType: sessionType ?? null,
      workspaceId: workspaceId ?? null,
      modelPref: modelPref ?? null,
      taskMode: taskMode ?? null,
    }).catch((e) => {
      set({ errorMessage: `创建会话失败：${toMessage(e)}` });
      throw e;
    });
    await get().loadSessions();
    // 新会话是空的：必须连上一轮生成的工具记录一起复位，
    // 否则「执行完任务 → 新建会话」时上一个会话的工具卡片会留在空会话里。
    // `errorMessage` 也要清掉：它是**上一次**发送的错误，不该跟着用户进新会话。
    set({ currentSessionId: sessionId, messages: [], errorMessage: null, ...emptyGenerationState() });
    return sessionId;
  },

  setTaskMode: async (sessionId, taskMode) => {
    try {
      await invoke("set_task_mode", { sessionId, taskMode });
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, task_mode: taskMode } : s
        ),
      }));
    } catch (e) {
      console.error("Failed to set task mode:", e);
      set({ errorMessage: `切换任务模式失败：${toMessage(e)}` });
    }
  },

  /**
   * 开关会话级 AUTO（自动允许所有审批类工具）
   *
   * 与任务模式/工作区同一约定：只写数据库，正在跑的这轮不受影响
   * （下一轮生成才会带上新的运行时快照），因此 UI 在生成中禁用该开关。
   */
  setSessionAutoApprove: async (sessionId, enabled) => {
    try {
      await invoke("set_session_auto_approve", { sessionId, enabled });
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, auto_approve_all: enabled } : s
        ),
      }));
    } catch (e) {
      console.error("Failed to set session auto approve:", e);
      set({ errorMessage: `切换 AUTO 失败：${toMessage(e)}` });
    }
  },

  /** 设置/清除任务会话绑定的工作区（null = 默认沙箱） */
  setSessionWorkspace: async (sessionId, workspaceId) => {
    try {
      await invoke("set_session_workspace", { sessionId, workspaceId });
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, workspace_id: workspaceId } : s
        ),
      }));
    } catch (e) {
      console.error("Failed to set session workspace:", e);
      set({ errorMessage: `切换工作区失败：${toMessage(e)}` });
    }
  },

  setSessionModel: async (sessionId, pref) => {
    try {
      await invoke("set_session_model", { sessionId, pref });
      // 本地同步：不重新拉会话列表（避免整列表闪烁），也不触碰 messages
      set((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === sessionId ? { ...s, model_pref: pref } : s
        ),
      }));
    } catch (e) {
      console.error("Failed to set session model:", e);
      set({ errorMessage: `设置模型失败：${toMessage(e)}` });
    }
  },

  loadModelCatalog: async () => {
    try {
      const catalog = await invoke<ModelCatalog>("get_model_catalog");
      set({ modelCatalog: catalog });
    } catch (e) {
      // 目录拉取失败不该打断聊天：界面退化为"只显示跟随全局"，
      // 用户仍可以照常发消息（后端解析模型时也会自行降级）
      console.error("Failed to load model catalog:", e);
    }
  },

  switchSession: async (sessionId) => {
    try {
      const messages = await invoke<Message[]>("get_messages", { sessionId });
      // 切换会话时不能沿用上一个会话的流式状态、工具状态与错误提示
      set({
        currentSessionId: sessionId,
        messages,
        errorMessage: null,
        ...emptyGenerationState(),
      });
      void get().loadToolInvocations(sessionId);
      void get().loadPlan(sessionId);
      void get().loadNotes(sessionId);
      void get().loadSessionGrants(sessionId);
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
          // 上一轮的报错属于被删掉的会话，跟着一起清掉
          errorMessage: null,
          ...emptyGenerationState(),
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

  sendMessage: (content) => startGeneration(set, get, { kind: "send", content }),

  retryMessage: (messageId) =>
    startGeneration(set, get, { kind: "retry", messageId }),

  editMessage: (messageId, content) =>
    startGeneration(set, get, { kind: "edit", messageId, content }),

  /**
   * 把本窗口乐观消息的本地 id 换成数据库 id
   *
   * 主窗口收到自己那轮的 `session-updated` 时调用（那次广播在本轮落库之后发出，
   * 因此 id 一定已经存在）。别的窗口或非本窗口发起的生成仍走 `refreshCurrentSession`。
   */
  adoptLocalMessageIds: (payload) => {
    const turn = localTurnIds.get(payload.stream_id);
    if (!turn) return;
    localTurnIds.delete(payload.stream_id);
    const dbUserId = payload.user_message_id ?? null;
    const dbAssistantId = payload.message_id ?? null;
    if (!dbUserId && !dbAssistantId) return;

    set((state) => {
      let changed = false;
      const messages = state.messages.map((message) => {
        if (dbUserId && message.id === turn.userId) {
          changed = true;
          return { ...message, id: dbUserId };
        }
        if (dbAssistantId && message.id === turn.assistantId) {
          changed = true;
          return { ...message, id: dbAssistantId };
        }
        return message;
      });
      // 工具卡片按 message_id 归组：键也要跟着换，否则气泡上的工具记录消失
      const toolsByMessage = { ...state.toolsByMessage };
      if (dbAssistantId && turn.assistantId && turn.assistantId !== dbAssistantId) {
        const calls = toolsByMessage[turn.assistantId];
        if (calls) {
          delete toolsByMessage[turn.assistantId];
          toolsByMessage[dbAssistantId] = calls;
          changed = true;
        }
      }
      return changed ? { messages, toolsByMessage } : {};
    });
  },

  /** 预览"回退到某条消息"的影响范围（只读，不写库） */
  previewRewind: async (messageId, inclusive = true) => {
    const sessionId = get().currentSessionId;
    if (!sessionId) throw new Error("没有当前会话");
    return invoke<RewindPreviewView>("preview_rewind", {
      sessionId,
      messageId,
      inclusive,
    });
  },

  /**
   * 回退：删除该条及其之后的全部消息
   *
   * - 目标是用户消息时把原文写进 `composerPrefill`，方便改完重发；
   * - `restoreStreamIds` 非空时按"新 → 旧"依次撤销这些轮次的文件改动
   *   （后一轮的快照先还原，最终状态才等于回退前）。
   */
  rewindTo: async (messageId, restoreStreamIds = []) => {
    const sessionId = get().currentSessionId;
    if (!sessionId || get().isStreaming) return;
    // 先捕获目标消息：后端广播的 session-updated 可能先于本函数返回，
    // 回读会把消息数组换掉，之后再按 id 找就找不到了（预填文本会丢）
    const target = get().messages.find((m) => m.id === messageId) ?? null;
    try {
      const outcome = await invoke<RewindOutcomeView>("delete_messages_from", {
        sessionId,
        messageId,
        inclusive: true,
      });

      // 本地即时截断（`session-updated` 触发的回读随后会对齐数据库状态）
      const state = get();
      const index = state.messages.findIndex((m) => m.id === messageId);
      const removed = index >= 0 ? state.messages.slice(index) : [];
      const toolsByMessage = { ...state.toolsByMessage };
      for (const message of removed) delete toolsByMessage[message.id];
      const snapshotRemoved =
        state.snapshot !== null &&
        outcome.affected_streams.includes(state.snapshot.stream_id);
      set({
        // 回读已经截断过时不再重复设置（数组本来就对）
        ...(index >= 0 ? { messages: state.messages.slice(0, index) } : {}),
        toolsByMessage,
        liveToolCalls: [],
        pendingApproval: null,
        stepLimitHit: false,
        toolSteps: 0,
        // 被回退掉的轮次不再有"最近一轮改动"的横幅
        ...(snapshotRemoved ? { snapshot: null, snapshotNotice: null } : {}),
        ...(target?.role === "user"
          ? { composerPrefill: { text: target.content, seq: Date.now() } }
          : {}),
      });
      void get().loadToolInvocations(sessionId);
      void get().loadSessions();

      // 可选：撤销这些轮次的文件改动（新 → 旧的顺序由后端 preview 给出）
      if (restoreStreamIds.length > 0) {
        const parts: string[] = [];
        for (const streamId of restoreStreamIds) {
          try {
            const report = await invoke<RestoreReportView>("restore_snapshot", {
              sessionId,
              streamId,
            });
            parts.push(`已还原 ${report.restored} 个文件`);
            if (report.missing > 0) parts.push(`${report.missing} 个备份已失效`);
            if (report.errors.length > 0) {
              parts.push(`${report.errors.length} 个失败：${report.errors[0]}`);
            }
          } catch (e) {
            parts.push(`回滚失败：${toMessage(e)}`);
          }
        }
        set({ snapshotNotice: parts.join("；") });
      }
    } catch (e) {
      console.error("Failed to rewind:", e);
      set({ errorMessage: `回退失败：${toMessage(e)}` });
      // 后端可能已经部分执行：回读对齐
      void get().refreshCurrentSession();
    }
  },

  consumeComposerPrefill: () => set({ composerPrefill: null }),

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
   * 回读某个会话的任务计划
   *
   * 计划是会话级状态，换会话/重挂载都要重新取一次；回读期间用户又换了会话时
   * 直接丢弃结果（否则会把上一个会话的计划画到新会话上）。
   */
  loadPlan: async (sessionId) => {
    if (!isMainWindow()) return;
    try {
      const view = await invoke<PlanView | null>("get_plan", { sessionId });
      if (get().currentSessionId !== sessionId) return;
      const items = view?.items ?? [];
      set({
        plan: items.length > 0 ? items : null,
        planNote: view?.note?.trim() ? view.note : null,
      });
    } catch (e) {
      // 拿不到计划不应影响对话：只记录，不弹全局错误
      console.error("Failed to load plan:", e);
    }
  },

  /** 用户手动清空计划（模型也能通过 update_plan 传空数组清空） */
  clearPlan: async () => {
    const sessionId = get().currentSessionId;
    if (!sessionId) return;
    try {
      await invoke("clear_plan", { sessionId });
      // 后端会广播 plan-updated，这里即时清空以免界面等一下才变
      if (get().currentSessionId === sessionId) {
        set({ plan: null, planNote: null });
      }
    } catch (e) {
      console.error("Failed to clear plan:", e);
      set({ errorMessage: `清空计划失败：${toMessage(e)}` });
    }
  },

  /**
   * 用户手动编辑计划（勾选完成 / 改标题 / 增删）
   *
   * 与模型 `update_plan` 写的是同一份数据：本地即时更新，后端广播
   * `plan-updated` 后模型下一轮也会看到同一份进度。
   */
  updatePlanItems: async (items, note) => {
    const sessionId = get().currentSessionId;
    if (!sessionId) return;
    try {
      await invoke("update_plan_items", { sessionId, items, note: note ?? null });
      if (get().currentSessionId === sessionId) {
        const trimmed = note?.trim();
        set({
          plan: items.length > 0 ? items : null,
          planNote: trimmed ? trimmed : null,
        });
      }
    } catch (e) {
      console.error("Failed to update plan:", e);
      set({ errorMessage: `更新计划失败：${toMessage(e)}` });
    }
  },

  /**
   * 批准计划并执行
   *
   * 先等模式切换真正落库（后端按发送那一刻的会话行解析模式），
   * 再发出执行指令；执行消息本身也留在对话历史里，可追溯。
   */
  approvePlan: async () => {
    const sessionId = get().currentSessionId;
    if (!sessionId || get().isStreaming) return;
    await get().setTaskMode(sessionId, "work");
    await get().sendMessage("请按上面的计划开始执行，每完成一步就更新计划进度。");
  },

  /**
   * 继续未完成的任务
   *
   * 停止生成后计划里的"进行中"会被后端标成"受阻"；这里把下一个该做的步骤
   * 明确写进指令里，避免模型重新调查已知的结论（计划与工作记忆都会随本轮注入）。
   */
  continueTask: async () => {
    const sessionId = get().currentSessionId;
    if (!sessionId || get().isStreaming) return;
    const plan = get().plan ?? [];
    const next =
      plan.find((item) => item.status === "doing") ??
      plan.find((item) => item.status === "pending") ??
      plan.find((item) => item.status === "blocked");
    const hint = next ? `从「${next.title}」继续。` : "";
    await get().sendMessage(
      `继续执行任务。${hint}如果上一步确实受阻，请说明原因或换一种做法。`
    );
  },

  /**
   * 回读某次生成留下的文件改动备份
   *
   * 只在主窗口调用（回滚按钮属于工具界面）；失败只记录，不影响对话。
   */
  loadSnapshot: async (sessionId, streamId) => {
    if (!isMainWindow() || !streamId) return;
    try {
      const info = await invoke<SnapshotInfoView | null>("get_snapshot", {
        sessionId,
        streamId,
      });
      if (get().currentSessionId !== sessionId) return;
      set({ snapshot: info, snapshotNotice: null });
    } catch (e) {
      console.error("Failed to load snapshot:", e);
    }
  },

  /** 回滚该次生成造成的全部文件改动 */
  restoreSnapshot: async () => {
    const info = get().snapshot;
    if (!info) return;
    try {
      const report = await invoke<RestoreReportView>("restore_snapshot", {
        sessionId: info.session_id,
        streamId: info.stream_id,
      });
      const parts = [`已还原 ${report.restored} 个文件`];
      if (report.missing > 0) parts.push(`${report.missing} 个备份已失效`);
      if (report.errors.length > 0) parts.push(`${report.errors.length} 个失败`);
      set({
        snapshotNotice: parts.join("，"),
        // 还原后备份仍保留：用户可以再点一次（幂等）
        snapshot: { ...info },
      });
    } catch (e) {
      console.error("Failed to restore snapshot:", e);
      set({ snapshotNotice: `回滚失败：${toMessage(e)}` });
    }
  },

  dismissSnapshot: () => set({ snapshot: null, snapshotNotice: null }),

  /**
   * 回读某个会话的工作记忆
   *
   * 与计划同样的竞态保护：回读期间换了会话就丢弃结果，
   * 否则会把上一个会话的笔记显示在新会话里（那是最容易误导人的一种错）。
   */
  loadNotes: async (sessionId) => {
    if (!isMainWindow()) return;
    try {
      const notes = await invoke<SessionNote[]>("get_notes", { sessionId });
      if (get().currentSessionId !== sessionId) return;
      set({ notes: notes ?? [] });
    } catch (e) {
      // 拿不到工作记忆不影响对话
      console.error("Failed to load notes:", e);
    }
  },

  /** 清空当前会话的工作记忆（用户对跨轮记忆的最终控制权） */
  clearNotes: async () => {
    const sessionId = get().currentSessionId;
    if (!sessionId) return;
    try {
      await invoke<number>("clear_notes", { sessionId });
      if (get().currentSessionId === sessionId) {
        set({ notes: [] });
      }
    } catch (e) {
      console.error("Failed to clear notes:", e);
      set({ errorMessage: `清空工作记忆失败：${toMessage(e)}` });
    }
  },

  /**
   * 回读某个会话的持久授权
   *
   * 与 plan/notes 同一套竞态保护：回读期间换了会话就丢弃结果。
   */
  loadSessionGrants: async (sessionId) => {
    if (!isMainWindow()) return;
    try {
      const grants = await invoke<string[]>("list_session_grants", { sessionId });
      if (get().currentSessionId !== sessionId) return;
      set({ sessionGrants: grants ?? [] });
    } catch (e) {
      console.error("Failed to load session grants:", e);
    }
  },

  /** 撤销某个工具的会话授权（撤销后下一次调用会重新弹审批） */
  revokeSessionGrant: async (tool) => {
    const sessionId = get().currentSessionId;
    if (!sessionId) return;
    try {
      await invoke<boolean>("revoke_session_grant", { sessionId, tool });
      if (get().currentSessionId === sessionId) {
        set((state) => ({
          sessionGrants: state.sessionGrants.filter((t) => t !== tool),
        }));
      }
    } catch (e) {
      console.error("Failed to revoke session grant:", e);
      set({ errorMessage: `撤销授权失败：${toMessage(e)}` });
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
      // 「本会话允许」已由后端落库，回读让任务状态条立即显示可撤销的授权
      if (decision === "allow_session") {
        const sessionId = get().currentSessionId;
        if (sessionId) void get().loadSessionGrants(sessionId);
      }
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
