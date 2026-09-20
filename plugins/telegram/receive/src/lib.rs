#![allow(unsafe_op_in_unsafe_fn)]

mod files;
mod normalize;

use serde::Deserialize;
use serde_json::{Value, json};
use telegram::exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use telegram::http;
use telegram::pluribus::plugin::state;
use telegram::pluribus::plugin::types::{Error, Event, Mutation, Proposal, StateEntry};
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

fn setup(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let outcome = initialize(config)?;
    Ok(outcome)
}

impl Guest for Telegram {
    async fn run(mut context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        telegram::pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        use telegram::pluribus::plugin::runtime;
        let mut delay: u32 = 0;
        loop {
            if Self::waiting(&mut context, async {
                telegram::wasi::clocks::monotonic_clock::wait_for(u64::from(delay) * 1_000_000)
                    .await;
                Ok(())
            })
            .await?
            .is_none()
            {
                return Ok(());
            }
            let config = CONFIG.with(Slot::load)?;
            let result: Result<bool, Error> = async {
                let request = poll_request(&config, stored_offset()?)?;
                let Some(response) = Self::waiting(&mut context, http::fetch(request)).await?
                else {
                    return Ok(true);
                };
                let out = Self::process_response(response, {
                    let t = telegram::wasi::clocks::system_clock::now();
                    t.seconds * 1000 + i64::from(t.nanoseconds / 1_000_000)
                })?;
                runtime::commit(&out.events, &out.mutations, None)?;
                Ok(false)
            }
            .await;
            match result {
                Ok(true) => return Ok(()),
                Ok(false) => delay = 100,
                Err(error) if !should_retry(&error) => return Err(error),
                Err(_) => delay = delay.saturating_mul(2).clamp(100, 30_000),
            }
        }
    }

    async fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
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

fn should_retry(error: &Error) -> bool {
    error.retryable
        || matches!(
            error.code,
            telegram::pluribus::plugin::types::ErrorCode::DeadlineExceeded
        )
}

struct Poll {
    observations: Vec<Proposal>,
    offset: Option<i64>,
    pending: Vec<Mutation>,
}

fn poll_request(config: &Config, offset: Option<i64>) -> Result<http::Request, Error> {
    telegram::api::request(
        "getUpdates",
        &json!({"offset":offset,"limit":100,"timeout":config.poll_timeout_seconds,
            "allowed_updates":["message","edited_message","channel_post","edited_channel_post","message_reaction","callback_query"]}),
        &config.credentials.bot_token,
        config
            .poll_timeout_seconds
            .saturating_add(10)
            .saturating_mul(1000),
    )
}

/// Ready observations or pending downloads commit atomically with the offset.
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
    let mut proposals = Vec::new();
    for observation in observations {
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
        } else {
            proposals.push(observation_proposal(&observation.payload)?);
        }
    }
    Ok(Poll {
        observations: proposals,
        offset: next,
        pending,
    })
}

fn observation_proposal(observation: &Value) -> Result<Proposal, Error> {
    proposal(
        "observation.received",
        "dev.pluribus.telegram.observation.v1",
        observation,
        Some(format!("telegram:update:{}", observation["update_id"])),
        None,
    )
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
        observation
            .as_object_mut()
            .unwrap()
            .remove("mediaRetryAtMs");
        proposals.push(observation_proposal(&observation)?);
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

impl Telegram {
    fn process_response(response: http::Response, now_ms: i64) -> Result<SourceOutput, Error> {
        let mut out = SourceOutput {
            events: vec![],
            mutations: vec![],
        };
        let config = CONFIG.with(Slot::load)?;
        let response = telegram::api::decode_response(response.status, response.body)?;
        let previous = stored_offset()?;
        let result = poll_response(&config, response.value, previous)?;
        fetch_pending(&config, now_ms, &mut out.events, &mut out.mutations)?;
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

        Ok(out)
    }
}

struct SourceOutput {
    events: Vec<telegram::pluribus::plugin::types::Proposal>,
    mutations: Vec<telegram::pluribus::plugin::types::Mutation>,
}

impl Telegram {
    /// Services internal deliveries while external work is suspended.
    async fn waiting<T>(
        context: &mut Context,
        work: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<Option<T>, Error> {
        use futures_util::future::{Either, select};
        use telegram::pluribus::plugin::runtime::{self, Wake};
        futures_util::pin_mut!(work);
        loop {
            match select(Box::pin(runtime::next()), work.as_mut()).await {
                Either::Left((wake, _)) => match wake? {
                    Wake::Stop(_) => {
                        // Finish cancelled imports before dropping their borrowed resources.
                        let _ = work.await;
                        return Ok(None);
                    }
                    Wake::Events(events) => match Self::handle(context.clone(), events).await {
                        Ok(out) => {
                            runtime::commit(&out.events, &out.mutations, out.checkpoint)?;
                            context.state_checkpoint =
                                out.checkpoint.unwrap_or(context.state_checkpoint);
                        }
                        Err(error) => runtime::reject(&error)?,
                    },
                },
                Either::Right((result, _)) => return result.map(Some),
            }
        }
    }
}

#[cfg(test)]
mod role_tests {
    #[test]
    fn long_poll_timeout_is_retryable() {
        let error = Error {
            code: telegram::pluribus::plugin::types::ErrorCode::DeadlineExceeded,
            message: "timeout".into(),
            retryable: false,
            details: None,
        };
        assert!(should_retry(&error));
    }

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
    fn media_observation_waits_for_downloads_but_text_does_not() {
        let config: Config = serde_json::from_value(
            json!({"credentials":{"bot-token":"fixture"},"trusted_senders":["7"]}),
        )
        .unwrap();
        let result = poll_response(&config, json!([
            {"update_id":47,"message":{"from":{"id":7},"chat":{"id":9},"caption":"photo","photo":[{"file_id":"file"}]}},
            {"update_id":48,"message":{"from":{"id":7},"chat":{"id":9},"text":"text"}}
        ]), None).unwrap();
        assert_eq!(result.observations.len(), 1);
        assert_eq!(
            result.observations[0].idempotency_key.as_deref(),
            Some("telegram:update:48")
        );
        assert_eq!(result.pending.len(), 1);
        assert_eq!(result.offset, Some(49));
        let Mutation::Set(entry) = &result.pending[0] else {
            panic!("pending media is stored")
        };
        let deferred: Value = serde_json::from_slice(&entry.value).unwrap();
        assert_eq!(deferred["message"]["caption"], "photo");
        assert_eq!(deferred["media"][0]["status"], "pending");
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
        let outcome =
            futures_util::FutureExt::now_or_never(Telegram::handle(context(), vec![event]))
                .unwrap()
                .unwrap();
        assert!(outcome.events.is_empty());
        assert!(outcome.mutations.is_empty());
        assert_eq!(outcome.checkpoint, Some(1));
    }
}
