pub mod search;

use pluribus_frequency::state::Configuration;

const KEY_EXA_API_KEY: &str = "web.exa_api_key";

/// Whether the Exa API key is configured.
#[must_use]
pub fn is_connected(config: &Configuration) -> bool {
    config.get::<String>(KEY_EXA_API_KEY).is_some()
}

#[must_use]
pub const fn config_key() -> &'static str {
    KEY_EXA_API_KEY
}
