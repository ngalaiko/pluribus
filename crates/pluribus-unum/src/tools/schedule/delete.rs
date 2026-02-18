use pluribus_frequency::state::Configuration;

use super::config_key;

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {
    /// The schedule id to delete.
    id: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = String;

    fn def(&self) -> pluribus_frequency::protocol::ToolDef {
        pluribus_frequency::protocol::ToolDef::new(
            pluribus_frequency::protocol::ToolName::new("schedule_delete"),
            "Delete a schedule by its id. Example: {\"id\": \"morning-reminder\"}",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move {
            let key = config_key(&input.id);
            if self.config.get::<super::Schedule>(&key).is_none() {
                return Err(format!("schedule not found: {}", input.id));
            }
            self.config.delete(&key).map_err(|e| e.to_string())?;
            Ok(format!("schedule '{}' deleted", input.id))
        })
    }
}
