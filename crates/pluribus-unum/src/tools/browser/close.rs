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
pub struct Input {}

#[derive(serde::Serialize)]
pub struct Output {
    closed: bool,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("browser_close"),
            "Close the current browser tab.",
        )
        .with_input_schema::<Input>()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn execute(
        &self,
        _input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!("browser_close: closing tab");
            self.sessions.close_tab().await?;
            tracing::info!("browser_close: tab closed");
            Ok(Output { closed: true })
        })
    }
}
