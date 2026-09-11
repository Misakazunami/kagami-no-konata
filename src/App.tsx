import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { register, unregister } from "@tauri-apps/plugin-global-shortcut";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { useChatStore } from "./stores/chatStore";
import { ChatWindow } from "./components/chat/ChatWindow";
import { SettingsPage } from "./components/settings/SettingsPage";
import { PersonaEditor } from "./components/persona/PersonaEditor";
import { OnboardingPage } from "./components/onboarding/OnboardingPage";
import { FloatingWidget } from "./components/float/FloatingWidget";
import { ToolApprovalDialog } from "./components/chat/ToolApprovalDialog";
import "./App.css";
import "highlight.js/styles/github-dark.css";

function applyTheme(theme: string, fontSize: number) {
  const root = document.documentElement;
  if (theme === "auto") {
    const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
    root.setAttribute("data-theme", prefersDark ? "dark" : "light");
  } else {
    root.setAttribute("data-theme", theme);
  }
  root.style.fontSize = `${fontSize}px`;
}

function App() {
  const currentPage = useChatStore((s) => s.currentPage);
  const [appReady, setAppReady] = useState(false);
  const currentWindow = getCurrentWebviewWindow();
  const isFloatWindow = currentWindow.label === "float";

  // 标记窗口类型（用于 CSS 透明背景）
  useEffect(() => {
    if (isFloatWindow) {
      document.documentElement.setAttribute("data-window", "float");
      document.body.setAttribute("data-window", "float");
    }
  }, [isFloatWindow]);

  // 启动初始化：等待配置加载完毕后再渲染页面，避免竞态导致卡在加载中
  useEffect(() => {
    let timeoutId: ReturnType<typeof setTimeout>;
    let settled = false;

    // 超时保护：如果 5 秒内没有完成，强制进入就绪状态
    const timeout = new Promise<void>((resolve) => {
      timeoutId = setTimeout(() => {
        if (!settled) {
          console.warn("Config loading timeout, forcing ready state");
          settled = true;
          applyTheme("dark", 14);
          setAppReady(true);
        }
        resolve();
      }, 5000);
    });

    const configPromise = invoke<{
      ui: { theme: string; font_size: number };
      llm: { providers: Array<{ id: string; api_key: string; model: string }>; active_provider_id: string };
    }>("get_config")
      .then((config) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeoutId);
        applyTheme(config.ui.theme, config.ui.font_size);
        // 只有**活跃**提供商才算配置完成：历史实现检查"任意一个提供商有 key"，
        // 多提供商场景下会跳过引导，随后因为活跃提供商缺 key/模型而直接失败。
        const active = config.llm.providers.find(
          (p) => p.id === config.llm.active_provider_id
        ) ?? config.llm.providers[0];
        const ready = !!active && !!active.api_key?.trim() && !!active.model?.trim();
        if (!isFloatWindow && !ready) {
          useChatStore.setState({ currentPage: "onboarding" });
        }
      })
      .catch((err) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeoutId);
        console.error("Failed to load config:", err);
        applyTheme("dark", 14);
        if (!isFloatWindow) {
          useChatStore.setState({ currentPage: "onboarding" });
        }
      })
      .finally(() => {
        if (!settled) {
          settled = true;
          clearTimeout(timeoutId);
        }
        setAppReady(true);
      });

    // 竞速：配置加载 vs 超时（两者都自行收敛状态，这里只需吞掉未处理 rejection）
    void Promise.race([configPromise, timeout]);

    return () => {
      clearTimeout(timeoutId);
    };
  }, []);

  // 主窗口：注册全局快捷键 + 监听事件
  useEffect(() => {
    if (isFloatWindow) return;

    const fns: Array<() => void> = [];
    let disposed = false;

    // 注册 Ctrl+Shift+K 切换主窗口（卸载时必须注销，否则 HMR/重挂载会残留回调）
    register("CmdOrCtrl+Shift+K", () => {
      invoke("toggle_main_window");
    }).catch(console.error);

    void (async () => {
      const listeners = await Promise.all([
        // 监听托盘菜单导航事件
        listen<string>("navigate-to", (event) => {
          if (event.payload === "settings") {
            useChatStore.setState({ currentPage: "settings" });
          }
        }),
        // 监听会话标题更新事件（后端标题生成在后台执行，需全局监听）
        listen<[string, string]>("session-title-updated", (event) => {
          const [sid, title] = event.payload;
          useChatStore.getState().updateSessionTitle(sid, title);
        }),
        // 监听跨窗口会话更新（悬浮窗发送了消息时同步到主窗口）
        listen<string>("session-updated", (event) => {
          const { currentSessionId, refreshCurrentSession } = useChatStore.getState();
          if (event.payload === currentSessionId) {
            refreshCurrentSession();
          }
        }),
        // 监听会话删除事件（确保主窗口状态同步）
        listen<string>("session-deleted", (event) => {
          const { currentSessionId, loadSessions } = useChatStore.getState();
          if (event.payload === currentSessionId) {
            useChatStore.setState({
              currentSessionId: null,
              messages: [],
              isStreaming: false,
              activeStreamId: null,
            });
            loadSessions();
          }
        }),
      ]);

      if (disposed) {
        listeners.forEach((fn) => fn());
        return;
      }
      fns.push(...listeners);
    })();

    return () => {
      disposed = true;
      fns.forEach((fn) => fn());
      // capabilities 已授予 global-shortcut:allow-unregister
      unregister("CmdOrCtrl+Shift+K").catch(() => {});
    };
  }, [isFloatWindow]);

  // 悬浮窗：渲染桌宠（含内嵌气泡 + 输入框）
  if (isFloatWindow) {
    return (
      <div className="app float-app">
        <FloatingWidget />
      </div>
    );
  }

  // 主窗口：配置加载完毕前显示加载提示，避免闪烁或卡住
  if (!appReady) {
    return (
      <div className="app" style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100vh", color: "var(--text-secondary, #a9b1d6)" }}>
        加载中...
      </div>
    );
  }

  // 主窗口：完整界面
  return (
    <div className="app">
      {currentPage === "onboarding" && <OnboardingPage />}
      {currentPage === "chat" && <ChatWindow />}
      {currentPage === "settings" && <SettingsPage />}
      {currentPage === "persona" && <PersonaEditor />}
      {/* 工具审批弹窗：跨页面都要能看到，且只在主窗口渲染（悬浮窗在上方已提前返回） */}
      <ToolApprovalDialog />
    </div>
  );
}

export default App;
