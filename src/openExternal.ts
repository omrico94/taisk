import { openUrl } from "@tauri-apps/plugin-opener";

/** Opens `url` in the system browser; plain `window.open` when running outside Tauri (Vite dev / e2e). */
export function openExternal(url: string): void {
  openUrl(url).catch(() => window.open(url, "_blank", "noopener"));
}
