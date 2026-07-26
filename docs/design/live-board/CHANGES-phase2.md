# Handoff — Phase 2 changes

> **Read `README.md` first** for the full Phase-1 spec (board layout, states, motion, search, drawer, tokens). This document covers ONLY what changed after Phase 1. Phase 1 is already implemented — do not re-build it; apply the deltas below to the existing **view 2a (Swimlanes deep build)**.

All changes are on the primary board. The three comparison boards (1a/1b/1c) are unchanged except for an internal field rename (see §6).

---

## 1. Session cards now carry a Title + Task description (was: single task line)

Each session has **two** distinct text fields:
- **`title`** — a short, stable name for the session (e.g. "Auth middleware refactor"). Rendered bold, 14px/600, `#f2f4f9`.
- **`desc`** — the live, changing current task (e.g. "Converting verify() calls to async/await"). Rendered 12px/400, `#98a0b3`, below the title.

Card row order is now: state row → **title** → **desc** → context bar → metrics row → plan chip → subagents toggle.

Drawer mirrors this: large title (18px/700) with the description beneath it (13.5px, `#a4abbd`).

**Data source (real app):** `title` = the session's initial prompt summarized to ≤4 words (reuse the local instruct model already used for categorization); `desc` = latest activity line from the transcript tail (the value Phase 1 showed as `task`).

## 2. Per-session metrics: tokens, cost, context window

New fields on every session: `tokens` (int), `cost` (float USD), `ctxUsed` (int), `ctxMax` (int, e.g. 200000).

**Card** shows two things:
- A **context-window bar** (label `CTX` + track + `%`). Fill color is a function of fill ratio: **<70% cyan `#38bdf8`, 70–90% amber `#fbbf24`, ≥90% red `#f87171`.** This is the `ctxColor()` rule — reproduce it.
- A **metrics row** (below a hairline divider): `◇ {tokens}` (e.g. `128K`) and `{cost}` (e.g. `$0.41`, cyan `#8fd6ff`), tool badge right-aligned.

Formatting: tokens → `fmtTokens` (`128400` → `128K`, `1_200_000` → `1.2M`); cost → `$` + 2 decimals.

**Drawer** shows a stat block: Tokens and Cost as two large stats side by side, plus a full-width context bar with `{ctxUsed} / {ctxMax}` and `%`.

**Live behavior:** in the prototype these accrue on each tick for `working` sessions (`accrue()` bumps tokens/cost/ctxUsed, and each working subagent too). In the real app, drive from actual usage events — **remove the simulated accrual.**

## 3. Subagent sessions (parent → child tree)

A session may have a `subs` array. Each subagent: `{ id, title, desc, state, tokens, cost, ctxUsed, ctxMax }` — same state vocabulary as a top-level session, but subagents belong to their parent (they don't move categories independently).

**Card:** below the parent card, subagents branch off a **connector rail** — a 2px cyan-tinted left border (`rgba(56,189,248,.22)`), each child on a short horizontal connector stub into a compact card (dot + title + state label + desc + mini context bar + tokens/cost).

**Drawer:** a "N subagents" section listing each child with its metrics.

## 4. Subagents are collapsible  ← newest change

The parent card's **"⤷ N subagents"** line is now a toggle:
- Clicking it expands/collapses the subagent branch.
- A chevron precedes it: **`‹` when expanded, `›` when collapsed**.
- The toggle **must `stopPropagation`** so it does not also open the detail drawer (the whole card is clickable).
- Collapsed state is per-session (prototype: `state.collapsedSubs[id]`), default **expanded**.

## 5. Categories: create + drag-to-recategorize

Categories are now **user-editable data**, not a fixed constant. In the prototype they moved from a hardcoded `catOrder` array to `state.cats`.

**Create a category:**
- A dashed **"＋ New category"** button sits below the last lane.
- Clicking it swaps in an inline input (autofocused). **Enter** commits, **Esc** cancels; there's also an Add button.
- Committing appends the name to `cats` (deduped) — it appears immediately as a new **empty lane**.

**Drag sessions between categories:**
- Session cards are `draggable`. On `dragstart` the dragged session id is held (prototype: `state.dragId`) and the source card dims to ~0.4 opacity.
- Lanes are drop targets (`dragover` → `preventDefault`; the hovered lane sets `dragCat`). The target lane highlights: cyan tint background + `inset` cyan ring.
- On `drop`, the session's `cat` is set to that lane and the drag state clears. The existing **FLIP animation** (Phase 1, 680ms) then springs the card into its new lane — no extra work needed.
- **Empty lanes stay visible** while there are sessions to drag (they show a dashed "Drop a session here" placeholder). When a search query is active, empty lanes are hidden.

**Real app:** persist `cats` and each session's category override locally; a manual drop is an explicit user override that should win over (and optionally retrain) auto-categorization (PRD FR9).

## 6. Plan link: session → the plan it executes

A session can link to an **execution plan** it is carrying out. Prototype data: `plans()` keyed by session id → `{ title, steps: [[text, done], …] }`; `planBits(id)` derives `{title, done, total, label}`.

**Card:** a violet **plan chip** — `◧ Plan · {done}/{total}` (accent `#b39dff`) with a small progress bar. Distinct violet keeps it visually separate from the cyan subagent affordance.

**Drawer:** a full **plan panel** (violet-tinted) with the plan title, an **"Open plan ↗"** link (wire to the plan document/route in the real app), a progress bar + `done/total`, and the **checklist of steps** — completed steps get a filled green check and strike-through, pending steps an empty ring.

## 7. Field rename (internal)

The session field `task` was renamed to **`desc`** across the data model. If your Phase-1 implementation stored `task`, rename it (or map it): the board card body, drawer, filtering, and the state-transition logic all read `desc` now. The comparison boards 1a/1b/1c bind the same value.

---

## Field summary (session model, Phase 2)

```
{
  id, tool, project, cat, state,           // Phase 1 (task → renamed desc)
  title,                                    // NEW — short session name
  desc,                                     // was `task` — live current task
  tokens, cost, ctxUsed, ctxMax,            // NEW — metrics
  subs: [ {id,title,desc,state,tokens,cost,ctxUsed,ctxMax}, … ],  // NEW — subagents
  // plan linked by id via plans()/planFor(id): { title, steps:[[text,done]] }
}
```

Derived UI state added this phase: `cats[]`, `dragId`, `dragCat`, `addingCat`, `newCat`, `collapsedSubs{}`.

## New design tokens

- Plan accent (violet): `#b39dff` (chip/links), panel tint `rgba(179,157,255,.06)` / border `rgba(179,157,255,.2)`.
- Context-window thresholds: cyan `#38bdf8` (<70%), amber `#fbbf24` (70–89%), red `#f87171` (≥90%).
- Plan-step complete check: green `#34d399`.
- Drop-target highlight: bg `rgba(56,189,248,.08)`, ring `inset 0 0 0 1.5px rgba(56,189,248,.45)`; empty-lane dashed border `rgba(255,255,255,.1)` (→ `rgba(56,189,248,.5)` when hot).

## File
`SessionBoard.dc.html` (updated) — implement view **2a**. Read its `class Component` for exact behavior: `accrue`, `ctxBits`, `decorateDeep`, `buildDetail`, `groupByCatDeep` (drag/drop + empty lanes), `toggleSubs`, `commitAddCat`, `dropOn`, `plans`/`planBits`.
