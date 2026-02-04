mod forget;
mod remember;

use std::collections::HashMap;

use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

const MEMORY_PREFIX: &str = "memory.";

pub struct Memory;

impl Provider for Memory {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        tools.register(remember::new(config.clone()));
        tools.register(forget::new(config.clone()));
        tools
    }
}

/// Read all memory entries from configuration.
#[must_use]
pub fn entries(config: &Configuration) -> HashMap<String, String> {
    config
        .list_by_prefix::<String>(MEMORY_PREFIX)
        .into_iter()
        .map(|(k, v)| (k[MEMORY_PREFIX.len()..].to_owned(), v))
        .collect()
}
