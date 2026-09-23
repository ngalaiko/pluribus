use crate::management::{manage, text};
use crate::schedule::Schedule;
use crate::timers::Timer;
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use futures_util::future::{Either, select};
use pluribus::plugin::types::{Error, ErrorCode, Event, Mutation, Payload, Proposal, StateEntry};
use pluribus::plugin::{events, runtime, state};
use pluribus_plugin_sdk::export;
pub use pluribus_plugin_sdk::{exports, pluribus, wasi};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

struct Scheduler;
const THROUGH: &str = "projection-through";
const PAGE: u32 = 64;

impl Guest for Scheduler {
    async fn run(mut context: Context, config: Vec<u8>) -> Result<(), Error> {
        let _: Value = serde_json::from_slice(&config).map_err(invalid)?;
        runtime::ready(vec![], vec![]).await?;
        loop {
            // Rebuild from committed receipts before producing external observations.
            while refresh(&context)? {
                wasi::clocks::monotonic_clock::wait_for(1).await;
            }
            let delay = fire(&context)?;
            let wake = {
                let next = std::pin::pin!(runtime::next());
                let clock =
                    std::pin::pin!(wasi::clocks::monotonic_clock::wait_for(delay * 1_000_000));
                match select(next, clock).await {
                    Either::Left((wake, _)) => Some(wake?),
                    Either::Right(_) => None,
                }
            };
            match wake {
                Some(runtime::Wake::Stop(_)) => return Ok(()),
                Some(runtime::Wake::Events(events)) => {
                    match Self::handle(context.clone(), events).await {
                        Ok(outcome) => {
                            runtime::commit(
                                &outcome.events,
                                &outcome.mutations,
                                outcome.checkpoint,
                            )?;
                            if let Some(checkpoint) = outcome.checkpoint {
                                context.state_checkpoint = checkpoint;
                            }
                        }
                        Err(error) => runtime::reject(&error)?,
                    }
                }
                None => {}
            }
        }
    }

    async fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut schedules = load::<Schedule>("schedule:")?;
        let mut output = Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: events.last().map(|e| e.sequence),
        };
        for event in events {
            if event.event_type != "capability.requested" {
                continue;
            }
            let request = payload(&event)?;
            let Some(capability) = request["capability"]
                .as_str()
                .filter(|name| name.starts_with("schedule."))
            else {
                continue;
            };
            let result = origin(&event).and_then(|(origin_id, origin)| {
                manage(
                    &mut schedules,
                    capability,
                    &request["arguments"],
                    &event.event_id,
                    &origin_id,
                    &origin,
                    now_ms(),
                )
                .map_err(invalid)
            });
            match result {
                Ok((value, changed)) => {
                    if let Some((id, schedule)) = changed {
                        let snapshot = json!({"id": id, "schedule": schedule});
                        output.events.push(proposal(
                            "schedule.updated",
                            snapshot,
                            &event.event_id,
                            None,
                        )?);
                        output
                            .mutations
                            .push(schedule_mutation(&id, schedule.as_ref())?);
                    }
                    output.events.push(proposal(
                        "capability.completed",
                        json!({"requestEventId":event.event_id,"output":value}),
                        &event.event_id,
                        None,
                    )?);
                }
                Err(error) => output.events.push(proposal(
                    "capability.failed",
                    json!({"requestEventId":event.event_id,"reason":error.message}),
                    &event.event_id,
                    None,
                )?),
            }
        }
        let _ = context;
        Ok(output)
    }

    fn stop(_context: Context, _deadline: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }
}

fn origin(request: &Event) -> Result<(String, Value), Error> {
    let mut event = request.clone();
    for _ in 0..4096 {
        if event.event_type == "observation.received" {
            let value = payload(&event)?;
            for field in ["provider", "externalSenderId", "conversationId"] {
                text(&value, field, 1024).map_err(invalid)?;
            }
            return Ok((
                event.event_id,
                json!({
                    "provider":value["provider"],"externalSenderId":value["externalSenderId"],
                    "conversationId":value["conversationId"],"trusted":value.get("trusted").cloned().unwrap_or(json!(true)),
                }),
            ));
        }
        let cause = event
            .causation_id
            .as_ref()
            .ok_or_else(|| invalid("schedule requires an observation origin"))?;
        let parent = events::get(cause)?;
        if parent.sequence >= event.sequence {
            return Err(invalid("invalid origin chain"));
        }
        event = parent;
    }
    Err(invalid("origin chain exceeds limit"))
}

fn refresh(context: &Context) -> Result<bool, Error> {
    let through: u64 = state::get(THROUGH)?
        .map(|b| serde_json::from_slice(&b))
        .transpose()
        .map_err(internal)?
        .unwrap_or(0);
    let page = events::query(
        &events::Filter {
            after_sequence: Some(through),
            event_types: vec![
                "timer.set".into(),
                "timer.cancel".into(),
                "timer.fired".into(),
                "schedule.updated".into(),
            ],
            correlation_id: None,
            activity_id: None,
            recorded_from_ms: None,
            recorded_to_ms: None,
            text_query: None,
            before_sequence: None,
            conversation_id: None,
            descending: false,
        },
        PAGE,
    )?;
    if page.events.is_empty() {
        return Ok(false);
    }
    let mut timers = load::<Timer>("timer:")?;
    let mut mutations = Vec::new();
    for event in &page.events {
        let value = payload(event)?;
        match event.event_type.as_str() {
            "timer.set" => {
                if let Some(due) = value["dueAtMs"].as_i64() {
                    let timer = Timer {
                        request: event.event_id.clone(),
                        owner: event.actor.id.clone(),
                        due,
                    };
                    let key = format!("timer:{}", event.event_id);
                    mutations.push(set(&key, &timer)?);
                    timers.insert(key, timer);
                }
            }
            "timer.cancel" | "timer.fired" => {
                if let Some(id) = value["requestEventId"].as_str() {
                    let key = format!("timer:{id}");
                    if event.event_type == "timer.fired"
                        || timers.get(&key).is_some_and(|t| t.owner == event.actor.id)
                    {
                        timers.remove(&key);
                        mutations.push(Mutation::Delete(key));
                    }
                }
            }
            "schedule.updated" if event.actor.id == context.instance_id => {
                let id = value["id"]
                    .as_str()
                    .ok_or_else(|| internal("snapshot missing id"))?;
                let schedule: Option<Schedule> =
                    serde_json::from_value(value["schedule"].clone()).map_err(internal)?;
                mutations.push(schedule_mutation(id, schedule.as_ref())?);
            }
            _ => {}
        }
    }
    mutations.push(set(THROUGH, &page.events.last().unwrap().sequence)?);
    runtime::commit(&[], &mutations, None)?;
    Ok(true)
}

fn fire(_context: &Context) -> Result<u64, Error> {
    let now = now_ms();
    let mut output = vec![];
    let mut mutations = vec![];
    let mut next = now.saturating_add(1000);
    for (key, timer) in load::<Timer>("timer:")? {
        if timer.due <= now && output.len() < PAGE as usize {
            output.push(proposal(
                "timer.fired",
                json!({"requestEventId":timer.request,"dueAtMs":timer.due}),
                &timer.request,
                Some(format!("timer:{}", timer.request)),
            )?);
            mutations.push(Mutation::Delete(key));
        } else {
            next = next.min(timer.due);
        }
    }
    for (_, mut schedule) in load::<Schedule>("schedule:")? {
        if output.len() < PAGE as usize
            && let Some((key, observation)) = schedule.occurrence(now)
        {
            output.push(proposal(
                "observation.received",
                observation,
                &schedule.cause,
                Some(key.clone()),
            )?);
            output.push(proposal(
                "schedule.updated",
                json!({"id":schedule.id,"schedule":schedule}),
                &schedule.cause,
                Some(format!("{key}:state")),
            )?);
            mutations.push(schedule_mutation(&schedule.id, Some(&schedule))?);
        }
        if !schedule.paused
            && let Some(due) = schedule.next_at_ms
        {
            next = next.min(due);
        }
    }
    if !output.is_empty() {
        runtime::commit(&output, &mutations, None)?;
    }
    Ok(next.saturating_sub(now).clamp(1, 1000) as u64)
}

fn load<T: for<'a> Deserialize<'a>>(prefix: &str) -> Result<BTreeMap<String, T>, Error> {
    let mut result = BTreeMap::new();
    let mut after = None;
    loop {
        let page = state::scan(prefix, after.as_deref(), PAGE)?;
        for entry in page.entries {
            result.insert(
                entry.key,
                serde_json::from_slice(&entry.value).map_err(internal)?,
            );
        }
        match page.next_key {
            Some(key) => after = Some(key),
            None => return Ok(result),
        }
    }
}

fn schedule_mutation(id: &str, schedule: Option<&Schedule>) -> Result<Mutation, Error> {
    let key = format!("schedule:{id}");
    schedule.map_or_else(|| Ok(Mutation::Delete(key.clone())), |s| set(&key, s))
}
fn set(key: &str, value: &impl Serialize) -> Result<Mutation, Error> {
    Ok(Mutation::Set(StateEntry {
        key: key.into(),
        value: serde_json::to_vec(value).map_err(internal)?,
    }))
}
fn payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes).map_err(invalid),
        Payload::Blob(_) => Err(invalid("JSON required")),
    }
}
fn proposal(kind: &str, value: Value, cause: &str, key: Option<String>) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: kind.into(),
        payload_schema: format!("pluribus.{kind}/1"),
        payload: Payload::Json(serde_json::to_vec(&value).map_err(internal)?),
        causation_id: Some(cause.into()),
        idempotency_key: key,
    })
}
fn now_ms() -> i64 {
    let now = wasi::clocks::system_clock::now();
    now.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(now.nanoseconds / 1_000_000))
}
fn invalid(error: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::InvalidArgument,
        message: error.to_string(),
        retryable: false,
        details: None,
    }
}
fn internal(error: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::Internal,
        message: error.to_string(),
        retryable: false,
        details: None,
    }
}
export!(Scheduler with_types_in pluribus_plugin_sdk);
