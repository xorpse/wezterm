# Stage 4 — config, key assignments, discovery/attach

**Output:** the user-facing surface — a `paseo_daemons` config section, key
assignments, automatic domain registration, and a launcher/picker flow to attach
a daemon and open specific sessions.

Prereq: Stages 1–3 (or land the config + registration early so Stage 2/3 have a
real domain to attach). References: `config/src/ssh.rs`, `config/src/config.rs`,
`config/src/keyassignment.rs`, `wezterm-gui/src/overlay/{launcher,selector}.rs`,
`wezterm-mux-server-impl/src/lib.rs`, `wezterm-gui/src/termwindow/mod.rs`.

## 1. Config: `PaseoDaemon`

Model on `SshDomain` (`config/src/ssh.rs:51`) — `#[derive(FromDynamic, ToDynamic)]`
structs registered as a field on the root `Config` with `#[dynamic(...)]`
attributes.

New file `config/src/paseo.rs`:

```rust
use crate::*;
use std::time::Duration;

#[derive(Clone, Debug, FromDynamic, ToDynamic)]
pub struct PaseoDaemon {
    /// Domain name, e.g. "paseo:work". Must be unique.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// Remote via relay: a pairing-offer URL (https://app.paseo.sh/#offer=...).
    #[dynamic(default)]
    pub pairing_offer_url: Option<String>,

    /// Remote via relay: explicit relay endpoint (host:port) if not using an offer URL.
    #[dynamic(default)]
    pub relay_endpoint: Option<String>,

    /// Local/direct: host:port of the daemon (e.g. "127.0.0.1:6767").
    #[dynamic(default)]
    pub local_endpoint: Option<String>,

    /// Optional shared-secret password for direct-TCP exposure.
    #[dynamic(default)]
    pub password: Option<String>,

    /// Attach automatically at startup.
    #[dynamic(default)]
    pub connect_automatically: bool,

    #[dynamic(default = "default_paseo_timeout")]
    pub timeout: Duration,
}
fn default_paseo_timeout() -> Duration { Duration::from_secs(60) }

impl_lua_conversion_dynamic!(PaseoDaemon);
```

Exactly one of `pairing_offer_url` / `relay_endpoint` / `local_endpoint` is the
transport selector (relay first). Validate at attach time.

Wire it in:
- `config/src/lib.rs`: `mod paseo; pub use paseo::PaseoDaemon;`.
- `config/src/config.rs` (near the domain fields `:359–384`, beside
  `ssh_domains` `:372`): `#[dynamic(default)] pub paseo_daemons: Vec<PaseoDaemon>,`
  and an accessor `pub fn paseo_daemons(&self) -> Vec<PaseoDaemon>` if the codebase
  convention wants one (compare `ssh_domains()` `:957`).

User config example (`~/.config/wezterm/wezterm.lua`):

```lua
config.paseo_daemons = {
  {
    name = 'paseo:work',
    pairing_offer_url = 'https://app.paseo.sh/#offer=...',
    connect_automatically = true,
  },
  { name = 'paseo:local', local_endpoint = '127.0.0.1:6767' },
}
```

## 2. Domain registration

Add Paseo to `update_mux_domains_impl` in
`wezterm-mux-server-impl/src/lib.rs:39` (where ssh/unix/wsl domains are built):

```rust
for daemon in &config.paseo_daemons {
    if mux.get_domain_by_name(&daemon.name).is_none() {
        let domain: Arc<dyn Domain> = Arc::new(PaseoDomain::new(daemon.clone()));
        mux.add_domain(&domain);
        if daemon.connect_automatically {
            // schedule attach (async) for this domain
        }
    }
}
```

Because it's a registered `Domain`, it appears in the launcher automatically
(next section). `connect_automatically` triggers `attach` at startup.

## 3. Key assignments

In `config/src/keyassignment.rs`, alongside `OpenReviewPane`/`ReviewMode` (`:650`):

```rust
// in KeyAssignment enum:
OpenPaseoAgentPane(PaseoAgentArgs),
PaseoAgentMode(PaseoAgentAssignment),
PaseoPicker,
// (AttachDomain already exists and works for PaseoDomain)

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct PaseoAgentArgs {
    pub agent_id: Option<String>,        // None → pick interactively
    #[dynamic(default)] pub direction: PaneDirection,
    #[dynamic(default)] pub size: SplitSize,
}

#[derive(Debug, Clone, Copy, PartialEq, FromDynamic, ToDynamic)]
pub enum PaseoAgentAssignment {
    ScrollUp, ScrollDown, PageUp, PageDown,
    FocusComposer, SubmitPrompt, Cancel,
    ApprovePermission, DenyPermission, Close,
}
```

Follow the exact derive/registration used by `ReviewPaneArgs`/`ReviewModeAssignment`
(`:671`/`:695`), including `impl_lua_conversion_dynamic!` where applicable.

Dispatch in `wezterm-gui/src/termwindow/mod.rs` near the `OpenReviewPane` arm
(`:3128`):

```rust
OpenPaseoAgentPane(args) => crate::paseo::open::open_paseo_agent_pane(self, args)?,
PaseoAgentMode(_) => {}   // handled by the pane's perform_assignment
PaseoPicker => self.show_paseo_picker()?,
```

## 4. Discovery / attach UX

Two levels, reusing existing overlay machinery.

### Level 1 — attach a daemon (free)
`wezterm-gui/src/overlay/launcher.rs` iterates `mux.iter_domains()` (`:120`) and
emits an "Attach {label}" entry → `KeyAssignment::AttachDomain(name)` (`:259`),
dispatched at `termwindow/mod.rs:3089`. Since `PaseoDomain` is registered, it
shows up with no extra code. `PaseoDomain::attach` lists surfaces and creates tabs.

### Level 2 — open a specific session (`PaseoPicker`)
For "open agent/terminal X" use `InputSelector`
(`wezterm-gui/src/overlay/selector.rs`, shown via `TermWindow::show_input_selector`
`termwindow/mod.rs:2305`): a fuzzy list of `InputSelectorEntry`, each firing a
callback action.

Flow:
1. `PaseoPicker` (or a two-step: pick daemon → pick session) fetches the daemon's
   live agents + terminals **on the paseo background task**, then marshals the
   list back with `spawn_into_main_thread`.
2. Build `InputSelectorEntry`s: agents labeled by title/status/provider, terminals
   by name/cwd. Each entry's callback opens that session as a new tab:
   - terminal → `PaseoDomain::spawn_existing(terminal_id)` (attach-one).
   - agent → `PaseoAgentPane` tab.
3. Selected sessions become tabs interleaved with local tabs in the vertical tab
   bar.

## 5. Tab presentation

The fork already has a left vertical tab bar (`feat: vertical tabs`;
`tab_bar_placement`, `tab_bar_width`, `show_tab_icons`). Paseo tabs inherit it.
Set meaningful `get_title` (agent: title + status glyph; terminal: name/cwd) and,
optionally, a distinct icon so Paseo sessions are visually distinguishable from
local tabs. The agent pane's `attention_required` should surface on the tab
(reuse the existing tab-bar activity/attention affordance the fork already
animates — `tabbar: animate an indeterminate progress spinner` commit).

## Definition of done (Stage 4)

- A `paseo_daemons` entry produces a launcher "Attach" entry; attaching opens the
  daemon's sessions as tabs.
- `connect_automatically = true` attaches at startup.
- `PaseoPicker` lists live agents + terminals and opens the selected one as a tab.
- `OpenPaseoAgentPane` splits an agent beside the current pane.
- Paseo tabs sit alongside local tabs in the vertical tab bar with sensible
  titles/attention indicators.

## Risks specific to this stage

- **Async list population for `InputSelector`** — the selector wants entries up
  front; fetch on the bg task and open the selector from the
  `spawn_into_main_thread` continuation (don't block the GUI thread awaiting the
  daemon).
- **Domain name uniqueness/validation** — reuse `validate_domain_name` so a
  duplicate/invalid `name` fails config load clearly.
