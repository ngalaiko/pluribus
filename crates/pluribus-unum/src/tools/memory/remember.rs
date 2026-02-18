use pluribus_frequency::state::Configuration;

use super::MEMORY_PREFIX;

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

pub const TOOL_NAME: &str = "remember";

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub key: String,
    pub value: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = ();

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new(TOOL_NAME),
            "Remembers a piece of information in memory. Input is a (key, value) pair. Example: {\"key\": \"user_name\", \"value\": \"Alice\"}",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.config
                .set(&format!("{MEMORY_PREFIX}{}", input.key), &input.value)
                .map_err(|e| e.to_string())
        })
    }
}
