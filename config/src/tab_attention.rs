use crate::{EasingFunction, RgbaColor};
use std::collections::HashMap;
use wezterm_dynamic::{FromDynamic, ToDynamic};

fn default_attention_var() -> String {
    "claude_status".to_string()
}

fn default_fade_in_ms() -> u64 {
    400
}

fn default_fade_out_ms() -> u64 {
    400
}

fn default_attention_colors() -> HashMap<String, RgbaColor> {
    let mut map = HashMap::new();
    map.insert("waiting".to_string(), RgbaColor::from((240u8, 223, 175)));
    map.insert("approval".to_string(), RgbaColor::from((204u8, 147, 147)));
    map
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct TabAttention {
    #[dynamic(default = "default_attention_var")]
    pub var: String,
    #[dynamic(default = "default_fade_in_ms")]
    pub fade_in_duration_ms: u64,
    #[dynamic(default)]
    pub fade_in_function: EasingFunction,
    #[dynamic(default = "default_fade_out_ms")]
    pub fade_out_duration_ms: u64,
    #[dynamic(default)]
    pub fade_out_function: EasingFunction,
    #[dynamic(default = "default_attention_colors")]
    pub colors: HashMap<String, RgbaColor>,
}

impl Default for TabAttention {
    fn default() -> Self {
        Self {
            var: default_attention_var(),
            fade_in_duration_ms: default_fade_in_ms(),
            fade_in_function: EasingFunction::default(),
            fade_out_duration_ms: default_fade_out_ms(),
            fade_out_function: EasingFunction::default(),
            colors: default_attention_colors(),
        }
    }
}
