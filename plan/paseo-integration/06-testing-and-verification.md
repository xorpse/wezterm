# 06 — Testing & verification

Per-stage test strategy plus the end-to-end checks. Follows Paseo's own testing
ethos (`docs/testing.md`): real dependencies over mocks, determinism, and
exercising the actual flow — but here the "real dependency" is a running Paseo
daemon you connect to.

## Prerequisites

- A Paseo daemon to test against. Local: `npm run dev` in the paseo repo (dev home
  under `.dev/paseo-home`), or a production daemon on `127.0.0.1:6767`.
- For the relay path: generate a pairing offer from the daemon host with
  `paseo daemon pair` (needs relay enabled). Keep it handy for the Stage-1 example.
- At least one live terminal and one agent session on that daemon to attach to
  (`paseo terminal create …`, `paseo run …`).

## Stage 1 — `paseo-client`

### Unit tests (fast, no daemon)
- **Binary frame codec** — round-trip `encode`/`decode` for each opcode; verify
  the 2-byte header `[opcode][slot]` and that Resize/Snapshot payloads are the
  exact JSON shapes from [05 §6](05-protocol-reference.md#binary-frame-format).
  Include a decode of a captured real frame.
- **Offer parsing** — a real `#offer=` URL decodes to `ConnectionOfferV2`; confirm
  the fragment is base64url while `daemonPublicKeyB64` is standard base64 (feed a
  known vector).
- **Envelope (de)serialization** — round-trip captured `hello`, `server_info`,
  `agent_stream`, `agent_update`, `subscribe_terminal_response` (both success and
  error arms), `rpc_error`. Assert forward-compat: an envelope with an unknown
  extra field and an unknown union variant still parses (unknown → catch-all).

### E2EE parity test
The load-bearing correctness test (risk #1). Guarantees `crypto_box::SalsaBox`
matches tweetnacl byte-for-byte.

1. In the paseo repo, write a one-off Node script using the same `nacl` the daemon
   uses (`packages/relay/src/crypto.ts`) to produce a fixture: given a fixed
   daemon keypair, a fixed client keypair, a fixed 24-byte nonce, and a known
   plaintext, output `{daemonPub, daemonSec, clientPub, clientSec, nonce,
   plaintext, ciphertextB64}`.
2. In `paseo-client`, a test loads the fixture and asserts:
   - `SalsaBox::new(daemonPub, clientSec).encrypt(nonce, plaintext)` → the same
     `[nonce||ciphertext]` bytes as the fixture (standard-base64 equal).
   - `SalsaBox::new(clientPub, daemonSec).decrypt(...)` → the original plaintext.
   - The shared key from both directions matches.
3. Also assert the **UTF-8 sniff**: a JSON plaintext decrypts to `Frame::Json`; a
   binary terminal frame (leading `0x01`) decrypts to `Frame::Binary` even if its
   payload bytes happen to be valid UTF-8 (force the opcode-byte discriminator).

Pin `crypto_box` to the tested version in `Cargo.toml`.

### Integration gate (needs daemon)
The `examples/connect.rs` acceptance from
[01 §Definition of done](01-paseo-client-crate.md#definition-of-done-stage-1-gate):
connect (relay **and** local), print `server_info`, list agents + terminals,
stream one terminal's bytes, send input + resize, exit clean. **Do not start Stage
2 until this passes on the relay path.**

## Stage 2 — terminal panes

- **Build**: `cargo build -p paseo-mux` then the full `cargo build` for the gui.
- **Manual E2E** (from [02](02-terminal-panes.md#definition-of-done-stage-2)):
  attach the domain from the launcher; a remote terminal opens as a pane; type and
  see correct echo; resize the pane and confirm the remote PTY reflows; open a
  second terminal as a second tab beside a local tab; kill it daemon-side and
  confirm the pane is pruned.
- **Thread-safety check** (risk #3): drive a high-throughput TUI (e.g. `htop`, a
  `yes`, or a full-screen editor) in the remote terminal and confirm no tearing,
  no panics, and that `perform_actions` only ever runs on the main thread (add a
  debug assertion `is_main_thread()` in the pane's `perform_actions` path during
  development).
- **Reconnect**: drop the connection (kill the relay link / daemon), confirm panes
  mark dead and prune; re-attach and confirm a fresh session works (fresh
  ephemeral key).

## Stage 3 — agent pane

- **Backfill + live** ([03](03-agent-pane.md#definition-of-done-stage-3)): open an
  agent; transcript backfills (assistant text, reasoning, ≥1 tool-call card, ≥1
  `edit` diff with review coloring); a sent prompt reaches the agent and the reply
  streams live.
- **Permissions**: trigger a tool that needs approval; confirm the inline prompt
  renders, `y` approves and `n` denies, and each takes effect daemon-side
  (observe via the Paseo app/CLI). Verify a prompt arriving **while composing**
  does not silently capture keystrokes (risk #6).
- **Diff reuse**: confirm `edit` tool calls render through the existing
  `git-review` diff parser/coloring, not a bespoke renderer.
- **Long transcript**: open a long session; scroll to top/bottom; confirm the
  incremental render cache keeps it responsive.

## Stage 4 — config, discovery

- Config with two `paseo_daemons` (relay + local) loads without error; an invalid
  duplicate `name` fails clearly.
- Launcher shows an Attach entry per daemon; attaching opens sessions.
- `connect_automatically = true` attaches at startup.
- `PaseoPicker` lists live agents + terminals and opens the selected one; the GUI
  thread never blocks while the list loads.
- `OpenPaseoAgentPane` splits an agent beside the current pane.
- Paseo tabs interleave with local tabs; titles + attention indicators are sane.

## Full end-to-end (all stages)

On a fresh build of the fork, against a **remote** daemon over the relay
(priority path):

1. Start WezTerm; the auto-connect daemon attaches; its sessions appear as tabs
   alongside local tabs.
2. Open a local shell tab, a Paseo terminal tab, and a Paseo agent tab; all three
   are usable simultaneously.
3. In the Paseo terminal: run a command, resize, scroll.
4. In the Paseo agent: send a prompt, watch tool-call cards + a diff render,
   approve a permission.
5. Kill and restore the connection; confirm graceful dead-marking and reconnect.
6. Repeat the terminal + agent checks against the **local** `127.0.0.1:6767`
   daemon to confirm the non-relay transport.

## Building & repo hygiene

- New crates: add `paseo-client` and `paseo-mux` to the workspace `members` in the
  root `Cargo.toml`; run `cargo build` for the workspace and `cargo build -p
  wezterm-gui`.
- Run `cargo fmt` (repo `.rustfmt.toml`) and `cargo clippy` before committing.
- Do **not** run WezTerm's full test suite blindly; build the specific crates you
  changed and run their unit tests (`cargo test -p paseo-client`).
- Keep the paseo repo read-only — it's the protocol reference, not a build input.

## What can't be unit-tested (call it out)

- The relay transport and E2EE against a **real** relay/daemon — only the
  Stage-1 example and manual E2E exercise it. The parity test covers the crypto
  math; it does not cover the live WS/TLS path. Treat the example as a required
  gate, not optional.
- Terminal emulation fidelity for exotic TUIs — covered by manual driving, not
  automated tests.
