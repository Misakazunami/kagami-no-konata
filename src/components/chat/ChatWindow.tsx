import { useEffect, useMemo, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore, type Session } from "../../stores/chatStore";
import { MessageList } from "./MessageList";
import { InputBox } from "./InputBox";
import { NotesPanel } from "./NotesPanel";
import { PlanPanel } from "./PlanPanel";
import { SnapshotBanner } from "./SnapshotBanner";
import type { WorkspaceView } from "../../types/tools";
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
  const errorMessage = useChatStore((s) => s.errorMessage);
  const clearError = useChatStore((s) => s.clearError);
  const initSession = useChatStore((s) => s.initSession);
  const refreshCurrentSession = useChatStore((s) => s.refreshCurrentSession);
  const createSession = useChatStore((s) => s.createSession);
  const switchSession = useChatStore((s) => s.switchSession);
  const deleteSession = useChatStore((s) => s.deleteSession);
  const renameSession = useChatStore((s) => s.renameSession);
  const setCurrentPage = useChatStore((s) => s.setCurrentPage);

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

  const openTaskModal = async () => {
    setTaskModalError("");
    // 勾选状态跟随设置里的"新建任务会话默认自动"（用户仍可在弹窗里改）
    setAutoModels(
      useChatStore.getState().modelCatalog?.settings.auto_by_default ?? false
    );
    try {
      const list = await invoke<WorkspaceView[]>("list_workspaces");
      setWorkspaces(list);
      const defaultWs = list.find((w) => w.is_default) ?? list[0];
      setSelectedWorkspace(defaultWs ? defaultWs.id : "");
      setShowTaskModal(true);
    } catch (e) {
      console.error("加载工作区失败:", e);
      // 降级直接创建
      createSession(undefined, selectedPersona, "task");
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
        autoModels ? { mode: "auto" } : { mode: "inherit" }
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

  // 创建任务弹窗：Esc 关闭 + 打开时聚焦（键盘用户不被困在背景里）
  const taskModalRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!showTaskModal) return;
    const previous = document.activeElement as HTMLElement | null;
    taskModalRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setShowTaskModal(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      previous?.focus?.();
    };
  }, [showTaskModal]);

  return (
    <div className="chat-window">
      {/* 侧边栏 */}
      <div className="sidebar">
        <div className="sidebar-header">
          <h2>✦ 镜中此方</h2>
          {/* 第一行：新建入口（两个按钮等宽，各自占一半，避免被侧边栏宽度挤破） */}
          <div className="new-session-controls">
            <button
              className="new-chat-btn"
              onClick={() => createSession(undefined, selectedPersona, "chat")}
              title="创建普通聊天会话"
            >
              + 对话
            </button>
            <button
              className="new-chat-btn task"
              onClick={openTaskModal}
              title="创建专业任务工程会话"
            >
              ⚡ 任务
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
                return (
                  <div
                    key={s.id}
                    className={`session-item ${s.id === currentSessionId ? "active" : ""}`}
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
                          <span className="session-type-badge">任务</span>
                        )}
                        <span className="session-title">{s.title}</span>
                        {personaName && s.persona_id !== DEFAULT_PERSONA_ID && (
                          <span className="session-persona-tag">{personaName}</span>
                        )}
                      </div>
                    )}
                    <button
                      className="session-delete"
                      aria-label={`删除会话「${s.title}」`}
                      title="删除会话"
                      onClick={(e) => {
                        e.stopPropagation();
                        if (confirm(`确认删除会话「${s.title}」？`)) {
                          deleteSession(s.id);
                        }
                      }}
                    >
                      ×
                    </button>
                  </div>
                );
              })}
            </div>
          ))}
          {sessions.length === 0 && (
            <div className="session-empty">暂无会话</div>
          )}
        </div>
        <div className="sidebar-footer">
          <button
            className="theme-toggle-btn"
            onClick={toggleTheme}
            title={isDark ? "切换到亮色模式" : "切换到暗色模式"}
          >
            {isDark ? "☀" : "🌙"}
          </button>
          <button
            className="settings-btn"
            onClick={() => setCurrentPage("settings")}
          >
            ⚙ 设置
          </button>
        </div>
      </div>

      {/* 主对话区 */}
      <div className="chat-main">
        {errorMessage && (
          <div className="chat-error-banner" role="alert">
            <span className="chat-error-text">⚠ {errorMessage}</span>
            <button
              className="chat-error-close"
              onClick={clearError}
              title="关闭提示"
            >
              ×
            </button>
          </div>
        )}
        {currentSessionId ? (
          <>
            <div className="chat-header">
              <span className="chat-persona-badge">{currentPersonaName}</span>
              <div className="chat-header-actions">
                <button className="header-action-btn" onClick={handleSwitchToFloat} title="切换到悬浮窗">
                  ↗ 切换
                </button>
                <button className="header-action-btn" onClick={handleToggleFloat} title={floatVisible ? "关闭悬浮窗" : "打开悬浮窗"}>
                  {floatVisible
                    ? `↙ 召回${currentPersonaShortName}`
                    : `↗ 召唤${currentPersonaShortName}`}
                </button>
              </div>
            </div>
            <MessageList personaShortName={currentPersonaShortName} />
            {/* 回滚条与计划面板都贴着输入框：它们是"这一轮任务"的状态，不是聊天内容 */}
            <SnapshotBanner />
            <PlanPanel />
            <NotesPanel />
            <InputBox />
          </>
        ) : (
          <div className="no-session">
            <div className="no-session-icon">✦</div>
            <h2>镜中此方</h2>
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
            <h3>⚡ 创建任务会话</h3>

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
                  value={customPath}
                  onChange={(e) => setCustomPath(e.target.value)}
                />
              </div>
              <div className="task-modal-inline">
                <input
                  type="text"
                  placeholder="标签（可选）"
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
                ⚠ {taskModalError}
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
    </div>
  );
}
