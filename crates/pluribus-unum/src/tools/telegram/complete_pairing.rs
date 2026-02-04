use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use pluribus_frequency::protocol::{ToolDef, ToolName};
use pluribus_frequency::state::Configuration;

use crate::telegram::ERROR_BACKOFF;

/// Total time to wait for the user to send the pairing code.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

/// Long-poll timeout per `getUpdates` call (seconds, server-side).
const POLL_TIMEOUT: u64 = 30;

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
    chat_id: i64,
    user_name: String,
}

impl crate::tools::Tool for Tool {
    type Input = Input;
    type Output = Output;

    fn def(&self) -> ToolDef {
        ToolDef::new(
            ToolName::new("completeTelegramPairing"),
            "Waits for the user to send the pairing code to the Telegram bot (up to 2 minutes). Stores the chat ID in the configuration on success.",
        )
    }

    fn execute(
        &self,
        _input: Input,
    ) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send + '_>> {
        Box::pin(async {
            let token = super::bot_token(&self.config)
                .ok_or("bot token not set — call setTelegramToken first")?;

            let code = super::pairing_code(&self.config)
                .ok_or("pairing code not set — call generateTelegramPairingCode first")?;

            let api = crate::telegram::ApiClient::new(&token);
            let deadline = std::time::Instant::now() + PAIRING_TIMEOUT;
            let mut offset: i64 = 0;

            while std::time::Instant::now() < deadline {
                match api.get_updates(offset, POLL_TIMEOUT).await {
                    Ok(updates) => {
                        for update in updates {
                            if update.update_id >= offset {
                                offset = update.update_id + 1;
                            }
                            if let Some(msg) = update.message {
                                if let Some(text) = &msg.text {
                                    if text.trim() == code {
                                        let user_name = msg.from.as_ref().map_or_else(
                                            || "unknown".to_owned(),
                                            |u| u.first_name.clone(),
                                        );

                                        let _ =
                                            api.send_message(msg.chat.id, "Connected!", None).await;

                                        super::set_chat_id(&self.config, msg.chat.id)
                                            .map_err(|e| e.to_string())?;

                                        let _ = super::delete_pairing_code(&self.config);

                                        return Ok(Output {
                                            chat_id: msg.chat.id,
                                            user_name,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Telegram getUpdates error during pairing: {e}");
                        async_io::Timer::after(ERROR_BACKOFF).await;
                    }
                }
            }

            let _ = super::delete_pairing_code(&self.config);
            Err("pairing timed out — no matching code received within 2 minutes".into())
        })
    }
}
