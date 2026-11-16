import { useCallback, useSyncExternalStore } from "react";

/**
 * 订阅一条媒体查询，返回它当前是否匹配
 *
 * 用 `useSyncExternalStore` 而不是"effect 里 addEventListener + setState"：
 * 后者在挂载后的第一帧才写入初始值，订阅发生在渲染之后，窗口尺寸恰好在
 * 这一刻变化时会漏掉一次变更（窄窗口启动时侧栏就不会自动折叠）。
 * 这里把 `matchMedia` 的当前值直接当作渲染快照，订阅前后都不会读到过期值。
 *
 * @param query 形如 `(max-width: 699px)`；断点值必须与 CSS 里的分档一致。
 */
export function useMediaQuery(query: string): boolean {
  const subscribe = useCallback(
    (onStoreChange: () => void) => {
      if (typeof window === "undefined" || !window.matchMedia) return () => {};
      const list = window.matchMedia(query);
      list.addEventListener("change", onStoreChange);
      return () => list.removeEventListener("change", onStoreChange);
    },
    [query]
  );

  const getSnapshot = useCallback(() => {
    if (typeof window === "undefined" || !window.matchMedia) return false;
    return window.matchMedia(query).matches;
  }, [query]);

  // 服务端/无 matchMedia 环境：恒为不匹配，不阻塞渲染
  const getServerSnapshot = useCallback(() => false, []);

  return useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);
}
