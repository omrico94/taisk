import { useEffect, type RefObject } from "react";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

/**
 * Keeps a popup window's height matched to `ref`'s actual rendered content
 * instead of a fixed size that leaves empty space below short content (the
 * "New task" input view vs. the taller board-picker view, e.g.) or clips
 * long content. Width stays fixed at the design's own panel width; height
 * tracks content via ResizeObserver, capped at `maxHeight` so a long list
 * scrolls (see the `.list` overflow-y in Popup.module.css) instead of
 * growing the window past a reasonable size.
 */
export function useAutoResizeWindow(ref: RefObject<HTMLElement | null>, width: number, maxHeight: number) {
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    let raf = 0;
    const resize = () => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        const current = ref.current;
        if (!current) return;
        const height = Math.min(Math.ceil(current.scrollHeight), maxHeight);
        const win = getCurrentWindow();
        void win.setSize(new LogicalSize(width, height)).then(() => win.center());
      });
    };
    const ro = new ResizeObserver(resize);
    ro.observe(el);
    resize();
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, [ref, width, maxHeight]);
}
