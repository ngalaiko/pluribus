#![allow(unsafe_op_in_unsafe_fn)]

mod files;
mod normalize;

use serde::Deserialize;
use serde_json::{Value, json};
use telegram::exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use telegram::pluribus::plugin::types::{Error, Event, Mutation, Payload, Proposal, StateEntry};
use telegram::pluribus::plugin::{events, state};
use telegram::{Slot, parse_config, proposal};

/// Key holding the `getUpdates` offset. State is a rebuildable projection: an
/// empty namespace re-polls from Telegram's own backlog.
const OFFSET_KEY: &str = "updates/offset";

#[derive(Clone, Deserialize)]
struct Config {
    credential_handle: String,
    #[serde(default)]
    trusted_senders: Vec<String>,
    #[serde(default = "default_poll_timeout")]
    poll_timeout_seconds: u32,
}

thread_local! {
    static CONFIG: Slot<(Config, u64)> = const { Slot::empty() };
}

struct Telegram;

impl Guest for Telegram {
    fn init(context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        initialize(
            &context.instance_id,
            config,
            next_generation(&context.instance_id)?,
        )
    }

    fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let (config, generation) = CONFIG.with(Slot::load)?;
        let mut proposals = Vec::new();
        let mut mutations = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type != "timer.fired" {
                continue;
            }
            let fired = json_payload(event)?;
            let request_id = fired["requestEventId"]
                .as_str()
                .ok_or_else(|| telegram::api::invalid("missing timer request"))?;
            let request = events::get(request_id)?;
            if json_payload(&request)?["generation"].as_u64() != Some(generation) {
                continue;
            }
            let Poll {
                observations,
                offset,
                pending,
            } = poll(&config)?;
            fetch_pending(
                &config,
                event.recorded_at_ms,
                &mut proposals,
                &mut mutations,
            )?;
            mutations.extend(pending);
            proposals.extend(observations);
            if let Some(offset) = offset {
                mutations.push(Mutation::Set(StateEntry {
                    key: OFFSET_KEY.to_owned(),
                    value: offset.to_string().into_bytes(),
                }));
            }
            let mut next = timer_request(generation, event.recorded_at_ms)?;
            next.causation_id = Some(event.event_id.clone());
            proposals.push(next);
        }

        Ok(Outcome {
            events: proposals,
            mutations,
            checkpoint,
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

/// Observations and offsets commit atomically, deduplicated by update ID.
fn poll(config: &Config) -> Result<Poll, Error> {
    let offset = stored_offset()?;
    let timeout_ms = config
        .poll_timeout_seconds
        .saturating_add(10)
        .saturating_mul(1_000);
    let response = telegram::api::call_json(
        "getUpdates",
        &json!({
            "offset": offset,
            "limit": 100,
            "timeout": config.poll_timeout_seconds,
            "allowed_updates": [
                "message",
                "edited_message",
                "channel_post",
                "edited_channel_post",
                "message_reaction",
                "callback_query"
            ]
        }),
        &config.credential_handle,
        timeout_ms,
    )?;
    let observations = normalize::updates(&response.value, &config.trusted_senders, 100)?;
    let next = response
        .value
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
        match files::download(file_id, &config.credential_handle) {
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

fn initialize(instance_id: &str, config: Vec<u8>, generation: u64) -> Result<Outcome, Error> {
    let parsed = parse_config(&config)?;
    CONFIG.with(|slot| slot.store((parsed, generation)));
    let mut start = timer_request(generation, 0)?;
    start.idempotency_key = Some(format!("{instance_id}:start:{generation}"));
    Ok(Outcome {
        events: vec![start],
        mutations: Vec::new(),
        checkpoint: None,
    })
}

/// Committed timer sequences distinguish receive lifetimes without volatile state.
fn next_generation(instance_id: &str) -> Result<u64, Error> {
    let mut after = None;
    let mut generation = 1;
    loop {
        let page = events::query(
            &events::Filter {
                after_sequence: after,
                event_types: vec!["timer.set".into()],
                correlation_id: None,
                activity_id: None,
                recorded_from_ms: None,
                recorded_to_ms: None,
            },
            256,
        )?;
        for event in &page.events {
            if event.actor.id == instance_id {
                generation = event.sequence.saturating_add(1);
            }
        }
        let Some(next) = page.next_sequence else {
            return Ok(generation);
        };
        after = Some(next);
    }
}

fn timer_request(generation: u64, due_at_ms: i64) -> Result<Proposal, Error> {
    proposal(
        "timer.set",
        "pluribus.timer-set/1",
        &json!({"dueAtMs": due_at_ms, "generation": generation}),
        None,
        None,
    )
}

fn json_payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => {
            serde_json::from_slice(bytes).map_err(|error| telegram::api::invalid(error.to_string()))
        }
        Payload::Blob(_) => Err(telegram::api::invalid("timer requires inline JSON")),
    }
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
        let init =
            initialize("fixture", br#"{"credential_handle":"fixture"}"#.to_vec(), 1).unwrap();
        assert_eq!(init.events.len(), 1);
        assert_eq!(init.events[0].event_type, "timer.set");
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
