#![allow(unsafe_op_in_unsafe_fn)]

mod files;
mod normalize;

use serde::Deserialize;
use serde_json::{Value, json};
use telegram::exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use telegram::pluribus::plugin::types::{Error, Event, Mutation, Proposal, StateEntry};
use telegram::pluribus::plugin::{http, state};
use telegram::{Slot, parse_config, proposal};

/// Key holding the `getUpdates` offset. State is a rebuildable projection: an
/// empty namespace re-polls from Telegram's own backlog.
const OFFSET_KEY: &str = "updates/offset";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "bot-token")]
    bot_token: String,
}

#[derive(Clone, Deserialize)]
struct Config {
    credentials: Credentials,
    #[serde(default)]
    trusted_senders: Vec<String>,
    #[serde(default = "default_poll_timeout")]
    poll_timeout_seconds: u32,
}

thread_local! {
    static CONFIG: Slot<Config> = const { Slot::empty() };
}

struct Telegram;

impl Guest for Telegram {
    fn init(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let outcome = initialize(config)?;
        subscribe(&CONFIG.with(Slot::load)?, stored_offset()?)?;
        Ok(outcome)
    }

    fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: events.last().map(|e| e.sequence),
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: Vec::new(),
            mutations: Vec::new(),
            checkpoint: None,
        })
    }
}

struct Poll {
    observations: Vec<Proposal>,
    offset: Option<i64>,
    pending: Vec<Mutation>,
}

fn subscribe(config: &Config, offset: Option<i64>) -> Result<(), Error> {
    http::subscribe(&telegram::api::request(
        "getUpdates",
        &json!({"offset":offset,"limit":100,"timeout":config.poll_timeout_seconds,
            "allowed_updates":["message","edited_message","channel_post","edited_channel_post","message_reaction","callback_query"]}),
        &config.credentials.bot_token,
        config
            .poll_timeout_seconds
            .saturating_add(10)
            .saturating_mul(1000),
    )?)
}

impl telegram::exports::pluribus::plugin::ingress::Guest for Telegram {
    fn receive(input: Vec<u8>) -> Result<telegram::pluribus::plugin::types::IngressOutcome, Error> {
        let input: Value = serde_json::from_slice(&input).map_err(telegram::api::internal)?;
        let mut out = telegram::pluribus::plugin::types::IngressOutcome {
            events: vec![],
            mutations: vec![],
        };
        if input["kind"] != "http" {
            return Ok(out);
        }
        let config = CONFIG.with(Slot::load)?;
        let response = telegram::api::decode_response(
            input["status"].as_u64().unwrap_or(0) as u16,
            telegram::pluribus::plugin::types::BlobRef {
                algorithm: "sha256".into(),
                digest: input["body"]["digest"]
                    .as_str()
                    .ok_or_else(|| telegram::api::invalid("missing body digest"))?
                    .into(),
                size: input["body"]["size"]
                    .as_u64()
                    .ok_or_else(|| telegram::api::invalid("missing body size"))?,
                media_type: input["body"]["mediaType"]
                    .as_str()
                    .unwrap_or("application/json")
                    .into(),
            },
        )?;
        let previous = stored_offset()?;
        let result = poll_response(&config, response.value, previous)?;
        fetch_pending(
            &config,
            input["receivedAtMs"].as_i64().unwrap_or(0),
            &mut out.events,
            &mut out.mutations,
        )?;
        out.events.extend(result.observations);
        out.mutations.extend(result.pending);
        if result.offset != previous
            && let Some(offset) = result.offset
        {
            out.mutations.push(Mutation::Set(StateEntry {
                key: OFFSET_KEY.into(),
                value: offset.to_string().into_bytes(),
            }));
        }
        subscribe(&config, result.offset)?;
        Ok(out)
    }
}

/// Observations and offsets commit atomically, deduplicated by update ID.
fn poll_response(config: &Config, response: Value, offset: Option<i64>) -> Result<Poll, Error> {
    let observations = normalize::updates(&response, &config.trusted_senders, 100)?;
    let next = response
        .as_array()
        .and_then(|updates| updates.last())
        .and_then(|update| update.get("update_id"))
        .and_then(Value::as_i64)
        .and_then(|id| id.checked_add(1))
        .or(offset);

    let mut pending = Vec::new();
    for observation in &observations {
        if observation.payload["media"]
            .as_array()
            .is_some_and(|media| media.iter().any(|item| item["status"] == "pending"))
        {
            pending.push(Mutation::Set(StateEntry {
                key: format!(
                    "pending-media/{:020}",
                    observation.payload["update_id"].as_i64().unwrap()
                ),
                value: serde_json::to_vec(&observation.payload).map_err(telegram::api::internal)?,
            }));
        }
    }
    let proposals = observations
        .into_iter()
        .map(|observation| {
            proposal(
                "observation.received",
                "dev.pluribus.telegram.observation.v1",
                &observation.payload,
                Some(observation.deduplication_key),
                None,
            )
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(Poll {
        observations: proposals,
        offset: next,
        pending,
    })
}

fn fetch_pending(
    config: &Config,
    now_ms: i64,
    proposals: &mut Vec<Proposal>,
    mutations: &mut Vec<Mutation>,
) -> Result<(), Error> {
    // Rotate the scan cursor so a failed attachment cannot starve later updates.
    let cursor = state::get("media/cursor")?.and_then(|bytes| String::from_utf8(bytes).ok());
    let page = state::scan("pending-media/", cursor.as_deref(), 1)?;
    let Some(entry) = page.entries.into_iter().next() else {
        mutations.push(Mutation::Delete("media/cursor".into()));
        return Ok(());
    };
    mutations.push(Mutation::Set(StateEntry {
        key: "media/cursor".into(),
        value: entry.key.as_bytes().to_vec(),
    }));
    let mut observation: Value =
        serde_json::from_slice(&entry.value).map_err(telegram::api::internal)?;
    if !normalize::allowed_sender(
        observation["externalSenderId"].as_str(),
        &config.trusted_senders,
    ) {
        mutations.push(Mutation::Delete(entry.key));
        return Ok(());
    }
    if observation["mediaRetryAtMs"].as_i64().unwrap_or(0) > now_ms {
        return Ok(());
    }
    let media = observation["media"]
        .as_array_mut()
        .ok_or_else(|| telegram::api::internal("invalid pending media"))?;
    for item in media
        .iter_mut()
        .filter(|item| item["status"] == "pending")
        .take(1)
    {
        let file_id = item["metadata"]["file_id"]
            .as_str()
            .ok_or_else(|| telegram::api::internal("missing pending file ID"))?;
        match files::download(file_id, &config.credentials.bot_token) {
            Ok((blob, name)) => {
                item["blob"] = normalize::blob_json(&blob);
                item["status"] = json!("ready");
                if item["fileName"].is_null() {
                    item["fileName"] = json!(name);
                }
            }
            Err(error) => {
                let attempts = item["attempts"].as_u64().unwrap_or(0) + 1;
                item["attempts"] = json!(attempts);
                item["error"] = json!(error.message);
                if !error.retryable || attempts >= 5 {
                    item["status"] = json!("failed");
                }
            }
        }
    }
    if media.iter().any(|item| item["status"] == "pending") {
        observation["mediaRetryAtMs"] = json!(now_ms.saturating_add(30_000));
        mutations.push(Mutation::Set(StateEntry {
            key: entry.key,
            value: serde_json::to_vec(&observation).map_err(telegram::api::internal)?,
        }));
    } else {
        let failed = media.iter().any(|item| item["status"] == "failed");
        observation["observationDeduplicationKey"] =
            json!(format!("telegram:update:{}", observation["update_id"]));
        proposals.push(proposal(
            if failed {
                "telegram.media-failed"
            } else {
                "telegram.media-ready"
            },
            "dev.pluribus.telegram.media.v1",
            &observation,
            Some(format!(
                "telegram:update:{}:media",
                observation["update_id"]
            )),
            None,
        )?);
        mutations.push(Mutation::Delete(entry.key));
    }
    Ok(())
}

fn initialize(config: Vec<u8>) -> Result<Outcome, Error> {
    let config = parse_config(&config)?;
    CONFIG.with(|slot| slot.store(config));
    Ok(Outcome {
        events: vec![],
        mutations: vec![],
        checkpoint: None,
    })
}

fn stored_offset() -> Result<Option<i64>, Error> {
    let Some(bytes) = state::get(OFFSET_KEY)? else {
        return Ok(None);
    };
    std::str::from_utf8(&bytes)
        .map_err(|_| telegram::api::invalid("stored offset is not UTF-8"))?
        .parse()
        .map(Some)
        .map_err(|_| telegram::api::invalid("stored offset is not an integer"))
}

fn default_poll_timeout() -> u32 {
    30
}

telegram::export!(Telegram);

#[cfg(test)]
mod role_tests {
    use super::*;
    use telegram::pluribus::plugin::types::{Payload, Principal, PrincipalKind};

    fn context() -> Context {
        Context {
            instance_id: "fixture".into(),
            agent: Principal {
                kind: PrincipalKind::Agent,
                id: "fixture".into(),
            },
            state_checkpoint: 0,
            depth: 0,
            deadline_at_ms: None,
        }
    }

    #[test]
    fn role_initialization_and_irrelevant_deliveries() {
        let init = initialize(br#"{"credentials":{"bot-token":"fixture"}}"#.to_vec()).unwrap();
        assert!(init.events.is_empty());
        let event = Event {
            event_id: "event".into(),
            sequence: 1,
            recorded_at_ms: 0,
            event_type: "capability.requested".into(),
            payload_schema: "fixture".into(),
            payload: Payload::Json(b"null".to_vec()),
            actor: Principal {
                kind: PrincipalKind::Agent,
                id: "fixture".into(),
            },
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
        };
        let outcome = Telegram::handle(context(), vec![event]).unwrap();
        assert!(outcome.events.is_empty());
        assert!(outcome.mutations.is_empty());
        assert_eq!(outcome.checkpoint, Some(1));
    }
}
