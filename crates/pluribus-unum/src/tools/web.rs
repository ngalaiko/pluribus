mod fetch;
mod search;

use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

pub struct Web;

impl Provider for Web {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        tools.register(fetch::new());
        tools.merge(search::resolve(config));
        tools
    }
}
