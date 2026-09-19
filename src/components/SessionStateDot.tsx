import { STATE_COLOR_VAR, STATE_GLOW_VAR } from "../styles/sessionStyle";
import type { SessionState } from "../types";
import styles from "./Kanban.module.css";

/** Shared by session rows, tray chips: Working breathes, Waiting glows, others plain. */
export function SessionStateDot({ state }: { state: SessionState }) {
  return (
    <span
      className={`${styles.stateDot} ${state === "Working" ? styles.stateDotWorking : ""}`}
      style={{
        background: STATE_COLOR_VAR[state],
        boxShadow: state === "Working" || state === "Waiting" ? `0 0 9px ${STATE_GLOW_VAR[state]}` : undefined,
      }}
      data-state={state}
    />
  );
}
