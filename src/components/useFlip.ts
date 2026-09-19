import { useEffect, useLayoutEffect, useRef, type RefObject } from "react";

const FLIP_MS = 640;
const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";

/**
 * FLIP move animation for every `[data-flip]` element under `rootRef`
 * (design: "measure rects before the update, animate the delta after
 * render"). Runs after every render of the owning component: any element
 * whose key (`data-flip`) also existed in the previous layout and whose
 * position changed slides from its old spot to its new one. A key that
 * moves to a different parent (a session leaving the tray for a task card)
 * animates the same way. Elements with no previous rect just appear.
 */
export function useFlip(rootRef: RefObject<HTMLElement | null>): void {
  const prev = useRef<Map<string, DOMRect>>(new Map());

  const measure = () => {
    const next = new Map<string, DOMRect>();
    rootRef.current?.querySelectorAll<HTMLElement>("[data-flip]").forEach((el) => {
      next.set(el.dataset.flip as string, el.getBoundingClientRect());
    });
    return next;
  };

  useLayoutEffect(() => {
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    const before = prev.current;
    const root = rootRef.current;
    if (root && !reduced && before.size > 0) {
      root.querySelectorAll<HTMLElement>("[data-flip]").forEach((el) => {
        const old = before.get(el.dataset.flip as string);
        if (!old) return;
        const now = el.getBoundingClientRect();
        const dx = old.left - now.left;
        const dy = old.top - now.top;
        if (Math.abs(dx) < 2 && Math.abs(dy) < 2) return;
        el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "translate(0, 0)" }], {
          duration: FLIP_MS,
          easing: EASE,
        });
      });
    }
    prev.current = measure();
  });

  // Scrolling moves every rect without any React render; re-measure so the
  // next render's diff isn't polluted by scroll offset.
  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const onScroll = () => {
      prev.current = measure();
    };
    root.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onScroll);
    return () => {
      root.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onScroll);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
