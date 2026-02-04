use pluribus_frequency::state::Configuration;

use super::MEMORY_PREFIX;

pub const TOOL_NAME: &str = "forget";

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub key: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = ();

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new(TOOL_NAME),
            "Forgets a piece of information from memory. Input is the key to forget.",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let full_key = format!("{MEMORY_PREFIX}{}", input.key);
            if self.config.get::<String>(&full_key).is_none() {
                return Err(format!("key not found: {}", input.key));
            }
            self.config.delete(&full_key).map_err(|e| e.to_string())
        })
    }
}
