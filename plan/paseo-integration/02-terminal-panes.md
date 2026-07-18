# Stage 2 — `paseo-mux` crate: terminal panes + domain

**Output:** a new crate `paseo-mux` providing `PaseoTerminalPane` (a real terminal
pane whose bytes come from `paseo-client` instead of a local PTY) and
`PaseoDomain` (a `mux::domain::Domain` that hosts Paseo surfaces as first-class
tabs/panes). Deliver **attach + render first**; stub write-side domain ops.

Prereq: Stage 1 gate green. Reference: `mux/src/localpane.rs`,
`mux/src/lib.rs`, `wezterm-client/src/domain.rs`, `mux/src/domain.rs`,
`mux/src/pane.rs`.

## Why a Domain (not bare pane insertion like git-review)

`git-review` inserts a transient `Arc<dyn Pane>` via `tab.split_and_insert` and
never registers a domain — fine for a throwaway side panel. Paseo terminals must
behave like real terminals: appear in the launcher, be attach/detach-able,
resize, reconnect, and support spawn/split later. That is what a `Domain`
provides. `PaseoDomain` also becomes the single place discovery/attach hangs off
(Stage 4).

## Why `LocalPane` model, not `ClientPane`

`wezterm-client`'s `ClientPane` consumes **remote screen diffs** — the remote
wezterm runs the emulator and ships surface deltas. Paseo ships **raw PTY bytes**,
so we run the emulator locally, exactly like `LocalPane`. Study `ClientPane` for
the *Domain* plumbing (attach/spawn/id-mapping), not for the pane's rendering
model.

## `PaseoTerminalPane`

A `LocalPane` analog. Model against `mux/src/localpane.rs` (`LocalPane` struct
`:124`, `impl Pane` `:139`, `perform_actions` `:390`, `writer()` `:428`,
`resize()` `:417`).

```rust
pub struct PaseoTerminalPane {
    pane_id: PaneId,
    domain_id: DomainId,
    terminal: Mutex<wezterm_term::Terminal>,   // LOCAL emulation
    writer: Mutex<PaseoPtyWriter>,             // impl std::io::Write → client.send_input
    handle: TerminalHandle,                    // from paseo-client
    remote_terminal_id: String,
    dead: AtomicBool,
}
```

`impl Pane` — most methods delegate to the local `Terminal` (this is what buys us
key encoding, mouse reporting, bracketed paste, scrollback for free):

- `pane_id`, `domain_id`.
- `get_lines`, `with_lines_mut`, `get_dimensions`, `get_cursor_position`,
  `get_current_seqno`, `get_changed_since`, `get_logical_lines` → delegate to the
  `Terminal`'s renderable (same as `LocalPane`).
- `writer()` → `MappedMutexGuard<dyn Write>` over `PaseoPtyWriter` whose
  `write(buf)` calls `handle.send_input(buf.to_vec())` (async → enqueue on the
  client's outbound channel; the `Write` impl itself is sync and non-blocking).
  This mirrors `wezterm-client`'s `PaneWriter` (`wezterm-client/src/pane/clientpane.rs:43`).
- `resize(size)` → lock `Terminal`, `terminal.resize(size)`, then
  `handle.resize(size.rows, size.cols)`.
- `key_down`/`key_up`/`mouse_event`/`send_paste` → delegate to the `Terminal`
  (it produces encoded bytes and writes them through `writer()`), like `LocalPane`.
- `is_dead()` → `dead`. `palette()`, `is_mouse_grabbed()`, `is_alt_screen_active()`
  → delegate to `Terminal`.
- `get_current_working_dir` → from the terminal's OSC-7 if present, else the
  Paseo terminal's cwd.

### Output pump (the async→GUI bridge)

One background task per terminal, spawned with `promise::spawn::spawn`:

```rust
let rx = handle.output();
promise::spawn::spawn(async move {
    let mut parser = termwiz::escape::parser::Parser::new();
    while let Ok(ev) = rx.recv_async().await {
        match ev {
            TerminalStreamEvent::Output(bytes) | TerminalStreamEvent::Restore(bytes) => {
                let actions = parse_to_actions(&mut parser, &bytes);   // OFF the main thread
                let pane_id = pane_id;
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(p) = Mux::get().get_pane(pane_id) {
                        p.perform_actions(actions);                    // ON the main thread
                        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane_id));
                    }
                }).detach();
            }
            TerminalStreamEvent::Snapshot(state) => { /* prime the emulator, see below */ }
        }
    }
    // stream ended → mark dead + prune
}).detach();
```

**Hard rule (risk #3):** parse to `Vec<Action>` on the background task, but only
call `perform_actions` on the main thread via `spawn_into_main_thread`. Never
mutate the `Terminal` from the read loop.

Consider reusing `mux/src/lib.rs`'s `parse_buffered_data` (`:142`) /
`send_actions_to_mux` (`:122`) coalescing/synchronized-output loop verbatim rather
than a naive parser, so fast TUIs don't tear.

### Snapshot handling
On `subscribe_terminal` we may request `restore` and receive a `Snapshot`
(`TerminalState`, [05 §6](05-protocol-reference.md#binary-frame-format)). Two
options: (a) synthesize the equivalent escape sequences to seed the `Terminal`,
or (b) request `restore: {mode:"live"}` and rely on `Restore` byte replay
(simplest — treat `Restore` exactly like `Output`). **Start with (b).** Only
build snapshot-grid seeding if live replay proves insufficient.

## `PaseoDomain`

`impl mux::domain::Domain` (trait `mux/src/domain.rs:50`, `#[async_trait(?Send)]`).
Holds an `Arc<PaseoClient>` and an id map, mirroring `ClientInner`
(`wezterm-client/src/domain.rs:20`).

```rust
pub struct PaseoDomain {
    domain_id: DomainId,
    name: String,          // e.g. "paseo:work"
    config: PaseoDaemon,   // from Stage 4 config
    client: Mutex<Option<Arc<PaseoClient>>>,
    state: Mutex<DomainState>,
    remote_to_local: Mutex<HashMap<String, PaneId>>, // paseo terminal/agent id → pane id
}
```

Methods (model on `ClientDomain` — `attach` `:931`, `spawn` `:817`,
`split_pane` `:863`, `process_pane_list` `:504`):

- `domain_id`, `domain_name`, `domain_label`, `state`.
- **`attach(window_id)`** — connect the client (relay or local per config), then
  list terminals (and, Stage 3, agents), and for each materialize a pane:
  - terminal → `subscribe_terminal` → build `PaseoTerminalPane` → `Tab::new` →
    `tab.assign_pane` → `mux.add_tab_and_active_pane` → `mux.add_tab_to_window`.
  - record `remote_to_local`.
  Set `state = Attached`. This is the read-path MVP — deliver this first.
- **`spawn(size, command, command_dir, window)`** — create a new Paseo terminal
  (`client.create_terminal`) then materialize as above. Can be **stubbed
  ("unsupported")** until attach works.
- **`split_pane`** — create terminal + `tab.split_and_insert` + `mux.add_pane`.
  Stub initially.
- `detach`, `detachable() -> true`, `spawnable()`.

On `TerminalExit`/`terminals_changed` from the client's event bus (subscribe once
per domain), prune the corresponding pane (`is_dead` + `mux.prune_dead_windows`),
mirroring `ClientPane`'s `PaneRemoved` handling (`clientpane.rs:209`).

## Reconnect (risk #7)

On client disconnect: mark all domain panes dead, prune. On reconnect: the client
regenerates a fresh ephemeral keypair (Stage 1), re-`attach` re-subscribes
terminals. Follow the `Reconnectable` pattern (`wezterm-client/src/client.rs:539`)
for backoff. Deliverable can start with "mark dead on disconnect; manual re-attach
from the launcher" and add auto-reconnect later.

## Crate wiring

- New workspace member `paseo-mux` in the root `Cargo.toml` members list.
- `paseo-mux` deps: `mux`, `paseo-client`, `wezterm-term`, `termwiz`, `promise`,
  `parking_lot`, `config` (for `PaseoDaemon`), `async-trait`, `anyhow`.

## Definition of done (Stage 2)

- Add a `paseo_daemons` entry to `~/.config/wezterm/wezterm.lua` (Stage 4 config
  must land first, or hardcode a temporary domain in `update_mux_domains_impl`).
- Launch the fork, attach the Paseo domain from the launcher.
- An existing remote Paseo terminal opens as a WezTerm pane: renders current
  output, accepts typed input (echoes correctly), resizes with the pane, and
  survives scrollback. A second terminal opens as a second tab, alongside a local
  tab.
- Killing the terminal daemon-side (or `terminal_stream_exit`) marks the pane dead
  and prunes it.

## Risks specific to this stage

- **Terminal thread-safety** (risk #3) — the output pump rule above.
- **Domain semantics for a non-PTY backend** (risk #5) — attach+render first;
  stub `spawn`/`split_pane`; map WezTerm's spawn/split onto Paseo's
  create-terminal only once the read path is solid.
- **Snapshot vs live replay** — start with live replay; defer grid seeding.
