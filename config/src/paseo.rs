use crate::config::validate_domain_name;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct PaseoDaemon {
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    #[dynamic(default)]
    pub pairing_offer_url: Option<String>,

    #[dynamic(default)]
    pub local_endpoint: Option<String>,

    #[dynamic(default)]
    pub use_tls: bool,

    #[dynamic(default)]
    pub password: Option<String>,

    #[dynamic(default)]
    pub connect_automatically: bool,
}
impl_lua_conversion_dynamic!(PaseoDaemon);
