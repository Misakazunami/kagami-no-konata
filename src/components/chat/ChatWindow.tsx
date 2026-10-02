import { useEffect, useMemo, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore, type Session } from "../../stores/chatStore";
import { useUiStore } from "../../stores/uiStore";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { useFocusTrap } from "../../hooks/useFocusTrap";
import { MessageList } from "./MessageList";
import { InputBox } from "./InputBox";
import { NotesPanel } from "./NotesPanel";
import { PlanPanel } from "./PlanPanel";
import { ChangesPanel } from "./ChangesPanel";
import { TaskStatusBar } from "./TaskStatusBar";
import type { WorkspaceView } from "../../types/tools";
import { copyText } from "../../utils/clipboard";
import { buildConversationMarkdown } from "../../utils/exportConversation";
import {
  IconAlert,
  IconCheck,
  IconClipboardList,
  IconCopy,
  IconExpand,
  IconMinimize,
  IconMoon,
  IconPlus,
  IconSettings,
  IconSparkles,
  IconSun,
  IconX,
  IconZap,
} from "../icons";
import {
  DEFAULT_PERSONA_ID,
  FALLBACK_SHORT_NAME,
  type PersonaSummary,
} from "../../types/persona";

/**
 * 本地日期键（`YYYY-MM-DD`）
 *
 * `toISOString()` 是 UTC：UTC+8 的凌晨 0-8 点会被算到前一天，
 * "今天/昨天"分组与排序都会错。
 */
function localDateKey(value: Date | string): string {
  const date = typeof value === "string" ? new Date(value) : value;
  if (Number.isNaN(date.getTime())) {
    // 解析失败按原字符串的日期部分兜底
    return typeof value === "string" ? value.split("T")[0] : "";
  }
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${date.getFullYear()}-${month}-${day}`;
}

/** 格式化日期标题 */
function formatDateHeader(dateStr: string): string {
  const today = localDateKey(new Date());
  const yesterday = localDateKey(new Date(Date.now() - 86400000));
  if (dateStr === today) return "今天";
  if (dateStr === yesterday) return "昨天";
  return dateStr;
}

/** 按日期分组会话 */
function groupSessionsByDate(sessions: Session[]): [string, Session[]][] {
  const groups: Record<string, Session[]> = {};
  for (const s of sessions) {
    const date = localDateKey(s.created_at);
    if (!groups[date]) groups[date] = [];
    groups[date].push(s);
  }
  return Object.entries(groups).sort(([a], [b]) => b.localeCompare(a));
}

export function ChatWindow() {
  const sessions = useChatStore((s) => s.sessions);
  const currentSessionId = useChatStore((s) => s.currentSessionId);
  const messages = useChatStore((s) => s.messages);
  const errorMessage = useChatStore((s) => s.errorMessage);
  const clearError = useChatStore((s) => s.clearError);
  const initSession = useChatStore((s) => s.initSession);
  const refreshCurrentSession = useChatStore((s) => s.refreshCurrentSession);
  const createSession = useChatStore((s) => s.createSession);
  const switchSession = useChatStore((s) => s.switchSession);
  const deleteSession = useChatStore((s) => s.deleteSession);
  const sessionsStatus = useChatStore((s) => s.sessionsStatus);
  const renameSession = useChatStore((s) => s.renameSession);
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);
  /** 各会话仍在进行中的生成（侧栏"生成中"圆点） */
  const backgroundStreams = useChatStore((s) => s.backgroundStreams);
  const isStreaming = useChatStore((s) => s.isStreaming);
  const pushToast = useUiStore((s) => s.pushToast);

  // 待确认的删除会话（原生 confirm 样式不可控、文案也放不下影响范围提示）
  const [deleteTarget, setDeleteTarget] = useState<Session | null>(null);

  // 人格列表
  const [personas, setPersonas] = useState<PersonaSummary[]>([]);
  // 默认人格 ID 与后端 persona::types::DEFAULT_PERSONA_ID 保持一致
  const [selectedPersona, setSelectedPersona] = useState(DEFAULT_PERSONA_ID);

  // 工作区弹窗与状态
  const [showTaskModal, setShowTaskModal] = useState(false);
  const [workspaces, setWorkspaces] = useState<WorkspaceView[]>([]);
  const [selectedWorkspace, setSelectedWorkspace] = useState<string>("");
  const [customPath, setCustomPath] = useState("");
  const [customLabel, setCustomLabel] = useState("");
  const [customWritable, setCustomWritable] = useState(true);
  const [taskModalError, setTaskModalError] = useState("");
  const [isCreatingTask, setIsCreatingTask] = useState(false);
  // 任务会话的模型选择方式：默认手动（跟随全局/会话内再选），可在此直接开启自动选择
  const [autoModels, setAutoModels] = useState(false);
  // 创建时即选运行模式：直接进入 Work 可以跳过 Plan 阶段
  const [newTaskMode, setNewTaskMode] = useState<"plan" | "work">("plan");

  const openTaskModal = async () => {
    setTaskModalError("");
    // 勾选状态跟随设置里的"新建任务会话默认自动"（用户仍可在弹窗里改）
    setAutoModels(
      useChatStore.getState().modelCatalog?.settings.auto_by_default ?? false
    );
    setNewTaskMode("plan");
    try {
      const list = await invoke<WorkspaceView[]>("list_workspaces");
      setWorkspaces(list);
      const defaultWs = list.find((w) => w.is_default) ?? list[0];
      setSelectedWorkspace(defaultWs ? defaultWs.id : "");
      setShowTaskModal(true);
    } catch (e) {
      // 不再静默降级成"无工作区任务"：用户以为选了目录，实际跑在默认沙箱里
      console.error("加载工作区失败:", e);
      useChatStore.setState({
        errorMessage: `加载工作区失败，未创建任务会话：${typeof e === "string" ? e : String(e)}`,
      });
    }
  };

  const handleConfirmTaskModal = async () => {
    setTaskModalError("");
    setIsCreatingTask(true);
    try {
      let targetWorkspaceId = selectedWorkspace;
      // 如果用户输入了自定义新路径，先添加工作区
      if (customPath.trim()) {
        const newWs = await invoke<WorkspaceView>("add_workspace", {
          path: customPath.trim(),
          label: customLabel.trim() || null,
          writable: customWritable,
        });
        targetWorkspaceId = newWs.id;
      }
      await createSession(
        undefined,
        selectedPersona,
        "task",
        targetWorkspaceId || undefined,
        // 自动选择：Plan 模式与子代理优先用子模型，Work 模式优先用主模型
        // （显式传 inherit 表示"跟随全局"，避免被设置里的默认自动覆盖）
        autoModels ? { mode: "auto" } : { mode: "inherit" },
        newTaskMode
      );
      setShowTaskModal(false);
      setCustomPath("");
      setCustomLabel("");
    } catch (err) {
      setTaskModalError(typeof err === "string" ? err : String(err));
    } finally {
      setIsCreatingTask(false);
    }
  };

  // 重命名状态
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editValue, setEditValue] = useState("");
  const editRef = useRef<HTMLInputElement>(null);

  // 首次加载：初始化会话 + 加载人格列表
  // 注意：App 按页条件渲染，离开 chat 时本组件会被卸载，从设置页返回属于"重新挂载"，
  // 因此这里用 initialized 区分"首次进入"与"返回"，返回时只刷新不新建会话。
  // （历史实现用 prevPageRef 比较页面变化，但组件已卸载，该分支永远不会执行。）
  useEffect(() => {
    if (useChatStore.getState().initialized) {
      refreshCurrentSession();
    } else {
      initSession();
    }
    invoke<PersonaSummary[]>("list_personas").then(setPersonas).catch(console.error);
  }, []);

  useEffect(() => {
    if (editingId && editRef.current) {
      editRef.current.focus();
      editRef.current.select();
    }
  }, [editingId]);

  const grouped = useMemo(() => groupSessionsByDate(sessions), [sessions]);

  // 当前会话的人格
  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const currentPersona = personas.find((p) => p.id === currentSession?.persona_id);
  const currentPersonaName = currentPersona?.name;
  // 按钮与空状态文案用短名（随人格变化），拿不到人格时回退到中性称呼
  const currentPersonaShortName = currentPersona?.short_name ?? FALLBACK_SHORT_NAME;

  const startRename = (s: Session, e: React.MouseEvent) => {
    e.stopPropagation();
    setEditingId(s.id);
    setEditValue(s.title);
  };

  const commitRename = () => {
    if (editingId && editValue.trim()) {
      renameSession(editingId, editValue.trim());
    }
    setEditingId(null);
  };

  const cancelRename = () => {
    setEditingId(null);
  };

  // 悬浮窗状态：通过后端事件实时同步
  const [floatVisible, setFloatVisible] = useState(false);
  useEffect(() => {
    // 查询结果可能晚于事件到达：事件已经改过状态时丢弃这次快照，
    // 否则会把更新的状态覆盖回旧值
    let eventSeen = false;
    invoke<boolean>("is_float_visible")
      .then((visible) => {
        if (!eventSeen) setFloatVisible(visible);
      })
      .catch(() => {});
    // 监听后端发射的可见性变化事件
    const unlisten = listen<boolean>("float-visibility-changed", (event) => {
      eventSeen = true;
      setFloatVisible(event.payload);
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 一键切换：隐藏主窗口 + 打开悬浮窗
  const handleSwitchToFloat = async () => {
    await invoke("show_float_window");
    await invoke("hide_main_window");
  };

  // 召唤/召回当前角色：切换悬浮窗显示
  const handleToggleFloat = async () => {
    await invoke("toggle_float_window");
    // 状态由后端事件自动同步，无需手动切换
  };

  // 一键复制整个会话（Markdown）：用于粘贴到笔记 / issue / 分享
  const [conversationCopied, setConversationCopied] = useState(false);
  const handleCopyConversation = async () => {
    if (messages.length === 0) return;
    try {
      const ok = await copyText(buildConversationMarkdown(currentSession, messages));
      if (!ok) {
        pushToast("复制失败：剪贴板不可用", "error");
        return;
      }
      setConversationCopied(true);
      setTimeout(() => setConversationCopied(false), 2000);
    } catch (e) {
      useChatStore.setState({
        errorMessage: `复制对话失败：${typeof e === "string" ? e : String(e)}`,
      });
    }
  };

  // 主题切换
  const [isDark, setIsDark] = useState(true);
  const toggleTheme = async () => {
    const next = isDark ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", next);
    setIsDark(!isDark);
    // 持久化到配置
    try {
      const config = await invoke<{ ui: { theme: string; font_size: number } }>("get_config");
      await invoke("update_config", {
        newConfig: { ...config, ui: { ...config.ui, theme: next } },
      });
    } catch (e) {
      console.error("Failed to save theme:", e);
    }
  };

  // 初始化主题状态
  useEffect(() => {
    const current = document.documentElement.getAttribute("data-theme");
    setIsDark(current !== "light");
  }, []);

  // 创建任务弹窗：焦点陷阱 + Esc 关闭 + 关闭后归还焦点（与其他弹窗同一套标准）
  const taskModalRef = useRef<HTMLDivElement>(null);
  useFocusTrap(taskModalRef, showTaskModal);
  useEffect(() => {
    if (!showTaskModal) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setShowTaskModal(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [showTaskModal]);

  return (
    <div className="chat-window">
      {/* 侧边栏 */}
      <div className="sidebar">
        <div className="sidebar-header">
          <h2>
            <IconSparkles /> Kagami no Konata
          </h2>
          {/* 第一行：新建入口（两个按钮等宽，各自占一半，避免被侧边栏宽度挤破） */}
          <div className="new-session-controls">
            <button
              className="new-chat-btn"
              onClick={() => createSession(undefined, selectedPersona, "chat")}
              title="创建普通聊天会话"
            >
              <IconPlus /> 对话
            </button>
            <button
              className="new-chat-btn task"
              onClick={openTaskModal}
              title="创建专业任务工程会话"
            >
              <IconZap /> 任务
            </button>
          </div>
          {/* 第二行：人设选择（单独一行，宽度不再与按钮抢空间） */}
          <div className="persona-row">
            <span className="persona-row-label">人设</span>
            <select
              className="persona-select"
              value={selectedPersona}
              onChange={(e) => setSelectedPersona(e.target.value)}
              title="新建会话使用的人设"
              aria-label="新建会话使用的人设"
            >
              {personas.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </div>
        </div>
        <div className="session-list">
          {grouped.map(([date, items]) => (
            <div key={date} className="session-group">
              <div className="session-date-header">
                {formatDateHeader(date)}
              </div>
              {items.map((s) => {
                const personaName = personas.find((p) => p.id === s.persona_id)?.name;
                // 本会话有一轮生成在跑（当前会话或后台会话都算）
                const generating =
                  s.id === currentSessionId ? isStreaming : s.id in backgroundStreams;
                return (
                  <div
                    key={s.id}
                    className={`session-item ${s.id === currentSessionId ? "active" : ""}`}
                  >
                    {/*
                      可点区与删除按钮是**兄弟**而不是嵌套：
                      `role="button"` 里再放一个 <button> 是非法 ARIA 结构，
                      读屏会把删除按钮并进条目一起朗读。
                    */}
                    <div
                      className="session-item-main"
                      role="button"
                      tabIndex={0}
                      aria-current={s.id === currentSessionId ? "true" : undefined}
                      onClick={() => {
                        if (editingId !== s.id) switchSession(s.id);
                      }}
                      onKeyDown={(e) => {
                        if (editingId === s.id) return;
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          switchSession(s.id);
                        }
                      }}
                      onDoubleClick={(e) => startRename(s, e)}
                    >
                      {editingId === s.id ? (
                        <input
                          ref={editRef}
                          className="session-title-edit"
                          value={editValue}
                          aria-label="会话标题"
                          onChange={(e) => setEditValue(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") commitRename();
                            if (e.key === "Escape") cancelRename();
                          }}
                          onBlur={commitRename}
                          onClick={(e) => e.stopPropagation()}
                        />
                      ) : (
                        <div className="session-title-wrap">
                          {s.session_type === "task" && (
                            <span
                              className={`session-type-badge ${s.task_mode === "work" ? "work" : "plan"}`}
                              title={
                                s.task_mode === "work"
                                  ? "任务会话 · 执行模式（Work）"
                                  : "任务会话 · 规划模式（Plan）"
                              }
                            >
                              {s.task_mode === "work" ? (
                                <>
                                  <IconZap />任务
                                </>
                              ) : (
                                <>
                                  <IconClipboardList />任务
                                </>
                              )}
                            </span>
                          )}
                          <span className="session-title">{s.title}</span>
                          {personaName && s.persona_id !== DEFAULT_PERSONA_ID && (
                            <span className="session-persona-tag">{personaName}</span>
                          )}
                        </div>
                      )}
                    </div>
                    {/* 切走后仍在生成的会话必须看得见，否则那一轮既无人盯也无人能停 */}
                    {generating && (
                      <span
                        className="session-streaming-dot"
                        role="status"
                        aria-label="该会话正在生成回复"
                        title="正在生成回复…"
                      />
                    )}
                    <button
                      className="session-delete"
                      aria-label={`删除会话「${s.title}」`}
                      title="删除会话"
                      onClick={(e) => {
                        e.stopPropagation();
                        setDeleteTarget(s);
                      }}
                    >
                      <IconX />
                    </button>
                  </div>
                );
              })}
            </div>
          ))}
          {sessionsStatus === "loading" && sessions.length === 0 && (
            <div className="session-empty" role="status">
              正在加载会话…
            </div>
          )}
          {sessionsStatus === "error" && sessions.length === 0 && (
            <div className="session-empty">
              会话列表加载失败
              <button
                className="session-retry"
                onClick={() => void useChatStore.getState().loadSessions()}
              >
                重试
              </button>
            </div>
          )}
          {sessionsStatus === "ready" && sessions.length === 0 && (
            <div className="session-empty">暂无会话，点上方按钮新建</div>
          )}
        </div>
        <div className="sidebar-footer">
          <button
            className="theme-toggle-btn"
            onClick={toggleTheme}
            title={isDark ? "切换到亮色模式" : "切换到暗色模式"}
          >
            {isDark ? <IconSun /> : <IconMoon />}
          </button>
          <button
            className="settings-btn"
            onClick={() => setCurrentPage("settings")}
          >
            <IconSettings /> 设置
          </button>
        </div>
      </div>

      {/* 主对话区 */}
      <div className="chat-main">
        {errorMessage && (
          <div className="chat-error-banner" role="alert">
            <span className="chat-error-text">
              <IconAlert /> {errorMessage}
            </span>
            <span className="chat-error-actions">
              {/*
                发送失败时原文已由 store 回填进输入框：这里把"下一步"说清楚，
                用户不必盯着一行红字猜该怎么办。
              */}
              <button
                className="chat-error-action"
                onClick={() => {
                  const el = document.querySelector<HTMLTextAreaElement>(
                    ".input-box textarea"
                  );
                  el?.focus();
                }}
                title="定位到输入框（原文已填回，可直接修改后重发）"
              >
                修改重发
              </button>
              <button
                className="chat-error-action"
                onClick={() => {
                  void copyText(errorMessage).then((ok) =>
                    pushToast(ok ? "已复制错误详情" : "复制失败：剪贴板不可用", ok ? "success" : "error")
                  );
                }}
                title="复制错误详情"
              >
                复制
              </button>
              <button
                className="chat-error-close"
                onClick={clearError}
                title="关闭提示"
                aria-label="关闭错误提示"
              >
                <IconX />
              </button>
            </span>
          </div>
        )}
        {currentSessionId ? (
          <>
            <div className="chat-header">
              <span className="chat-persona-badge">{currentPersonaName}</span>
              <TaskStatusBar />
              <div className="chat-header-actions">
                <button
                  className={`header-action-btn icon-only${conversationCopied ? " done" : ""}`}
                  onClick={handleCopyConversation}
                  disabled={messages.length === 0}
                  aria-label="复制对话"
                  title={conversationCopied ? "已复制为 Markdown" : "把整个会话复制为 Markdown"}
                >
                  {conversationCopied ? <IconCheck /> : <IconCopy />}
                </button>
                <button
                  className="header-action-btn icon-only"
                  onClick={handleSwitchToFloat}
                  aria-label="切换到悬浮窗"
                  title="切换到悬浮窗（隐藏主窗口）"
                >
                  <IconExpand />
                </button>
                <button
                  className="header-action-btn icon-only"
                  onClick={handleToggleFloat}
                  aria-label={floatVisible ? `召回${currentPersonaShortName}` : `召唤${currentPersonaShortName}`}
                  title={floatVisible ? "关闭悬浮窗" : "打开悬浮窗"}
                >
                  {floatVisible ? <IconMinimize /> : <IconSparkles />}
                </button>
              </div>
            </div>
            <MessageList personaShortName={currentPersonaShortName} />
            {/* 改动记录与计划面板都贴着输入框：它们是"这一轮任务"的状态，不是聊天内容 */}
            <ChangesPanel />
            <PlanPanel />
            <NotesPanel />
            <InputBox />
          </>
        ) : (
          <div className="no-session">
            <div className="no-session-icon">
              <IconSparkles />
            </div>
            <h2>Kagami no Konata</h2>
            <p>正在准备今日会话...</p>
          </div>
        )}
      </div>

      {/* 创建任务会话 / 工作区选择弹窗 */}
      {showTaskModal && (
        <div className="modal-overlay" onClick={() => setShowTaskModal(false)}>
          <div
            className="modal-card task-modal-card"
            ref={taskModalRef}
            tabIndex={-1}
            role="dialog"
            aria-modal="true"
            aria-label="创建任务会话"
            onClick={(e) => e.stopPropagation()}
          >
            <h3>
              <IconZap /> 创建任务会话
            </h3>

            <label>
              <span>执行工作区</span>
              <select
                value={selectedWorkspace}
                onChange={(e) => setSelectedWorkspace(e.target.value)}
              >
                {workspaces.map((w) => (
                  <option key={w.id} value={w.id}>
                    {w.label}
                    {w.is_default ? "（默认沙箱）" : ""}
                    {w.writable ? "" : " [只读]"}
                  </option>
                ))}
              </select>
            </label>

            <div className="task-modal-section">
              <div className="task-modal-section-title">或添加自定义项目路径</div>
              <div className="task-modal-inline">
                <input
                  type="text"
                  placeholder="绝对路径，例如 /home/me/project 或 D:\project"
                  aria-label="自定义项目绝对路径"
                  value={customPath}
                  onChange={(e) => setCustomPath(e.target.value)}
                />
              </div>
              <div className="task-modal-inline">
                <input
                  type="text"
                  placeholder="标签（可选）"
                  aria-label="工作区标签（可选）"
                  value={customLabel}
                  onChange={(e) => setCustomLabel(e.target.value)}
                />
                <label className="task-modal-check">
                  <input
                    type="checkbox"
                    checked={customWritable}
                    onChange={(e) => setCustomWritable(e.target.checked)}
                  />
                  允许写入
                </label>
              </div>
              <div className="task-modal-hint">
                填写路径后会先把它加入工作区列表，并用它作为本次任务的执行目录。
              </div>
            </div>

            <div className="task-modal-section">
              <div className="task-modal-section-title">运行模式</div>
              <div className="task-modal-mode-group" role="group" aria-label="任务运行模式">
                <button
                  type="button"
                  className={`task-modal-mode-btn${newTaskMode === "plan" ? " active plan" : ""}`}
                  aria-pressed={newTaskMode === "plan"}
                  onClick={() => setNewTaskMode("plan")}
                >
                  <IconClipboardList /> 规划 (Plan)
                </button>
                <button
                  type="button"
                  className={`task-modal-mode-btn${newTaskMode === "work" ? " active work" : ""}`}
                  aria-pressed={newTaskMode === "work"}
                  onClick={() => setNewTaskMode("work")}
                >
                  <IconZap /> 执行 (Work)
                </button>
              </div>
              <div className="task-modal-hint">
                建议先规划：Plan 阶段只读调查并产出计划，批准后再执行；
                直接选 Work 则跳过规划，适合目标已经明确的任务。
              </div>
            </div>

            <div className="task-modal-section">
              <div className="task-modal-section-title">模型选择</div>
              <label className="task-modal-check">
                <input
                  type="checkbox"
                  checked={autoModels}
                  onChange={(e) => setAutoModels(e.target.checked)}
                />
                自动选择（Plan 模式与子代理用子模型，Work 模式用主模型）
              </label>
              <div className="task-modal-hint">
                主模型与子模型在「设置 → 模型路由」里配置；不勾选则先跟随全局提供商，
                进入会话后仍可在底部工具条随时切换（只影响下一轮）。
              </div>
            </div>

            {taskModalError && (
              <div className="task-modal-error" role="alert">
                <IconAlert /> {taskModalError}
              </div>
            )}

            <div className="modal-actions">
              <button
                type="button"
                className="modal-cancel-btn"
                onClick={() => setShowTaskModal(false)}
              >
                取消
              </button>
              <button
                type="button"
                className="modal-confirm-btn"
                onClick={handleConfirmTaskModal}
                disabled={isCreatingTask}
              >
                {isCreatingTask ? "创建中..." : "确定创建"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 删除会话确认：给出影响范围，并说明会话类型（任务会话的计划/笔记一并消失） */}
      {deleteTarget && (
        <ConfirmDialog
          title="确认删除会话"
          description={
            <>
              将删除会话「{deleteTarget.title}」
              {deleteTarget.session_type === "task"
                ? "（任务会话，计划与工作记忆一并删除）"
                : ""}
              ，且无法恢复。
            </>
          }
          confirmLabel="确认删除"
          danger
          onConfirm={() => deleteSession(deleteTarget.id)}
          onClose={() => setDeleteTarget(null)}
        />
      )}
    </div>
  );
}
