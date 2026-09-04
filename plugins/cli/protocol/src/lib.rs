//! Wire format between the cli component and its bridge.
//!
//! One newline-terminated JSON request and one response per connection, as the
//! shell plugin does. The component always connects; the bridge always listens.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_REQUEST: usize = 64 * 1024;
pub const MAX_RESPONSE: usize = 256 * 1024;
pub const MAX_TEXT: usize = 16 * 1024;
/// Messages one poll returns. The bridge keeps the rest for the next one.
pub const MAX_MESSAGES: usize = 32;
pub const MAX_TIMEOUT_MS: u32 = 300_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Waits up to `timeout_ms` for input newer than `after`.
    Poll {
        version: u32,
        after: u64,
        timeout_ms: u32,
    },
    /// Delivers an agent reply to the terminal.
    Reply {
        version: u32,
        conversation_id: String,
        text: String,
    },
}

impl Request {
    /// # Errors
    /// Returns the offending field.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Poll {
                version,
                timeout_ms,
                ..
            } => {
                if *version != VERSION {
                    return Err("unsupported protocol version");
                }
                if *timeout_ms == 0 || *timeout_ms > MAX_TIMEOUT_MS {
                    return Err("invalid timeout");
                }
            }
            Self::Reply {
                version,
                conversation_id,
                text,
            } => {
                if *version != VERSION {
                    return Err("unsupported protocol version");
                }
                if conversation_id.is_empty() || conversation_id.len() > 256 {
                    return Err("invalid conversation");
                }
                if text.is_empty() || text.len() > MAX_TEXT {
                    return Err("invalid reply text");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Response {
    /// Input the person typed, oldest first. Empty when the poll timed out.
    Messages {
        messages: Vec<Message>,
    },
    Delivered,
    Unavailable {
        message: String,
    },
}

/// One line the person typed, numbered so a restart resumes where it left off.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub sequence: u64,
    pub at_ms: i64,
    pub text: String,
}
