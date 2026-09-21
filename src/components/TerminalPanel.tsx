import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { DEFAULT_TERMINAL_HEIGHT, MIN_TERMINAL_HEIGHT, useSessionStore } from "../store/sessionStore";
import { useTerminalSocket } from "../store/useTerminalSocket";
import { STATE_COLOR_VAR } from "../styles/sessionStyle";
import styles from "./TerminalPanel.module.css";

const THEME = {
  background: "#0a0c12",
  foreground: "#e4e7ef",
  cursor: "#8fd6ff",
  selectionBackground: "rgba(143, 214, 255, 0.28)",
};

/**
 * The board's single terminal pane. One xterm instance for its whole life:
 * switching sessions only changes which pty the socket hook streams into it
 * (the backend replays that pty's scrollback), so a switch is a repaint, not
 * a remount. The header is always visible — even collapsed — so it's never
 * unclear which session the terminal belongs to.
 */
export function TerminalPanel() {
  const terminal = useSessionStore((s) => s.terminal);
  const session = useSessionStore((s) => (s.terminal.sessionId ? s.sessions[s.terminal.sessionId] : undefined));
  const taskTitle = useSessionStore((s) => s.tasks.find((t) => t.id === s.terminal.taskId)?.title);
  const toggle = useSessionStore((s) => s.toggleTerminalCollapsed);
  const height = useSessionStore((s) => s.terminalHeight);
  const setHeight = useSessionStore((s) => s.setTerminalHeight);
  const [dragging, setDragging] = useState(false);

  const hostRef = useRef<HTMLDivElement>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const [term, setTerm] = useState<Terminal | null>(null);

  const mounted = terminal.ptyId !== null;

  // Created when the pane first appears, disposed when it's fully closed.
  useEffect(() => {
    if (!mounted || !hostRef.current) return;
    const t = new Terminal({
      theme: THEME,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 12.5,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: true,
    });
    const fit = new FitAddon();
    t.loadAddon(fit);
    t.open(hostRef.current);
    fitRef.current = fit;
    setTerm(t);

    const host = hostRef.current;
    let raf = 0;
    // Skip while collapsed/animating shut: fitting a ~0px box yields nonsense sizes.
    const refit = () => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        if (host.clientHeight > 40 && host.clientWidth > 40) fit.fit();
      });
    };
    const ro = new ResizeObserver(refit);
    ro.observe(host);
    refit();

    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      t.dispose();
      fitRef.current = null;
      setTerm(null);
    };
  }, [mounted]);

  useTerminalSocket(term);

  // Give keyboard focus to the terminal whenever it's opened or switched to.
  useEffect(() => {
    if (term && !terminal.collapsed) term.focus();
  }, [term, terminal.ptyId, terminal.collapsed]);

  if (!mounted) return null;

  // Drag the top edge to resize. Keep at least ~160px for the board above.
  const maxHeight = () => Math.max(MIN_TERMINAL_HEIGHT, window.innerHeight - 160);
  const startResize = (e: React.PointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    const startY = e.clientY;
    const startH = height;
    setDragging(true);
    const move = (ev: PointerEvent) => {
      setHeight(Math.min(maxHeight(), startH + (startY - ev.clientY)));
    };
    const up = () => {
      setDragging(false);
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const label = session ? (
    <>
      <span className={styles.dot} style={{ background: STATE_COLOR_VAR[session.state] }} />
      <span className={styles.title}>{session.title}</span>
      <span className={styles.project}>{session.project}</span>
    </>
  ) : terminal.sessionId ? (
    <span className={styles.project}>{terminal.sessionId}</span>
  ) : (
    <>
      <span className={styles.pending} />
      <span className={styles.title}>Starting session{taskTitle ? ` for “${taskTitle}”` : ""}…</span>
    </>
  );

  return (
    <div className={styles.panel} data-testid="terminal-panel">
      {!terminal.collapsed && (
        <div
          className={styles.resizer}
          onPointerDown={startResize}
          onDoubleClick={() => setHeight(DEFAULT_TERMINAL_HEIGHT)}
          role="separator"
          aria-orientation="horizontal"
          title="Drag to resize · double-click to reset"
          data-testid="terminal-resizer"
        />
      )}
      <button
        className={styles.header}
        onClick={toggle}
        aria-expanded={!terminal.collapsed}
        title={terminal.collapsed ? "Expand terminal" : "Collapse terminal"}
      >
        <span className={styles.chevron} data-collapsed={terminal.collapsed}>
          ▾
        </span>
        <span className={styles.kind}>Terminal</span>
        <span className={styles.label} data-testid="terminal-label">
          {label}
        </span>
        {!terminal.alive && <span className={styles.ended}>· session ended</span>}
      </button>
      <div
        className={`${styles.body} ${terminal.collapsed ? styles.bodyCollapsed : ""} ${dragging ? styles.bodyDragging : ""}`}
      >
        <div className={styles.bodyInner} style={{ height }}>
          <div ref={hostRef} className={styles.host} />
        </div>
      </div>
    </div>
  );
}
