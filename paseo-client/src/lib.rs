pub mod binary_frames;
pub mod client;
pub mod e2ee;
pub mod envelope;
pub mod error;
pub mod events;
pub mod offer;
pub mod protocol;
pub mod transport;

pub use client::{PaseoClient, TerminalHandle};
pub use error::{PaseoError, Result};
pub use events::{ConnectionState, DaemonEvent, TerminalStreamEvent};
pub use offer::{parse_offer_url, ConnectionOffer};
pub use protocol::agents::PermissionResponse;
pub use protocol::terminals::CreateTerminalOpts;
pub use protocol::{AgentSnapshot, ServerInfo, TerminalInfo};
