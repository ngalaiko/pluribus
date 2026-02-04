mod complete_pairing;
mod generate_pairing_code;
mod set_token;
mod status;

use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

const KEY_BOT_TOKEN: &str = "telegram.bot_token";
const KEY_CHAT_ID: &str = "telegram.chat_id";
const KEY_PAIRING_CODE: &str = "telegram.pairing_code";

enum State {
    Unconfigured,
    TokenSet,
    Pairing,
    Connected,
}

fn state(config: &Configuration) -> State {
    if config.get::<String>(KEY_BOT_TOKEN).is_none() {
        return State::Unconfigured;
    }
    if config.get::<i64>(KEY_CHAT_ID).is_some() {
        return State::Connected;
    }
    if config.get::<String>(KEY_PAIRING_CODE).is_some() {
        return State::Pairing;
    }
    State::TokenSet
}

pub struct Telegram;

impl Provider for Telegram {
    fn resolve(&self, config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        match state(config) {
            State::Unconfigured => {
                tools.register(set_token::new(config.clone()));
            }
            State::TokenSet => {
                tools.register(generate_pairing_code::new(config.clone()));
            }
            State::Pairing => {
                tools.register(complete_pairing::new(config.clone()));
            }
            State::Connected => {
                tools.register(status::new(config.clone()));
                tools.register(crate::tools::disconnect_tool(
                    &[KEY_BOT_TOKEN, KEY_CHAT_ID, KEY_PAIRING_CODE],
                    "disconnect_telegram",
                    "Disconnect Telegram integration",
                    config.clone(),
                ));
            }
        }
        tools
    }
}

/// Read the stored bot token.
#[must_use]
pub fn bot_token(config: &Configuration) -> Option<String> {
    config.get(KEY_BOT_TOKEN)
}

/// Read the stored chat ID.
#[must_use]
pub fn chat_id(config: &Configuration) -> Option<i64> {
    config.get(KEY_CHAT_ID)
}

/// Read the stored pairing code.
#[must_use]
pub fn pairing_code(config: &Configuration) -> Option<String> {
    config.get(KEY_PAIRING_CODE)
}

/// Store a bot token.
pub fn set_bot_token(
    config: &Configuration,
    token: &str,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_BOT_TOKEN, &token)
}

/// Store a chat ID.
pub fn set_chat_id(
    config: &Configuration,
    id: i64,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_CHAT_ID, &id)
}

/// Store a pairing code.
pub fn set_pairing_code(
    config: &Configuration,
    code: &str,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_PAIRING_CODE, &code)
}

/// Delete the stored pairing code.
pub fn delete_pairing_code(
    config: &Configuration,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.delete(KEY_PAIRING_CODE)
}
