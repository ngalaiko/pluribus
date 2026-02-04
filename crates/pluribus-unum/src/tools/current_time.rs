use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct Input {}

pub struct Tool;

#[must_use]
pub const fn new() -> Tool {
    Tool {}
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(ToolName::new("getCurrentTime"), "Returns the current time")
    }

    fn execute(
        &self,
        _input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Ok(chrono::Local::now().to_rfc3339()) })
    }
}
