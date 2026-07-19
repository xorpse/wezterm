# Paseo → WezTerm integration

Bring your [Paseo](https://paseo.sh) agent sessions and terminals into WezTerm as
first-class tabs/panes, alongside local tabs. Connect to a remote (relay/E2EE) or
local Paseo daemon the way the web UI does, and interact with sessions natively.

This directory is the implementation plan, split into stages. Each stage doc is
self-contained enough to hand to an implementer (or an agent) working in a
worktree of this repo.

## Decisions (fixed)

- **Both surfaces, agents-first.** Structured agent sessions are the headline;
  PTY terminals come along because they map cleanly onto panes.
- **Native Rust in this fork.** Mirrors the existing `git-review` feature; no Lua
  plugin layer, no external sidecar process.
- **Remote relay (E2EE) first.** The priority path is a remote daemon over the
  encrypted relay; the local `127.0.0.1:6767` path is a strict subset added
  alongside.
- **Full structured agent interaction.** Tool-call cards, diffs, reasoning, a
  prompt composer, and inline permission approve/deny — not a plain text dump.
- **smol/futures, no tokio.** Stay in WezTerm's own async family
  (`async_executor`/`smol`/`futures`/`flume`). Introducing tokio would mean a
  second runtime and the classic cross-runtime deadlock; nothing in the Paseo
  protocol needs it.

## The two surfaces (this is why the design is shaped the way it is)

Paseo exposes two fundamentally different things over one `/ws` WebSocket:

| Surface | What it is | WezTerm representation |
| --- | --- | --- |
| **Terminal** | Raw PTY byte stream over a 2-byte-header binary frame protocol | A real WezTerm pane with **local** `wezterm_term::Terminal` emulation, hosted in a custom `PaseoDomain` |
| **Agent** | A *structured* message/turn timeline (assistant text, reasoning, tool-call cards, diffs) + prompt input + permission prompts | A **custom-rendered** virtual pane, cloned from `ReviewPane` |

## Stages

| Doc | Stage | Output |
| --- | --- | --- |
| [00-architecture.md](00-architecture.md) | — | System design, crate layering, async/threading model, data flow, risks |
| [01-paseo-client-crate.md](01-paseo-client-crate.md) | 1 | New `paseo-client` crate: connection, relay+E2EE, RPCs, streams. Gated by a standalone CLI example. |
| [02-terminal-panes.md](02-terminal-panes.md) | 2 | New `paseo-mux` crate: `PaseoTerminalPane` + `PaseoDomain` (attach + render first) |
| [03-agent-pane.md](03-agent-pane.md) | 3 | `wezterm-gui/src/paseo/`: `PaseoAgentPane` (transcript, composer, permissions) |
| [04-config-keybindings-discovery.md](04-config-keybindings-discovery.md) | 4 | `PaseoDaemon` config, key assignments, domain registration, launcher/picker UX |
| [05-protocol-reference.md](05-protocol-reference.md) | ref | Exact wire reference cited by every stage (offer, handshake, envelopes, RPCs, binary frames, forward-compat) |
| [06-testing-and-verification.md](06-testing-and-verification.md) | ref | Per-stage test strategy, the E2EE parity test, end-to-end manual verification |

## Status (implemented)

All stages are implemented on `feature/paseo-integration` and validated live
against real daemons (enki local + a relay/E2EE Mac daemon):

- **Stage 1 — `paseo-client`**: relay+E2EE and local transports, RPCs, terminal
  binary frames, agent timeline/stream, controls, create_agent. E2EE parity
  verified live.
- **Stage 2 — `paseo-mux`**: `PaseoTerminalPane` (real terminal panes) +
  `PaseoDomain` (attach, and **spawn** — create terminals from WezTerm).
- **Stage 3 — `PaseoAgentPane`**: structured transcript (messages, reasoning,
  tool cards with targets, diffs); **fixed-layout custom scroll with a pinned
  composer/status footer** (mouse + keyboard); compose/send; inline permission
  approve/deny; agent controls (stop, mode/model/effort) with live
  `agent_update` sync; **in-pane agent picker**; tab status/attention glyph.
- **Stage 4 — config/discovery**: `PaseoDaemon` config, domain registration,
  launcher attach, `OpenPaseoAgentPane` (open existing / create new). Agent
  panes **auto-connect** (no attach-first). Panes prune on terminal exit /
  disconnect.
- **Stage 5 — hub picker & projects**: the in-pane picker is a hub that lists
  open agents and known workspaces and offers create actions — new agent in a
  workspace, new directory + agent, clone GitHub repo + agent — backed by
  `fetch_workspaces` / `project.add` / `project.create_directory` /
  `project.github.clone` client RPCs.
- **Stage 6 — tab groups**: with `tab_bar_group_by_domain`, a vertical fancy tab
  bar inserts a labeled header before each run of tabs from a different mux
  domain, so each Paseo daemon's tabs cluster under their own header.
- **Stage 7 — unified git-review over the daemon**: the git-review
  `ReviewPane` is now the single diff renderer for both local repos and
  remote Paseo daemons. Its diff source is pluggable (`DiffSource::LocalGit`
  vs `Paseo`); a `paseo_source` converter turns the daemon's
  `subscribe_checkout_diff` stream into `git_review::GitDiffData`
  (reconstructing per-line numbers from hunk offsets), so file tree,
  collapse, navigation, comment anchoring, and mode cycling all work over
  the daemon. Pressing `d` in an agent pane opens a ReviewPane split for the
  agent's workspace; the standalone review command also detects a Paseo
  source pane. `WorkingTree`→uncommitted, `Branch`/`MergeBase`→base+baseRef;
  `send_paste` inserts into the composer. Daemon-bound gaps: staged-only
  mode folds to uncommitted, and oversized files can't be lazily expanded
  remotely (no working-tree per-file diff RPC).

Remaining polish (not blocking): timeline pagination for very long histories,
snapshot-grid restore, auto-reconnect with backoff, richer tool-card rendering.

## Recommended build order

1. **Stage 1 first, in full**, including the CLI example gate. It de-risks the
   hardest part (E2EE byte-parity + `async-tungstenite`/`futures-rustls` against
   the relay) with zero WezTerm code in the way.
2. **Stage 2** (terminals) before Stage 3 (agents): terminals validate the
   `Domain`/pane plumbing and the async→GUI bridge with a simpler payload (raw
   bytes) before the agent pane adds custom rendering + input modes on top.
3. **Stage 4** last, once there is something real to attach to and pick from.

Read [05-protocol-reference.md](05-protocol-reference.md) alongside Stages 1–3 —
it is the load-bearing wire spec; the stage docs reference it rather than
repeating it.

## Source repos

- This repo: `/Users/slt/Projects/wezterm` — the WezTerm fork, where all impl code lands.
- Paseo: `/Users/slt/Projects/paseo` — protocol source of truth (read-only reference).
  Key files: `packages/protocol/src/messages.ts`, `packages/protocol/src/binary-frames/terminal.ts`,
  `packages/protocol/src/connection-offer.ts`, `packages/relay/src/{crypto,encrypted-channel}.ts`,
  `packages/client/src/daemon-client.ts` (the reference client).
