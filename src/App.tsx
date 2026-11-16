import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { register } from "@tauri-apps/plugin-global-shortcut";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { emptyGenerationState, getLastLocalStreamId, useChatStore } from "./stores/chatStore";
import { useUiStore } from "./stores/uiStore";
import {
  PLATFORM_EVENT,
  type GlobalShortcutStatusData,
  type HarnessNoticeData,
  type SessionUpdatedData,
} from "./types/events";
import type { PlatformCapabilities } from "./types/platform";
import { ChatWindow } from "./components/chat/ChatWindow";
import { SettingsPage } from "./components/settings/SettingsPage";
import { PersonaEditor } from "./components/persona/PersonaEditor";
import { OnboardingPage } from "./components/onboarding/OnboardingPage";
import { FloatingWidget } from "./components/float/FloatingWidget";
import { ToolApprovalDialog } from "./components/chat/ToolApprovalDialog";
import { AppErrorBoundary } from "./components/ui/AppErrorBoundary";
import { ToastViewport } from "./components/ui/ToastViewport";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/chat.css";
import "./styles/settings.css";
import "./styles/float.css";
import "./styles/tools.css";
import "./styles/panels.css";
import "./styles/conversation.css";
import "./styles/a11y.css";
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
  const platform = useUiStore((s) => s.platform);
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

  // 平台能力：主窗口与悬浮窗都需要（悬浮窗据此提示定位降级）。
  // 只探测一次；失败不阻塞启动，界面按“未知平台”保守处理。
  useEffect(() => {
    let disposed = false;
    invoke<PlatformCapabilities>("get_platform_capabilities")
      .then((caps) => {
        if (disposed) return;
        useUiStore.getState().setPlatformCapabilities(caps);
        if (isFloatWindow || caps.sessionType !== "wayland") return;

        const notice =
          caps.globalShortcutBackend === "portal"
            ? caps.globalShortcut
              ? "Wayland 会话：全局快捷键已通过系统门户启用（实验）；窗口绝对定位、置顶和桌面控制仍不可用。"
              : "Wayland 会话：已请求通过系统门户绑定 Ctrl+Shift+K（实验），首次可能需要在系统弹窗中确认；" +
                "绑定成功前快捷键不可用。窗口绝对定位、置顶和桌面控制仍不可用。"
            : caps.screenCaptureOnly
              ? "Wayland 会话：已启用只读截图（实验），可以查看屏幕，但仍不能点击、输入或切换窗口；" +
                "窗口绝对定位、置顶和全局快捷键也不可用。需要完整桌面能力请注销并选择 X11/Xorg 会话。"
              : "Wayland 会话：窗口绝对定位、置顶、全局快捷键、屏幕截图和桌面控制不可用。" +
                "需要完整桌面能力请注销并选择 X11/Xorg 会话。";
        const key =
          caps.globalShortcutBackend === "portal"
            ? "konata-wayland-portal-shortcut-notice-shown"
            : caps.screenCaptureOnly
              ? "konata-wayland-readonly-notice-shown"
              : "konata-wayland-notice-shown";
        try {
          if (sessionStorage.getItem(key)) return;
          sessionStorage.setItem(key, "1");
        } catch {
          // storage 不可用（隐私模式等）时不做持久去重，宁可多提示一次
        }
        useUiStore.getState().pushToast(notice, "info", 9000);
      })
      .catch((e) => console.error("Failed to load platform capabilities:", e));
    return () => {
      disposed = true;
    };
  }, [isFloatWindow]);

  // 主窗口：注册全局快捷键 + 监听事件
  useEffect(() => {
    if (isFloatWindow) return;

    const fns: Array<() => void> = [];
    let disposed = false;

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
        listen<SessionUpdatedData>("session-updated", (event) => {
          const { currentSessionId, refreshCurrentSession, adoptLocalMessageIds } =
            useChatStore.getState();
          const { session_id, stream_id } = event.payload ?? {};
          if (session_id !== currentSessionId) return;
          /*
           * 自己发起的那一轮：把本地乐观消息的 id 换成数据库 id，而不是整表回读。
           *
           * 本地 id（crypto.randomUUID）与数据库 id 不同源，不换的话紧接着点
           * 「重试 / 编辑 / 回退」时后端按本地 id 查不到消息。回读虽然也能对齐，
           * 但会重建所有消息对象、让整列表的 Markdown 重新解析（长会话会卡）。
           *
           * 这次广播由后端在本轮落库之后发出，因此载荷里的 id 一定已经存在；
           * 其它窗口（含悬浮窗）的生成仍通过回读同步。
           */
          if (stream_id && stream_id === getLastLocalStreamId()) {
            adoptLocalMessageIds(event.payload);
            return;
          }
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
        // 后端运行期提示（截图失败、能力降级等）：作为一次性 toast 展示，
        // 不写进聊天消息，避免污染 LLM 上下文。
        listen<HarnessNoticeData>("harness-notice", (event) => {
          const payload = event.payload;
          if (!payload?.message) return;
          useUiStore
            .getState()
            .pushToast(
              payload.message,
              payload.level === "error" ? "error" : "info",
              payload.level === "warning" ? 7000 : undefined
            );
        }),
        // 悬浮窗定位在 Wayland 下由合成器决定：明确提示，不假装成功
        listen("float-position-unsupported", () => {
          useUiStore
            .getState()
            .pushToast(
              "当前图形会话不支持悬浮窗绝对定位，位置由合成器决定。",
              "info",
              6000
            );
        }),
        // Wayland 门户快捷键：绑定/失效都是异步的，必须按后端回报更新能力。
        listen<GlobalShortcutStatusData>(PLATFORM_EVENT.globalShortcutStatus, (event) => {
          const payload = event.payload;
          if (!payload?.message) return;
          useUiStore.getState().setGlobalShortcutStatus(payload);
          useUiStore
            .getState()
            .pushToast(
              payload.message,
              payload.available ? "success" : "info",
              payload.available ? 6000 : 9000
            );
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

      // 监听装好后再读一次能力：门户绑定可能在上面的 IPC 往返期间完成，
      // 那一条事件会错过；补一次快照，后续变化仍由事件驱动。
      invoke<PlatformCapabilities>("get_platform_capabilities")
        .then((caps) => {
          if (!disposed) useUiStore.getState().setPlatformCapabilities(caps);
        })
        .catch((e) => console.error("Failed to refresh platform capabilities:", e));
    })();

    return () => {
      disposed = true;
      fns.forEach((fn) => fn());
    };
  }, [isFloatWindow]);

  /*
   * 全局快捷键 Ctrl+Shift+K 切换主窗口
   *
   * 注册与注销都是异步的，且主窗口在进程生命周期内只有一个实例：
   * 每次挂载都 unregister 会与下一次 register 竞态（最终可能落在"已注销"
   * 状态）。因此这里只注册一次（模块级守卫），不随组件卸载注销；
   * 失败只记录日志，不影响其它功能。
   *
   * Wayland 下 global-hotkey 不保证可用，能力探测为 false 时直接跳过。
   */
  useEffect(() => {
    if (
      isFloatWindow ||
      !platform ||
      // 门户后端由后端直接监听 Activated 并切换窗口；前端再注册会重复触发。
      platform.globalShortcutBackend === "portal" ||
      !platform.globalShortcut ||
      mainShortcutRegistered
    ) {
      return;
    }
    mainShortcutRegistered = true;
    register("CmdOrCtrl+Shift+K", () => {
      invoke("toggle_main_window");
    }).catch((e) => {
      mainShortcutRegistered = false;
      console.error("Failed to register global shortcut:", e);
    });
  }, [isFloatWindow, platform]);

  // 悬浮窗：渲染桌宠（含内嵌气泡 + 输入框）
  if (isFloatWindow) {
    return (
      <div className="app float-app">
        <FloatingWidget />
        <ToastViewport />
      </div>
    );
  }

  // 主窗口：配置加载完毕前显示加载提示，避免闪烁或卡住
  if (!appReady) {
    return (
      <div className="app" style={{ display: "flex", alignItems: "center", justifyContent: "center", height: "100vh", color: "var(--text-secondary, #a9b1d6)" }}>
        <span role="status" aria-live="polite">加载中…</span>
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
      <ToastViewport />
    </div>
  );
}

/**
 * 根错误边界：任何组件渲染期抛错都兜在这里，而不是把 WebView 变成白屏。
 * 放在导出层而非 App 内部，两个窗口（含桌宠）都被覆盖。
 */
export default function Root() {
  return (
    <AppErrorBoundary>
      <App />
    </AppErrorBoundary>
  );
}
