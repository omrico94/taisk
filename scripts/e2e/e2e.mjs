// End-to-end run of every user story against a REAL Core Engine (dev_server, isolated HOME,
// deterministic fake Ollama) and the REAL frontend, driven in real Chrome via playwright-core.
// Sessions are created by replaying real-shaped hook events (scripts/e2e-seed.py).
//
//   npm i --no-save playwright-core        (or PW_PATH=/path/to/playwright-core/index.mjs)
//   scripts/e2e/e2e.mjs needs: vite on :1430 (--mode e2e, VITE_API_PORT=37999); see docs/e2e/README
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const { chromium } = await import(process.env.PW_PATH ?? "playwright-core");
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const OUT = path.join(ROOT, "docs/e2e");
const HOME = process.env.E2E_HOME ?? "/tmp/sbe2e";
const PORT = process.env.PORT ?? "37999";
const API = `http://127.0.0.1:${PORT}`;
const UI = process.env.UI ?? "http://localhost:1430";
const ONLY = process.env.ONLY?.split(",");

const sh = (cmd, args, env = {}) =>
  spawnSync(cmd, args, { env: { ...process.env, HOME, ...env }, encoding: "utf8" });
const server = (op, env = {}) => {
  const r = sh(path.join(ROOT, "scripts/e2e-run.sh"), op.split(" "), { E2E_HOME: HOME, PORT, HOME: process.env.HOME, ...env });
  if (r.status !== 0) throw new Error(`e2e-run ${op}: ${r.stdout}${r.stderr}`);
};
const seed = (...args) => {
  const r = sh("python3", [path.join(ROOT, "scripts/e2e-seed.py"), ...args]);
  if (r.status !== 0) throw new Error(`seed ${args.join(" ")}: ${r.stderr || r.stdout}`);
};
const api = async (p, method = "GET", body) => {
  const r = await fetch(API + p, { method, headers: { "Content-Type": "application/json" }, body: body ? JSON.stringify(body) : undefined });
  const t = await r.text();
  return { status: r.status, json: t ? (() => { try { return JSON.parse(t); } catch { return t; } })() : null };
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let lastStep = "";
const until = async (fn, msg, ms = 8000) => {
  lastStep = "until: " + msg;
  const t0 = Date.now();
  for (;;) {
    try { const v = await fn(); if (v) return v; } catch {}
    if (Date.now() - t0 > ms) throw new Error("timeout: " + msg);
    await sleep(120);
  }
};
let checks = [];
const ok = (c, msg) => { if (!c) throw new Error("assert: " + msg); checks.push(msg); };

const browser = await chromium.launch({ channel: "chrome", headless: true });
let ctx, page, requests;
async function open({ width = 1240, height = 820, reduced = false } = {}) {
  await ctx?.close();
  ctx = await browser.newContext({ viewport: { width, height }, reducedMotion: reduced ? "reduce" : "no-preference" });
  page = await ctx.newPage();
  page.setDefaultTimeout(8000);
  requests = [];
  page.on("request", (r) => requests.push(`${r.method()} ${new URL(r.url()).pathname}`));
  page.consoleErrors = [];
  page.on("pageerror", (e) => page.consoleErrors.push(String(e)));
  await page.goto(UI);
  await page.getByTestId("column-todo").waitFor();
}
async function fresh(env = {}, viewport) {
  server("stop");
  server("start --fresh", env);
  await open(viewport);
}
const shot = async (id, name) => {
  await sleep(450); // let entrance animations (overlay scale-in) finish so frames aren't mid-fade
  const f = `${id}-${name}.png`;
  await page.screenshot({ path: path.join(OUT, f) });
  return f;
};

// ---- board helpers
const card = (title) => page.getByTestId("task-card").filter({ hasText: title });
const col = (stage) => page.getByTestId(`column-${stage}`);
const colTitles = (stage) => col(stage).getByTestId("task-card").locator("[class*=taskTitle]").allInnerTexts();
async function addTask(stage, title) {
  await col(stage).getByText("+ Add task").click();
  await col(stage).getByLabel("New task title").fill(title);
  await page.keyboard.press("Enter");
  await card(title).waitFor();
}
async function mk(id, prompt, { cwd = "/work/api-gateway", tokens = 1200, ctx: c = 30000, entrypoint } = {}) {
  const extra = entrypoint ? ["--entrypoint", entrypoint] : [];
  seed("start", id, cwd, prompt, "--tokens", String(tokens), "--ctx", String(c), ...extra);
  await until(async () => (await api("/sessions")).json.some((s) => s.id === id && s.desc !== "Starting…"), `session ${id} summarized`);
  await page.locator(`[data-session-id="${id}"]`).first().waitFor();
}
const sess = async (id) => (await api("/sessions")).json.find((s) => s.id === id);
const chip = (id) => page.locator(`[data-testid=tray-chip][data-session-id="${id}"]`);
const row = (id) => page.locator(`[data-testid=session-row][data-session-id="${id}"]`);
const assignVia = async (id, taskTitle) => {
  await chip(id).getByLabel("Assign to a task").click();
  await page.getByTestId("assign-menu").getByRole("menuitem", { name: new RegExp(taskTitle) }).click();
};
const taskId = async (title) => (await api("/tasks")).json.tasks.find((t) => t.title === title)?.id;
const assignApi = (sid, tid) => api(`/sessions/${sid}/task`, "PUT", { task_id: tid });
const stageOf = async (title) => (await api("/tasks")).json.tasks.find((t) => t.title === title)?.stage;
// Manual mouse drag: real pointer events drive Chrome's native HTML5 DnD (locator.dragTo hangs on it).
// Move onto a target and hover there like a person would: Chrome dispatches `dragover` on a timer,
// so releasing the instant the pointer arrives can drop on whatever the *previous* dragover hit.
async function glide(x, y, steps = 12) {
  await page.mouse.move(x, y, { steps });
  await sleep(150);
  await page.mouse.move(x + 1, y + 1);
  await sleep(150);
}
async function settle() {
  await until(() => page.evaluate(() => document.getAnimations().every((a) => a instanceof CSSAnimation || a instanceof CSSTransition)), "FLIP animations settled");
}
async function drag(from, to) {
  lastStep = "drag start";
  await settle();
  lastStep = "drag measuring";
  const a = await from.boundingBox(), b = await to.boundingBox();
  await page.mouse.move(a.x + 20, a.y + a.height / 2);
  await page.mouse.down();
  await page.mouse.move(a.x + 50, a.y + a.height / 2 + 30, { steps: 4 });
  await glide(b.x + b.width / 2, b.y + b.height / 2);
  if (process.env.DEBUG_SHOT) await page.screenshot({ path: "/tmp/sbe2e-tools/mid.png" });
  lastStep = "drag mouse.up";
  await page.mouse.up();
  lastStep = "drag done";
}
const overlay = () => page.getByTestId("session-overlay");

// ---- story runner
const results = [];
async function story(id, title, fn) {
  if (ONLY && !ONLY.includes(id)) return;
  const shots = [];
  checks = [];
  const s = (name) => shot(id, name).then((f) => shots.push(f));
  process.stdout.write(`${id} ${title} … `);
  const t0 = Date.now();
  try {
    await Promise.race([fn(s), new Promise((_, rej) => setTimeout(() => rej(new Error("story timed out after 90s at " + lastStep)), 90000))]);
    results.push({ id, title, pass: true, shots, checks: [...checks], ms: Date.now() - t0 });
    console.log("PASS");
  } catch (e) {
    await s("FAILED").catch(() => {});
    results.push({ id, title, pass: false, error: e.message, shots, checks: [...checks], ms: Date.now() - t0 });
    console.log("FAIL —", e.message);
  }
}

fs.mkdirSync(OUT, { recursive: true });
for (const f of fs.readdirSync(OUT)) if (f.endsWith(".png") && (!ONLY || ONLY.some((o) => f.startsWith(o + "-")))) fs.unlinkSync(path.join(OUT, f));

// =====================================================================================
await story("E01", "First launch, empty board", async (s) => {
  await fresh();
  for (const [k, l] of [["backlog", "Backlog"], ["todo", "To Do"], ["inprogress", "In Progress"], ["done", "Done"]]) {
    ok((await col(k).innerText()).toUpperCase().includes(l.toUpperCase()), `column ${l}`);
    ok((await page.getByTestId(`count-${k}`).innerText()) === "0", `count ${k} = 0`);
  }
  ok((await page.getByTestId("tray").count()) === 0, "tray hidden when there are no sessions");
  const pill = await page.locator("[class*=pill]").first().innerText();
  ok(/0\s*working/.test(pill.replace(/\n/g, " ")) && /0\s*need you/.test(pill.replace(/\n/g, " ")), `status pill: ${pill}`);
  ok((await page.getByTestId("wip-badge").innerText()) === "WIP 0/5", "WIP badge 0/5");
  await s("empty-board");
});

await story("E02", "Add task in each column (Enter commits, Esc cancels, empty discarded, persists)", async (s) => {
  await fresh();
  await addTask("backlog", "Backlog task");
  await addTask("todo", "Todo task");
  await addTask("inprogress", "Doing task");
  await addTask("done", "Finished task");
  // Escape cancels
  await col("todo").getByText("+ Add task").click();
  await col("todo").getByLabel("New task title").fill("Never saved");
  await page.keyboard.press("Escape");
  ok((await col("todo").getByLabel("New task title").count()) === 0, "composer closed on Esc");
  ok((await card("Never saved").count()) === 0, "Esc did not create a task");
  // Empty discarded
  await col("todo").getByText("+ Add task").click();
  await page.keyboard.press("Enter");
  ok((await page.getByTestId("count-todo").innerText()) === "1", "empty submit discarded");
  await s("four-tasks");
  await page.reload();
  await card("Doing task").waitFor();
  const stages = (await api("/tasks")).json.tasks.map((t) => `${t.title}:${t.stage}`).sort();
  ok(JSON.stringify(stages) === JSON.stringify(["Backlog task:backlog", "Doing task:inprogress", "Finished task:done", "Todo task:todo"]), `stages ${stages}`);
  ok((await colTitles("done")).includes("Finished task"), "persisted after reload");
  await s("after-reload");
});

await story("E03", "New sessions land in the Unassigned tray with the right state dots", async (s) => {
  await fresh();
  await mk("s1", "Refactor auth middleware to async");
  await mk("s2", "Write OpenAPI spec for gateway");
  await mk("s3", "Fix flaky payments webhook test");
  await mk("s4", "Backfill the revenue fact table");
  seed("permission", "s2");
  seed("stop", "s3");
  await until(async () => (await sess("s2")).state === "Waiting" && (await sess("s3")).state === "Done", "states applied");
  // Idle: make s4 done, then let the done-sweeper age it (see E22 for the sweep itself) — here just assert 3 states.
  ok((await page.getByTestId("tray-count").innerText()) === "4", "tray count 4");
  const states = await page.locator("[data-testid=tray-chip] [data-state]").evaluateAll((els) => els.map((e) => e.dataset.state));
  ok(states.includes("Working") && states.includes("Waiting") && states.includes("Done"), `dot states ${states}`);
  const dot = await chip("s2").locator("[data-state]").evaluate((e) => getComputedStyle(e).backgroundColor);
  ok(dot === "rgb(251, 191, 36)", `waiting dot amber, got ${dot}`);
  const w = await chip("s1").locator("[data-state]").evaluate((e) => getComputedStyle(e).animationName);
  ok(/breathe/.test(w), `working dot breathes, got ${w}`);
  ok((await page.locator("[class*=trayHint]").innerText()).includes("Drag onto a task"), "tray hint copy");
  await s("tray-states");
});

await story("E04", "Assign a session via the ▾ menu", async (s) => {
  await fresh();
  await addTask("inprogress", "Ship async auth");
  await addTask("todo", "Second task");
  await mk("s1", "Refactor auth middleware now");
  await mk("s2", "Second orphan session here");
  await chip("s1").getByLabel("Assign to a task").click();
  const menu = page.getByTestId("assign-menu");
  await menu.waitFor();
  ok((await menu.evaluate((e) => getComputedStyle(e).position)) === "fixed", "menu is position:fixed");
  ok((await menu.evaluate((e) => e.getBoundingClientRect().width)) === 210, "menu is 210px wide");
  ok((await menu.getByRole("menuitem").count()) === 2, "one row per task");
  await s("menu-open");
  await page.mouse.click(600, 600); // outside click closes
  ok((await menu.count()) === 0, "closes on outside click");
  await chip("s1").getByLabel("Assign to a task").click();
  await menu.waitFor();
  await page.keyboard.press("Escape");
  ok((await menu.count()) === 0, "closes on Escape");
  await assignVia("s1", "Ship async auth");
  await row("s1").waitFor();
  ok((await chip("s1").count()) === 0, "chip left the tray");
  ok((await card("Ship async auth").getByTestId("session-row").count()) === 1, "row in the task card");
  ok((await page.getByTestId("tray-count").innerText()) === "1", "tray count 1");
  ok((await api("/tasks")).json.assignments.s1, "assignment persisted in backend");
  await s("assigned");
});

await story("E05", "Assign by dragging a tray chip onto a task card (drop outline + FLIP)", async (s) => {
  await fresh();
  await addTask("inprogress", "Drop target task");
  await mk("s1", "Refactor auth middleware now");
  await mk("s2", "Another unassigned session");
  await settle();
  const src = await chip("s1").boundingBox();
  const dst = await card("Drop target task").boundingBox();
  await page.mouse.move(src.x + 20, src.y + src.height / 2);
  await page.mouse.down();
  await page.mouse.move(src.x + 60, src.y + 40, { steps: 5 });
  await glide(dst.x + dst.width / 2, dst.y + dst.height / 2);
  await until(async () => (await card("Drop target task").evaluate((e) => getComputedStyle(e).outlineStyle)) === "dashed", "card shows dashed drop outline while dragging over it");
  await s("dragging-over-card");
  await page.mouse.up();
  await row("s1").waitFor();
  const flip = await page.evaluate(() => document.getAnimations().filter((a) => !(a instanceof CSSAnimation) && !(a instanceof CSSTransition)).length);
  ok(flip > 0, `FLIP animation ran after the move (${flip})`);
  await s("just-dropped");
  ok((await chip("s1").count()) === 0 && (await api("/tasks")).json.assignments.s1, "assigned via drag");
});

await story("E06", "Move a session from one task to another by drag", async (s) => {
  await fresh();
  await addTask("inprogress", "Task A");
  await addTask("todo", "Task B");
  await mk("s1", "Session that will move");
  await mk("s2", "Session that stays put");
  await assignApi("s1", await taskId("Task A"));
  await assignApi("s2", await taskId("Task A"));
  await until(() => row("s1").count());
  await page.evaluate(() => { const seen = (window.__seen = {}); for (const ev of ["dragstart", "dragover", "drop", "dragend"]) document.addEventListener(ev, (e) => { const k = ev + "@" + (e.target.getAttribute?.("data-testid") || String(e.target.className).slice(0, 16)); seen[k] = (seen[k] || 0) + 1; }, true); });
  await drag(row("s1"), card("Task B"));
  await sleep(500);
  lastStep = "events " + JSON.stringify(await page.evaluate(() => window.__seen));
  await until(async () => (await api("/tasks")).json.assignments.s1 === (await taskId("Task B")), "s1 now in B; " + lastStep);
  ok((await card("Task A").getByTestId("session-row").count()) === 1, "A has 1 session");
  ok((await card("Task B").getByTestId("session-row").count()) === 1, "B has 1 session");
  ok((await card("Task B").innerText()).includes("1 session"), "B rollup label");
  await s("moved-between-tasks");
});

await story("E07", "Unassign via ↩ on the row, via the overlay button, and by dragging onto the tray", async (s) => {
  await fresh();
  await addTask("inprogress", "Task A");
  const tid = await taskId("Task A");
  for (const id of ["s1", "s2", "s3"]) await mk(id, `Unassign scenario ${id} prompt`);
  for (const id of ["s1", "s2", "s3"]) await assignApi(id, tid);
  await until(() => row("s3").count());
  ok((await page.getByTestId("tray").count()) === 0, "tray hidden with no orphans");
  await row("s1").getByLabel("Unassign session").click();
  await chip("s1").waitFor();
  await row("s2").click();
  await overlay().waitFor();
  ok((await overlay().innerText()).includes("part of task"), "overlay names the task");
  await s("overlay-with-unassign");
  await overlay().getByRole("button", { name: /Unassign/ }).click();
  await chip("s2").waitFor();
  ok((await overlay().count()) === 0, "overlay closed after unassign");
  // drag row onto tray
  await drag(row("s3"), page.getByTestId("tray"));
  await chip("s3").waitFor();
  ok(Object.keys((await api("/tasks")).json.assignments).length === 0, "all three unassigned in backend");
  await s("all-unassigned");
});

await story("E08", "Drag a task card across every column (highlight, ghost opacity, persistence)", async (s) => {
  await fresh();
  await addTask("backlog", "Mover");
  const body = (k) => page.getByTestId(`column-body-${k}`);
  for (const k of ["todo", "inprogress", "done", "backlog"]) {
    await settle();
    const src = await card("Mover").boundingBox();
    const dst = await body(k).boundingBox();
    await page.mouse.move(src.x + 30, src.y + 20);
    await page.mouse.down();
    await page.mouse.move(src.x + 60, src.y + 40, { steps: 4 });
    await glide(dst.x + dst.width / 2, dst.y + 80, 10);
    if (k !== "backlog") await until(async () => (await body(k).evaluate((e) => getComputedStyle(e).boxShadow)).includes("inset"), `${k} column highlights while dragging over it`);
    if (k === "todo") {
      await until(async () => (await card("Mover").evaluate((e) => getComputedStyle(e).opacity)) === "0.4", "dragged card fades to opacity .4");
      await s("dragging-over-todo");
    }
    await page.mouse.up();
    await until(async () => (await stageOf("Mover")) === k, `stage ${k}`);
    await col(k).getByTestId("task-card").waitFor();
  }
  await page.reload();
  await card("Mover").waitFor();
  ok((await colTitles("backlog")).includes("Mover"), "final stage persisted after reload");
  await s("back-in-backlog");
});

await story("E09", "WIP badge turns red past the limit", async (s) => {
  await fresh();
  for (let i = 1; i <= 5; i++) await addTask("inprogress", `WIP task ${i}`);
  const badge = page.getByTestId("wip-badge");
  ok((await badge.innerText()) === "WIP 5/5", "5/5 at the limit");
  const c5 = await badge.evaluate((e) => getComputedStyle(e).color);
  ok(c5 !== "rgb(248, 113, 113)", "not red at the limit");
  await s("wip-at-limit");
  await addTask("inprogress", "WIP task 6");
  ok((await badge.innerText()) === "WIP 6/5", "6/5 over the limit");
  const c6 = await badge.evaluate((e) => getComputedStyle(e).color);
  ok(c6 === "rgb(248, 113, 113)", `red over the limit, got ${c6}`);
  await s("wip-over-limit");
});

await story("E10", "Task card rollup: progress, sums, session count, expand/collapse", async (s) => {
  await fresh();
  await addTask("inprogress", "Rollup task");
  const tid = await taskId("Rollup task");
  await mk("s1", "First session prompt words", { tokens: 5000 });
  await mk("s2", "Second session prompt words", { tokens: 7000 });
  await mk("s3", "Third session prompt words", { tokens: 3000 });
  for (const id of ["s1", "s2", "s3"]) await assignApi(id, tid);
  seed("stop", "s3");
  await until(async () => (await sess("s3")).state === "Done");
  const c = card("Rollup task");
  await until(async () => (await c.innerText()).includes("1/3 done"), "label 1/3 done");
  const t = await c.innerText();
  ok(t.includes("3 sessions"), "3 sessions label");
  const all = (await api("/sessions")).json;
  const tok = all.reduce((a, x) => a + x.tokens, 0), cost = all.reduce((a, x) => a + x.cost, 0);
  ok(t.includes(`$${cost.toFixed(2)}`), `cost sum $${cost.toFixed(2)} in: ${t.replace(/\n/g, " | ")}`);
  ok(tok > 0 && (t.includes("K") || t.includes(String(tok))), "token sum shown");
  const fill = await c.locator("[class*=rollFill]").evaluate((e) => e.style.width);
  ok(fill === "33%", `bar 33%, got ${fill}`);
  await s("expanded");
  const h1 = await c.locator("[class*=sessionListInner]").evaluate((e) => e.getBoundingClientRect().height);
  await c.locator("[class*=taskTitle]").click(); // click card body toggles
  await until(async () => (await c.locator("[class*=sessionListInner]").evaluate((e) => e.getBoundingClientRect().height)) < 2, "collapsed");
  ok(h1 > 40, "was expanded by default");
  await s("collapsed");
  await c.locator("[class*=taskTitle]").click();
  await until(async () => (await c.locator("[class*=sessionListInner]").evaluate((e) => e.getBoundingClientRect().height)) > 40, "expanded again");
  // done column shows green bar + dimmed card
  seed("stop", "s1"); seed("stop", "s2");
  await until(async () => (await stageOf("Rollup task")) === "done", "auto-moved to Done");
  await until(async () => (await colTitles("done")).includes("Rollup task"), "in Done column");
  ok((await card("Rollup task").evaluate((e) => getComputedStyle(e).opacity)) === "0.72", "Done card opacity .72");
  await s("done-card");
});

await story("E11", "Waiting flow: needs-you card, status pill, Approve / Reject / Send hit distinct endpoints", async (s) => {
  await fresh();
  await addTask("inprogress", "Approval task");
  const tid = await taskId("Approval task");
  for (const id of ["s1", "s2", "s3"]) await mk(id, `Needs approval scenario ${id}`);
  for (const id of ["s1", "s2", "s3"]) await assignApi(id, tid);
  seed("permission", "s1"); seed("permission", "s2"); seed("permission", "s3");
  await until(async () => (await sess("s3")).state === "Waiting", "all waiting");
  await card("Approval task").locator("[class*=needsPill]").waitFor();
  const cs = await card("Approval task").evaluate((e) => ({ b: getComputedStyle(e).borderColor, a: getComputedStyle(e).animationName }));
  ok(/waitGlow/.test(cs.a), `waitGlow animation, got ${cs.a}`);
  ok(/251, 191, 36/.test(cs.b), `amber border, got ${cs.b}`);
  ok((await page.locator("[class*=pill]").first().innerText()).replace(/\n/g, " ").includes("3 need you"), "status pill counts 3 waiting");
  await s("needs-you");
  const paths = [];
  page.on("request", (r) => paths.push(`${r.method()} ${new URL(r.url()).pathname}`));
  // Approve
  await row("s1").click(); await overlay().waitFor();
  await s("overlay-waiting");
  await overlay().getByRole("button", { name: "Approve" }).click();
  await until(async () => (await sess("s1")).state === "Working", "s1 resumed by approve");
  // Reject
  await row("s2").click(); await overlay().waitFor();
  await overlay().getByRole("button", { name: "Reject" }).click();
  await until(async () => (await sess("s2")).state === "Working", "s2 resumed by reject");
  // Send reply
  await row("s3").click(); await overlay().waitFor();
  await overlay().locator("textarea").fill("please also add tests");
  await overlay().getByRole("button", { name: /Send/ }).click();
  await until(async () => (await sess("s3")).state === "Working", "s3 resumed by reply");
  ok((await sess("s3")).desc.includes("please also add tests"), "reply text reached the engine");
  ok(paths.includes("POST /sessions/s1/approve") && paths.includes("POST /sessions/s2/reject") && paths.includes("POST /sessions/s3/reply"), `three distinct endpoints: ${paths.filter((p) => p.startsWith("POST")).join(", ")}`);
  await until(async () => (await card("Approval task").locator("[class*=needsPill]").count()) === 0, "needs-you pill cleared");
  await s("resolved");
});

await story("E12", "AskUserQuestion pre-tool-use special case → Waiting", async (s) => {
  await fresh();
  await mk("s1", "Ask me something please");
  seed("ask", "s1");
  await until(async () => (await sess("s1")).state === "Waiting", "waiting");
  await until(async () => (await chip("s1").locator("[data-state]").getAttribute("data-state")) === "Waiting", "chip shows waiting");
  await s("ask-user-question");
});

await story("E13", "Working / Done visual treatments", async (s) => {
  await fresh();
  await addTask("inprogress", "Live task");
  await addTask("done", "Old task");
  await mk("s1", "Currently working session"); await mk("s2", "Finished long ago session");
  await assignApi("s1", await taskId("Live task")); await assignApi("s2", await taskId("Old task"));
  seed("stop", "s2");
  await until(async () => (await sess("s2")).state === "Done");
  const live = await card("Live task").evaluate((e) => getComputedStyle(e).boxShadow);
  ok(live.includes("56, 189, 248"), `cyan glow on a working task: ${live}`);
  ok((await card("Old task").evaluate((e) => getComputedStyle(e).opacity)) === "0.72", "done task dimmed");
  ok((await row("s2").locator("[data-state]").evaluate((e) => getComputedStyle(e).backgroundColor)) === "rgb(52, 211, 153)", "done dot green");
  await s("working-and-done");
});

await story("E14", "Auto rollup: all sessions Done → task moves to Done; a live session pulls it back", async (s) => {
  await fresh();
  await addTask("inprogress", "Auto task");
  const tid = await taskId("Auto task");
  await mk("s1", "Rollup session number one"); await mk("s2", "Rollup session number two");
  await assignApi("s1", tid); await assignApi("s2", tid);
  seed("stop", "s1");
  await sleep(600);
  ok((await stageOf("Auto task")) === "inprogress", "one done, one working → stays In Progress");
  seed("stop", "s2");
  await until(async () => (await colTitles("done")).includes("Auto task"), "UI moved the card to Done live");
  await s("moved-to-done");
  seed("work", "s1");
  await until(async () => (await colTitles("inprogress")).includes("Auto task"), "pulled back to In Progress live");
  ok((await stageOf("Auto task")) === "inprogress", "backend agrees");
  await s("pulled-back");
  // dropping a live session onto a Done task
  await addTask("done", "Closed task");
  await mk("s3", "Late arriving live session");
  await assignApi("s3", await taskId("Closed task"));
  await until(async () => (await stageOf("Closed task")) === "inprogress", "Done task with a live session → In Progress");
});

await story("E15", "Session overlay: identity, stats, ctx colours, subagents, plan, transcript, close paths", async (s) => {
  await fresh();
  await addTask("inprogress", "Overlay task");
  const tid = await taskId("Overlay task");
  await mk("lo", "Low context session here", { ctx: 100000 });   // 50% cyan
  await mk("mid", "Mid context session here", { ctx: 150000 });  // 75% amber
  await mk("hi", "High context session here", { ctx: 190000 });  // 95% red
  for (const id of ["lo", "mid", "hi"]) await assignApi(id, tid);
  for (const [id, c] of [["lo", 100000], ["mid", 150000], ["hi", 190000]]) { seed("usage", id, "1500", String(c)); seed("work", id); }
  await until(async () => (await sess("hi")).ctx_max > 0 && (await sess("lo")).ctx_max > 0 && (await sess("mid")).ctx_max > 0, "metrics present");
  seed("sub", "hi", "researcher", "Investigate flaky tests", "Look at CI history for flaky tests");
  seed("plan", "hi", "Write failing test:done", "Fix the bug:done", "Refactor helper:todo");
  await until(async () => { const v = await sess("hi"); return v.subs.length === 1 && v.plan; }, "subagent+plan visible");
  const fillOf = async (id) => {
    await row(id).click(); await overlay().waitFor();
    const f = await overlay().locator("[class*=ctxFill]").evaluate((e) => ({ w: e.style.width, bg: e.style.background }));
    await page.keyboard.press("Escape");
    await until(async () => (await overlay().count()) === 0, "overlay closed");
    return f;
  };
  const lo = await fillOf("lo"), mid = await fillOf("mid"), hi = await fillOf("hi");
  ok(lo.bg.includes("state-working") && lo.w === "50%", `cyan <70%: ${JSON.stringify(lo)}`);
  ok(mid.bg.includes("state-waiting") && mid.w === "75%", `amber 70-89%: ${JSON.stringify(mid)}`);
  ok(hi.bg.includes("state-danger") && hi.w === "95%", `red ≥90%: ${JSON.stringify(hi)}`);
  await row("hi").click(); await overlay().waitFor();
  await overlay().locator("[class*=transcriptText]").first().waitFor();
  const t = await overlay().innerText();
  ok(/1 subagent/i.test(t) && t.includes("Investigate flaky"), "subagents section; overlay text: " + t.replace(/\n/g, " | ").slice(0, 400));
  ok(t.includes("2/3") && t.includes("Write failing test") && t.includes("Refactor helper"), "plan card with 2/3");
  ok(/transcript tail/i.test(t) && t.includes("On it") && /\byou\b/i.test(t), "transcript tail rows: " + t.slice(-300).replace(/\n/g, " | "));
  ok(/tokens/i.test(t) && /cost/i.test(t) && /context window/i.test(t), "stats card");
  ok(t.includes("part of task") && t.includes("Overlay task"), "part-of-task caption");
  const geo = await overlay().locator("[role=dialog]").evaluate((e) => ({ w: e.offsetWidth, r: getComputedStyle(e).borderRadius }));
  ok(geo.w === 448 && geo.r === "20px", `panel 448px/20px radius, got ${JSON.stringify(geo)}`);
  await s("overlay-full");
  await overlay().locator("[role=dialog]").evaluate((e) => (e.scrollTop = 9999));
  await s("overlay-scrolled");
  // close: ✕, scrim, Esc
  await overlay().getByRole("button", { name: "✕" }).click(); ok((await overlay().count()) === 0, "closes via ✕");
  await row("hi").click(); await overlay().waitFor();
  await page.mouse.click(10, 10); await until(async () => (await overlay().count()) === 0, "closes via scrim");
  await row("hi").click(); await overlay().waitFor();
  await page.keyboard.press("Escape"); await until(async () => (await overlay().count()) === 0, "closes via Esc");
  // tray chip title also opens it
  await api(`/sessions/hi/task`, "PUT", { task_id: null });
  await chip("hi").locator("[class*=chipTitle]").click(); await overlay().waitFor();
  ok(!(await overlay().innerText()).includes("part of task"), "no caption for an unassigned session");
});

await story("E16", "Jump into session: enabled for CLI, disabled for Desktop", async (s) => {
  await fresh();
  await mk("cli", "A plain terminal session");
  await mk("dsk", "A Claude Desktop session", { entrypoint: "claude-desktop" });
  seed("stop", "cli"); seed("stop", "dsk");
  await until(async () => (await sess("dsk")).state === "Done", "done");
  await chip("cli").locator("[class*=chipTitle]").click(); await overlay().waitFor();
  const jump = overlay().getByRole("button", { name: /Jump into this session/ });
  ok(await jump.isEnabled(), "CLI session: jump enabled");
  await s("jump-enabled");
  await page.keyboard.press("Escape");
  await chip("dsk").locator("[class*=chipTitle]").click(); await overlay().waitFor();
  ok(await overlay().getByRole("button", { name: /Jump into this session/ }).isDisabled(), "Desktop session: jump disabled");
  await s("jump-disabled");
});

await story("E17", "Delete a session from the row, the chip and the overlay — and it stays gone after restart", async (s) => {
  await fresh();
  await addTask("inprogress", "Delete scenarios");
  const tid = await taskId("Delete scenarios");
  await mk("s1", "Deleted from row session"); await mk("s2", "Deleted from chip session"); await mk("s3", "Deleted from overlay session"); await mk("s4", "Survivor session stays here");
  await assignApi("s1", tid); await assignApi("s3", tid);
  await row("s1").getByLabel("Delete session").click();
  await until(async () => (await row("s1").count()) === 0, "row delete");
  await chip("s2").getByLabel("Delete session").click();
  await until(async () => (await chip("s2").count()) === 0, "chip delete");
  await row("s3").click(); await overlay().waitFor();
  await overlay().getByRole("button", { name: "Delete session" }).click();
  ok((await overlay().getByRole("button", { name: "Confirm delete" }).count()) === 1, "overlay delete is two-click (arm)");
  await s("overlay-confirm-delete");
  await overlay().getByRole("button", { name: "Confirm delete" }).click();
  await until(async () => (await overlay().count()) === 0 && (await row("s3").count()) === 0, "overlay delete closes + removes");
  ok(!(await api("/tasks")).json.assignments.s1, "assignment dropped with the session");
  server("stop"); server("start");
  await page.reload();
  await until(async () => (await api("/sessions")).json.some((x) => x.id === "s4"), "survivor reconstructed");
  const ids = (await api("/sessions")).json.map((x) => x.id).sort();
  ok(JSON.stringify(ids) === '["s4"]', `only the survivor after restart, got ${ids}`);
  await chip("s4").waitFor();
  await s("after-restart");
});

await story("E18", "Delete a task: its sessions return to the tray", async (s) => {
  await fresh();
  await addTask("todo", "Doomed task");
  await addTask("todo", "Safe task");
  await mk("s1", "Session in doomed task"); await mk("s2", "Session in safe task");
  await assignApi("s1", await taskId("Doomed task")); await assignApi("s2", await taskId("Safe task"));
  await row("s1").waitFor();
  await card("Doomed task").hover();
  await s("hover-delete-button");
  await card("Doomed task").getByTestId("delete-task").click();
  await until(async () => (await card("Doomed task").count()) === 0, "task removed");
  await chip("s1").waitFor();
  ok((await row("s2").count()) === 1, "other task untouched");
  ok(!(await api("/tasks")).json.assignments.s1, "assignment dropped");
  await s("after-delete");
});

await story("E19", "Persistence across a backend restart", async (s) => {
  await fresh();
  await addTask("inprogress", "Persistent task");
  await addTask("done", "Persistent done task");
  await mk("s1", "Persistent session one"); await mk("s2", "Persistent session two"); await mk("s3", "Orphan across restart");
  await assignApi("s1", await taskId("Persistent task")); await assignApi("s2", await taskId("Persistent done task"));
  seed("end", "s2");
  await until(async () => (await sess("s2")).state === "Done");
  await s("before-restart");
  server("stop"); server("start");
  await page.reload();
  await until(async () => (await api("/sessions")).json.length === 3, "sessions reconstructed");
  await row("s1").waitFor();
  ok((await colTitles("inprogress")).includes("Persistent task") && (await colTitles("done")).includes("Persistent done task"), "tasks in their stages");
  ok((await card("Persistent task").getByTestId("session-row").count()) === 1, "s1 still attached");
  ok((await card("Persistent done task").getByTestId("session-row").count()) === 1, "s2 still attached");
  ok((await chip("s3").count()) === 1, "s3 still an orphan");
  ok((await sess("s2")).state === "Done", "ended session reconstructs as Done");
  ok((await stageOf("Persistent done task")) === "done", "done task not bounced by reconstruction");
  await s("after-restart");
});

await story("E20", "WebSocket reconnect: board refetches and stays consistent", async (s) => {
  await fresh();
  await addTask("todo", "Before outage");
  server("stop");
  await sleep(1500);
  // While the backend is down the page keeps its last state.
  ok((await card("Before outage").count()) === 1, "stale state kept while down");
  await s("backend-down");
  server("start");
  await api("/tasks", "POST", { title: "Created during reconnect", stage: "backlog" });
  await mk_after_restart();
  async function mk_after_restart() {
    await until(async () => (await card("Created during reconnect").count()) === 1, "new task appears with no manual reload", 25000);
  }
  ok((await card("Before outage").count()) === 1, "old task still there (persisted)");
  await s("reconnected");
});

await story("E21", "⌘K Ask memory and the toolbar filter stay independent; no category field", async (s) => {
  await fresh();
  await addTask("inprogress", "Auth work");
  await addTask("todo", "Billing work");
  await mk("s1", "Refactor auth middleware now"); await mk("s2", "Billing invoice export feature");
  await assignApi("s1", await taskId("Auth work")); await assignApi("s2", await taskId("Billing work"));
  await row("s2").waitFor();
  const filter = page.getByPlaceholder(/Filter sessions/);
  await filter.fill("auth");
  await until(async () => (await card("Billing work").count()) === 0, "filter narrowed");
  ok((await card("Auth work").count()) === 1, "matching task kept");
  await s("filtered");
  await page.keyboard.press("ControlOrMeta+k");
  const askInput = page.getByPlaceholder(/Search everything you've ever asked/);
  await askInput.waitFor();
  ok((await askInput.inputValue()) === "", "Ask overlay input independent of the toolbar filter");
  await askInput.fill("billing");
  await page.locator("[class*=snippet]").first().waitFor();
  ok((await page.locator("[class*=snippet]").allInnerTexts()).some((t) => /billing/i.test(t)), "Ask overlay shows a billing memory");
  ok((await filter.inputValue()) === "auth", "toolbar filter untouched by Ask typing");
  const results = await api("/search?q=billing");
  ok(Array.isArray(results.json) && results.json.length >= 1 && !("category" in results.json[0]), `search results have no category: ${JSON.stringify(Object.keys(results.json[0] ?? {}))}`);
  ok(!/category/i.test(await page.locator("body").innerText()), "no 'category' text anywhere in the UI");
  await s("ask-memory");
  await page.keyboard.press("Escape");
  await filter.fill("zzzz-no-match");
  await page.getByText("Search your full history instead").waitFor();
  await s("no-results");
});

await story("E22", "Hide-idle toggle for orphan sessions", async (s) => {
  await fresh({ DONE: "2", SWEEP: "1" });
  await mk("s1", "Will go idle soon enough"); await mk("s2", "Stays working the whole time");
  seed("stop", "s1");
  await until(async () => (await sess("s1")).state === "Idle", "aged to Idle", 15000);
  await until(async () => (await chip("s1").count()) === 0, "idle orphan hidden by default");
  const toggle = page.getByRole("button", { name: /1 idle hidden/ });
  await toggle.waitFor();
  await s("idle-hidden");
  await toggle.click();
  await chip("s1").waitFor();
  ok((await page.getByRole("button", { name: /Hide 1 idle/ }).count()) === 1, "toggle relabelled");
  await s("idle-shown");
});

await story("E23", "Edge cases: long titles, many sessions, empty task, horizontal scroll, reduced motion", async (s) => {
  await fresh();
  await addTask("inprogress", "A task with a really really long title that should wrap nicely across multiple lines without breaking the layout of the card at all");
  await addTask("todo", "Empty task");
  ok((await card("Empty task").innerText()).includes("No sessions yet"), "empty task copy");
  const tid = await taskId("A task with a really really long title that should wrap nicely across multiple lines without breaking the layout of the card at all");
  const long = "Extraordinarily_long_unbroken_session_title_that_must_be_truncated_with_an_ellipsis_rather_than_overflow";
  await mk("long", long);
  await assignApi("long", tid);
  for (let i = 1; i <= 11; i++) { await mk(`m${i}`, `Bulk session number ${i} prompt`); await assignApi(`m${i}`, tid); }
  await until(async () => (await page.locator("[data-testid=session-row]").count()) === 12, "12 rows");
  const el = await row("long").locator("[class*=sessionTitle]").evaluate((e) => ({ over: e.scrollWidth > e.clientWidth, ell: getComputedStyle(e).textOverflow }));
  ok(el.over && el.ell === "ellipsis", `long session title truncates: ${JSON.stringify(el)}`);
  const fits = await card("A task with a really").evaluate((e) => e.scrollWidth <= e.clientWidth + 1);
  ok(fits, "card content does not overflow horizontally");
  const scrolls = await page.getByTestId("column-body-inprogress").evaluate((e) => e.scrollHeight > e.clientHeight);
  ok(scrolls, "column body scrolls with many sessions");
  await s("many-sessions");
  // horizontal scroll at a narrow width
  await page.setViewportSize({ width: 700, height: 820 });
  const hs = await page.locator("[class*=columns]").evaluate((e) => e.scrollWidth > e.clientWidth);
  ok(hs, "columns row scrolls horizontally when narrow");
  await s("narrow");
  await page.setViewportSize({ width: 1240, height: 820 });
  // reduced motion: no FLIP animations
  await open({ reduced: true });
  await addTask("backlog", "Reduced motion mover");
  const src = await card("Reduced motion mover").boundingBox(); const dst = await page.getByTestId("column-body-done").boundingBox();
  await page.mouse.move(src.x + 30, src.y + 20); await page.mouse.down(); await glide(dst.x + 60, dst.y + 60, 10); await page.mouse.up();
  await until(async () => (await stageOf("Reduced motion mover")) === "done");
  const flips = await page.evaluate(() => document.getAnimations().filter((a) => !(a instanceof CSSAnimation) && !(a instanceof CSSTransition)).length);
  ok(flips === 0, `prefers-reduced-motion: no FLIP animations (${flips})`);
});

await story("E24", "Category removal regression + legacy data dir migrates", async (s) => {
  await fresh();
  ok((await api("/categories")).status === 404, "GET /categories gone");
  ok((await api("/categories", "POST", { name: "x" })).status === 404, "POST /categories gone");
  ok([404, 405].includes((await api("/sessions/x/recategorize", "POST", { category: "x" })).status), "recategorize gone");
  await mk("s1", "Anything at all for search");
  ok(!("category" in (await sess("s1"))), "SessionView has no category");
  await page.locator("[class*=chip]").first().waitFor();
  await s("no-category-ui");
  // legacy: boot the new backend on a copy of the REAL pre-Kanban data directory
  const legacy = "/tmp/sbe2e-legacy";
  const realData = path.join(process.env.HOME, "Library/Application Support/SessionBoard");
  fs.rmSync(legacy, { recursive: true, force: true });
  const dest = path.join(legacy, "Library/Application Support/SessionBoard");
  fs.mkdirSync(dest, { recursive: true });
  const cp = spawnSync("rsync", ["-a", "--exclude", "engine.sock", "--exclude", "tasks.json", realData + "/", dest + "/"]);
  ok(cp.status === 0, "copied the real data dir (read-only copy; the original is untouched)");
  const tables = fs.readdirSync(path.join(dest, "lancedb"));
  server("stop");
  server("start", { E2E_HOME: legacy, PORT: "37998" });
  const r = await fetch("http://127.0.0.1:37998/sessions").then((x) => x.json());
  ok(Array.isArray(r), "legacy data dir boots and serves /sessions");
  const sr = await fetch("http://127.0.0.1:37998/search?q=session").then((x) => x.json());
  ok(Array.isArray(sr), "search works against the legacy LanceDB");
  results.at(-1).note = `legacy dir tables: ${tables.join(", ")}; reconstructed ${r.length} live sessions; search returned ${sr.length} rows`;
  server("stop", { E2E_HOME: legacy, PORT: "37998" });
  fs.rmSync(legacy, { recursive: true, force: true });
});

await story("E25", "Visual check: populated board next to the design prototype (view 1a)", async (s) => {
  await fresh();
  const T = [["backlog", "Migrate logging to OpenTelemetry"], ["todo", "Payments webhook idempotency"], ["todo", "Onboarding flow redesign"],
    ["inprogress", "Ship async auth for the gateway"], ["inprogress", "Rebuild the analytics dashboard"], ["inprogress", "Q2 revenue model coverage"],
    ["done", "Staging environment bring-up"]];
  for (const [st, t] of T) await api("/tasks", "POST", { title: t, stage: st });
  const ids = {};
  for (const t of (await api("/tasks")).json.tasks) ids[t.title] = t.id;
  const S = [
    ["a1", "/work/api-gateway", "Auth middleware refactor", "Ship async auth for the gateway", 128000, 86000],
    ["a2", "/work/api-gateway", "OpenAPI 3.1 spec", "Ship async auth for the gateway", 22000, 19000],
    ["a3", "/work/api-gateway", "Auth unit tests", "Ship async auth for the gateway", 31000, 24000],
    ["b1", "/work/web-dashboard", "Chart rebuild with Recharts", "Rebuild the analytics dashboard", 64000, 52000],
    ["b2", "/work/web-dashboard", "Settings panel styling", "Rebuild the analytics dashboard", 39000, 28000],
    ["c1", "/work/data-pipeline", "dbt tests for fct_revenue", "Q2 revenue model coverage", 212000, 158000],
    ["d1", "/work/infra-terraform", "Provision staging VPC", "Staging environment bring-up", 98000, 71000],
    ["o1", "/work/payments-svc", "Investigate stripe retry storm", null, 41000, 30000],
    ["o2", "/work/web-dashboard", "Prototype command palette", null, 12000, 9000],
  ];
  for (const [id, cwd, prompt, task, tok, ctxv] of S) {
    seed("start", id, cwd, prompt, "--tokens", String(tok), "--ctx", String(ctxv));
    await until(async () => (await sess(id))?.desc !== "Starting…", `session ${id}`);
    if (task) await assignApi(id, ids[task]);
  }
  seed("usage", "a1", "128000", "86000"); seed("work", "a1"); seed("sub", "a1", "explorer", "Map the auth call sites", "Find every caller of the old auth middleware");
  seed("stop", "a3"); seed("stop", "d1"); seed("permission", "b1"); seed("stop", "a2");
  seed("work", "a2");
  await sleep(1500); await settle();
  await s("live-board");
  const DESIGN = process.env.DESIGN ?? "/private/tmp/claude-501/-Users-omricohen-Desktop/677a5e14-ad9e-4056-a96f-5b2d7f4fe79f/scratchpad/design/design_handoff_kanban_redesign/SessionBoard Kanban.dc.html";
  if (fs.existsSync(DESIGN)) {
    const dp = await ctx.newPage();
    await dp.setViewportSize({ width: 1400, height: 1100 });
    await dp.goto("file://" + DESIGN);
    await dp.waitForTimeout(1500);
    const el = dp.locator("#\\31 a, [id='1a']").first();
    if (await el.count()) await el.screenshot({ path: path.join(OUT, "E25-design-reference.png") });
    else await dp.screenshot({ path: path.join(OUT, "E25-design-reference.png") });
    await dp.close();
  }
});

// -------------------------------------------------------------------------------------
await ctx?.close();
await browser.close();
server("stop");
fs.writeFileSync(path.join(OUT, "results.json"), JSON.stringify(results, null, 2));
const failed = results.filter((r) => !r.pass);
console.log(`\n${results.length - failed.length}/${results.length} stories passed`);
process.exit(failed.length ? 1 : 0);
