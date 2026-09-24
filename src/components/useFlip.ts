import { useEffect, useLayoutEffect, useRef, type RefObject } from "react";

const FLIP_MS = 420;
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
  // Natural (transform-free) rects from the previous layout, by `data-flip` key.
  const prev = useRef<Map<string, DOMRect>>(new Map());

  const flipEls = () => Array.from(rootRef.current?.querySelectorAll<HTMLElement>("[data-flip]") ?? []);

  /** Natural position: any in-flight FLIP animation is cancelled first, so its
   * transform can't leak into the rect (that would read as a phantom move). */
  const naturalRect = (el: HTMLElement): { visual: DOMRect; natural: DOMRect; wasAnimating: boolean } => {
    const visual = el.getBoundingClientRect();
    const running = el.getAnimations().filter((a) => a.id === "flip");
    running.forEach((a) => a.cancel());
    return { visual, natural: running.length ? el.getBoundingClientRect() : visual, wasAnimating: running.length > 0 };
  };

  useLayoutEffect(() => {
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    const next = new Map<string, DOMRect>();
    for (const el of flipEls()) {
      const key = el.dataset.flip as string;
      const { visual, natural, wasAnimating } = naturalRect(el);
      next.set(key, natural);
      if (reduced) continue;
      // Continue smoothly from where an interrupted animation visibly was;
      // otherwise start from the element's previous natural position.
      const from = wasAnimating ? visual : prev.current.get(key);
      if (!from) continue;
      const dx = from.left - natural.left;
      const dy = from.top - natural.top;
      if (Math.abs(dx) < 2 && Math.abs(dy) < 2) continue;
      el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "translate(0, 0)" }], {
        id: "flip",
        duration: FLIP_MS,
        easing: EASE,
      });
    }
    prev.current = next;
  });

  // Scrolling or resizing moves every rect without any React render; re-measure
  // so the next render's diff isn't polluted by the offset.
  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const remeasure = () => {
      const next = new Map<string, DOMRect>();
      for (const el of flipEls()) next.set(el.dataset.flip as string, naturalRect(el).natural);
      prev.current = next;
    };
    root.addEventListener("scroll", remeasure, true);
    window.addEventListener("resize", remeasure);
    return () => {
      root.removeEventListener("scroll", remeasure, true);
      window.removeEventListener("resize", remeasure);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
