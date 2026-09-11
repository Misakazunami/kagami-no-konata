import { useEffect, useMemo, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useChatStore, type Session } from "../../stores/chatStore";
import { MessageList } from "./MessageList";
import { InputBox } from "./InputBox";
import {
  DEFAULT_PERSONA_ID,
  FALLBACK_SHORT_NAME,
  type PersonaSummary,
} from "../../types/persona";

/** 格式化日期标题 */
function formatDateHeader(dateStr: string): string {
  const today = new Date().toISOString().split("T")[0];
  const yesterday = new Date(Date.now() - 86400000).toISOString().split("T")[0];
  if (dateStr === today) return "今天";
  if (dateStr === yesterday) return "昨天";
  return dateStr;
}

/** 按日期分组会话 */
function groupSessionsByDate(sessions: Session[]): [string, Session[]][] {
  const groups: Record<string, Session[]> = {};
  for (const s of sessions) {
    const date = s.created_at.split("T")[0];
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
  const [selectedPersona, setSelectedPersona] = useState("konata-default");

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

  const handleNewSession = () => {
    createSession(undefined, selectedPersona);
  };

  // 悬浮窗状态：通过后端事件实时同步
  const [floatVisible, setFloatVisible] = useState(false);
  useEffect(() => {
    // 初始化：查询当前悬浮窗状态
    invoke<boolean>("is_float_visible").then(setFloatVisible).catch(() => {});
    // 监听后端发射的可见性变化事件
    const unlisten = listen<boolean>("float-visibility-changed", (event) => {
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

  return (
    <div className="chat-window">
      {/* 侧边栏 */}
      <div className="sidebar">
        <div className="sidebar-header">
          <h2>✦ 镜中此方</h2>
          <div className="new-session-controls">
            <select
              className="persona-select"
              value={selectedPersona}
              onChange={(e) => setSelectedPersona(e.target.value)}
            >
              {personas.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            <button className="new-chat-btn" onClick={handleNewSession}>
              + 新会话
            </button>
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
                    onClick={() => {
                      if (editingId !== s.id) switchSession(s.id);
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
                        <span className="session-title">{s.title}</span>
                        {personaName && s.persona_id !== DEFAULT_PERSONA_ID && (
                          <span className="session-persona-tag">{personaName}</span>
                        )}
                      </div>
                    )}
                    <button
                      className="session-delete"
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
    </div>
  );
}
