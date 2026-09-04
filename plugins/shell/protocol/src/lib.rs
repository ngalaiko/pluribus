//! Wire format between the shell component and its executor.
//!
//! One newline-terminated JSON request and one response per connection.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_COMMAND: usize = 65_536;
pub const MAX_TIMEOUT_MS: u32 = 300_000;
pub const MAX_OUTPUT: usize = 1024 * 1024;
pub const MAX_REQUEST: usize = 512 * 1024;
pub const MAX_RESPONSE: usize = 12 * MAX_OUTPUT + 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub command: String,
    pub timeout_ms: u32,
    pub invocation_id: String,
    pub authority_id: String,
    pub activity_id: String,
    pub origin_event_id: String,
}

impl Request {
    /// Rejects malformed commands, timeouts, and provenance fields.
    ///
    /// # Errors
    /// Returns the offending field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != VERSION {
            return Err("unsupported protocol version");
        }
        if self.command.is_empty()
            || self.command.len() > MAX_COMMAND
            || self.command.contains('\0')
        {
            return Err("invalid command");
        }
        if self.timeout_ms == 0 || self.timeout_ms > MAX_TIMEOUT_MS {
            return Err("invalid timeout");
        }
        if [
            &self.invocation_id,
            &self.authority_id,
            &self.activity_id,
            &self.origin_event_id,
        ]
        .iter()
        .any(|value| value.is_empty() || value.len() > 1024)
        {
            return Err("invalid provenance");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Response {
    Completed {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        truncated: bool,
    },
    Cancelled,
    DeadlineExceeded,
    Unavailable {
        message: String,
    },
}
