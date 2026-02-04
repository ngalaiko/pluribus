mod telegram;

use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

/// Which messaging backend to use.
#[derive(serde::Deserialize, schemars::JsonSchema)]
enum MessageBackend {
    /// Send via `Telegram`.
    #[serde(rename = "telegram")]
    Telegram,
}

struct SendMessageTool {
    config: Configuration,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SendMessageInput {
    /// The message text to send.
    message: String,
    /// Messaging backend. Default: telegram.
    backend: Option<MessageBackend>,
}

impl crate::tools::Tool for SendMessageTool {
    type Input = SendMessageInput;
    type Output = String;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("send_message"),
            "Send a message to the user via Telegram.",
        )
    }

    fn execute(
        &self,
        input: SendMessageInput,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            match input.backend.unwrap_or(MessageBackend::Telegram) {
                MessageBackend::Telegram => telegram::send(&self.config, &input.message).await,
            }
        })
    }
}

pub struct Message;

impl Provider for Message {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        if telegram::is_connected(config) {
            tools.register(SendMessageTool {
                config: config.clone(),
            });
        } else {
            tools.register(super::disabled_tool(
                "send_message",
                "Send a message to the user.",
                "No messaging backend connected",
            ));
        }
        tools
    }
}
