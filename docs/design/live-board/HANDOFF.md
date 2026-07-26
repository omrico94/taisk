# Handoff: SessionBoard — Local Board for Parallel AI Coding Sessions

## Overview
SessionBoard is a local-first desktop app (plus a planned VS Code panel) that shows every active AI coding-agent session (Claude Code, Aider, Cursor, …) as a live card on a single board. Cards are auto-categorized, signal their state at a glance, and are searchable by natural language across all current and historical sessions. Everything runs on-device (Ollama for categorization/embeddings, an embedded vector DB such as LanceDB for storage/search).

This handoff covers the **primary desktop board UI** and its interactions, based on the "Swimlanes · Cool & technical" direction that was selected and built out (option **2a** in the prototype).

## About the Design Files
The file in this bundle (`SessionBoard.dc.html`) is a **design reference created in HTML** — a working prototype showing the intended look, motion, and behavior. It is **not production code to copy directly**. It uses a lightweight internal template runtime (`.dc.html`), inline styles, and a mocked in-memory session engine that simulates state changes on a timer.

The task is to **recreate this design in the target codebase's environment** using its established patterns and libraries. If no frontend environment exists yet, this is a desktop app — a sensible stack is **Electron or Tauri + React + TypeScript**, with a local background service (the "Core Engine" in the PRD) exposing sessions over a localhost HTTP/WebSocket API. Use that live data in place of the prototype's mock engine.

## Fidelity
**High-fidelity.** Final colors, typography, spacing, motion timing, and interactions are specified below and should be reproduced faithfully. The only mocked parts are the *data* and the *simulated* state transitions — in the real app those come from the collectors + Core Engine.

## The prototype contains 4 views (one canvas)
The HTML file is a comparison canvas. Only **2a** is the target to implement; **1a/1b/1c** are earlier explorations kept for context.
- **2a — Swimlanes deep build** ← THE DESIGN TO IMPLEMENT (full desktop window, all interactions)
- 1a — Swimlanes, static (cool/technical palette source of truth)
- 1b — Semantic constellations (alternative layout, not chosen)
- 1c — State columns / Kanban (alternative layout, not chosen)

---

## Screen: The Board (view 2a)

### Purpose
The user's home base. See every running session, spot which need attention, filter/search, open a session for detail, and reply to a blocked session — without leaving the board.

### Layout
A single desktop window, **1440 × 820px** in the mock (should be the app's resizable main window; treat these as the design reference size). Border-radius `26px`, `1px solid rgba(255,255,255,.09)` border, background vertical gradient `#0d1018 → #090b11`, drop shadow `0 50px 120px -50px rgba(0,0,0,.95)`. Vertical flex column:

1. **Title bar** (fixed, ~48px): macOS traffic-light dots (12px: `#ff5f57`, `#febc2e`, `#28c840`), "SessionBoard" label, right-aligned mono caption `100% local · Ollama + LanceDB on-device`. Bottom border `1px solid rgba(255,255,255,.06)`.
2. **Toolbar** (fixed, ~64px, padding `16px 22px`, gap `14px`, flex row): search field (left, flex, max-width 520px), "Ask memory ⌘K" button, spacer, live status pill (right).
3. **Body** (flex:1, `position:relative`, flex row): scrollable board area (flex:1) + optional detail drawer (390px, right). The ⌘K overlay renders absolutely over this region.

### Components

**Search field** (toolbar left)
- Container: padding `10px 15px`, radius `13px`, bg `rgba(255,255,255,.05)`, border `1px solid rgba(255,255,255,.1)`, gap `9px`, `⌕` glyph in `#6b7185`.
- Input: transparent, no border/outline, text `#e8eaf0`, 13.5px. Placeholder: "Filter sessions — or ⌘K to search everything you've ever asked", placeholder color `#6b7185`.
- Behavior: live-filters the board as you type (see Interactions).

**Ask memory button** (toolbar)
- Padding `10px 15px`, radius `13px`, bg `rgba(56,189,248,.12)`, border `1px solid rgba(56,189,248,.32)`, text `#8fd6ff` 13px/600. Content: `✦ Ask memory ⌘K` (the `⌘K` is mono 11px, opacity .7).
- Opens the semantic search overlay.

**Status pill** (toolbar right)
- Radius `100px`, bg `rgba(255,255,255,.04)`, border `1px solid rgba(255,255,255,.08)`, padding `9px 15px`, 12.5px.
- Content: breathing cyan dot + `{workingCount} working` · amber dot + `{waitingCount} waiting`. Counts are live.

**Swimlane** (board area, one per category)
- Row, gap `18px`, align flex-start. Left label column is 132px: category name (14px/600, `#dfe3ec`) + mono sub `{count} live` (11px, `#6b7185`).
- Right: wrapping flex of session cards, gap `14px`.
- Categories shown in fixed order: `Backend / API`, `Frontend`, `Data`, `Infra / DevOps`. Empty lanes are hidden. Lanes stack with gap `22px`.

**Session card** (the core component)
- Width `258px`, padding `13px 15px`, radius `15px`, `box-sizing:border-box`, gap `11px`, column.
- Base: bg `rgba(255,255,255,0.045)`, border `1px solid rgba(255,255,255,0.09)`, `backdrop-filter: blur(14px)`.
- Transition: `opacity .6s, filter .6s, box-shadow .6s, border-color .6s`.
- Hover: `translateY(-2px)`, border-color `rgba(255,255,255,.18)`. Cursor pointer.
- **Row 1**: state dot (9px) + state label (10.5px/700, uppercase, letter-spacing .08em, colored by state) + spacer + elapsed (mono 11px, `#6b7185`, e.g. `8m`, `1h 04m`).
- **Row 2**: current task (14px/500, line-height 1.36, `#e4e7ef`, `text-wrap:pretty`, min-height 38px).
- **Row 3**: tool badge (10.5px/600, radius 6px, bg `rgba(255,255,255,.06)`, mono, color by tool) + project (mono 11.5px, `#7a8194`).

**State-dependent card styling** (critical — this is the "signal at a glance"):
| State | Dot | Card treatment |
|---|---|---|
| **Working** | `#38bdf8`, `box-shadow 0 0 12px 1px rgba(56,189,248,.6)`, **breathe** animation | subtle glow `box-shadow 0 8px 30px -16px rgba(56,189,248,.6)` |
| **Waiting** | `#fbbf24`, `box-shadow 0 0 10px 0 rgba(251,191,36,.55)` | border-color = amber, **waitGlow** pulsing box-shadow animation |
| **Idle** | `#64748b` | `opacity .46`, `filter saturate(.5)` |
| **Done** | `#34d399` | `opacity .7` |

**Tool badge colors**: Claude Code `#d98a6a`, Aider `#7fbf7f`, Cursor `#7aa7ff`.

**Empty / no-results state** (query matches no live session)
- Centered column, padding `80px 20px`: message `No live sessions match "{query}".` (15px, `#8b93a7`) + button `✦ Search your full history instead` (opens overlay; bg `rgba(56,189,248,.14)`, border `rgba(56,189,248,.34)`, text `#8fd6ff`).

**Detail drawer** (right, appears on card click)
- Width `390px`, left border `1px solid rgba(255,255,255,.08)`, bg gradient `rgba(20,24,32,.9) → rgba(12,15,21,.95)`, `backdrop-filter: blur(20px)`, column.
- Enters with `slideIn` animation (`.42s cubic-bezier(.22,1,.36,1)`, from `translateX(26px)` + opacity 0).
- **Header**: state pill (`✦`-less; dot + label, radius 100px, colored border/text by state, bg `rgba(255,255,255,.05)`) + spacer + elapsed (mono) + close `✕` button (26px square, radius 8px).
- **Identity**: tool badge + project (14px/600 mono `#e8eaf0`), then current task (16px/600, line-height 1.4, `#f2f4f9`).
- **Memory chip**: bg `rgba(56,189,248,.07)`, border `rgba(56,189,248,.16)`, radius 12px — `✦ In {category} · {N} similar in memory`.
- **Transcript tail**: label "Transcript tail" (mono 10.5px uppercase, letter-spacing .1em, `#6b7185`); list of rows, each = role tag (mono 10px/700 uppercase, 46px wide, colored: You `#7cc5ff`, Claude `#d98a6a`, tool `#7fbf7f`, other `#6b7185`) + text (12.5px, line-height 1.45, `#c4cad8`). Scrollable (`flex:1`).
- **Footer — if Waiting**: bg tint `rgba(251,191,36,.05)`, top border. Textarea (min-height 54px, radius 11px, placeholder "Reply, or just approve…") + button row: **Approve** (flex:1, solid `#34d399`, text `#04120b`/700), **Reject** (`rgba(255,255,255,.06)` bg), **Send ↵** (`rgba(56,189,248,.16)` bg, `#8fd6ff`).
- **Footer — otherwise**: **Jump to session ↗** (flex:1) + **Re-categorize** buttons.

**Semantic search overlay** (⌘K / Ask memory)
- Full-cover scrim over the body: `position:absolute; inset:0; background rgba(6,8,12,.62); backdrop-filter blur(6px)`; content top-aligned, `padding-top:70px`, `z-index:20`. Click scrim to dismiss.
- Panel: 600px, radius 20px, bg `rgba(18,22,30,.96)`, border `rgba(255,255,255,.12)`, shadow `0 40px 100px -30px rgba(0,0,0,.9)`. Enters with `overlayIn` (`.3s cubic-bezier(.22,1,.36,1)`, from scale .96 + opacity 0).
- **Input row**: `✦` (`#38bdf8` 17px) + search input (15.5px, `#f2f4f9`, placeholder "Search everything you've ever asked an agent…") + mono `esc` hint. Bound to the same query as the toolbar search.
- **Results**: section label "Semantic matches · live + history" (mono uppercase). Each row: relevance badge (mono 11px/700, `#38bdf8` on `rgba(56,189,248,.12)`, border `rgba(56,189,248,.3)`, radius 8px, e.g. `96%`) + snippet (13.5px/500 `#e6eaf2`) + `{tool} · {project}` (mono 11px `#7a8194`) + when (mono 11px; live = `● live now` in `#34d399`, historical = date in `#7a8194`). Row hover bg `rgba(255,255,255,.04)`. Clicking a **live** result closes the overlay and opens that session's drawer.

---

## Interactions & Behavior

- **Live filter**: typing in the toolbar (or overlay) input filters visible cards where the query (case-insensitive) matches task + project + tool + category. Empty lanes hide; if nothing matches, show the empty state. Toolbar input and overlay input share one query value.
- **⌘K / Ctrl+K**: toggles the semantic overlay. **Esc**: closes overlay and drawer. (Global keydown listener.)
- **Card click**: opens the detail drawer for that session; drawer slides in.
- **Approve / Reject / Send** (Waiting drawer): resolves the session → transitions it to **Working** with an updated task line, then closes the drawer.
- **Re-categorize** (non-Waiting drawer): moves the session to the next category (in the real app: manual override / merge, FR9).
- **Motion — cards physically move**: when a session changes state or category and therefore moves within/between lanes, animate the move with a **FLIP** technique (measure rects before update, animate the delta after): `element.animate([{transform:'translate(dx,dy)'},{transform:'translate(0,0)'}], {duration:680, easing:'cubic-bezier(.22,1,.36,1)'})`. This is the signature "calm & physical" motion — reproduce it.
- **Ambient motion**: Working dots continuously **breathe**; Waiting cards continuously **waitGlow**. These run regardless of re-renders.
- In the mock, a timer mutates one random session every ~2.6s to demo motion. **Remove this** — real transitions are driven by the Core Engine (Claude Code hooks for authoritative state, transcript-tailing for content). See PRD §6.2.

## State Management
Per-session model: `{ id, tool, project, category, state, task, startedAt/elapsed }`. Derived UI state:
- `query` (string) — drives filtering + overlay.
- `selectedId` (id | null) — open drawer.
- `askOpen` (bool) — overlay visibility.
- `replyText` (string) — Waiting reply composer.
- Derived: `workingCount`, `waitingCount`, categories grouped in fixed order, filtered board, semantic results.

Real data sources (from PRD, replacing the mock):
- **State transitions** ← Claude Code hooks (`Notification`, `Stop`, `PreToolUse`/`PostToolUse`, `SessionEnd`); TTL → Idle.
- **Task summaries / transcript** ← tail the per-session JSONL (`~/.claude/projects/<project>/<session-uuid>.jsonl`), append-safe.
- **Category** ← local embedding (e.g. `nomic-embed-text`) compared to category exemplars in the vector DB; small instruct model (`qwen2.5:1.5b` / `llama3.2:3b`) proposes the label. Card should render immediately as "Uncategorized" and update when categorization returns (don't block on the model).
- **Search** ← embed the query locally, match against the vector DB; return live + historical ranked by relevance.

## Design Tokens

**Colors**
- App bg gradient: `#0d1018` → `#090b11`; page bg `#08090d`
- Surfaces: card `rgba(255,255,255,.045)`; drawer `rgba(20,24,32,.9)`→`rgba(12,15,21,.95)`; overlay panel `rgba(18,22,30,.96)`; scrim `rgba(6,8,12,.62)`
- Borders: `rgba(255,255,255,.06)` / `.08` / `.09` / `.1` / `.12`
- Text: primary `#f2f4f9` / `#e8eaf0` / `#e4e7ef`; secondary `#a4abbd` / `#c4cad8`; muted `#8b93a7` / `#7a8194`; faint `#6b7185`; divider-dim `#454b5c`
- Accent (cyan / Working): `#38bdf8`, glow `rgba(56,189,248,.6)`
- Waiting (amber): `#fbbf24`, glow `rgba(251,191,36,.55)`
- Idle (slate): `#64748b`
- Done (green): `#34d399`
- Tool colors: Claude Code `#d98a6a`, Aider `#7fbf7f`, Cursor `#7aa7ff`
- Traffic lights: `#ff5f57`, `#febc2e`, `#28c840`
- Link: `#7cc5ff`, hover `#a9d8ff`

**Typography**
- UI: system stack `-apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif` (SF Pro on macOS — matches the Apple feel).
- Mono (labels, project names, elapsed, relevance): `ui-monospace, SFMono-Regular, Menlo, monospace`.
- Scale used: 10–11px mono labels; 12.5–14px body; 16px drawer task; 22px view heading; 40px canvas H1. Weights 500/600/700.

**Radii**: cards 15px, inputs/buttons 11–13px, window/overlay 20–26px, pills 100px.

**Spacing**: card gap 11px; lane gap 22px; toolbar gap 14px; card wrap gap 14px; content padding 20–22px.

**Motion / keyframes**
- `breathe` (Working dot): `0/100% {scale(1);opacity 1} 50% {scale(1.6);opacity .5}`, `2.6s ease-in-out infinite`.
- `waitGlow` (Waiting card): pulse box-shadow between `0 6px 26px -12px` and `0 12px 44px -6px` of the amber, `2.6s ease-in-out infinite`.
- `slideIn` (drawer): `translateX(26px)+opacity0 → 0`, `.42s cubic-bezier(.22,1,.36,1)`.
- `overlayIn` (spotlight): `scale(.96)+opacity0 → 1`, `.3s cubic-bezier(.22,1,.36,1)`.
- FLIP moves: `680ms cubic-bezier(.22,1,.36,1)`.
- Standard settle easing everywhere: `cubic-bezier(.22,1,.36,1)` (soft spring, gentle overshoot-free settle).

## Assets
No image or icon assets — all glyphs are Unicode (`✦ ⌕ ✕ ● �void ↗ ↵ ·`) and CSS-drawn dots. If the target app has an icon set (SF Symbols, Lucide, etc.), swap the Unicode glyphs for equivalents.

## Files
- `SessionBoard.dc.html` — the full prototype (all four views on one canvas). Implement **view 2a**. Reference 1a for the base swimlane/palette, 1b/1c only as rejected alternatives.
- The logic (state model, filter, FLIP, state-transition simulation, semantic-result mock) lives in the `<script data-dc-script>` `class Component` block inside that file — read it to see exact behavior.
