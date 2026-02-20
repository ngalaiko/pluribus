use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use pluribus_frequency::protocol::{ToolDef, ToolName};

use super::Sessions;

pub struct Tool {
    sessions: Sessions,
}

pub const fn new(sessions: Sessions) -> Tool {
    Tool { sessions }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    /// The URL to navigate to.
    url: String,
}

#[derive(serde::Serialize)]
pub struct Output {
    title: String,
    url: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_navigate"),
            "Navigate the browser to a URL. Opens a new browser tab if one is not already open. Returns the page title and final URL.",
        )
        .with_input_schema::<Input>()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn execute(
        &self,
        input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!(url = %input.url, "browser_navigate: navigating");
            if self.sessions.session().is_err() {
                self.sessions.open_tab().await?;
            }
            let session = self.sessions.session()?;
            let nav = super::navigate(&session, &input.url).await?;
            tracing::info!(title = %nav.title, url = %nav.url, "browser_navigate: page loaded");
            Ok(Output {
                title: nav.title,
                url: nav.url,
            })
        })
    }
}
