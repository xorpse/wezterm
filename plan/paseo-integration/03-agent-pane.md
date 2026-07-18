# Stage 3 — `PaseoAgentPane` (structured agent sessions)

**Output:** `wezterm-gui/src/paseo/{mod,agent,open}.rs` — a custom-rendered pane
that shows an agent session's structured timeline (assistant text, reasoning,
tool-call cards, diffs), a prompt composer, and inline permission approve/deny.
This is the headline feature.

Prereq: Stage 1 gate green; Stage 2 recommended first (it validates the
async→GUI bridge with a simpler payload). Template: `wezterm-gui/src/review/mod.rs`
(`ReviewPane`) — **clone its skeleton wholesale** and swap the model.

## Why a custom pane, not a terminal

Agents are **not** PTYs ([05 §5](05-protocol-reference.md#5-agent-rpcs--push-messages)).
Their content is a structured timeline of typed items. To render tool-call cards
and diffs and to support inline permission actions, we render it ourselves —
exactly the problem `ReviewPane` already solves for git diffs.

## What to copy from `ReviewPane`

From `wezterm-gui/src/review/mod.rs`:

- **Struct skeleton** (`ReviewPane` `:472`): `pane_id` (`alloc_pane_id()`),
  `domain_id`, `state: Mutex<…>`, `writer: Mutex<Vec<u8>>` (dummy sink),
  `window: ::window::Window`, `weak: Mutex<Weak<Self>>` (so bg tasks `upgrade()`).
- **`impl Pane`** (`:1389`): `get_lines` (`:1444`), `with_lines_mut` (`:1460`)
  with the `rendered`/`rendered_keys` incremental cache (`sync_view` `:334`),
  `get_dimensions` (`:1486`), `get_cursor_position` (`:1394`),
  `get_current_seqno`/`get_changed_since` (`:1425`/`:1429`), `resize` (`:1528`),
  `writer`/`send_paste`/`reader` (no-ops `:1513`/`:1521`/`:1517`),
  `key_down` (`:1540`), `perform_assignment` (`:1610`), `mouse_event` (`:1620`),
  `get_title` (`:1501`).
- **Insertion** (`open_review_pane` `:1254`): `alloc_pane_id`,
  `tab.compute_split_size`, `mux.add_pane`, `tab.split_and_insert`.
- **State mutation + repaint** (`mutate` `:482`): lock → mutate → `seqno += 1` →
  `window.invalidate()`.
- **Cross-thread GUI ops** (`:497`, `:667`):
  `window.notify(TermWindowNotif::Apply(Box::new(move |tw| { … })))`.
- **Multiline editor** (`EditState` `:98`, `handle_edit_key` `:738`) — reuse
  verbatim for the composer.
- **Rendering helpers** (`make_line` `:932`, attr mapping `:942`) — reuse for
  colored rows (adds/deletes/cards).
- **KeyAssignment registration** pattern (git-review uses
  `config/src/keyassignment.rs:650` + dispatch in `termwindow/mod.rs:3128`).

## Model: transcript rows

Replace `ReviewState.rows: Vec<RenderRow>` with a transcript projected to wrapped
display rows, each tagged by kind:

```rust
enum AgentRowKind {
    UserMessage, AssistantText, Reasoning,
    ToolCallHeader,           // "▶ shell: npm test" + status glyph
    ToolCallBody,             // shell output / read preview / etc.
    DiffAdd, DiffDel, DiffCtx, DiffHunkHeader,   // reuse review diff coloring
    Todo, ErrorRow, Info,
    PermissionPrompt,         // interactive
    ComposerLine,             // the prompt input at the bottom
}
```

Keep the `rows_version` + `rendered`/`rendered_keys` incremental cache — it's
ideal for an append-heavy transcript (only changed/new rows rebuild).

### Ingesting timeline data

Two sources, same reducer:

1. **Backfill**: on open, `client.fetch_agent_timeline(agent_id, {direction:"tail", limit})`
   and page backward with `direction:"before"` + `cursor:startCursor` while
   `hasOlder`. Convert each `AgentTimelineEntry.item` → rows.
2. **Live**: `client.set_timeline_subscription([agent_id])`, then consume
   `DaemonEvent::AgentStream` from `client.events()`. `event.type == "timeline"`
   appends an item; `turn_*` updates status; `permission_requested` inserts a
   `PermissionPrompt` row; `attention_required` flags the tab.

The live pump runs as a **long-lived** `promise::spawn::spawn` task (not the
one-shot `spawn_compute` git-review uses). Each event:
`spawn_into_main_thread` → `pane.mutate(|s| s.append_event(ev))` (bumps `seqno` +
`window.invalidate()`). Use `weak.upgrade()` to reach the pane; stop when it fails.

### Rendering the item types

- `assistant_message`/`reasoning`/`user_message` → wrapped text rows (reasoning
  dimmed).
- `tool_call` → a header row (name + `ToolCallDetail.type` + status glyph from
  `running`/`completed`/`failed`/`canceled`) plus body rows from the detail:
  - `shell` → command + captured output (+ exit code).
  - `edit` → parse `unifiedDiff` with `git_review`'s existing diff parser
    (`git-review/src/parse.rs` `parse_bulk_diff`) and reuse the review diff
    coloring — this is a concrete reuse win, since the diff renderer already
    exists in the fork.
  - `read`/`write`/`search`/`fetch`/`plan`/`plain_text` → concise summaries.
  - `unknown` → the raw text/JSON.
- `todo` → checklist rows. `error` → error-styled row.

## Interaction: a mode state machine (risk #6)

Unlike git-review, permission prompts arrive **asynchronously mid-typing**. Model
explicit focus modes and route `key_down` accordingly:

```rust
enum Mode {
    Scroll,                     // j/k/PgUp/PgDn browse the transcript
    Compose,                    // typing in the composer (EditState)
    Permission { request_id: String, actions: Vec<PermAction> },
}
```

- **Scroll**: `j/k`, `g/G`, wheel, `PageUp/Down`; `i`/`Enter` → `Compose`;
  if a permission is pending, `p` → `Permission`.
- **Compose**: `EditState` editing; `Enter` submits →
  `client.send_agent_message(agent_id, text)`, clear composer; `Esc` → `Scroll`.
- **Permission**: render the prompt's `actions` (allow/deny) as a row; bind
  `y`/`Enter` → allow (default action), `n` → deny, digits → `selectedActionId`.
  Calls `client.respond_permission(agent_id, request_id, PermissionResponse::Allow{..}|Deny{..})`.
  After response, drop back to the prior mode.

A newly arriving `permission_requested` while in `Compose` should **not** steal
keystrokes silently — surface a visible banner row and require an explicit
switch (or make it the top affordance), so the user doesn't approve by accident.
Decide the exact affordance during implementation; the safe default is: show the
prompt, keep the current mode, require `p` to focus it.

## Insertion — two entry points

1. **Via `PaseoDomain`** (Stage 2/4): agent surfaces materialize as first-class
   tabs during `attach`/picker-open. A `Domain` may host heterogeneous panes, so
   `PaseoTerminalPane` and `PaseoAgentPane` coexist under one domain.
2. **Split helper** `open_paseo_agent_pane(term_window, &PaseoAgentArgs)` mirroring
   `open_review_pane` — "open this agent beside my shell". Add
   `KeyAssignment::OpenPaseoAgentPane(PaseoAgentArgs)` (Stage 4) dispatched in
   `termwindow/mod.rs`.

## In-pane actions

Add `KeyAssignment::PaseoAgentMode(PaseoAgentAssignment)` (Stage 4) with variants
`ScrollUp/ScrollDown/PageUp/PageDown/FocusComposer/SubmitPrompt/ApprovePermission/DenyPermission/Cancel/Close`,
handled in `PaseoAgentPane::perform_assignment` — exactly how `ReviewMode` maps to
`ReviewPane::perform_assignment`.

## Definition of done (Stage 3)

- Open an agent session (via picker or split helper). The transcript backfills and
  renders: assistant text, reasoning, tool-call cards, and at least one `edit`
  diff rendered with the review diff coloring.
- Typing a prompt in the composer and submitting reaches the agent; the reply
  streams in live.
- A permission request appears inline and can be approved and denied, each taking
  effect daemon-side.
- Status/attention reflects on the tab (running/needs-attention).

## Risks specific to this stage

- **Focus vs async permission interrupts** (risk #6) — the explicit `Mode` state
  machine; never let an incoming prompt silently capture composer keys.
- **Timeline correctness** — treat `fetch_agent_timeline` as authoritative and
  live `agent_stream` as immediacy-only; dedup on `seq`/`epoch`
  ([05 §5](05-protocol-reference.md#5-agent-rpcs--push-messages) / Paseo
  `docs/timeline-sync.md`). Start simple (append live, backfill on open); add gap
  reconciliation if you observe drift.
- **Volume** — long transcripts: keep the incremental render cache; virtualize by
  only building `get_lines` for the requested range (ReviewPane already does).
