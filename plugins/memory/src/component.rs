use crate::engine::{Change, Config, Engine, Store};
use serde_json::{Value, json};
use std::cell::RefCell;
wit_bindgen::generate!({ generate_all,path:"../../wit",world:"plugin"});
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::types::{
    Error, ErrorCode, Event, Mutation, Payload, PrincipalKind, Proposal, StateEntry,
};
use pluribus::plugin::{events, state};
thread_local! { static CONFIG: RefCell<Config> = RefCell::new(Config::default()); }
struct Memory;
struct HostStore;
impl Store for HostStore {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        state::get(key)
            .map_err(|e| e.message)?
            .map(|v| serde_json::from_slice(&v).map_err(|e| e.to_string()))
            .transpose()
    }
    fn scan(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, String> {
        state::scan(prefix, after, limit as u32)
            .map_err(|e| e.message)?
            .entries
            .into_iter()
            .map(|entry| {
                Ok((
                    entry.key,
                    serde_json::from_slice(&entry.value).map_err(|e| e.to_string())?,
                ))
            })
            .collect()
    }
}
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../shared/run.rs"));

fn setup(_: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let parsed: Config = serde_json::from_slice(&config).map_err(failure)?;
    parsed.validate().map_err(failure)?;
    CONFIG.with_borrow_mut(|c| *c = parsed);
    Ok(Outcome {
        events: vec![],
        mutations: vec![],
        checkpoint: None,
    })
}

impl Guest for Memory {
    async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        serve::<Self>(context).await
    }

    async fn handle(context: Context, batch: Vec<Event>) -> Result<Outcome, Error> {
        let config = CONFIG.with_borrow(Clone::clone);
        let mut engine = Engine::new(&HostStore, &config, &context.instance_id);
        let mut proposals = vec![];
        let mut checkpoint = None;
        // Four state keys per write leave room under the host's 256-key ceiling.
        for event in batch.iter().take(32) {
            checkpoint = Some(event.sequence);
            if ["memory.remembered", "memory.superseded", "memory.forgotten"]
                .contains(&event.event_type.as_str())
            {
                if event.actor.kind == PrincipalKind::Component
                    && event.actor.id == context.instance_id
                {
                    let change: Change =
                        serde_json::from_value(payload(event)?).map_err(failure)?;
                    engine.apply(&change).map_err(failure)?;
                }
                continue;
            }
            if event.event_type != "capability.requested" {
                continue;
            }
            let request = payload(event)?;
            let Some(operation) = request["capability"]
                .as_str()
                .and_then(|s| s.strip_prefix("memory."))
            else {
                continue;
            };
            let result = engine.execute(
                operation,
                &request["arguments"],
                event.recorded_at_ms,
                event.sequence,
                |id| events::get(id).is_ok_and(|source| source.sequence < event.sequence),
            );
            match result {
                Ok((output, change)) => {
                    if let Some(change) = change {
                        let kind = match operation {
                            "remember" => "remembered",
                            "supersede" => "superseded",
                            _ => "forgotten",
                        };
                        proposals.push(proposal(
                            &format!("memory.{kind}"),
                            &format!("dev.pluribus.memory.{kind}/1"),
                            &serde_json::to_value(change).unwrap(),
                            &event.event_id,
                        ));
                    }
                    proposals.push(proposal(
                        "capability.completed",
                        &format!("dev.pluribus.memory.{operation}.result/1"),
                        &json!({"requestEventId":event.event_id,"output":output}),
                        &event.event_id,
                    ));
                }
                Err(code) => {
                    // Storage failure must leave the request retryable.
                    if ![
                        "invalid-argument",
                        "source-unavailable",
                        "unavailable",
                        "conflict",
                        "operation-conflict",
                        "capacity-exceeded",
                        "result-too-large",
                    ]
                    .contains(&code.as_str())
                    {
                        return Err(failure(code));
                    }
                    proposals.push(proposal(
                        "capability.failed",
                        &format!("dev.pluribus.memory.{operation}.result/1"),
                        &json!({"requestEventId":event.event_id,"code":code,"message":code}),
                        &event.event_id,
                    ));
                }
            }
        }
        Ok(Outcome {
            events: proposals,
            mutations: engine
                .changes
                .into_iter()
                .map(|(key, value)| {
                    Mutation::Set(StateEntry {
                        key,
                        value: serde_json::to_vec(&value).unwrap(),
                    })
                })
                .collect(),
            checkpoint,
        })
    }
    fn stop(_: Context, _: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }
}
fn payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes).map_err(failure),
        _ => Err(failure("JSON required")),
    }
}
fn proposal(kind: &str, schema: &str, value: &Value, cause: &str) -> Proposal {
    Proposal {
        event_type: kind.into(),
        payload_schema: schema.into(),
        payload: Payload::Json(serde_json::to_vec(value).unwrap()),
        causation_id: Some(cause.into()),
        idempotency_key: None,
    }
}
fn failure(message: impl ToString) -> Error {
    Error {
        code: ErrorCode::Internal,
        message: message.to_string(),
        retryable: true,
        details: None,
    }
}
export!(Memory);
