use crate::config::validate_domain_name;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct OrcaRuntime {
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    #[dynamic(default)]
    pub pairing_code: String,

    #[dynamic(default)]
    pub ssh: String,
}
impl_lua_conversion_dynamic!(OrcaRuntime);
