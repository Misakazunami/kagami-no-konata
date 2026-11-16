import { create } from "zustand";
import type { GlobalShortcutStatusData, InputInjectionStatusData } from "../types/events";
import type { PlatformCapabilities } from "../types/platform";

export type ToastKind = "success" | "error" | "info";

export interface Toast {
  id: string;
  kind: ToastKind;
  text: string;
  /** 毫秒；0 = 不自动消失 */
  duration: number;
}

interface UiState {
  toasts: Toast[];
  /** 平台能力（启动时由 App 调用 `get_platform_capabilities` 填充） */
  platform: PlatformCapabilities | null;
  setPlatformCapabilities: (platform: PlatformCapabilities) => void;
  /** Wayland 门户快捷键最近一次状态（仅 portal 后端会收到） */
  globalShortcutStatus: GlobalShortcutStatusData | null;
  setGlobalShortcutStatus: (status: GlobalShortcutStatusData) => void;
  /** Wayland EIS 输入服务最近一次状态（仅 eis 后端会收到） */
  inputInjectionStatus: InputInjectionStatusData | null;
  setInputInjectionStatus: (status: InputInjectionStatusData) => void;
  /** 推一条 toast；同一文本只保留最新一条（避免重复点击刷屏） */
  pushToast: (text: string, kind?: ToastKind, duration?: number) => void;
  dismissToast: (id: string) => void;
}

const DEFAULT_DURATION: Record<ToastKind, number> = {
  success: 2500,
  info: 3500,
  // 错误多停留一会儿，用户需要时间读懂
  error: 6000,
};

/**
 * 全局 toast
 *
 * 应用内唯一的轻量反馈通道（设置页保存、复制失败、撤销授权……）。
 * 与 `errorMessage`（聊天页顶部的持久错误横幅）分工：toast 是"刚刚发生的一次性反馈"，
 * 自动消退、不占版面、不需要用户手动关闭。
 */
export const useUiStore = create<UiState>((set, get) => ({
  toasts: [],
  platform: null,
  globalShortcutStatus: null,
  inputInjectionStatus: null,

  setPlatformCapabilities: (platform) => set({ platform }),

  setGlobalShortcutStatus: (status) =>
    set((state) => ({
      globalShortcutStatus: status,
      // 门户绑定是异步的：把最新结果合并回能力，设置页与快捷键注册
      // 都读取同一份状态，避免“启动时 false、绑定后仍显示不可用”。
      platform: state.platform
        ? {
            ...state.platform,
            globalShortcut: status.available,
            notes: state.platform.notes.includes(status.message)
              ? state.platform.notes
              : [...state.platform.notes, status.message],
          }
        : null,
    })),

  setInputInjectionStatus: (status) =>
    set((state) => ({
      inputInjectionStatus: status,
      platform: state.platform
        ? {
            ...state.platform,
            inputInjection: status.available,
            notes: state.platform.notes.includes(status.message)
              ? state.platform.notes
              : [...state.platform.notes, status.message],
          }
        : null,
    })),

  pushToast: (text, kind = "info", duration) => {
    const trimmed = text.trim();
    if (!trimmed) return;
    const id = crypto.randomUUID();
    const toast: Toast = {
      id,
      kind,
      text: trimmed,
      duration: duration ?? DEFAULT_DURATION[kind],
    };
    set((state) => ({
      // 相同文本先去重：连点保存不会堆一列一样的 toast
      toasts: [...state.toasts.filter((t) => t.text !== trimmed), toast].slice(-4),
    }));
    if (toast.duration > 0) {
      setTimeout(() => get().dismissToast(id), toast.duration);
    }
  },

  dismissToast: (id) => set((state) => ({ toasts: state.toasts.filter((t) => t.id !== id) })),
}));
