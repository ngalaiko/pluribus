#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod compaction;
mod engine;
mod jobs;
mod provenance;
mod storage;
#[cfg(target_arch = "wasm32")]
mod component {
    use super::engine::{Config, Engine, resume};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    wit_bindgen::generate!({ generate_all,path:"../../../wit",world:"plugin"});
    use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
    use pluribus::plugin::types::{
        Error, ErrorCode, Event, Mutation, Payload, PrincipalKind, Proposal, StateEntry,
    };
    use pluribus::plugin::{events, state};
    thread_local! {static CONFIG:RefCell<Option<Config>>=const {RefCell::new(None)};}
    struct Cognition;
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../shared/run.rs"));

    fn setup(_: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let parsed: Config = serde_json::from_slice(&config).map_err(failure)?;
        parsed.budget.validate().map_err(failure)?;
        CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: None,
        })
    }

    impl Guest for Cognition {
        async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
            let outcome = setup(context.clone(), config)?;
            pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

            serve::<Self>(context).await
        }

        async fn handle(context: Context, batch: Vec<Event>) -> Result<Outcome, Error> {
            let config = CONFIG
                .with_borrow(|slot| slot.clone())
                .ok_or_else(|| failure("not initialized"))?;
            let checkpoint_batch = batch
                .iter()
                .all(|event| event.event_type == "cognition.checkpoint");
            let mut engine = if checkpoint_batch {
                let mut engine = Engine::default();
                let records = read_records(&super::storage::record_prefix("sequence", ""))?;
                if !records.is_empty() {
                    let bytes: Vec<u8> = records.into_values().flatten().collect();
                    let record: super::storage::Record =
                        serde_json::from_slice(&bytes).map_err(failure)?;
                    engine.sequence = record
                        .value
                        .as_u64()
                        .ok_or_else(|| failure("invalid sequence"))?;
                }
                engine
            } else {
                load_engine(&std::collections::BTreeMap::new(), &batch)?
            };
            let mut replay = std::collections::BTreeMap::new();
            let before_sequence = engine.sequence;
            let before = super::storage::records(&engine).map_err(failure)?;
            let mut drafts = vec![];
            for event in &batch {
                let Payload::Json(bytes) = &event.payload else {
                    continue;
                };
                let mut value: Value = serde_json::from_slice(bytes).map_err(failure)?;
                if event.actor.id == context.instance_id
                    && ["cognition.job-updated", "cognition.observation-associated"]
                        .contains(&event.event_type.as_str())
                {
                    continue;
                }
                if event.event_type == "cognition.checkpoint" {
                    if event.actor.id == context.instance_id {
                        if value["version"] != 1 {
                            return Err(failure("unsupported checkpoint version"));
                        }
                        if value["sequence"]
                            .as_u64()
                            .is_some_and(|sequence| sequence > engine.sequence)
                        {
                            for mutation in value["mutations"]
                                .as_array()
                                .ok_or_else(|| failure("missing checkpoint mutations"))?
                            {
                                let key = mutation["key"]
                                    .as_str()
                                    .ok_or_else(|| failure("missing checkpoint key"))?;
                                if !key.starts_with("engine/record/") {
                                    return Err(failure("invalid checkpoint key"));
                                }
                                let bytes: Option<Vec<u8>> = decode_record(&mutation["value"])?;
                                replay.insert(key.to_owned(), bytes);
                            }
                        }
                    }
                    continue;
                }
                if event.sequence <= engine.sequence {
                    continue;
                }
                engine.now_ms = event.recorded_at_ms;
                engine.sequence = event.sequence;
                if event.event_type.starts_with("operator.")
                    && !(matches!(event.actor.kind, PrincipalKind::Node)
                        && event.actor.id == format!("operator:{}", context.agent.id))
                {
                    continue;
                }
                if engine.result_seen(&event.event_type, &event.event_id, &value) {
                    continue;
                }
                if event.event_type == "model.completed" {
                    validate_summary_provenance(&mut engine, &value)?;
                }
                if event.event_type == "code.completed" {
                    validate_checkpoint_provenance(&mut engine, &mut value)?;
                }
                let generated = engine.event(
                    &config,
                    &event.event_id,
                    &event.event_type,
                    &value,
                    event.causation_id.as_deref(),
                );
                let resumed = generated.iter().any(|d| {
                    d.kind == "code.resumed"
                        && d.payload["sessionId"] == value["sessionId"]
                        && d.payload["response"]["id"] == value["id"]
                });
                drafts.extend(generated);
                if !resumed
                    && event.event_type == "code.yielded"
                    && value["method"] == "history.read"
                {
                    let session = value["sessionId"].as_str().unwrap_or("");
                    if let Some(window) = engine.history_window(&config, session) {
                        let args = &value["args"];
                        // A read is clamped into the window, so a child
                        // cannot page past the range its parent named.
                        let start = args["after"].as_u64().unwrap_or(0).max(window.after);
                        let remaining = window
                            .after
                            .saturating_add(u64::from(window.limit))
                            .saturating_sub(start);
                        let limit = args["limit"]
                            .as_u64()
                            .unwrap_or(100)
                            .min(100)
                            .min(remaining);
                        let event_types: Vec<String> = match args.get("eventTypes") {
                            Some(filter) => match serde_json::from_value(filter.clone()) {
                                Ok(types) => types,
                                Err(_) => {
                                    drafts.push(resume(
                                        session,
                                        &value["id"],
                                        Err("eventTypes must be an array of strings".into()),
                                        &event.event_id,
                                    ));
                                    continue;
                                }
                            },
                            None => vec![],
                        };
                        let end = window.after.saturating_add(u64::from(window.limit));
                        let page = events::query(
                            &events::Filter {
                                after_sequence: Some(start),
                                event_types,
                                correlation_id: None,
                                activity_id: None,
                                recorded_from_ms: None,
                                recorded_to_ms: None,
                                text_query: None,
                                before_sequence: None,
                                conversation_id: None,
                                descending: false,
                            },
                            limit as u32,
                        )?;
                        let mut rows = vec![];
                        let mut size = 0;
                        let mut after = start;
                        for item in page.events {
                            if item.sequence > end {
                                break;
                            }
                            let payload = match item.payload {
                                Payload::Json(bytes) => {
                                    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
                                }
                                Payload::Blob(_) => Value::Null,
                            };
                            let mut row = json!({"eventId":item.event_id,"sequence":item.sequence,"type":item.event_type,"actorId":item.actor.id,"recordedAtMs":item.recorded_at_ms,"causationId":item.causation_id,"payload":payload});
                            let bytes = row.to_string().len();
                            if bytes > 63 * 1024 {
                                row["payload"] = Value::Null;
                                row["payloadOmitted"] = json!({"bytes":bytes});
                            }
                            let bytes = row.to_string().len();
                            if size + bytes > 63 * 1024 {
                                break;
                            }
                            size += bytes;
                            after = item.sequence;
                            rows.push(row);
                        }
                        drafts.push(resume(
                            session,
                            &value["id"],
                            Ok(json!({"events":rows,"after":after})),
                            &event.event_id,
                        ));
                    } else {
                        drafts.push(resume(
                            session,
                            &value["id"],
                            Err("history access is not granted".into()),
                            &event.event_id,
                        ));
                    }
                }
                if !resumed
                    && event.event_type == "code.yielded"
                    && value["method"] == "history.search"
                {
                    let session = value["sessionId"].as_str().unwrap_or("");
                    if let Some(window) = engine.history_window(&config, session) {
                        let args = &value["args"];
                        let Some(text) = args["query"].as_str() else {
                            drafts.push(resume(
                                session,
                                &value["id"],
                                Err("history.search requires a string query".into()),
                                &event.event_id,
                            ));
                            continue;
                        };
                        if text.trim().is_empty() || text.len() > 4096 {
                            drafts.push(resume(
                                session,
                                &value["id"],
                                Err("history.search requires a nonempty query of at most 4096 bytes".into()),
                                &event.event_id,
                            ));
                            continue;
                        }
                        if ["conversationId", "correlationId", "activityId"]
                            .iter()
                            .any(|name| args.get(*name).is_some_and(|value| !value.is_string()))
                        {
                            drafts.push(resume(
                                session,
                                &value["id"],
                                Err(
                                    "conversationId, correlationId, and activityId must be strings"
                                        .into(),
                                ),
                                &event.event_id,
                            ));
                            continue;
                        }
                        let event_types: Vec<String> = match args.get("eventTypes") {
                            Some(filter) => match serde_json::from_value(filter.clone()) {
                                Ok(types) => types,
                                Err(_) => {
                                    drafts.push(resume(
                                        session,
                                        &value["id"],
                                        Err("eventTypes must be an array of strings".into()),
                                        &event.event_id,
                                    ));
                                    continue;
                                }
                            },
                            None => vec![],
                        };
                        let parse_time = |name: &str| -> Result<Option<i64>, String> {
                            match args.get(name) {
                                None => Ok(None),
                                Some(value) => value
                                    .as_i64()
                                    .map(Some)
                                    .ok_or_else(|| format!("{name} must be an integer")),
                            }
                        };
                        let parse_u64 = |name: &str| -> Result<Option<u64>, String> {
                            match args.get(name) {
                                None => Ok(None),
                                Some(value) => value
                                    .as_u64()
                                    .map(Some)
                                    .ok_or_else(|| format!("{name} must be a nonnegative integer")),
                            }
                        };
                        let (recorded_from_ms, recorded_to_ms) =
                            match (parse_time("recordedFromMs"), parse_time("recordedToMs")) {
                                (Ok(from), Ok(to)) => (from, to),
                                (Err(error), _) | (_, Err(error)) => {
                                    drafts.push(resume(
                                        session,
                                        &value["id"],
                                        Err(error.into()),
                                        &event.event_id,
                                    ));
                                    continue;
                                }
                            };
                        if recorded_from_ms
                            .zip(recorded_to_ms)
                            .is_some_and(|(from, to)| from > to)
                        {
                            drafts.push(resume(
                                session,
                                &value["id"],
                                Err("recordedFromMs must not exceed recordedToMs".into()),
                                &event.event_id,
                            ));
                            continue;
                        }
                        let (requested_before, requested_limit) =
                            match (parse_u64("before"), parse_u64("limit")) {
                                (Ok(before), Ok(limit)) => (before, limit),
                                (Err(error), _) | (_, Err(error)) => {
                                    drafts.push(resume(
                                        session,
                                        &value["id"],
                                        Err(error.into()),
                                        &event.event_id,
                                    ));
                                    continue;
                                }
                            };
                        if requested_limit == Some(0) {
                            drafts.push(resume(
                                session,
                                &value["id"],
                                Err("limit must be greater than zero".into()),
                                &event.event_id,
                            ));
                            continue;
                        }
                        let upper = window.after.saturating_add(u64::from(window.limit));
                        let before = requested_before
                            .unwrap_or(upper.saturating_add(1))
                            .min(upper.saturating_add(1));
                        let lower = window.after;
                        let limit = requested_limit
                            .unwrap_or(20)
                            .min(100)
                            .min(u64::from(window.limit));
                        let page = events::query(
                            &events::Filter {
                                text_query: Some(text.to_owned()),
                                after_sequence: Some(lower),
                                before_sequence: Some(before),
                                event_types,
                                conversation_id: args["conversationId"].as_str().map(str::to_owned),
                                correlation_id: args["correlationId"].as_str().map(str::to_owned),
                                activity_id: args["activityId"].as_str().map(str::to_owned),
                                recorded_from_ms,
                                recorded_to_ms,
                                descending: true,
                            },
                            limit as u32,
                        )?;
                        let mut rows = vec![];
                        let mut size = 0;
                        let mut after = page.next_sequence;
                        let mut error = None;
                        for item in page.events {
                            let excerpt = match item.payload {
                                Payload::Json(bytes) => String::from_utf8_lossy(&bytes)
                                    .chars()
                                    .take(1024)
                                    .collect::<String>(),
                                Payload::Blob(_) => String::new(),
                            };
                            let row = json!({"eventId":item.event_id,"sequence":item.sequence,"type":item.event_type,"actorId":item.actor.id,"recordedAtMs":item.recorded_at_ms,"causationId":item.causation_id,"excerpt":excerpt});
                            let bytes = row.to_string().len();
                            if rows.is_empty() && bytes > 63 * 1024 {
                                error = Some(
                                    "history.search event metadata exceeds the page limit".into(),
                                );
                                break;
                            }
                            if size + bytes > 63 * 1024 {
                                break;
                            }
                            size += bytes;
                            after = Some(item.sequence);
                            rows.push(row);
                        }
                        drafts.push(resume(
                            session,
                            &value["id"],
                            match error {
                                Some(error) => Err(error),
                                None => Ok(json!({"events":rows,"nextBefore":after})),
                            },
                            &event.event_id,
                        ));
                    } else {
                        drafts.push(resume(
                            session,
                            &value["id"],
                            Err("history access is not granted".into()),
                            &event.event_id,
                        ));
                    }
                }
            }
            let replay_only = batch
                .iter()
                .all(|event| event.event_type == "cognition.checkpoint");
            if !replay_only && engine.sequence != before_sequence {
                for (id, job) in &engine.jobs {
                    project_change(
                        &before,
                        "jobs",
                        id,
                        job,
                        ("cognition.job-updated", "job"),
                        &batch,
                        &mut drafts,
                    )?;
                }
                for (id, observation) in &engine.inbox {
                    project_change(
                        &before,
                        if observation.value.is_null() {
                            "observations_seen"
                        } else {
                            "inbox"
                        },
                        id,
                        observation,
                        ("cognition.observation-associated", "observation"),
                        &batch,
                        &mut drafts,
                    )?;
                }
                engine.retire();
            }
            let mut mutations = Vec::new();
            if !replay.is_empty() {
                for (key, value) in &replay {
                    mutations.push(match value {
                        Some(value) => Mutation::Set(StateEntry {
                            key: key.clone(),
                            value: value.clone(),
                        }),
                        None => Mutation::Delete(key.clone()),
                    });
                }
            }
            if !replay_only {
                let changes = record_changes(&before, &engine)?;
                for group in changes.chunks(4) {
                    drafts.push(super::engine::Draft {
                        kind: "cognition.checkpoint".into(),
                        payload: json!({"version":1,"sequence":engine.sequence,"mutations":group}),
                        cause: batch.last().unwrap().event_id.clone(),
                    });
                }
                for change in changes {
                    let key = change["key"].as_str().unwrap().to_owned();
                    mutations.push(if change["value"].is_null() {
                        Mutation::Delete(key)
                    } else {
                        Mutation::Set(StateEntry {
                            key,
                            value: decode_record(&change["value"])?.unwrap(),
                        })
                    });
                }
            }
            let proposals = drafts
                .into_iter()
                .map(|d| Proposal {
                    event_type: d.kind.clone(),
                    payload_schema: match d.kind.as_str() {
                        "cognition.job-updated" => "pluribus.job/1",
                        "cognition.observation-associated" => "pluribus.observation-association/1",
                        "cognition.checkpoint" => "pluribus.cognition-checkpoint/1",
                        "cognition.cancel-requested" => "pluribus.cognition-cancel/1",
                        "model.requested" => "pluribus.model-request/1",
                        "capability.requested" => "pluribus.capability-request/1",
                        _ => "pluribus.rlm/1",
                    }
                    .into(),
                    payload: Payload::Json(serde_json::to_vec(&d.payload).unwrap()),
                    idempotency_key: None,
                    causation_id: Some(d.cause),
                })
                .collect();
            Ok(Outcome {
                events: proposals,
                mutations,
                checkpoint: batch.last().map(|e| e.sequence),
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
    fn decode_record(value: &Value) -> Result<Option<Vec<u8>>, Error> {
        match value {
            Value::String(encoded) => STANDARD.decode(encoded).map(Some).map_err(failure),
            _ => serde_json::from_value(value.clone()).map_err(failure),
        }
    }

    fn validate_summary_provenance(engine: &mut Engine, value: &Value) -> Result<(), Error> {
        let Some(call_id) = value["call_id"].as_str() else {
            return Ok(());
        };
        let Some(content) = value["message"]["content"].as_array() else {
            return Ok(());
        };
        let Some(arguments) = content
            .iter()
            .find(|part| part["kind"] == "tool-call" && part["name"] == "compact")
            .map(|part| &part["arguments"])
        else {
            return Ok(());
        };
        engine.begin_compaction_provenance(call_id);
        let Ok(summary) = super::compaction::validate(arguments) else {
            return Ok(());
        };
        let Some(scope) = engine.compaction_scope(call_id) else {
            return Ok(());
        };
        let scope = match scope {
            Ok(scope) => scope,
            Err(error) => {
                engine.set_compaction_provenance_error(call_id, error);
                return Ok(());
            }
        };
        let mut events = std::collections::BTreeMap::new();
        for source_id in &summary.source_ids {
            let event = match events::get(source_id) {
                Ok(event) => event,
                Err(_) => {
                    engine.set_compaction_provenance_error(
                        call_id,
                        "working summary references an inaccessible source event".into(),
                    );
                    return Ok(());
                }
            };
            events.insert(
                source_id.clone(),
                super::provenance::SourceEvent {
                    sequence: event.sequence,
                    original: event.event_type != "cognition.checkpoint",
                },
            );
        }
        if let Err(error) = super::provenance::validate(&summary, &events, scope) {
            engine.set_compaction_provenance_error(call_id, error);
        } else {
            engine.set_compaction_provenance_verified(call_id);
        }
        Ok(())
    }

    fn validate_checkpoint_provenance(engine: &mut Engine, value: &mut Value) -> Result<(), Error> {
        let Some(session) = value["sessionId"].as_str() else {
            return Ok(());
        };
        engine.begin_checkpoint_provenance(session);
        let Some(summary) = value.pointer("/checkpoint/state/workingSummary") else {
            return Ok(());
        };
        let Ok(summary) = super::compaction::validate(summary) else {
            return Ok(());
        };
        let Some(scope) = engine.checkpoint_scope(session) else {
            return Ok(());
        };
        let scope = match scope {
            Ok(scope) => scope,
            Err(error) => {
                engine.set_checkpoint_provenance_error(session, error);
                return Ok(());
            }
        };
        let mut events = std::collections::BTreeMap::new();
        for source_id in &summary.source_ids {
            let event = match events::get(source_id) {
                Ok(event) => event,
                Err(_) => {
                    engine.set_checkpoint_provenance_error(
                        session,
                        "working summary references an inaccessible source event".into(),
                    );
                    return Ok(());
                }
            };
            events.insert(
                source_id.clone(),
                super::provenance::SourceEvent {
                    sequence: event.sequence,
                    original: event.event_type != "cognition.checkpoint",
                },
            );
        }
        if let Err(error) = super::provenance::validate(&summary, &events, scope) {
            engine.set_checkpoint_provenance_error(session, error);
        } else {
            engine.set_checkpoint_provenance_verified(session);
        }
        Ok(())
    }
    fn project_change(
        before: &std::collections::BTreeMap<String, Vec<u8>>,
        field: &str,
        id: &str,
        value: &impl serde::Serialize,
        (kind, item): (&str, &str),
        batch: &[Event],
        drafts: &mut Vec<super::engine::Draft>,
    ) -> Result<(), Error> {
        let mut record = std::collections::BTreeMap::new();
        super::storage::insert_record(&mut record, field, id, value).map_err(failure)?;
        if record
            .iter()
            .any(|(key, bytes)| before.get(key) != Some(bytes))
        {
            drafts.push(super::engine::Draft {
                kind: kind.into(),
                payload: json!({"version":1,(item):value}),
                cause: batch.last().unwrap().event_id.clone(),
            });
        }
        Ok(())
    }
    fn record_changes(
        old: &std::collections::BTreeMap<String, Vec<u8>>,
        after: &Engine,
    ) -> Result<Vec<Value>, Error> {
        let new = super::storage::records(after).map_err(failure)?;
        let mut changes = Vec::new();
        for (key, value) in &new {
            if old.get(key) != Some(value) {
                changes.push(json!({"key":key,"value":STANDARD.encode(value)}));
            }
        }
        for key in old.keys().filter(|key| !new.contains_key(*key)) {
            changes.push(json!({"key":key,"value":null}));
        }
        // Sequence commits after all record fragments during checkpoint replay.
        changes.sort_by_key(|change| {
            change["key"]
                .as_str()
                .unwrap()
                .starts_with("engine/record/sequence/")
        });
        Ok(changes)
    }
    fn read_records(prefix: &str) -> Result<std::collections::BTreeMap<String, Vec<u8>>, Error> {
        let mut entries = std::collections::BTreeMap::new();
        let mut after = None;
        loop {
            let page = state::scan(prefix, after.as_deref(), 100)?;
            for entry in page.entries {
                entries.insert(entry.key, entry.value);
            }
            match page.next_key {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(entries)
    }
    fn load_engine(
        overlay: &std::collections::BTreeMap<String, Vec<u8>>,
        batch: &[Event],
    ) -> Result<Engine, Error> {
        let mut engine = serde_json::to_value(Engine::default()).map_err(failure)?;
        let mut entries = std::collections::BTreeMap::new();
        for field in [
            "jobs", "inbox", "tasks", "calls", "budgets", "queue", "now_ms", "sequence",
        ] {
            entries.extend(read_records(&format!("engine/record/{field}/"))?);
        }
        for event in batch {
            if event.event_type == "observation.received" {
                entries.extend(read_records(&super::storage::record_prefix(
                    "observations_seen",
                    &event.event_id,
                ))?);
            }
            if let Payload::Json(bytes) = &event.payload {
                let value: Value = serde_json::from_slice(bytes).map_err(failure)?;
                if let Some(key) =
                    super::engine::result_key(&event.event_type, &event.event_id, &value)
                {
                    entries.extend(read_records(&super::storage::record_prefix(
                        "seen_results",
                        &key,
                    ))?);
                }
            }
        }
        entries.extend(overlay.clone());
        let mut groups: std::collections::BTreeMap<String, Vec<u8>> =
            std::collections::BTreeMap::new();
        for (key, bytes) in entries {
            let prefix = key
                .rsplit_once('/')
                .ok_or_else(|| failure("invalid record key"))?
                .0
                .to_owned();
            // A null record masks the remaining fragments of its prior value.
            if let Some(current) = groups.get(&prefix)
                && serde_json::from_slice::<super::storage::Record>(current).is_ok()
            {
                continue;
            }
            groups.entry(prefix).or_default().extend(bytes);
        }
        for bytes in groups.into_values() {
            let record = serde_json::from_slice(&bytes).map_err(failure)?;
            super::storage::apply_record(&mut engine, record);
        }
        serde_json::from_value(engine).map_err(failure)
    }
    fn failure(e: impl std::fmt::Display) -> Error {
        Error {
            code: ErrorCode::Internal,
            message: e.to_string(),
            retryable: false,
            details: None,
        }
    }
    export!(Cognition);
}
