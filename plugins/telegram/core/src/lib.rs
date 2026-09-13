//! The bindings, transport, and configuration plumbing both Telegram
//! components build on. Each component crate exports the world from here.

pub mod api;

use pluribus::plugin::types::{Error, ErrorCode, Payload, Proposal};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::cell::RefCell;

wit_bindgen::generate!({
    path: "../../../wit",
    world: "source",
    pub_export_macro: true,
    // Component crates take this crate as `telegram`.
    default_bindings_module: "telegram",
    export_macro_name: "export",
    generate_unused_types: true,
});

/// Configuration arrives in `init` and no import returns it later, so the
/// instance holds it. A restart runs `init` again.
pub struct Slot<T>(RefCell<Option<T>>);

impl<T: Clone> Slot<T> {
    #[must_use]
    pub const fn empty() -> Self {
        Self(RefCell::new(None))
    }

    pub fn store(&self, value: T) {
        *self.0.borrow_mut() = Some(value);
    }

    pub fn load(&self) -> Result<T, Error> {
        self.0.borrow().clone().ok_or_else(|| Error {
            code: ErrorCode::Unavailable,
            message: "plugin is not initialized".into(),
            retryable: true,
            details: None,
        })
    }
}

impl<T: Clone> Default for Slot<T> {
    fn default() -> Self {
        Self::empty()
    }
}

pub fn parse_config<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes)
        .map_err(|error| api::invalid(format!("invalid configuration: {error}")))
}

pub fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &Value,
    idempotency_key: Option<String>,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(serde_json::to_vec(value).map_err(api::internal)?),
        idempotency_key,
        causation_id,
    })
}
