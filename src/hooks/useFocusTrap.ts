import { useEffect, type RefObject } from "react";

const FOCUSABLE_SELECTOR = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

/**
 * 模态框焦点陷阱
 *
 * - 打开时把焦点送进容器（优先 `initialFocusSelector`，默认第一个可聚焦元素）；
 * - Tab / Shift+Tab 在容器内循环，键盘用户不会 Tab 到遮罩背后的界面；
 * - 关闭时把焦点还给打开前的元素。
 *
 * 应用内所有声明 `aria-modal="true"` 的弹窗都应使用它，避免三套弹窗三套焦点策略。
 */
export function useFocusTrap(
  containerRef: RefObject<HTMLElement | null>,
  active: boolean,
  options?: { initialFocusSelector?: string }
) {
  useEffect(() => {
    if (!active) return;
    const container = containerRef.current;
    if (!container) return;

    const previous = document.activeElement as HTMLElement | null;

    const focusFirst = () => {
      const target = options?.initialFocusSelector
        ? container.querySelector<HTMLElement>(options.initialFocusSelector)
        : null;
      (target ?? container.querySelector<HTMLElement>(FOCUSABLE_SELECTOR) ?? container)?.focus();
    };
    focusFirst();

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const nodes = Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
        (node) => node.offsetParent !== null || node === document.activeElement
      );
      if (nodes.length === 0) {
        event.preventDefault();
        container.focus();
        return;
      }
      const first = nodes[0];
      const last = nodes[nodes.length - 1];
      const focused = document.activeElement;
      if (
        event.shiftKey &&
        (focused === first || focused === container || !container.contains(focused))
      ) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && focused === last) {
        event.preventDefault();
        first.focus();
      }
    };

    container.addEventListener("keydown", onKeyDown);
    return () => {
      container.removeEventListener("keydown", onKeyDown);
      previous?.focus?.();
    };
  }, [active, containerRef, options?.initialFocusSelector]);
}
