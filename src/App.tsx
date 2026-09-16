import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { register } from "@tauri-apps/plugin-global-shortcut";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { emptyGenerationState, getLastLocalStreamId, useChatStore } from "./stores/chatStore";
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

/**
 * 全局快捷键是否已注册
 *
 * 模块级守卫：主窗口单实例且与进程同生命周期，注册一次即可；
 * 若随组件卸载注销，会与下一次挂载的注册竞态（见 effect 内注释）。
 */
let mainShortcutRegistered = false;

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
        // 主题与字号**永远**要应用：超时兜底只是让界面先可交互，
        // 若之后配置才返回而这里直接 return，用户配置的主题会被永久吞掉
        applyTheme(config.ui.theme, config.ui.font_size);
        if (settled) return;
        settled = true;
        clearTimeout(timeoutId);
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

    /*
     * 全局快捷键 Ctrl+Shift+K 切换主窗口
     *
     * 注册与注销都是异步的，且主窗口在进程生命周期内只有一个实例：
     * 每次挂载都 unregister 会与下一次 register 竞态（最终可能落在"已注销"
     * 状态）。因此这里只注册一次（模块级守卫），不随组件卸载注销；
     * 失败只记录日志，不影响其它功能。
     */
    if (!mainShortcutRegistered) {
      mainShortcutRegistered = true;
      register("CmdOrCtrl+Shift+K", () => {
        invoke("toggle_main_window");
      }).catch((e) => {
        mainShortcutRegistered = false;
        console.error("Failed to register global shortcut:", e);
      });
    }

    void (async () => {
      // allSettled：单个订阅失败不能带走其它监听
      // （Promise.all 失败时，已成功返回的 unlisten 函数会全部泄漏）
      const results = await Promise.allSettled([
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
        listen<{ session_id: string; stream_id: string }>("session-updated", (event) => {
          const { currentSessionId, refreshCurrentSession } = useChatStore.getState();
          const { session_id, stream_id } = event.payload ?? {};
          if (session_id !== currentSessionId) return;
          // 自己发起的那一轮不回读：本地消息已带统计/模型标签，回读只会闪一下
          // （历史实现每次发送都触发全量刷新，把模型标签当场抹掉）。
          // 其它窗口（含悬浮窗）的生成仍要通过回读同步。
          if (stream_id && stream_id === getLastLocalStreamId()) return;
          refreshCurrentSession();
        }),
        // 监听会话删除事件（确保主窗口状态同步）
        listen<string>("session-deleted", (event) => {
          const { currentSessionId, loadSessions } = useChatStore.getState();
          if (event.payload === currentSessionId) {
            useChatStore.setState({
              currentSessionId: null,
              messages: [],
              // 报错属于被删掉的会话，不能跟着用户留在界面上
              errorMessage: null,
              // liveToolCalls / plan / notes 等"这一轮生成"与"会话级"状态
              // 必须一起复位，否则会以孤儿卡片的形式出现在下一个会话里
              ...emptyGenerationState(),
            });
            loadSessions();
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
          fns.push(result.value);
        } else {
          console.error("Failed to subscribe window event:", result.reason);
        }
      }
    })();

    return () => {
      disposed = true;
      fns.forEach((fn) => fn());
      // 快捷键故意不注销（见上方注释）：窗口与进程同生命周期
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
