use pluribus_frequency::state::Configuration;

use crate::telegram::ApiClient;
use crate::tools::telegram::{bot_token, chat_id};

/// Check whether Telegram is connected (bot token + chat ID both set).
#[must_use]
pub fn is_connected(config: &Configuration) -> bool {
    bot_token(config).is_some() && chat_id(config).is_some()
}

/// Send a message via `Telegram`.
pub async fn send(config: &Configuration, message: &str) -> Result<String, String> {
    let token = bot_token(config).ok_or("Telegram bot token not configured")?;
    let cid = chat_id(config).ok_or("Telegram chat ID not configured")?;
    let client = ApiClient::new(&token);
    client.send_message(cid, message, None).await?;
    Ok("Message sent via Telegram.".to_owned())
}
