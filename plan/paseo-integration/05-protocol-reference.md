# 05 — Protocol reference (appendix)

The exact Paseo wire spec, verified against the Paseo source tree at
`/Users/slt/Projects/paseo`. Every stage cites this doc rather than repeating it.
Line numbers are against the tree as explored on 2026-07-18; re-verify if the
Paseo repo has moved. **The Zod schemas in `packages/protocol/src/messages.ts`
are the source of truth**; this is a faithful summary for building Rust structs.

All Rust structs deserializing daemon output MUST follow the forward-compat rules
in [§7](#7-forward-compatibility-rules).

---

## 1. Pairing offer URL

`packages/protocol/src/connection-offer.ts`.

- Form: `https://app.paseo.sh/#offer=<base64url>`. Marker constant is `#offer=`
  (`:37`); `parseConnectionOfferFromUrl` (`:54`) takes everything after it.
- The fragment payload is **base64url** (`-`→`+`, `_`→`/`, re-pad to /4), then
  UTF-8 JSON (`decodeBase64UrlToUtf8`, `:24`).
- Decoded shape `ConnectionOfferV2` (`:9`):
  ```jsonc
  {
    "v": 2,
    "serverId": "<non-empty string>",
    "daemonPublicKeyB64": "<32-byte X25519 pubkey, STANDARD base64 w/ padding>",
    "relay": { "endpoint": "host:port", "useTls": true }
  }
  ```

> ⚠️ Two different base64 alphabets: the **outer fragment** is base64url; the
> **inner `daemonPublicKeyB64`** is standard base64. Do not conflate.

---

## 2. Relay WebSocket URL

`packages/protocol/src/daemon-endpoints.ts` `buildRelayWebSocketUrl` (`:176`).

- Scheme: `wss` if `relay.useTls` else `ws`. Host/port from `relay.endpoint`
  (IPv6 in brackets). Path `/ws`.
- Query: `serverId=<id>`, `role=client`, `v=2`.
- The **client does not send a `connectionId`** — the relay assigns one and pairs
  it with the daemon socket. The relay forwards application frames **verbatim**
  (`packages/relay/src/cloudflare-adapter.ts` `webSocketMessage` `:448`), so the
  client speaks E2EE directly over the raw WS; there is no relay-level envelope
  around app frames.
- Default relay endpoint `relay.paseo.sh:443` (`:19`).

---

## 3. E2EE handshake (relay path only)

`packages/relay/src/crypto.ts` + `packages/relay/src/encrypted-channel.ts`
(client = initiator, `createClientChannel` `:120`).

### Primitives
- Key exchange: **X25519** (`nacl.box.before`, `crypto.ts:124`) → 32-byte shared
  key (X25519 ECDH + HSalsa20). Equivalent to `crypto_box::SalsaBox::new(&their_pub, &our_secret)`.
- AEAD: **XSalsa20-Poly1305** (`nacl.box.after`/`open.after`, `:135`/`:150`).
- Frame bundle: `[24-byte random nonce][ciphertext(+16-byte Poly1305 tag)]`
  (`:131`). Nonce length 24, key length 32.

### Sequence
1. Client generates a **fresh ephemeral X25519 keypair per connection**
   (`:125`), imports `daemonPublicKeyB64` (standard base64 → 32 bytes), derives
   the shared box.
2. Client sends a **plaintext WS text frame**:
   `{"type":"e2ee_hello","key":"<our-pubkey standard-base64>"}` (`:132`,`:143`).
   The TS client resends every 1000 ms until open (`:163`); a Rust client may
   send once and optionally retry.
3. Daemon replies with a **plaintext WS text frame** `{"type":"e2ee_ready"}`
   (`:72`,`:307`). Only on receipt does the channel go `handshaking → open`
   (`:302`). **No application frames before `open`** — sends are buffered (`:385`)
   and flushed after open (`:311`).
4. After open, every application frame:
   `encrypt(sharedKey, bytes)` → `[nonce||ciphertext]` → **standard base64** →
   WS **text** frame (`:385`, `arrayBufferToBase64` = standard alphabet,
   `packages/relay/src/base64.ts:3`).
5. Inbound after open (`handleMessage` `:302`):
   - If the text parses as JSON `e2ee_hello`/`e2ee_ready` → ignore.
   - Any *other* plaintext JSON → fatal "plaintext frame on encrypted channel".
   - Else base64-decode (accepts url-safe or standard, re-pads, `:353`) →
     split nonce(24)/ciphertext → `box.open.after` decrypt →
     **UTF-8 sniff** (`decrypt`, `crypto.ts:142`): valid UTF-8 ⇒ a JSON envelope
     string; invalid UTF-8 ⇒ raw bytes (a binary terminal/file frame).

> ⚠️ The UTF-8 sniff distinguishes encrypted JSON from encrypted binary on the
> relay path. Safer Rust implementation: try `serde_json::from_slice` first; on
> failure treat as a binary frame. Additionally use the **leading byte**:
> `0x01–0x05` = terminal frame (§6), other opcodes = file-transfer
> (`packages/protocol/src/binary-frames/demux.ts:16`). Terminal Output payloads
> can theoretically be valid UTF-8, so do not rely on the sniff alone.

### Crypto crate
`crypto_box::SalsaBox` (RustCrypto) is the exact NaCl `box` match. **Add a
round-trip test** encrypting/decrypting against a ciphertext captured from the TS
`encrypt()` to guarantee byte parity, and pin the crate version. See
[06-testing](06-testing-and-verification.md#e2ee-parity-test).

---

## 4. Session envelopes & handshake

`packages/protocol/src/messages.ts`; reference client
`packages/client/src/daemon-client.ts`.

- Top-level inbound (client→daemon): `ping` | `hello` | `recording_state` |
  `session` (`WSInboundMessageSchema` `:5595`). Outbound (daemon→client):
  `pong` | `session` (`:5602`).
- `session` wraps the rich union: `{ "type":"session", "message": <SessionMessage> }`
  (`:5584`). All RPCs and pushes below travel inside `message`.
- **Hello** (`WSHelloMessageSchema` `:5556`; sent by `sendHelloMessage`
  `daemon-client.ts:5124`):
  ```jsonc
  { "type":"hello", "clientId":"<stable uuid>", "clientType":"cli",
    "protocolVersion":1, "appVersion":"<optional>",
    "capabilities": { "custom_mode_icons":true, "reasoning_merge_enum":true,
                      "terminal_reflowable_snapshot":true, "provider_subagents":true,
                      "project_updates":true /* keys from client-capabilities.ts */ } }
  ```
- **server_info**: after hello (and, on relay, after e2ee open), the daemon sends
  a `session`→`status` message with payload `ServerInfoStatusPayloadSchema`
  (`:2659`): `{ status:"server_info", serverId, hostname?, version?, desktopManaged?,
  capabilities?, features? }`. Treat `connected` as true only after this arrives.
  Every `features.*` is an **optional bool defaulting false**; the object is open
  (unknown flags appear over time). Gate newer RPCs on the relevant flag.
- **Liveness ping**: client sends top-level `{"type":"ping"}` (`daemon-client.ts:1933`);
  daemon replies top-level `{"type":"pong"}` (no payload, `:5552`). (A separate
  *session-level* ping with `requestId`/`clientSentAt` also exists — `:1852` — but
  the top-level one is the heartbeat.)
- **RPC correlation**: every request carries a client-generated `requestId`.
  Responses are `{type:"…", payload:{ requestId, … }}`; match on `payload.requestId`.
  Response types are exact (`*_response` / `*.response`) or matched by the generic
  suffix rule (ends with `_response`/`.response`/`/response`, `daemon-client.ts:199`).
- **Errors**: `RpcErrorMessageSchema` (`:2767`):
  `{ type:"rpc_error", payload:{ requestId, requestType?, error:string, code? } }`.
  Resolve the matching pending request as an error.

---

## 5. Agent RPCs & push messages

Requests (top-level fields inside the session message; `messages.ts`):

| RPC `type` | Key fields | Response `type` |
| --- | --- | --- |
| `fetch_agents_request` (`:1007`) | `scope?`, `filter?{labels,statuses,includeArchived,…}`, `sort?[]`, `page?{limit≤200,cursor?}`, `subscribe?` | `fetch_agents_response` (`:3104`) |
| `fetch_agent_request` (`:1102`) | `agentId` (id / unique prefix / exact title) | `fetch_agent_response` |
| `fetch_agent_timeline_request` (`:1363`) | `agentId`, `direction?`(`tail`/`before`/`after`), `cursor?{epoch,seq}`, `limit?`(0=all), `projection?` | `fetch_agent_timeline_response` (`:3385`) |
| `agent.timeline.set_subscription.request` (`:1391`) | `agentIds:[]` | `agent.timeline.set_subscription.response` (`:3493`) |
| `send_agent_message_request` (`:1109`) | `agentId`, `text`, `messageId?`, `images?[]`, `attachments?` | `send_agent_message_response` |
| `create_agent_request` (`:1247`) | `config`, `workspaceId?`, `initialPrompt?`, `labels`, … | (`agent_created` / `agent_create_failed` status) |
| `agent_permission_response` (`:1598`) | `agentId`, `requestId`, `response`(union, §5b) | (resolution push) |

`fetch_agents_response.payload` (`:3104`): `{ requestId, subscriptionId?,
entries:[{ agent: AgentSnapshot, project }], pageInfo:{ nextCursor, prevCursor, hasMore } }`.

`fetch_agent_timeline_response.payload` (`:3385`): `{ requestId, agentId,
agent: AgentSnapshot|null, direction, projection, epoch, reset, staleCursor, gap,
window:{minSeq,maxSeq,nextSeq}, startCursor|null, endCursor|null, hasOlder,
hasNewer, entries:[AgentTimelineEntry], error|null }`. Page backward with
`direction:"before"` + `cursor:startCursor`; watch `hasOlder`/`hasNewer`.
`AgentTimelineEntry` (`:3375`): `{ provider, item: AgentTimelineItem, timestamp,
seqStart, seqEnd, sourceSeqRanges, collapsed }`.

### 5a. Push messages
- `agent_update` (`:3050`): `{ payload: {kind:"upsert", agent, project?} | {kind:"remove", agentId} }` — discriminate on `kind`.
- `agent_stream` (`:3065`): `{ payload:{ agentId, event: AgentStreamEvent, timestamp, seq?, epoch? } }`.
  `AgentStreamEvent` (`:605`) discriminates on `type`: `thread_started` |
  `turn_started` | `turn_completed`(usage?) | `turn_failed`(error,code?) |
  `turn_canceled`(reason) | `timeline`(item) | `permission_requested`(request) |
  `permission_resolved`(requestId, resolution) | `attention_required`(reason, timestamp, shouldNotify, notification?).
  Every event carries `provider: string` (open string, not enum).
- `agent_permission_request` (`:3753`): `{ payload:{ agentId, request: AgentPermissionRequest } }`.
- `agent_permission_resolved` (`:3761`).

### 5b. Permission request/response shapes
`AgentPermissionRequest` (`packages/protocol/src/agent-types.ts` around `:380`):
`{ id, provider, name, kind:"tool"|"plan"|"question"|"mode"|"other", title?,
description?, input?, detail?: ToolCallDetail, suggestions?,
actions?:[{id,label,behavior:"allow"|"deny",variant?,intent?}], metadata? }`.
Pending ones are also mirrored on `AgentSnapshot.pendingPermissions[]`.

`AgentPermissionResponse` (`agent-types.ts:432`) — **discriminated on `behavior`**:
- `{ behavior:"allow", selectedActionId?, updatedInput?, updatedPermissions? }`
- `{ behavior:"deny", selectedActionId?, message?, interrupt?:bool }`

### 5c. Timeline item variants
`AgentTimelineItemPayloadSchema` (`:568`) — a plain `z.union`, switch on `type`:
- `user_message {text, messageId?}`
- `assistant_message {text, messageId?}`
- `reasoning {text}`
- `todo {items:[{text,completed}]}`
- `error {message}`
- `compaction {status:"loading"|"completed", trigger?, preTokens?}`
- `tool_call` — nested union on `status` (`:530`): base `{type:"tool_call",
  callId, name, detail: ToolCallDetail, metadata?}` × status
  `running`/`completed`/`canceled` (`error:null`) or `failed` (non-null `error`).

`ToolCallDetail` (`:431`) — **discriminatedUnion on `type`**: `worktree_setup`,
`shell`(command,cwd,output,exitCode), `read`, `edit`(unifiedDiff), `write`,
`search`, `fetch`, `sub_agent`, `plain_text`, `plan`, `unknown`. Keep `unknown`
as the catch-all; add a Rust `Unknown(Value)` fallback for future variants.

### 5d. AgentSnapshot (fields the GUI surfaces)
`AgentSnapshotPayload` (`:686`): `{ id, provider, cwd, workspaceId?, model|null,
status, title|null, labels, pendingPermissions[], currentModeId, availableModes,
createdAt, updatedAt, lastUserMessageAt, requiresAttention?, attentionReason?,
attentionTimestamp?, archivedAt?, providerUnavailable?, capabilities, runtimeInfo?,
lastUsage?, lastError? }`. `AgentStatus` (`packages/protocol/src/shared/agent-lifecycle.ts`):
`initializing` | `idle` | `running` | `error` | `closed`.

---

## 6. Terminal RPCs & binary frames

JSON RPCs (`messages.ts`; methods in `daemon-client.ts`):

| RPC `type` | Fields | Response |
| --- | --- | --- |
| `list_terminals_request` | `cwd?`, `workspaceId?` | `list_terminals_response` `{cwd?, terminals:[TerminalInfo], requestId}` (`:4805`) |
| `create_terminal_request` (`daemon-client.ts:4668`) | `cwd`, `name?`, `agentId?`, `command?`, `args?`, `workspaceId?`, `size?{rows,cols}` | `create_terminal_response` `{terminal|null, error|null, requestId}` (`:4822`) |
| `subscribe_terminal_request` (`:2280`) | `terminalId`, `restore?{mode:"live"|"visible-snapshot"|"full-snapshot", scrollbackLines?, size?}` | `subscribe_terminal_response` (`:4840`) — see below |
| `unsubscribe_terminal_request` (`:2298`) | `terminalId` | (fire-and-forget) |
| `kill_terminal_request` | `terminalId` | `kill_terminal_response` `{terminalId, success, requestId}` (`:4857`) |

`subscribe_terminal_response.payload` (`:4840`) is an **untagged union**:
- success `{ terminalId, slot: int 0..255, error: null, requestId }`
- failure `{ terminalId, error: string, requestId }`

Discriminate structurally on whether `slot`/`error` is present (there is no tag).
The **slot** byte multiplexes this terminal's binary frames — record
`terminalId → slot` and `slot → terminalId → Sender`.

`TerminalInfo` (`:4756`): `{ id, name, cwd, workspaceId?, title?, activity? }`.

Push: `terminals_changed` `{payload:{cwd, terminals}}` (`:4814`);
`terminal_stream_exit` `{payload:{terminalId}}` (`:4876`) — drop the slot mapping.

### Binary frame format
`packages/protocol/src/binary-frames/terminal.ts` (exact):

- Opcodes (`:9`): `Output=0x01`, `Input=0x02`, `Resize=0x03`, `Snapshot=0x04`,
  `Restore=0x05`.
- Layout (`encodeTerminalStreamFrame :64`, `decodeTerminalStreamFrame :77`):
  ```
  byte 0: opcode
  byte 1: slot & 0xff
  bytes 2..: payload
  ```
  Minimum length 2; unknown opcode ⇒ drop the frame.
- Payloads:
  - `Output`(daemon→client), `Input`(client→daemon), `Restore`(daemon→client):
    **raw PTY bytes**, no structure. `Restore` is replayed scrollback; treat like
    `Output` (`packages/client/src/terminal-stream-router.ts:101`).
  - `Resize`(client→daemon): UTF-8 JSON `{"rows":int,"cols":int}` (`:106`).
  - `Snapshot`(daemon→client): UTF-8 JSON `TerminalState` (`:92`).
    `TerminalStateSchema` (`messages.ts:4789`): `{rows, cols, grid:[[TerminalCell]],
    scrollback:[[TerminalCell]], cursor:{row,col,hidden?,style?,blink?}, title?,
    gridWrapped?:[bool], scrollbackWrapped?:[bool]}`; `TerminalCell` (`:4765`):
    `{char, fg?, bg?, fgMode?, bgMode?, bold?, italic?, underline?, dim?, inverse?,
    strikethrough?}`.

### Transport nuance
- **Local WS**: binary frames are real WS **binary** frames
  (`ws.binaryType="arraybuffer"`; `daemon-client.ts:1539` sends the `Uint8Array`).
  WS binary ⇒ decode frame; WS text ⇒ JSON envelope.
- **Relay E2EE**: binary frames are `encrypt`ed and sent as **base64 text**, and
  arrive as base64 text decrypting to bytes. After decrypt, run the §3 sniff
  (JSON vs binary, plus leading-opcode discriminator).

---

## 7. Forward-compatibility rules

From `CLAUDE.md`, `docs/protocol-validation.md`, `docs/rpc-namespacing.md`:

- **Ignore unknown fields.** The daemon adds fields over time (see `COMPAT(...)`
  notes). Use `#[serde(default)]` on every optional; never `#[serde(deny_unknown_fields)]`.
  For `.passthrough()` objects (`server_info.features`, `capabilities`) keep an
  open map (`#[serde(flatten)] extra: serde_json::Map<String,Value>` or just
  tolerate extras).
- **Enums are open.** `provider` is `string`, not a closed set. Tagged unions
  (`type`/`kind`/`status`/`behavior`) need a catch-all variant
  (`#[serde(other)]` or `Unknown(Value)`) so a new variant doesn't fail parsing.
- **Tagged unions** discriminate on a literal — use `#[serde(tag="…")]` for
  envelope `type`, `agent_update.kind`, `tool_call.status`, `ToolCallDetail.type`,
  `AgentPermissionResponse.behavior`.
- **Untagged unions** (no tag): `AgentTimelineItem` (switch on `type` field which
  is present but the union is plain) and `subscribe_terminal_response` (match on
  `slot` presence). Model the latter `#[serde(untagged)]` or as an enum decoded by
  hand.
- **Optional-with-default**: many fields default (`labels`→`{}`, `attachments`→`[]`).
  Mirror with `#[serde(default)]`.
- **RPC names**: dotted-with-direction (`agent.timeline.set_subscription.request`)
  and flat snake_case (`fetch_agents_request`) both exist. Send the exact string
  the schema declares; match responses by exact type or the generic suffix rule.
- **Gate features** on `server_info.features.*` (default false) before using
  newer RPCs.

---

## 8. Local connection (subset of relay path)

`packages/protocol/src/daemon-endpoints.ts`; auth in `SECURITY.md`.

- URL: `buildDaemonWebSocketUrl(endpoint, {useTls})` → `ws://host:port/ws` or
  `wss://…` (`:169`). Default `127.0.0.1:6767`.
- Direct-TCP URI form: `tcp://host:port?ssl=true&password=…` (`parseConnectionUri`
  `:85`); `ssl=true` ⇒ wss.
- Auth (optional shared secret): HTTP `Authorization: Bearer <password>` **and**
  WS subprotocol `Sec-WebSocket-Protocol: paseo.bearer.<password>`
  (`daemon-client.ts:1207`). Local default is no auth.
- From `hello` onward the local path is identical to the relay path; skip the
  E2EE layer and use raw WS binary frames instead of base64-text.
