import { create } from "zustand";

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
