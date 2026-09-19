// Turns docs/e2e/results.json (written by e2e.mjs) into docs/e2e/REPORT.md.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const OUT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../docs/e2e");
const results = JSON.parse(fs.readFileSync(path.join(OUT, "results.json"), "utf8"));
const passed = results.filter((r) => r.pass).length;
const flows = {
  E01: "Board & first launch", E02: "Tasks", E03: "Unassigned tray", E04: "Assigning", E05: "Assigning", E06: "Assigning",
  E07: "Assigning", E08: "Workflow", E09: "Workflow", E10: "Task cards", E11: "Waiting / approvals", E12: "Waiting / approvals",
  E13: "Task cards", E14: "Auto rollup", E15: "Session overlay", E16: "Session overlay", E17: "Deleting", E18: "Deleting",
  E19: "Backend", E20: "Backend", E21: "Search", E22: "Tray", E23: "Edge cases", E24: "Migration", E25: "Visual fidelity",
};
const secs = (ms) => (ms / 1000).toFixed(1) + "s";

let md = `# SessionBoard Kanban — end-to-end report

**Branch:** \`sessionBoard-Kanban\` · **Result: ${passed}/${results.length} user stories pass**

Every story below was run by a script driving **real Chrome** against the **real Core Engine** (\`dev_server\`, the same \`bootstrap::start()\` as the app) and the real React frontend. Sessions are not mocked: they are created by replaying real-shaped Claude Code hook events (\`session-start\`, \`pre-tool-use\`, \`permission-request\`, \`stop\`, \`session-end\`) over the engine's Unix socket and by writing fixture transcripts, subagent files and \`~/.claude/tasks\` plans where Claude Code would. Each story starts from a fresh, isolated data directory (own \`HOME\`, own API port, short idle TTLs, deterministic fake Ollama), so nothing touches your real SessionBoard data. Screenshots are the actual frames from those runs.

Re-run: see "How to re-run" at the bottom.

## Summary

| ID | Flow | User story | Result | Time |
|---|---|---|---|---|
`;
for (const r of results) md += `| ${r.id} | ${flows[r.id] ?? ""} | ${r.title} | ${r.pass ? "✅ pass" : "❌ FAIL"} | ${secs(r.ms)} |\n`;

md += `\n## Visual check against the design (E25)\n\nThe populated live board (left) next to the design prototype's view **1a** (right). Same layout, tokens, cards, tray, WIP badge, needs-you glow. Deliberate differences: no fake traffic-light title bar (the real window chrome is native), data is real (so token/cost numbers differ), and a task's tool/project label is derived from its sessions (hidden while a task has none).\n\n<table><tr><td><img src="E25-live-board.png" width="520"></td><td><img src="E25-design-reference.png" width="380"></td></tr></table>\n`;

md += `\n## Stories in detail\n`;
for (const r of results) {
  md += `\n### ${r.id} — ${r.title}\n\n**${r.pass ? "PASS" : "FAIL"}**${r.error ? ` — ${r.error}` : ""}\n\n`;
  if (r.checks?.length) md += r.checks.map((c) => `- ✔ ${c}`).join("\n") + "\n\n";
  if (r.note) md += `_${r.note}_\n\n`;
  for (const f of r.shots) md += `<img src="${f}" width="640"> \n\n`;
}

md += `
## Bugs the E2E run found in the new code (all fixed, stories re-run)

1. **Phantom FLIP moves broke drag-and-drop.** \`useFlip\` measured element rects while a previous FLIP animation was still in flight, so its transform leaked into the "before" position and the next render animated elements that hadn't really moved — cards drifted under the cursor and a session dropped onto another task silently did nothing. Fixed: rects are now measured with in-flight animations cancelled, and interrupted animations continue from where they visibly were. (Found by E06.)
2. **Drag handlers depended on React having re-rendered.** Dragover/drop read the drag from the store, which is published one tick after \`dragstart\` (mutating the DOM inside \`dragstart\` can make Chrome abort the drag). A fast drop could land before the render. Fixed with a synchronous \`getDrag()\` used by all handlers; the store copy only drives visuals. (E05–E08.)
3. **Empty tray pushed the whole board down mid-drag.** Showing the tray on drag start shifted every card under the cursor. Fixed: with no orphans it now floats over the top of the columns (no layout shift) so there is always somewhere to drop a session to unassign it. (E06/E07.)
4. **CSS keyframes silently did nothing.** CSS modules localize \`animation-name\`, so \`animation: breathe\` referenced a hashed name that didn't exist — the working-dot pulse, needs-you glow and overlay entrance never ran (the same was true of the pre-existing \`StatusPill\` dot). Fixed by declaring the keyframes in each module that uses them. (E03/E11.)
5. **Assign menu was 224px, not the specced 210px** (content-box + padding) and the overlay panel 450px, not 448px. Fixed with \`box-sizing: border-box\`. (E04/E15.)
6. Design-fidelity fixes from the E25 comparison: the "Needs you" pill wrapped onto two lines; the add-task footer belongs directly under the last card (it now scrolls with the column) rather than pinned to the window bottom; a meaningless "0" token count and "—" project label are hidden until there is data.

## Additions beyond the design handoff (worth a look)

- **Delete task** (✕ on hover, top-right of a card). The handoff has no way to remove a task; its sessions return to the tray.
- **Floating tray while dragging** (see bug 3) so unassign-by-drag works even when the tray is otherwise hidden.
- **Filter box + idle toggle kept** from the previous board in the title bar (the design only shows the status pill and Ask memory).
- Window default enlarged to 1240×820 (was 800×600) so four columns fit.

## Not covered / known gaps

- **The native Tauri window** is not driven (no tool can); the frontend is exercised in Chrome. "Jump into this session" is verified for enabled/disabled state only — actually launching Terminal needs the Tauri \`invoke\` bridge.
- **Real Ollama** is not used in E2E (fake gives deterministic titles = first four prompt words). The unparseable-output fallback is unit-tested; a real \`qwen2.5\` title is not asserted.
- **"N sessions queued"** on empty tasks (a mock-only \`planned\` field in the prototype) is not implemented — there is no real data behind it.
- Token/cost figures on a session appear after its first \`stop\` hook (pre-existing behaviour), so brand-new sessions show none.
- A running \`npm run tauri dev\` on this checkout hot-reloads to this branch when the working tree is switched — see the hand-off notes.

## How to re-run

\`\`\`bash
npm i --no-save playwright-core            # once
echo "VITE_API_PORT=37999" > .env.e2e.local
npx vite --port 1430 --strictPort --mode e2e &   # scratch frontend pointed at the scratch backend
PW_PATH=$PWD/node_modules/playwright-core/index.mjs node scripts/e2e/e2e.mjs   # ONLY=E05,E06 to filter
node scripts/e2e/report.mjs
\`\`\`
`;
fs.writeFileSync(path.join(OUT, "REPORT.md"), md);
console.log("wrote", path.join(OUT, "REPORT.md"));
