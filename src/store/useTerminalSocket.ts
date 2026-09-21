import { useEffect } from "react";
import type { Terminal } from "@xterm/xterm";
import { terminalWsUrl } from "../api";
import { useSessionStore } from "./sessionStore";

const encoder = new TextEncoder();

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function bytesToB64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

const MAX_RETRY_DELAY_MS = 15_000;
/** Consecutive connects that never delivered a frame (e.g. the pty is gone). */
const MAX_BARREN_ATTEMPTS = 5;

/**
 * Streams the attached pty into `term` and forwards keystrokes back. Keyed on
 * the store's `terminal.ptyId`: switching sessions tears the socket down and
 * opens one for the new pty, and the server replays that pty's scrollback
 * first, so the one xterm instance simply resets and repaints. Reconnect/
 * backoff mirrors `useSessionEngine`.
 */
export function useTerminalSocket(term: Terminal | null): void {
  const ptyId = useSessionStore((s) => s.terminal.ptyId);
  const resolveTerminalSession = useSessionStore((s) => s.resolveTerminalSession);
  const setTerminalAlive = useSessionStore((s) => s.setTerminalAlive);

  useEffect(() => {
    if (!ptyId || !term) return;

    let ws: WebSocket | undefined;
    let cancelled = false;
    let exited = false;
    let retryDelayMs = 1000;
    let barrenAttempts = 0;
    let reconnectTimer: ReturnType<typeof setTimeout> | undefined;

    const send = (frame: object) => {
      if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(frame));
    };

    term.reset();
    setTerminalAlive(true);

    const connect = () => {
      if (cancelled) return;
      let gotFrame = false;
      ws = new WebSocket(terminalWsUrl(ptyId));
      ws.onopen = () => {
        retryDelayMs = 1000;
        send({ type: "resize", cols: term.cols, rows: term.rows });
      };
      ws.onmessage = (event) => {
        gotFrame = true;
        barrenAttempts = 0;
        const msg = JSON.parse(event.data as string);
        if (msg.type === "scrollback") {
          // (Re)attach: the server always sends the recent history first.
          term.reset();
          term.write(b64ToBytes(msg.data));
        } else if (msg.type === "data") {
          term.write(b64ToBytes(msg.data));
        } else if (msg.type === "linked") {
          resolveTerminalSession(msg.session_id);
        } else if (msg.type === "exited") {
          exited = true;
          setTerminalAlive(false);
        }
      };
      ws.onclose = () => {
        if (cancelled || exited) return;
        if (!gotFrame && ++barrenAttempts >= MAX_BARREN_ATTEMPTS) return;
        reconnectTimer = setTimeout(connect, retryDelayMs);
        retryDelayMs = Math.min(retryDelayMs * 2, MAX_RETRY_DELAY_MS);
      };
    };
    connect();

    const onData = term.onData((data) => send({ type: "input", data: bytesToB64(encoder.encode(data)) }));
    // Non-UTF-8 input (some mouse reports) arrives as a latin-1 string.
    const onBinary = term.onBinary((data) => {
      send({ type: "input", data: bytesToB64(Uint8Array.from(data, (c) => c.charCodeAt(0))) });
    });
    const onResize = term.onResize(({ cols, rows }) => send({ type: "resize", cols, rows }));

    return () => {
      cancelled = true;
      clearTimeout(reconnectTimer);
      onData.dispose();
      onBinary.dispose();
      onResize.dispose();
      ws?.close();
    };
  }, [ptyId, term, resolveTerminalSession, setTerminalAlive]);
}
