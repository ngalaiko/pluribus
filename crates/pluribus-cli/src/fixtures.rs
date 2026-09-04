//! A configuration the tests share. Nothing ships it: an agent starts empty.

use crate::registry::Config;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Where `pluribus-sync-plugins` leaves the built packages.
fn package(name: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    url::Url::from_directory_path(root.join(name))
        .expect("absolute package path")
        .to_string()
}

/// Telegram, Codex and RLM instances, wired the way an agent uses them.
pub fn config() -> Config {
    let value = json!({
        "agent_id": "personal",
        "identity": "fixture",
        "model": "gpt-5.6-luna",
        "trusted_senders": [],
        "maximum_blob_bytes": 268_435_456u64,
        "plugin_instances": {
            "telegram-1": {
                "package": package("telegram"),
                "aliases": ["telegram", "dev.pluribus.telegram"],
                "config": {"credential_handle": "telegram:primary", "poll_timeout_seconds": 30},
                "components": {
                    "receive": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}},
                    "send": {"http": {"origins": ["https://api.telegram.org"], "methods": ["GET", "POST"]}}
                }
            },
            "codex-1": {
                "package": package("openai-codex"),
                "aliases": ["codex", "openai-codex", "dev.pluribus.openai-codex"],
                "config": {"credential": "codex:primary", "models": ["gpt-5.6-luna"], "timeout_ms": 300_000},
                "components": {
                    "main": {"http": {"origins": ["https://chatgpt.com"], "methods": ["POST"]}}
                },
                "enrollment_origins": ["https://auth.openai.com"]
            },
            "rlm": {
                "package": package("rlm"),
                "config": {"model": "gpt-5.6-luna", "identity": "fixture", "connectors": ["telegram-1/receive"], "trusted_users": [], "js": {}},
                "components": {"cognition": {}, "js": {}}
            }
        },
        "model_instance": "codex-1/main"
    });
    serde_json::from_value(value).expect("fixture configuration")
}

pub fn packages_are_built() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugins/rlm/plugin.toml")
        .is_file()
}
