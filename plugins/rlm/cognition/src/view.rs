use super::*;
use std::collections::BTreeSet;

const MAX_VIEW_BYTES: usize = 2 * 1024 * 1024;
const INDEX_FIELDS: [&str; 3] = ["jobs", "tasks", "inbox"];

fn resource_error(field: &str, key: &str, bytes: usize, limit: usize) -> Error {
    Error {
        code: ErrorCode::ResourceExhausted,
        message: format!("{field} record exceeds the bounded working set"),
        retryable: false,
        details: Some(
            serde_json::to_vec(&json!({
                "resource":"state","currentBytes":0,"requestedBytes":bytes,"limitBytes":limit,
                "phase":"restore","jobId":if field == "jobs" {Some(key)} else {None},
                "sessionId":if field == "tasks" {Some(key)} else {None}
            }))
            .unwrap(),
        ),
    }
}

fn read_record(field: &str, key: &str) -> Result<Option<super::super::storage::Record>, Error> {
    let prefix = super::super::storage::record_prefix(field, key);
    let mut bytes = Vec::new();
    for i in 0.. {
        let Some(part) = state::get(&format!("{prefix}{i:08}"))? else {
            break;
        };
        let size = bytes.len() + part.len();
        if size > super::super::storage::MAX_RECORD_BYTES {
            return Err(resource_error(
                field,
                key,
                size,
                super::super::storage::MAX_RECORD_BYTES,
            ));
        }
        bytes.extend(part);
        // Null tombstones may mask fragments left by an earlier checkpoint.
        if let Ok(record) = serde_json::from_slice(&bytes) {
            return Ok(Some(record));
        }
    }
    if bytes.is_empty() {
        Ok(None)
    } else {
        serde_json::from_slice(&bytes).map(Some).map_err(failure)
    }
}

struct Fragments {
    prefix: String,
    next: usize,
    bytes: std::io::Cursor<Vec<u8>>,
}
impl std::io::Read for Fragments {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = std::io::Read::read(&mut self.bytes, buffer)?;
            if n != 0 {
                return Ok(n);
            }
            let part = state::get(&format!("{}{:08}", self.prefix, self.next))
                .map_err(|e| std::io::Error::other(e.message))?;
            self.next += 1;
            let Some(part) = part else { return Ok(0) };
            self.bytes = std::io::Cursor::new(part);
        }
    }
}

fn index_key(field: &str, id: &str) -> String {
    format!(
        "{}00000000",
        super::super::storage::record_prefix(&format!("index_{field}"), id)
    )
}

pub(super) fn migrate() -> Result<(), Error> {
    use pluribus::plugin::runtime;
    for field in INDEX_FIELDS {
        let marker = format!("engine/index-migration/1/{field}");
        let mut after: Option<String> = state::get(&marker)?
            .map(|v| serde_json::from_slice(&v))
            .transpose()
            .map_err(failure)?;
        if after.as_deref() == Some("done") {
            continue;
        }
        let prefix = format!("engine/record/{field}/");
        loop {
            let page = state::scan(&prefix, after.as_deref(), 1)?;
            let Some(first) = page.entries.into_iter().next() else {
                runtime::commit(
                    &[],
                    &[Mutation::Set(StateEntry {
                        key: marker.clone(),
                        value: br#""done""#.to_vec(),
                    })],
                    None,
                )?;
                break;
            };
            let base = first
                .key
                .rsplit_once('/')
                .ok_or_else(|| failure("invalid state fragment"))?
                .0;
            let reader = Fragments {
                prefix: format!("{base}/"),
                next: 0,
                bytes: std::io::Cursor::new(vec![]),
            };
            let record = super::super::metadata::decode(reader).map_err(failure)?;
            after = Some(format!("{base}/~"));
            let index = super::super::storage::Record {
                field: format!("index_{field}"),
                key: record.key.clone(),
                value: record.value,
            };
            let index_bytes = serde_json::to_vec(&index).map_err(failure)?;
            if index_bytes.len() > 32 * 1024 {
                return Err(resource_error(
                    field,
                    &record.key,
                    index_bytes.len(),
                    32 * 1024,
                ));
            }
            runtime::commit(
                &[],
                &[
                    Mutation::Set(StateEntry {
                        key: index_key(field, &record.key),
                        value: index_bytes,
                    }),
                    Mutation::Set(StateEntry {
                        key: marker.clone(),
                        value: serde_json::to_vec(&after).map_err(failure)?,
                    }),
                ],
                None,
            )?;
        }
    }
    Ok(())
}

fn scan_index(
    field: &str,
    mut visit: impl FnMut(&str, &Value) -> Result<(), Error>,
) -> Result<(), Error> {
    let prefix = format!("engine/record/index_{field}/");
    let mut after = None;
    loop {
        let page = state::scan(&prefix, after.as_deref(), 1)?;
        for entry in page.entries {
            let record: super::super::storage::Record =
                serde_json::from_slice(&entry.value).map_err(failure)?;
            if !record.value.is_null() {
                visit(&record.key, &record.value)?;
            }
        }
        match page.next_key {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(())
}

fn index(field: &str, key: &str) -> Result<Value, Error> {
    Ok(read_record(&format!("index_{field}"), key)?.map_or(Value::Null, |r| r.value))
}

fn route_ids(
    value: &Value,
    jobs: &mut BTreeSet<String>,
    sessions: &mut BTreeSet<String>,
    observations: &mut BTreeSet<String>,
) {
    if let Some(id) = value["jobId"].as_str() {
        jobs.insert(id.into());
    }
    if let Some(id) = value["sessionId"].as_str().or(value["rlmSession"].as_str()) {
        sessions.insert(id.into());
    }
    if let Some(id) = value["resolvesObservationId"].as_str() {
        observations.insert(id.into());
    }
    for part in value["message"]["content"].as_array().into_iter().flatten() {
        if part["kind"] == "tool-call" && part["name"] == "associate" {
            route_ids(&part["arguments"], jobs, sessions, observations);
        }
    }
}

pub(super) fn load(batch: &[Event], config: &Config) -> Result<Engine, Error> {
    let mut value = serde_json::to_value(Engine::default()).map_err(failure)?;
    let mut jobs = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut resource_errors = std::collections::BTreeMap::new();
    let mut used = 0;
    for field in ["queue", "sequence", "now_ms"] {
        if let Some(record) = read_record(field, "")? {
            super::super::storage::apply_record(&mut value, record);
        }
    }
    if let Some(queue) = value["queue"].as_array_mut() {
        let mut live = vec![];
        for id in queue.iter() {
            if let Some(id) = id.as_str()
                && !index("tasks", id)?.is_null()
            {
                if live.len() < 4 {
                    sessions.insert(id.into());
                }
                live.push(json!(id));
            }
        }
        *queue = live;
    }
    // There is at most one admitted model call in the cognition scheduler.
    let mut after = None;
    loop {
        let page = state::scan("engine/record/calls/", after.as_deref(), 1)?;
        for entry in page.entries {
            used += entry.value.len();
            if used > 128 * 1024 {
                return Err(resource_error("calls", "", used, 128 * 1024));
            }
            let record: super::super::storage::Record =
                serde_json::from_slice(&entry.value).map_err(failure)?;
            if let Some(id) = record.value.as_str() {
                sessions.insert(id.into());
            }
            super::super::storage::apply_record(&mut value, record);
        }
        match page.next_key {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    for event in batch {
        if event.event_type == "observation.received" {
            observations.insert(event.event_id.clone());
            if let Some(record) = read_record("observations_seen", &event.event_id)? {
                super::super::storage::apply_record(&mut value, record);
            }
        }
        if let Payload::Json(bytes) = &event.payload {
            let body: Value = serde_json::from_slice(bytes).map_err(failure)?;
            route_ids(&body, &mut jobs, &mut sessions, &mut observations);
            if event.event_type == "cognition.resource-exhausted" {
                route_ids(
                    &body["input"]["payload"],
                    &mut jobs,
                    &mut sessions,
                    &mut observations,
                );
                if body["input"]["eventType"] == "observation.received"
                    && let Some(original) = body["input"]["eventId"].as_str()
                {
                    observations.insert(original.into());
                    if let Some(record) = read_record("observations_seen", original)? {
                        super::super::storage::apply_record(&mut value, record);
                    }
                }
            }
            if event.event_type != "cognition.resource-exhausted"
                && let Some(request) = body["requestEventId"].as_str()
                && let Ok(original) = events::get(request)
                && let Payload::Json(bytes) = original.payload
            {
                let request: Value = serde_json::from_slice(&bytes).map_err(failure)?;
                route_ids(&request, &mut jobs, &mut sessions, &mut observations);
            }
            if let Some(key) =
                super::super::engine::result_key(&event.event_type, &event.event_id, &body)
                && let Some(record) = read_record("seen_results", &key)?
            {
                super::super::storage::apply_record(&mut value, record);
            }
        }
    }
    for session in &sessions {
        let task = index("tasks", session)?;
        if let Some(root) = task["root"].as_str() {
            jobs.insert(root.into());
        }
        if let Some(origin) = task["origin"].as_str() {
            observations.insert(origin.into());
        }
        if let Some(origin) = task["association"].as_str() {
            observations.insert(origin.into());
        }
    }
    scan_index("tasks", |id, task| {
        if task["root"]
            .as_str()
            .is_some_and(|root| jobs.contains(root))
        {
            sessions.insert(id.into());
        }
        Ok(())
    })?;
    for job in &jobs {
        if let Some(record) = read_record("budgets", job)? {
            super::super::storage::apply_record(&mut value, record);
        }
    }
    for session in &sessions {
        if let Some(record) = read_record("budgets", session)? {
            super::super::storage::apply_record(&mut value, record);
        }
    }
    let mut recovery_tasks = vec![];
    for (field, ids) in [("jobs", &jobs), ("tasks", &sessions)] {
        for id in ids {
            let record = match read_record(field, id) {
                Ok(record) => record,
                Err(error) if matches!(error.code, ErrorCode::ResourceExhausted) => {
                    let summary = index(field, id)?;
                    if summary.is_null() {
                        return Err(error);
                    }
                    let resource = error
                        .details
                        .as_deref()
                        .and_then(|v| serde_json::from_slice::<Value>(v).ok())
                        .unwrap_or(Value::Null);
                    resource_errors.insert(id.clone(), resource);
                    if field == "tasks" {
                        recovery_tasks.push((id.clone(), summary));
                        None
                    } else {
                        Some(super::super::storage::Record {
                            field: field.into(),
                            key: id.clone(),
                            value: summary,
                        })
                    }
                }
                Err(error) => return Err(error),
            };
            if let Some(record) = record {
                used += serde_json::to_vec(&record).map_err(failure)?.len();
                if used > MAX_VIEW_BYTES {
                    return Err(resource_error(field, id, used, MAX_VIEW_BYTES));
                }
                if field == "jobs" {
                    for source in record.value["sources"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .take(8)
                    {
                        if let Some(source) = source.as_str() {
                            observations.insert(source.into());
                        }
                    }
                }
                super::super::storage::apply_record(&mut value, record);
            }
        }
    }
    for observation in observations {
        if value["inbox"].get(&observation).is_some() {
            continue;
        }
        let record = match read_record("inbox", &observation) {
            Ok(record) => record,
            Err(error) if matches!(error.code, ErrorCode::ResourceExhausted) => {
                let mut summary = index("inbox", &observation)?;
                if summary.is_null() {
                    return Err(error);
                }
                summary["value"]["source"] = json!({"eventId":observation,"payloadOmitted":true});
                Some(super::super::storage::Record {
                    field: "inbox".into(),
                    key: observation.clone(),
                    value: summary,
                })
            }
            Err(error) => return Err(error),
        };
        if let Some(record) = record {
            used += serde_json::to_vec(&record).map_err(failure)?.len();
            if used > MAX_VIEW_BYTES {
                return Err(resource_error("inbox", &observation, used, MAX_VIEW_BYTES));
            }
            super::super::storage::apply_record(&mut value, record);
        }
    }
    let mut engine: Engine = serde_json::from_value(value).map_err(failure)?;
    engine.set_partial();
    for (id, summary) in recovery_tasks {
        engine.restore_resource_task(config, &id, &summary);
    }
    for (id, resource) in resource_errors {
        engine.queue_resource_error(&id, resource);
    }
    let mut origins: Vec<Value> = batch
        .iter()
        .filter(|e| e.event_type == "observation.received")
        .filter_map(|e| match &e.payload {
            Payload::Json(bytes) => serde_json::from_slice(bytes).ok(),
            _ => None,
        })
        .collect();
    origins.extend(engine.inbox.values().map(|o| o.value.clone()));
    let mut candidates: Vec<(super::super::jobs::Job, Value)> = vec![];
    let mut recent_jobs: Vec<(super::super::jobs::Job, Value)> = vec![];
    scan_index("jobs", |_, summary| {
        let job: super::super::jobs::Job =
            serde_json::from_value(summary.clone()).map_err(failure)?;
        let task = index("tasks", &job.id)?;
        let origin = if task["context"]["observation"].is_null() {
            index("inbox", &job.origin)?["value"].clone()
        } else {
            task["context"]["observation"].clone()
        };
        if origins
            .iter()
            .any(|value| super::super::engine::same_conversation(&origin, value))
        {
            let selected = if ["completed", "failed", "cancelled"].contains(&job.status.as_str()) {
                &mut recent_jobs
            } else {
                &mut candidates
            };
            selected.push((job, origin));
            selected.sort_by_key(|(j, _)| std::cmp::Reverse(j.incorporated_sequence));
            selected.truncate(16);
        }
        Ok(())
    })?;
    let mut clarifications: Vec<super::super::jobs::Observation> = vec![];
    let mut recent: Vec<super::super::jobs::Observation> = vec![];
    // Same-conversation observations left outside the working set, so the turn envelope can
    // report how many exist rather than how many were loaded.
    let mut unloaded = std::collections::BTreeMap::new();
    scan_index("inbox", |id, summary| {
        if origins
            .iter()
            .any(|value| super::super::engine::same_conversation(&summary["value"], value))
        {
            if !engine.inbox.contains_key(id) {
                *unloaded
                    .entry(super::super::engine::conversation_key(&summary["value"]))
                    .or_insert(0usize) += 1;
            }
            let selected = if summary["status"] == "waiting-input" && summary["job"].is_null() {
                &mut clarifications
            } else {
                &mut recent
            };
            let observation = serde_json::from_value(summary.clone()).map_err(failure)?;
            selected.push(observation);
            selected.sort_by_key(|o| std::cmp::Reverse(o.sequence));
            selected.truncate(6);
        }
        Ok(())
    })?;
    for observation in clarifications.into_iter().chain(recent) {
        if !engine.inbox.contains_key(&observation.id)
            && let Some(count) =
                unloaded.get_mut(&super::super::engine::conversation_key(&observation.value))
        {
            *count -= 1;
        }
        // Routing summaries are read-only; do not replace durable observations.
        engine.add_routing_observation(observation);
    }
    engine.set_unloaded_observations(unloaded);
    for (job, origin) in candidates.into_iter().chain(recent_jobs) {
        engine.add_routing_job(job, origin);
    }
    Ok(engine)
}

pub(super) fn index_changes(changes: &[Value]) -> Result<Vec<Value>, Error> {
    let mut prefixes = BTreeSet::new();
    for change in changes {
        let key = change["key"].as_str().unwrap_or("");
        for field in INDEX_FIELDS {
            if key.starts_with(&format!("engine/record/{field}/")) {
                prefixes.insert((field, key.rsplit_once('/').unwrap().0.to_owned()));
            }
        }
    }
    let mut out = vec![];
    for (field, prefix) in prefixes {
        // Unchanged fragments are read from the committed projection.
        let mut assembled = vec![];
        for i in 0.. {
            let key = format!("{prefix}/{i:08}");
            let part = if let Some(change) = changes.iter().find(|c| c["key"] == key) {
                change["value"]
                    .as_str()
                    .map(|s| STANDARD.decode(s).map_err(failure))
                    .transpose()?
            } else {
                state::get(&key)?
            };
            let Some(part) = part else { break };
            assembled.extend(part);
            if let Ok(record) = super::super::metadata::decode(std::io::Cursor::new(&assembled)) {
                let index = super::super::storage::Record {
                    field: format!("index_{field}"),
                    key: record.key.clone(),
                    value: record.value,
                };
                out.push(json!({"key":index_key(field,&record.key),"value":STANDARD.encode(serde_json::to_vec(&index).map_err(failure)?)}));
                break;
            }
        }
        if assembled.is_empty() {
            let index_prefix = prefix.replacen(
                &format!("engine/record/{field}/"),
                &format!("engine/record/index_{field}/"),
                1,
            );
            out.push(json!({"key":format!("{index_prefix}/00000000"),"value":null}));
        }
    }
    Ok(out)
}
