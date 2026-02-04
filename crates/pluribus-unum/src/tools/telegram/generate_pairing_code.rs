use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

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
    code: String,
    bot_name: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("generateTelegramPairingCode"),
            "Generates a 6-digit pairing code and stores it in the configuration. The user must send this code to the bot in Telegram to complete pairing.",
        )
    }

    fn execute(
        &self,
        _input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async {
            let token = super::bot_token(&self.config)
                .ok_or("bot token not set — call setTelegramToken first")?;

            let api = crate::telegram::ApiClient::new(&token);
            let bot = api.get_me().await?;

            let nanos = u64::from(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos(),
            );
            let code = format!("{:06}", nanos % 1_000_000);

            super::set_pairing_code(&self.config, &code).map_err(|e| e.to_string())?;

            Ok(Output {
                code,
                bot_name: bot.username.unwrap_or(bot.first_name),
            })
        })
    }
}
