# SessionBoard Kanban — end-to-end report

**Branch:** `sessionBoard-Kanban` · **Result: 25/25 user stories pass**

Every story below was run by a script driving **real Chrome** against the **real Core Engine** (`dev_server`, the same `bootstrap::start()` as the app) and the real React frontend. Sessions are not mocked: they are created by replaying real-shaped Claude Code hook events (`session-start`, `pre-tool-use`, `permission-request`, `stop`, `session-end`) over the engine's Unix socket and by writing fixture transcripts, subagent files and `~/.claude/tasks` plans where Claude Code would. Each story starts from a fresh, isolated data directory (own `HOME`, own API port, short idle TTLs, deterministic fake Ollama), so nothing touches your real SessionBoard data. Screenshots are the actual frames from those runs.

Re-run: see "How to re-run" at the bottom.

## Summary

| ID | Flow | User story | Result | Time |
|---|---|---|---|---|
| E01 | Board & first launch | First launch, empty board | ✅ pass | 11.2s |
| E02 | Tasks | Add task in each column (Enter commits, Esc cancels, empty discarded, persists) | ✅ pass | 11.6s |
| E03 | Unassigned tray | New sessions land in the Unassigned tray with the right state dots | ✅ pass | 11.4s |
| E04 | Assigning | Assign a session via the ▾ menu | ✅ pass | 11.8s |
| E05 | Assigning | Assign by dragging a tray chip onto a task card (drop outline + FLIP) | ✅ pass | 12.6s |
| E06 | Assigning | Move a session from one task to another by drag | ✅ pass | 12.8s |
| E07 | Assigning | Unassign via ↩ on the row, via the overlay button, and by dragging onto the tray | ✅ pass | 15.0s |
| E08 | Workflow | Drag a task card across every column (highlight, ghost opacity, persistence) | ✅ pass | 15.0s |
| E09 | Workflow | WIP badge turns red past the limit | ✅ pass | 11.5s |
| E10 | Task cards | Task card rollup: progress, sums, session count, expand/collapse | ✅ pass | 13.0s |
| E11 | Waiting / approvals | Waiting flow: needs-you card, status pill, Approve / Reject / Send hit distinct endpoints | ✅ pass | 14.6s |
| E12 | Waiting / approvals | AskUserQuestion pre-tool-use special case → Waiting | ✅ pass | 11.4s |
| E13 | Task cards | Working / Done visual treatments | ✅ pass | 11.3s |
| E14 | Auto rollup | Auto rollup: all sessions Done → task moves to Done; a live session pulls it back | ✅ pass | 12.8s |
| E15 | Session overlay | Session overlay: identity, stats, ctx colours, subagents, plan, transcript, close paths | ✅ pass | 14.8s |
| E16 | Session overlay | Jump into session: enabled for CLI, disabled for Desktop | ✅ pass | 11.7s |
| E17 | Deleting | Delete a session from the row, the chip and the overlay — and it stays gone after restart | ✅ pass | 25.6s |
| E18 | Deleting | Delete a task: its sessions return to the tray | ✅ pass | 12.8s |
| E19 | Backend | Persistence across a backend restart | ✅ pass | 21.8s |
| E20 | Backend | WebSocket reconnect: board refetches and stays consistent | ✅ pass | 26.3s |
| E21 | Search | ⌘K Ask memory and the toolbar filter stay independent; no category field | ✅ pass | 12.6s |
| E22 | Tray | Hide-idle toggle for orphan sessions | ✅ pass | 13.9s |
| E23 | Edge cases | Edge cases: long titles, many sessions, empty task, horizontal scroll, reduced motion | ✅ pass | 14.2s |
| E24 | Migration | Category removal regression + legacy data dir migrates | ✅ pass | 21.2s |
| E25 | Visual fidelity | Visual check: populated board next to the design prototype (view 1a) | ✅ pass | 15.7s |

## Visual check against the design (E25)

The populated live board (left) next to the design prototype's view **1a** (right). Same layout, tokens, cards, tray, WIP badge, needs-you glow. Deliberate differences: no fake traffic-light title bar (the real window chrome is native), data is real (so token/cost numbers differ), and a task's tool/project label is derived from its sessions (hidden while a task has none).

<table><tr><td><img src="E25-live-board.png" width="520"></td><td><img src="E25-design-reference.png" width="380"></td></tr></table>

## Stories in detail

### E01 — First launch, empty board

**PASS**

- ✔ column Backlog
- ✔ count backlog = 0
- ✔ column To Do
- ✔ count todo = 0
- ✔ column In Progress
- ✔ count inprogress = 0
- ✔ column Done
- ✔ count done = 0
- ✔ tray hidden when there are no sessions
- ✔ status pill: 0
working
·
0
need you
- ✔ WIP badge 0/5

<img src="E01-empty-board.png" width="640"> 


### E02 — Add task in each column (Enter commits, Esc cancels, empty discarded, persists)

**PASS**

- ✔ composer closed on Esc
- ✔ Esc did not create a task
- ✔ empty submit discarded
- ✔ stages Backlog task:backlog,Doing task:inprogress,Finished task:done,Todo task:todo
- ✔ persisted after reload

<img src="E02-four-tasks.png" width="640"> 

<img src="E02-after-reload.png" width="640"> 


### E03 — New sessions land in the Unassigned tray with the right state dots

**PASS**

- ✔ tray count 4
- ✔ dot states Working,Waiting,Done,Working
- ✔ waiting dot amber, got rgb(251, 191, 36)
- ✔ working dot breathes, got _breathe_ri04i_1
- ✔ tray hint copy

<img src="E03-tray-states.png" width="640"> 


### E04 — Assign a session via the ▾ menu

**PASS**

- ✔ menu is position:fixed
- ✔ menu is 210px wide
- ✔ one row per task
- ✔ closes on outside click
- ✔ closes on Escape
- ✔ chip left the tray
- ✔ row in the task card
- ✔ tray count 1
- ✔ assignment persisted in backend

<img src="E04-menu-open.png" width="640"> 

<img src="E04-assigned.png" width="640"> 


### E05 — Assign by dragging a tray chip onto a task card (drop outline + FLIP)

**PASS**

- ✔ FLIP animation ran after the move (2)
- ✔ assigned via drag

<img src="E05-dragging-over-card.png" width="640"> 

<img src="E05-just-dropped.png" width="640"> 


### E06 — Move a session from one task to another by drag

**PASS**

- ✔ A has 1 session
- ✔ B has 1 session
- ✔ B rollup label

<img src="E06-moved-between-tasks.png" width="640"> 


### E07 — Unassign via ↩ on the row, via the overlay button, and by dragging onto the tray

**PASS**

- ✔ tray hidden with no orphans
- ✔ overlay names the task
- ✔ overlay closed after unassign
- ✔ all three unassigned in backend

<img src="E07-overlay-with-unassign.png" width="640"> 

<img src="E07-all-unassigned.png" width="640"> 


### E08 — Drag a task card across every column (highlight, ghost opacity, persistence)

**PASS**

- ✔ final stage persisted after reload

<img src="E08-dragging-over-todo.png" width="640"> 

<img src="E08-back-in-backlog.png" width="640"> 


### E09 — WIP badge turns red past the limit

**PASS**

- ✔ 5/5 at the limit
- ✔ not red at the limit
- ✔ 6/5 over the limit
- ✔ red over the limit, got rgb(248, 113, 113)

<img src="E09-wip-at-limit.png" width="640"> 

<img src="E09-wip-over-limit.png" width="640"> 


### E10 — Task card rollup: progress, sums, session count, expand/collapse

**PASS**

- ✔ 3 sessions label
- ✔ cost sum $0.02 in: Claude Code | api-gateway | ✕ | Rollup task | 1/3 done | First session prompt words | ↩ | ✕ | Second session prompt words | ↩ | ✕ | Third session prompt words | 4K | ↩ | ✕ | 3 sessions | 4K | $0.02
- ✔ token sum shown
- ✔ bar 33%, got 33%
- ✔ was expanded by default
- ✔ Done card opacity .72

<img src="E10-expanded.png" width="640"> 

<img src="E10-collapsed.png" width="640"> 

<img src="E10-done-card.png" width="640"> 


### E11 — Waiting flow: needs-you card, status pill, Approve / Reject / Send hit distinct endpoints

**PASS**

- ✔ waitGlow animation, got _waitGlow_ri04i_1
- ✔ amber border, got rgba(251, 191, 36, 0.55)
- ✔ status pill counts 3 waiting
- ✔ reply text reached the engine
- ✔ three distinct endpoints: POST /sessions/s1/approve, POST /sessions/s2/reject, POST /sessions/s3/reply

<img src="E11-needs-you.png" width="640"> 

<img src="E11-overlay-waiting.png" width="640"> 

<img src="E11-resolved.png" width="640"> 


### E12 — AskUserQuestion pre-tool-use special case → Waiting

**PASS**

<img src="E12-ask-user-question.png" width="640"> 


### E13 — Working / Done visual treatments

**PASS**

- ✔ cyan glow on a working task: rgba(56, 189, 248, 0.03) 0px 0px 0px 0.220919px, rgba(56, 189, 248, 0.09) 0px 1.76735px 6.62758px -3.09287px
- ✔ done task dimmed
- ✔ done dot green

<img src="E13-working-and-done.png" width="640"> 


### E14 — Auto rollup: all sessions Done → task moves to Done; a live session pulls it back

**PASS**

- ✔ one done, one working → stays In Progress
- ✔ backend agrees

<img src="E14-moved-to-done.png" width="640"> 

<img src="E14-pulled-back.png" width="640"> 


### E15 — Session overlay: identity, stats, ctx colours, subagents, plan, transcript, close paths

**PASS**

- ✔ cyan <70%: {"w":"50%","bg":"var(--state-working)"}
- ✔ amber 70-89%: {"w":"75%","bg":"var(--state-waiting)"}
- ✔ red ≥90%: {"w":"95%","bg":"var(--state-danger)"}
- ✔ subagents section; overlay text: DONE | 0m | ↩ Unassign | Delete session | ✕ | Claude Code | api-gateway | High context session here | High context session here | part of task Overlay task | TOKENS | 4K | COST | $0.05 | CONTEXT WINDOW | 190K / 200K | 95% | ⤷ | 1 SUBAGENT | Investigate flaky tests | WORKING | Look at CI history for flaky tests | ◇ 1K | $0.01 | ◧ | High context session here plan | 2/3 | ✓ | Write failing test | ✓ |
- ✔ plan card with 2/3
- ✔ transcript tail rows: Investigate flaky tests | WORKING | Look at CI history for flaky tests | ◇ 1K | $0.01 | ◧ | High context session here plan | 2/3 | ✓ | Write failing test | ✓ | Fix the bug | Refactor helper | TRANSCRIPT TAIL | YOU |  | High context session here |  | CLAUDE |  | On it — High context session here |  | CLAUDE |  | more work |  | Jump into this session ↗
- ✔ stats card
- ✔ part-of-task caption
- ✔ panel 448px/20px radius, got {"w":448,"r":"20px"}
- ✔ closes via ✕
- ✔ no caption for an unassigned session

<img src="E15-overlay-full.png" width="640"> 

<img src="E15-overlay-scrolled.png" width="640"> 


### E16 — Jump into session: enabled for CLI, disabled for Desktop

**PASS**

- ✔ CLI session: jump enabled
- ✔ Desktop session: jump disabled

<img src="E16-jump-enabled.png" width="640"> 

<img src="E16-jump-disabled.png" width="640"> 


### E17 — Delete a session from the row, the chip and the overlay — and it stays gone after restart

**PASS**

- ✔ overlay delete is two-click (arm)
- ✔ assignment dropped with the session
- ✔ only the survivor after restart, got s4

<img src="E17-overlay-confirm-delete.png" width="640"> 

<img src="E17-after-restart.png" width="640"> 


### E18 — Delete a task: its sessions return to the tray

**PASS**

- ✔ other task untouched
- ✔ assignment dropped

<img src="E18-hover-delete-button.png" width="640"> 

<img src="E18-after-delete.png" width="640"> 


### E19 — Persistence across a backend restart

**PASS**

- ✔ tasks in their stages
- ✔ s1 still attached
- ✔ s2 still attached
- ✔ s3 still an orphan
- ✔ ended session reconstructs as Done
- ✔ done task not bounced by reconstruction

<img src="E19-before-restart.png" width="640"> 

<img src="E19-after-restart.png" width="640"> 


### E20 — WebSocket reconnect: board refetches and stays consistent

**PASS**

- ✔ stale state kept while down
- ✔ old task still there (persisted)

<img src="E20-backend-down.png" width="640"> 

<img src="E20-reconnected.png" width="640"> 


### E21 — ⌘K Ask memory and the toolbar filter stay independent; no category field

**PASS**

- ✔ matching task kept
- ✔ Ask overlay input independent of the toolbar filter
- ✔ Ask overlay shows a billing memory
- ✔ toolbar filter untouched by Ask typing
- ✔ search results have no category: ["text","project","tool","session_id","distance","live","created_at"]
- ✔ no 'category' text anywhere in the UI

<img src="E21-filtered.png" width="640"> 

<img src="E21-ask-memory.png" width="640"> 

<img src="E21-no-results.png" width="640"> 


### E22 — Hide-idle toggle for orphan sessions

**PASS**

- ✔ toggle relabelled

<img src="E22-idle-hidden.png" width="640"> 

<img src="E22-idle-shown.png" width="640"> 


### E23 — Edge cases: long titles, many sessions, empty task, horizontal scroll, reduced motion

**PASS**

- ✔ empty task copy
- ✔ long session title truncates: {"over":true,"ell":"ellipsis"}
- ✔ card content does not overflow horizontally
- ✔ column body scrolls with many sessions
- ✔ columns row scrolls horizontally when narrow
- ✔ prefers-reduced-motion: no FLIP animations (0)

_legacy dir tables: __manifest, category_exemplars.lance, memories.lance; reconstructed 0 live sessions; search returned 10 rows_

<img src="E23-many-sessions.png" width="640"> 

<img src="E23-narrow.png" width="640"> 


### E24 — Category removal regression + legacy data dir migrates

**PASS**

- ✔ GET /categories gone
- ✔ POST /categories gone
- ✔ recategorize gone
- ✔ SessionView has no category
- ✔ copied the real data dir (read-only copy; the original is untouched)
- ✔ legacy data dir boots and serves /sessions
- ✔ search works against the legacy LanceDB

<img src="E24-no-category-ui.png" width="640"> 


### E25 — Visual check: populated board next to the design prototype (view 1a)

**PASS**

<img src="E25-live-board.png" width="640"> 


## Bugs the E2E run found in the new code (all fixed, stories re-run)

1. **Phantom FLIP moves broke drag-and-drop.** `useFlip` measured element rects while a previous FLIP animation was still in flight, so its transform leaked into the "before" position and the next render animated elements that hadn't really moved — cards drifted under the cursor and a session dropped onto another task silently did nothing. Fixed: rects are now measured with in-flight animations cancelled, and interrupted animations continue from where they visibly were. (Found by E06.)
2. **Drag handlers depended on React having re-rendered.** Dragover/drop read the drag from the store, which is published one tick after `dragstart` (mutating the DOM inside `dragstart` can make Chrome abort the drag). A fast drop could land before the render. Fixed with a synchronous `getDrag()` used by all handlers; the store copy only drives visuals. (E05–E08.)
3. **Empty tray pushed the whole board down mid-drag.** Showing the tray on drag start shifted every card under the cursor. Fixed: with no orphans it now floats over the top of the columns (no layout shift) so there is always somewhere to drop a session to unassign it. (E06/E07.)
4. **CSS keyframes silently did nothing.** CSS modules localize `animation-name`, so `animation: breathe` referenced a hashed name that didn't exist — the working-dot pulse, needs-you glow and overlay entrance never ran (the same was true of the pre-existing `StatusPill` dot). Fixed by declaring the keyframes in each module that uses them. (E03/E11.)
5. **Assign menu was 224px, not the specced 210px** (content-box + padding) and the overlay panel 450px, not 448px. Fixed with `box-sizing: border-box`. (E04/E15.)
6. Design-fidelity fixes from the E25 comparison: the "Needs you" pill wrapped onto two lines; the add-task footer belongs directly under the last card (it now scrolls with the column) rather than pinned to the window bottom; a meaningless "0" token count and "—" project label are hidden until there is data.

## Additions beyond the design handoff (worth a look)

- **Delete task** (✕ on hover, top-right of a card). The handoff has no way to remove a task; its sessions return to the tray.
- **Floating tray while dragging** (see bug 3) so unassign-by-drag works even when the tray is otherwise hidden.
- **Filter box + idle toggle kept** from the previous board in the title bar (the design only shows the status pill and Ask memory).
- Window default enlarged to 1240×820 (was 800×600) so four columns fit.

## Not covered / known gaps

- **The native Tauri window** is not driven (no tool can); the frontend is exercised in Chrome. "Jump into this session" is verified for enabled/disabled state only — actually launching Terminal needs the Tauri `invoke` bridge.
- **Real Ollama** is not used in E2E (fake gives deterministic titles = first four prompt words). The unparseable-output fallback is unit-tested; a real `qwen2.5` title is not asserted.
- **"N sessions queued"** on empty tasks (a mock-only `planned` field in the prototype) is not implemented — there is no real data behind it.
- Token/cost figures on a session appear after its first `stop` hook (pre-existing behaviour), so brand-new sessions show none.
- A running `npm run tauri dev` on this checkout hot-reloads to this branch when the working tree is switched — see the hand-off notes.

## How to re-run

```bash
npm i --no-save playwright-core            # once
echo "VITE_API_PORT=37999" > .env.e2e.local
npx vite --port 1430 --strictPort --mode e2e &   # scratch frontend pointed at the scratch backend
PW_PATH=$PWD/node_modules/playwright-core/index.mjs node scripts/e2e/e2e.mjs   # ONLY=E05,E06 to filter
node scripts/e2e/report.mjs
```
