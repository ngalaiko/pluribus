//! Wire format between the shell component and its executor.
//!
//! One newline-terminated JSON request and one response per connection.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 3;
pub const MAX_COMMAND: usize = 65_536;
pub const MAX_TIMEOUT_MS: u32 = 300_000;
pub const MAX_OUTPUT: usize = 1024 * 1024;
pub const MAX_REQUEST: usize = 512 * 1024;
pub const MAX_RESPONSE: usize = 12 * MAX_OUTPUT + 1024;
pub const MAX_CORE_BYTES: usize = 1024 * 1024;
pub const MAX_CORE_FRAME_BYTES: usize = MAX_CORE_BYTES * 5 + 4096;
pub const MAX_UPLOAD_CHUNK: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CoreOperation {
    Secret {
        binding: String,
    },
    Attachment {
        digest: String,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_attachment_chunk")]
        max_bytes: u32,
    },
    AttachmentOpen {
        media_type: String,
        size: u64,
    },
    AttachmentWrite {
        handle: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    AttachmentFinish {
        handle: String,
    },
}

impl CoreOperation {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Secret { binding } if valid_env_name(binding) => Ok(()),
            Self::Secret { .. } => Err("invalid secret binding"),
            Self::Attachment {
                digest, max_bytes, ..
            } if digest.len() == 64
                && (1..=MAX_UPLOAD_CHUNK as u32).contains(max_bytes)
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
            {
                Ok(())
            }
            Self::Attachment { .. } => Err("invalid attachment reference or chunk size"),
            Self::AttachmentOpen { media_type, .. }
                if !media_type.is_empty()
                    && media_type.len() <= 255
                    && !media_type.bytes().any(|byte| byte.is_ascii_control()) =>
            {
                Ok(())
            }
            Self::AttachmentOpen { .. } => Err("invalid attachment media type"),
            Self::AttachmentWrite { handle, bytes, .. }
                if !handle.is_empty()
                    && handle.len() <= 1024
                    && bytes.len() <= MAX_UPLOAD_CHUNK =>
            {
                Ok(())
            }
            Self::AttachmentWrite { .. } => Err("invalid attachment chunk"),
            Self::AttachmentFinish { handle } if !handle.is_empty() && handle.len() <= 1024 => {
                Ok(())
            }
            Self::AttachmentFinish { .. } => Err("invalid attachment upload handle"),
        }
    }
}

fn default_attachment_chunk() -> u32 {
    MAX_UPLOAD_CHUNK as u32
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreRequest {
    pub id: u64,
    pub operation: CoreOperation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreReply {
    pub id: u64,
    pub bytes: Option<Vec<u8>>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutorFrame {
    CoreRequest { request: CoreRequest },
    Result { response: Response },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ComponentFrame {
    CoreReply(CoreReply),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub command: String,
    pub env: std::collections::BTreeMap<String, String>,
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
        if self.env.len() > 64
            || self.env.iter().any(|(name, value)| {
                !valid_env_name(name) || value.contains('\0') || value.len() > 16 * 1024
            })
            || self
                .env
                .iter()
                .map(|(n, v)| n.len() + v.len())
                .sum::<usize>()
                > 64 * 1024
        {
            return Err("invalid environment");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !["HOME", "PATH", "LANG", "PLURIBUS_CORE_SOCKET"].contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_operations_validate_scope_and_attachment_digest() {
        let secret = CoreOperation::Secret {
            binding: "GH_TOKEN".into(),
        };
        assert!(secret.validate().is_ok());
        assert!(
            CoreOperation::Secret {
                binding: "private:handle".into(),
            }
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_value::<CoreOperation>(serde_json::json!({
                "operation": "attachment",
                "digest": "a".repeat(64),
            }))
            .unwrap()
            .validate()
            .is_ok()
        );
        assert!(
            serde_json::from_value::<CoreOperation>(serde_json::json!({
                "operation": "attachment",
                "digest": "A".repeat(64),
            }))
            .unwrap()
            .validate()
            .is_err()
        );
    }

    #[test]
    fn core_operations_support_chunked_file_imports() {
        let open: CoreOperation = serde_json::from_value(serde_json::json!({
            "operation": "attachment-open",
            "media_type": "image/png",
            "size": 3
        }))
        .unwrap();
        assert!(matches!(
            open,
            CoreOperation::AttachmentOpen { size: 3, .. }
        ));
        let write: CoreOperation = serde_json::from_value(serde_json::json!({
            "operation": "attachment-write",
            "handle": "upload-1",
            "offset": 0,
            "bytes": [1, 2, 3]
        }))
        .unwrap();
        assert!(matches!(
            write,
            CoreOperation::AttachmentWrite { offset: 0, .. }
        ));
        let finish: CoreOperation = serde_json::from_value(serde_json::json!({
            "operation": "attachment-finish",
            "handle": "upload-1"
        }))
        .unwrap();
        assert!(matches!(finish, CoreOperation::AttachmentFinish { .. }));
    }
}
