# Stage 1 — `paseo-client` crate

**Output:** a new workspace crate that speaks the Paseo daemon protocol
(relay/E2EE first, local second), exposing an executor-agnostic async API the
WezTerm layers consume. **Gated by a standalone CLI example** that connects to a
real daemon and streams a terminal — proving the hardest parts (E2EE parity +
`async-tungstenite`/`futures-rustls` against the relay) with no WezTerm code in
the way.

Read alongside [05-protocol-reference.md](05-protocol-reference.md) — every wire
detail lives there; this doc is structure, API, and sequencing.

## Why it's its own crate, executor-agnostic

- It pulls network + crypto deps (`async-tungstenite`, `futures-rustls`,
  `crypto_box`). Keeping it separate keeps those out of `mux`/`config`.
- It is **plain `async fn`s + `futures`/`flume` channels** — it never spawns or
  owns a runtime. The embedder drives it: WezTerm via `promise::spawn::spawn`,
  the example via `smol::block_on`. This is the single most important constraint;
  see [00-architecture §Async](00-architecture.md#async--threading-model-the-load-bearing-decision).

## Module layout

```
paseo-client/
  Cargo.toml
  src/
    lib.rs                 // re-exports; PaseoClient; ConnectionState
    error.rs               // PaseoError (thiserror)
    offer.rs               // ConnectionOffer, parse_offer_url, relay/daemon URL builders
    transport/
      mod.rs               // trait Transport; Frame { Json(String), Binary(Vec<u8>) }
      local_ws.rs          // async-tungstenite ws/wss to /ws (+ optional bearer)
      relay_e2ee.rs        // async-tungstenite to relay wrapped in e2ee::Channel
    e2ee.rs                // SalsaBox handshake, encrypt/decrypt, UTF-8 sniff
    envelope.rs            // WsInbound/WsOutbound, Hello, session wrapper, RpcError
    protocol/
      mod.rs
      agents.rs            // fetch_agents/fetch_agent/timeline/send/permission/create
      terminals.rs         // list/create/subscribe/unsubscribe/kill; TerminalInfo; TerminalState
      timeline_items.rs    // AgentTimelineItem + ToolCallDetail unions
      stream_events.rs     // AgentStreamEvent union; AgentUpdate
      snapshot.rs          // AgentSnapshot; TerminalInfo; ServerInfo/features
    binary_frames.rs       // opcodes; encode/decode terminal frames; demux by 1st byte
    client.rs              // connect loop, request/response correlation, dispatch
    events.rs              // DaemonEvent (GUI-facing), TerminalStreamEvent, TerminalHandle
  examples/
    connect.rs            // the Stage-1 gate: smol::block_on driver
```

## Dependencies (smol family, no tokio)

New:
- `async-tungstenite` — WebSocket; drive it over `futures` streams. Use the TLS
  connector from `futures-rustls` (or its `async-native-tls` feature) rather than
  tokio TLS.
- `futures-rustls` — rustls over `futures` `AsyncRead`/`AsyncWrite`. Reuses the
  `rustls` already in this repo's lockfile.
- `crypto_box` — NaCl `box` (X25519 + XSalsa20-Poly1305, 24-byte nonce). Pin the
  version; parity-tested (see gate).
- `async-broadcast` — the global `DaemonEvent` bus (multiple panels subscribe).
- `async-trait` — for the `Transport` trait.

Reuse from this repo's `Cargo.lock` (do not add fresh copies): `rustls`,
`futures`/`futures-util`, `flume`, `serde` + `serde_derive`, `serde_json`,
`base64`, `rand`/`getrandom`, `url`, `zeroize`, `bytes`, `anyhow`, `thiserror`.

> Before adding each new dep, check `cargo tree`/`Cargo.lock` — some may already
> be present transitively at a compatible version.

## Transport abstraction

```rust
pub enum Frame { Json(String), Binary(Vec<u8>) }

#[async_trait]
pub trait Transport: Send {
    async fn send_text(&self, s: String) -> Result<()>;
    async fn send_binary(&self, b: Vec<u8>) -> Result<()>; // local: WS binary; relay: encrypt→b64 text
    fn incoming(&self) -> flume::Receiver<Frame>;          // decrypted + demuxed
    async fn close(&self);
}
```

- **`LocalWsTransport`**: `send_binary` → WS Binary, `send_text` → WS Text;
  inbound WS Binary → `Frame::Binary`, WS Text → `Frame::Json`. Optional bearer:
  set `Authorization: Bearer <pw>` header + `paseo.bearer.<pw>` subprotocol.
- **`RelayE2eeTransport`**: wraps a raw relay WS with `e2ee::Channel`. Performs
  the `e2ee_hello`/`e2ee_ready` handshake before surfacing readiness; encrypts
  both `send_text` and `send_binary` into base64 text; on inbound, decrypts then
  UTF-8-sniffs (try `serde_json` first; leading opcode `0x01–0x05` = terminal)
  into `Frame::Json`/`Frame::Binary`.

`PaseoClient` is written against the trait only — it doesn't know which transport
it has.

## E2EE module (`e2ee.rs`)

Mirror `packages/relay/src/{crypto,encrypted-channel}.ts` exactly
([05 §3](05-protocol-reference.md#3-e2ee-handshake-relay-path-only)):

```rust
pub struct Channel { sbox: SalsaBox, /* state */ }

impl Channel {
    // fresh ephemeral keypair per connection
    pub fn new(daemon_pub_b64: &str) -> Result<(Self, HelloFrame)>; // returns e2ee_hello to send
    pub fn on_ready(&mut self);                                     // e2ee_ready received → open
    pub fn encrypt(&self, plaintext: &[u8]) -> String;             // [nonce||ct] → std base64
    pub fn decrypt(&self, b64: &str) -> Result<Frame>;             // → Json(String) | Binary(Vec<u8>)
}
```

- Ephemeral keypair via `crypto_box::SecretKey::generate` (use the repo's `rand`).
- `daemon_pub_b64` is **standard** base64 → 32 bytes → `PublicKey`.
- Nonce: 24 random bytes per frame.
- `decrypt`: base64-decode (accept url-safe or standard, re-pad) → split
  nonce/ct → `sbox.decrypt` → `serde_json::from_slice` success ⇒ `Json`, else
  `Binary`.

## RPC correlation & dispatch (`client.rs`)

- Each request builds a `requestId` (uuid). Register a `flume::bounded(1)` (or
  `futures::channel::oneshot`) sender in `HashMap<String, Sender<Result<Value>>>`
  keyed by `requestId`, send the `session` envelope, await the receiver.
- The **read loop** (one long-lived task) pulls `transport.incoming()`:
  - `Frame::Json` → parse the top-level envelope. `pong` → heartbeat. `session` →
    inspect `message.type`/`message.payload.requestId`:
    - matches a pending `requestId` (or `rpc_error`) → complete that oneshot.
    - `server_info` → set `ConnectionState::Connected`.
    - a push (`agent_update`, `agent_stream`, `agent_permission_request`,
      `terminals_changed`, `terminal_stream_exit`) → map to a `DaemonEvent` and
      `broadcast`.
  - `Frame::Binary` → `decode_terminal_stream_frame`; look up `slot → terminalId`
    and forward to that terminal's `Sender` as `TerminalStreamEvent`.
- Timeouts: default 60 s per RPC (match the reference client).

## Public API (`lib.rs` / `events.rs`)

```rust
pub enum ConnectionState { Connecting, Handshaking, Connected, Disconnected(String) }

pub enum DaemonEvent {
    AgentUpsert(AgentSnapshot),
    AgentRemove(String),
    AgentStream { agent_id: String, event: AgentStreamEvent, seq: Option<u64> },
    PermissionRequest { agent_id: String, request: AgentPermissionRequest },
    TerminalsChanged { cwd: Option<String>, terminals: Vec<TerminalInfo> },
    TerminalExit(String),
}

pub enum TerminalStreamEvent { Output(Vec<u8>), Restore(Vec<u8>), Snapshot(TerminalState) }

impl PaseoClient {
    pub async fn connect_relay(offer: ConnectionOffer, client_id: String, caps: Capabilities) -> Result<PaseoClient>;
    pub async fn connect_local(host_port: String, password: Option<String>, client_id: String, caps: Capabilities) -> Result<PaseoClient>;

    pub fn connection_state(&self) -> async_broadcast::Receiver<ConnectionState>; // or watch-like
    pub fn events(&self) -> async_broadcast::Receiver<DaemonEvent>;
    pub fn server_info(&self) -> ServerInfo; // features gating

    // agents
    pub async fn fetch_agents(&self, opts: FetchAgentsOpts) -> Result<FetchAgentsPage>;
    pub async fn fetch_agent(&self, agent_id: &str) -> Result<AgentSnapshot>;
    pub async fn fetch_agent_timeline(&self, agent_id: &str, opts: TimelineOpts) -> Result<TimelinePage>;
    pub async fn set_timeline_subscription(&self, agent_ids: Vec<String>) -> Result<()>;
    pub async fn send_agent_message(&self, agent_id: &str, text: &str, opts: SendOpts) -> Result<()>;
    pub async fn respond_permission(&self, agent_id: &str, request_id: &str, resp: PermissionResponse) -> Result<()>;
    pub async fn create_agent(&self, req: CreateAgentReq) -> Result<AgentSnapshot>;

    // terminals
    pub async fn list_terminals(&self, cwd: Option<String>) -> Result<Vec<TerminalInfo>>;
    pub async fn create_terminal(&self, cwd: String, opts: CreateTerminalOpts) -> Result<TerminalInfo>;
    pub async fn subscribe_terminal(&self, terminal_id: &str, restore: Option<RestoreOpts>) -> Result<TerminalHandle>;
    pub async fn kill_terminal(&self, terminal_id: &str) -> Result<()>;
}

pub struct TerminalHandle { /* ... */ }
impl TerminalHandle {
    pub fn output(&self) -> flume::Receiver<TerminalStreamEvent>; // byte order matters — dedicated stream
    pub async fn send_input(&self, bytes: Vec<u8>);               // Input frame (0x02)
    pub async fn resize(&self, rows: u16, cols: u16);            // Resize frame (0x03) + JSON
    pub async fn unsubscribe(self);
}
```

- Global events → `async-broadcast` (fan-out to every panel).
- Terminal bytes → a dedicated per-terminal `flume` channel (order-preserving;
  unbounded to avoid stalling the PTY, or bounded with explicit backpressure).
- The client owns the `slot → terminalId → Sender` map.

## Connect sequence (relay)

```text
offer  = parse_offer_url(url)                       // base64url fragment → ConnectionOfferV2
ws_url = build_relay_ws_url(offer.relay.endpoint, offer.relay.useTls, offer.serverId)
ws     = async_tungstenite::connect(ws_url) over futures-rustls
(chan, hello) = e2ee::Channel::new(offer.daemonPublicKeyB64)
ws.send_text(hello)                                 // {"type":"e2ee_hello","key":...}
loop { f = ws.recv(); if json(f).type == "e2ee_ready" { chan.on_ready(); break } }  // ignore else
send_session(hello_msg)                             // {"type":"hello",clientType:"cli",protocolVersion:1,caps}
loop { m = recv(); if server_info(m) { state = Connected; break } }
spawn read_loop()                                   // dispatch → waiters / events / terminal streams
```

Local: identical from `hello` onward; skip e2ee; add optional bearer; raw WS
binary frames.

## Definition of done (Stage 1 gate)

`examples/connect.rs`, driven by `smol::block_on`:

```
cargo run -p paseo-client --example connect -- '<pairing-offer-url>'
# or: cargo run -p paseo-client --example connect -- --local 127.0.0.1:6767 [--password PW]
```

Must:
1. Connect (relay: full E2EE handshake) and print the `server_info` (serverId,
   version, features).
2. `fetch_agents` and `list_terminals`, printing a summary of each.
3. `subscribe_terminal` on one terminal and stream its `Output` bytes to stdout
   for a few seconds; send a test `Input` (e.g. `"echo hi\r"`) and observe the
   echo; send a `resize`.
4. Exit cleanly (unsubscribe, close).

Unit tests:
- Binary frame codec round-trip (`binary_frames.rs`) against known byte layouts.
- **E2EE parity test** — see [06-testing](06-testing-and-verification.md#e2ee-parity-test).
- Offer URL parsing (base64url fragment, standard-base64 inner key).
- Envelope (de)serialization for a handful of captured messages.

Only once this gate is green do we start Stage 2.

## Risks specific to this stage

- **crypto_box ↔ tweetnacl parity** (risk #1). Pin + parity test.
- **`async-tungstenite`/`futures-rustls` vs the relay** (risk #2). The example is
  the proof.
- **UTF-8 sniff misrouting** (risk #4). Try `serde_json` first; use the leading
  opcode byte as the primary terminal-vs-json discriminator.
- **base64 alphabet mix-ups** — outbound standard+padded; inbound accept both.
  The offer fragment is base64url; the inner daemon key is standard base64.
