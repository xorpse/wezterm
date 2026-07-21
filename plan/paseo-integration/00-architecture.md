# 00 — Architecture

## Goal

A user running this WezTerm fork can point it at a Paseo daemon (remote via the
E2E-encrypted relay, or local) and get their Paseo **agent sessions** and
**terminals** as native tabs/panes, interleaved with local tabs. Agents are fully
interactive (prompt, tool-call cards, diffs, permission approve/deny); terminals
behave like any other terminal pane.

## Layering

Four layers. Two are new crates, one is a new gui module, one is edits to
existing crates.

```
┌─────────────────────────────────────────────────────────────────────┐
│ wezterm-gui                                                          │
│   src/paseo/{mod,agent,open}.rs   ← PaseoAgentPane (ReviewPane clone)│  Stage 3
│   termwindow/mod.rs (dispatch)    ← key assignment arms             │  Stage 4
│   overlay/ (launcher, selector)   ← discovery/attach UX             │  Stage 4
└───────────────┬─────────────────────────────────────────────────────┘
                │ uses
┌───────────────▼──────────────┐   ┌──────────────────────────────────┐
│ paseo-mux (new crate)        │   │ config (edits)                   │  Stage 4
│   PaseoDomain: mux::Domain   │   │   src/paseo.rs (PaseoDaemon)     │
│   PaseoTerminalPane          │   │   src/config.rs (field)          │
│   (LocalPane analog)         │   │   src/keyassignment.rs (variants)│
└───────────────┬──────────────┘   └──────────────────────────────────┘  Stage 2
                │ uses                            │
┌───────────────▼─────────────────────────────────▼───────────────────┐
│ paseo-client (new crate)                                            │  Stage 1
│   transport (local WS | relay+E2EE)  ·  RPC correlation             │
│   agent streams  ·  per-terminal byte streams  ·  binary frames     │
└───────────────┬──────────────────────────────────────────────────────┘
                │ speaks
        ┌───────▼────────┐
        │ Paseo daemon   │  /ws WebSocket, default 127.0.0.1:6767, or via relay
        └────────────────┘
```

Why `paseo-mux` is separate from `mux`: it depends on `paseo-client` (network,
crypto, WS). Keeping it out of `mux` keeps the core multiplexer free of that
dependency, exactly as `wezterm-client` is a separate crate from `mux`.

Why `PaseoAgentPane` lives in `wezterm-gui` (not `paseo-mux`): it needs
`window::Window` and `crate::termwindow::TermWindowNotif`, which are gui-side —
the same reason `ReviewPane` lives in `wezterm-gui/src/review/`.

## Two panes, two mechanisms

| | `PaseoTerminalPane` (Stage 2) | `PaseoAgentPane` (Stage 3) |
| --- | --- | --- |
| Backing | Real `wezterm_term::Terminal`, advanced by bytes from the client | Custom in-memory row model |
| Rendering | WezTerm's normal terminal renderer (free) | Custom `get_lines`/`with_lines_mut`, like `ReviewPane` |
| Input | Delegated to the `Terminal` (key encoding, mouse, paste) → `writer()` → client | Modal keymap: scroll / compose / answer-permission |
| Template | `mux/src/localpane.rs` `LocalPane` | `wezterm-gui/src/review/mod.rs` `ReviewPane` |
| Container | Hosted in `PaseoDomain` → first-class tabs | Inserted via `PaseoDomain` **or** a `split`-style helper |

Important: use the **`LocalPane` model** (local emulation of a raw byte stream)
for terminals, **not** the `ClientPane` model from `wezterm-client`. `ClientPane`
consumes remote *screen diffs* because the remote wezterm does the emulation.
Paseo ships raw PTY bytes, so we emulate locally.

## Async & threading model (the load-bearing decision)

WezTerm's executor is `async_executor` (smol family), configured in
`promise/src/spawn.rs`. **We stay entirely in that family — no tokio.**

- `paseo-client` is **executor-agnostic**: it is plain `async fn`s plus `futures`
  and `flume`/`async-channel` primitives. It does not spawn or own a runtime.
- **Inside WezTerm**, the embedder drives it:
  - Spawn the long-lived connection/read loop with `promise::spawn::spawn(fut)`
    (WezTerm's background executor).
  - Marshal each inbound item onto the GUI/mux thread with
    `promise::spawn::spawn_into_main_thread(async move { … })`.
  - Trigger repaint with `Mux::notify_from_any_thread(MuxNotification::PaneOutput(id))`
    (terminals) or `window.invalidate()` (agent pane), and run `&mut TermWindow`
    closures via `window.notify(TermWindowNotif::Apply(Box::new(...)))`.
- **In the Stage-1 standalone example**, the embedder is simply
  `smol::block_on(async { … })`.

This mirrors how `wezterm-client` runs its own async stack and marshals back —
except we don't even need a dedicated OS thread, because we reuse WezTerm's
executor rather than a foreign one.

### The one hard rule about the terminal emulator

`wezterm_term::Terminal` must be mutated on a single thread. Parse PTY bytes into
`termwiz` `Action`s on a background task, then `spawn_into_main_thread` to call
`pane.perform_actions(actions)` + `Mux::notify_from_any_thread(PaneOutput(id))`.
Never call `perform_actions` directly from the read loop. This is exactly what
`mux/src/lib.rs` `send_actions_to_mux` / `emit_output_for_pane` already do.

## Data flow

### Terminal (bidirectional PTY)

```
daemon --Output frame(slot)--> paseo-client --bytes--> [bg task]
   parse to Vec<Action> --spawn_into_main_thread--> pane.perform_actions
   + Mux::notify_from_any_thread(PaneOutput) --> repaint

keypress --> Terminal encodes --> pane.writer().write() --> client.send_input(id, bytes)
   --Input frame(slot)--> daemon
resize --> Terminal.resize + client.resize(id, rows, cols) --Resize frame--> daemon
```

### Agent (structured)

```
daemon --agent_stream event--> paseo-client --DaemonEvent--> [bg task]
   spawn_into_main_thread --> agent_pane.mutate(|s| s.append_event(ev))
   (bumps seqno + window.invalidate()) --> custom get_lines re-render

compose+submit --> client.send_agent_message(agent_id, text)
permission prompt row + y/n --> client.respond_permission(agent_id, req_id, allow|deny)
```

## Connection lifecycle

1. Config declares `paseo_daemons` (Stage 4). Each becomes a registered
   `PaseoDomain` via `update_mux_domains_impl`.
2. `PaseoDomain::attach` (or `connect_automatically`) → `paseo-client` connects:
   - **Relay:** parse pairing offer → open relay WS → E2EE handshake
     (`e2ee_hello`/`e2ee_ready`) → `hello` → wait for `server_info`.
   - **Local:** open `ws(s)://host:port/ws` → optional bearer → `hello` →
     `server_info`.
3. Attach lists terminals (and agents) and materializes tabs.
4. On disconnect: mark panes dead, prune; reconnect regenerates a fresh
   ephemeral keypair and re-subscribes timeline/terminal state.

## New/changed files (whole feature)

New crates:
- `paseo-client/` — see [01](01-paseo-client-crate.md).
- `paseo-mux/` — see [02](02-terminal-panes.md).

New gui module:
- `wezterm-gui/src/paseo/{mod,agent,open}.rs` — see [03](03-agent-pane.md).

Edits:
- `config/src/paseo.rs` (new), `config/src/lib.rs`, `config/src/config.rs`,
  `config/src/keyassignment.rs` — see [04](04-config-keybindings-discovery.md).
- `wezterm-gui/src/termwindow/mod.rs` (dispatch arms) — Stage 3/4.
- `wezterm-mux-server-impl/src/lib.rs` (`update_mux_domains_impl`) — Stage 4.
- Workspace `Cargo.toml` members list.

## Risk register (see each stage for detail)

| # | Risk | Where | Mitigation |
| --- | --- | --- | --- |
| 1 | `crypto_box::SalsaBox` must be byte-identical to tweetnacl | Stage 1 | Round-trip test vs captured TS ciphertext; pin the version |
| 2 | `async-tungstenite` + `futures-rustls` against the real relay | Stage 1 | Exercise in the CLI example before any WezTerm code |
| 3 | Terminal emulator thread-safety | Stage 2 | Parse off-thread, `perform_actions` on main thread only |
| 4 | Relay UTF-8 sniff misrouting a binary frame as JSON | Stage 1 | Try `serde_json` first; use leading opcode byte `0x01–0x05` as discriminator |
| 5 | `Domain` semantics for a non-PTY backend | Stage 2 | Implement `attach`+render first; stub `spawn`/`split_pane` |
| 6 | Agent pane input focus vs async permission interrupts | Stage 3 | Explicit mode state machine (scroll / compose / permission) |
| 7 | Reconnect / dead-pane pruning | Stage 2/4 | `Reconnectable` pattern; fresh ephemeral key on reconnect |
