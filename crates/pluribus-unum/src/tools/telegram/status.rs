use std::future::Future;
use std::pin::Pin;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

pub struct Tool {
    config: Configuration,
}

pub const fn new(config: Configuration) -> Tool {
    Tool { config }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct Input {}

#[derive(serde::Serialize)]
pub struct Output {
    connected: bool,
    bot_name: Option<String>,
    chat_id: Option<i64>,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("getTelegramStatus"),
            "Returns the current Telegram integration status: whether a bot token and chat ID are configured.",
        )
    }

    fn execute(
        &self,
        _input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async {
            let token = super::bot_token(&self.config);
            let chat_id = super::chat_id(&self.config);

            let bot_name = if let Some(ref t) = token {
                crate::telegram::ApiClient::new(t)
                    .get_me()
                    .await
                    .ok()
                    .and_then(|u| u.username)
            } else {
                None
            };

            Ok(Output {
                connected: token.is_some() && chat_id.is_some(),
                bot_name,
                chat_id,
            })
        })
    }
}
