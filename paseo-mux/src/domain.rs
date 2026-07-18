use crate::pane::PaseoTerminalPane;
use async_trait::async_trait;
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use mux::pane::{alloc_pane_id, Pane};
use mux::tab::Tab;
use mux::window::WindowId;
use mux::Mux;
use parking_lot::Mutex;
use paseo_client::PaseoClient;
use portable_pty::CommandBuilder;
use std::sync::Arc;
use wezterm_term::TerminalSize;

#[derive(Clone)]
pub enum ConnectTarget {
    Relay {
        offer_url: String,
    },
    Local {
        host_port: String,
        use_tls: bool,
        password: Option<String>,
    },
}

pub struct PaseoDomain {
    domain_id: DomainId,
    name: String,
    target: ConnectTarget,
    client: Mutex<Option<PaseoClient>>,
    state: Mutex<DomainState>,
}

impl PaseoDomain {
    pub fn new(name: impl Into<String>, target: ConnectTarget) -> Arc<PaseoDomain> {
        Arc::new(PaseoDomain {
            domain_id: alloc_domain_id(),
            name: name.into(),
            target,
            client: Mutex::new(None),
            state: Mutex::new(DomainState::Detached),
        })
    }

    fn client_id(&self) -> String {
        format!("wezterm-paseo-{}", self.name)
    }

    async fn connect(&self) -> anyhow::Result<PaseoClient> {
        let client = match &self.target {
            ConnectTarget::Relay { offer_url } => {
                let offer = paseo_client::parse_offer_url(offer_url)?;
                PaseoClient::connect_relay(&offer, &self.client_id()).await?
            }
            ConnectTarget::Local {
                host_port,
                use_tls,
                password,
            } => {
                PaseoClient::connect_local(
                    host_port,
                    *use_tls,
                    password.as_deref(),
                    &self.client_id(),
                )
                .await?
            }
        };
        Ok(client)
    }
}

fn default_size() -> TerminalSize {
    TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

#[async_trait(?Send)]
impl Domain for PaseoDomain {
    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("paseo domain does not support spawn yet")
    }

    fn spawnable(&self) -> bool {
        false
    }

    fn detachable(&self) -> bool {
        true
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        let client = self.connect().await?;
        *self.client.lock() = Some(client.clone());

        {
            let client = client.clone();
            promise::spawn::spawn(async move {
                let _ = client.run().await;
            })
            .detach();
        }

        let mux = Mux::get();
        let window_id = match window_id {
            Some(window_id) => window_id,
            None => *mux.new_empty_window(None, None),
        };

        let size = default_size();
        let terminals = client.list_terminals(None).await?;
        for info in terminals {
            let handle = client.subscribe_terminal(&info.id, "live").await?;
            let remote = handle.writer();
            let (pane, input_rx) = PaseoTerminalPane::new(
                alloc_pane_id(),
                self.domain_id,
                info.id.clone(),
                size,
                remote,
            );

            let pane_dyn: Arc<dyn Pane> = pane.clone();
            mux.add_pane(&pane_dyn)?;

            let tab = Arc::new(Tab::new(&size));
            tab.assign_pane(&pane_dyn);
            mux.add_tab_and_active_pane(&tab)?;
            mux.add_tab_to_window(&tab, window_id)?;

            pane.start_io(handle, input_rx);
        }

        *self.state.lock() = DomainState::Attached;
        Ok(())
    }

    fn detach(&self) -> anyhow::Result<()> {
        *self.client.lock() = None;
        *self.state.lock() = DomainState::Detached;
        Ok(())
    }

    fn state(&self) -> DomainState {
        *self.state.lock()
    }
}
