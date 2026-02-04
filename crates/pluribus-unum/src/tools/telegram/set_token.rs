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
pub struct Input {
    /// The Telegram bot token (from `@BotFather`).
    token: String,
}

#[derive(serde::Serialize)]
pub struct Output {
    bot_name: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("setTelegramToken"),
            "Validates a Telegram bot token and stores it in the configuration. Returns the bot name on success.",
        )
    }

    fn execute(
        &self,
        input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async move {
            let api = crate::telegram::ApiClient::new(&input.token);
            let bot = api.get_me().await?;

            super::set_bot_token(&self.config, &input.token).map_err(|e| e.to_string())?;

            Ok(Output {
                bot_name: bot.username.unwrap_or(bot.first_name),
            })
        })
    }
}
