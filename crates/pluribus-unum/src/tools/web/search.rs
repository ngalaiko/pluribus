mod ddg;
mod exa;

use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

use crate::tools::Tools;

/// Which search provider to use.
#[derive(serde::Deserialize, schemars::JsonSchema)]
enum SearchProvider {
    /// Free web search via `DuckDuckGo`.
    #[serde(rename = "duckduckgo")]
    DuckDuckGo,
    /// AI-powered web search via Exa. Higher quality results.
    #[serde(rename = "exa")]
    Exa,
}

struct SearchTool {
    config: Configuration,
    exa_connected: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SearchInput {
    /// The search query.
    query: String,
    /// Search provider. Default: duckduckgo.
    provider: Option<SearchProvider>,
}

impl crate::tools::Tool for SearchTool {
    type Input = SearchInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        let desc = if self.exa_connected {
            "Search the web. Providers: duckduckgo (default, free), exa (AI-powered, higher quality). Example: {\"query\": \"rust async programming\"}"
        } else {
            "Search the web using DuckDuckGo. Example: {\"query\": \"rust async programming\"}"
        };
        ToolDef::new(ToolName::new("web_search"), desc)
    }

    fn execute(
        &self,
        input: SearchInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            match input.provider.unwrap_or(SearchProvider::DuckDuckGo) {
                SearchProvider::DuckDuckGo => ddg::search(input.query).await,
                SearchProvider::Exa => {
                    let key = self
                        .config
                        .get::<String>(exa::config_key())
                        .ok_or("Exa not connected. Use connect_exa first.")?;
                    exa::search::search(&key, input.query).await
                }
            }
        })
    }
}

/// Build search tools based on current configuration.
#[must_use]
pub fn resolve(config: &Configuration) -> Tools {
    let mut tools = Tools::new();
    let exa_connected = exa::is_connected(config);
    tools.register(SearchTool {
        config: config.clone(),
        exa_connected,
    });
    if exa_connected {
        tools.register(crate::tools::disconnect_tool(
            &[exa::config_key()],
            "disconnect_exa",
            "Disconnect Exa search",
            config.clone(),
        ));
    } else {
        tools.register(crate::tools::connect_tool(
            exa::config_key(),
            "connect_exa",
            "Connect Exa AI-powered web search. Higher quality results than DuckDuckGo. Get an API key at exa.ai. Example: {\"api_key\": \"your-exa-api-key\"}",
            config.clone(),
        ));
    }
    tools
}
