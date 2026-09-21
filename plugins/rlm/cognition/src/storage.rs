use crate::engine::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CHUNK_BYTES: usize = 128 * 1024;
/// Maximum serialized size of one durable record.  This is checked before a
/// record is split into mutation fragments.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;
/// Maximum bytes admitted from one state page while assembling records.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundError {
    RecordTooLarge {
        field: String,
        key: String,
        bytes: usize,
        limit: usize,
    },
}

impl std::fmt::Display for BoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecordTooLarge {
                field,
                key,
                bytes,
                limit,
            } => {
                write!(f, "record {field}/{key} is {bytes} bytes; limit is {limit}")
            }
        }
    }
}

pub fn resource_details(error: &BoundError) -> Value {
    let BoundError::RecordTooLarge {
        field,
        key,
        bytes,
        limit,
    } = error;
    serde_json::json!({"resource":"cognition-record","state":"storage","field":field,"key":key,"currentBytes":0,"requestedBytes":bytes,"limitBytes":limit,"phase":"checkpoint","jobId":if field=="jobs" {Some(key)}else{None},"sessionId":if field=="tasks" {Some(key)}else{None}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_receipts_do_not_expand_one_delivery_mutations() {
        let mut value = serde_json::to_value(Engine::default()).unwrap();
        value["seen_results"] = serde_json::json!(
            (0..20000)
                .map(|i| format!("capability:{i:020}{}", "x".repeat(1800)))
                .collect::<Vec<_>>()
        );
        let engine: Engine = serde_json::from_value(value).unwrap();
        let before = records(&engine).unwrap();
        let mut value = serde_json::to_value(&engine).unwrap();
        value["seen_results"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("capability:new"));
        let after = records(&serde_json::from_value(value).unwrap()).unwrap();
        assert_eq!(
            after
                .iter()
                .filter(|(key, value)| before.get(*key) != Some(*value))
                .count(),
            1
        );
        assert!(after.keys().all(|key| key.len() <= 512));
    }
    #[test]
    fn growing_state_uses_bounded_values_and_preserves_receipts() {
        let mut value = serde_json::to_value(Engine::default()).unwrap();
        value["seen_results"] = serde_json::json!(
            (0..30000)
                .map(|i| format!("capability:request-{i:060}"))
                .collect::<Vec<_>>()
        );
        let engine: Engine = serde_json::from_value(value).unwrap();
        let fragments = records(&engine).unwrap();
        assert!(fragments.values().all(|part| part.len() <= CHUNK_BYTES));
        let mut groups: std::collections::BTreeMap<String, Vec<u8>> =
            std::collections::BTreeMap::new();
        for (key, part) in fragments {
            groups
                .entry(key.rsplit_once('/').unwrap().0.to_owned())
                .or_default()
                .extend(part);
        }
        let mut value = serde_json::to_value(Engine::default()).unwrap();
        for bytes in groups.into_values() {
            apply_record(&mut value, serde_json::from_slice(&bytes).unwrap());
        }
        let restored: Engine = serde_json::from_value(value).unwrap();
        assert!(restored.result_seen(
            "capability.completed",
            "receipt",
            &serde_json::json!({"requestEventId": format!("request-{:060}", 29999)})
        ));
    }

    #[test]
    fn oversized_record_is_rejected_before_fragmentation() {
        let mut records = std::collections::BTreeMap::new();
        let error = insert_record_bounded(
            &mut records,
            "jobs",
            "large",
            &serde_json::json!({"payload":"x".repeat(MAX_RECORD_BYTES)}),
        )
        .unwrap_err();
        assert!(matches!(error, BoundError::RecordTooLarge { .. }));
        assert!(records.is_empty());
        let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
        let _ = serde_json::to_writer(
            &mut writer,
            &BorrowedRecord {
                field: "jobs",
                key: "large",
                value: &serde_json::json!({"payload":"x".repeat(MAX_RECORD_BYTES)}),
            },
        );
        assert!(writer.bytes.len() <= MAX_RECORD_BYTES);
        let error = insert_record(
            &mut records,
            "jobs",
            "large",
            &serde_json::json!({"payload":"x".repeat(MAX_RECORD_BYTES)}),
        )
        .unwrap_err();
        let details = resource_details(&error);
        assert_eq!(details["field"], "jobs");
        assert_eq!(details["key"], "large");
    }

    /*
    #[test]
    fn assembling_one_record_does_not_admit_an_oversized_page() {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "engine/record/jobs/a/00000000".into(),
            vec![b'x'; MAX_PAGE_BYTES + 1],
        );
        let error = assemble_records(entries).unwrap_err();
        assert!(matches!(error, BoundError::PageTooLarge { .. }));
    }

    #[test]
    fn scheduler_entry_excludes_checkpoint_and_activity_payloads() {
        let mut engine = Engine::default();
        let job = Job {
            activities: [("large".into(), serde_json::json!({"x":"y".repeat(100_000)}))]
                .into_iter()
                .collect(),
            id: "job".into(),
            objective: "o".into(),
            completion_conditions: serde_json::Value::Null,
            origin: "observation".into(),
            sources: vec!["observation".into()],
            revision: 4,
            incorporated_sequence: 2,
            status: "running".into(),
            wait_reason: None,
            outstanding_question: None,
            recent_reply: None,
            completed_steps: serde_json::Value::Null,
            next_step: serde_json::Value::Null,
            blockers: serde_json::Value::Null,
            notes: serde_json::Value::Null,
            checkpoint: serde_json::json!({"x":"z".repeat(100_000)}),
            checkpoint_version: 1,
            cycle_started_ms: 0,
            retry_count: 0,
            wake: None,
        };
        engine.jobs.insert("job".into(), job);
        let entry = scheduler_entries(&engine).next().unwrap();
        assert!(serde_json::to_vec(&entry).unwrap().len() < MAX_INDEX_ENTRY_BYTES);
        assert_eq!(entry.source.as_deref(), Some("observation"));
    }

    #[test]
    fn legacy_index_decoder_discards_large_job_fields() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "id":"job", "origin":"obs", "status":"running", "revision":3,
            "sources":["obs"], "activities":{"large":"x".repeat(200_000)},
            "checkpoint":{"large":"x".repeat(200_000)}
        }))
        .unwrap();
        let entry = legacy_job_index(&bytes).unwrap();
        assert_eq!(entry.id, "job");
        assert_eq!(entry.source.as_deref(), Some("obs"));
    }

    #[test]
    fn record_assembler_emits_one_record_and_bounds_the_next() {
        let mut stream = RecordAssembler::default();
        assert!(
            stream
                .push("engine/record/jobs/a/00000000", b"ab")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            stream.push("engine/record/jobs/a/00000001", b"cd").unwrap(),
            None
        );
        assert_eq!(
            stream.push("engine/record/jobs/b/00000000", b"x").unwrap(),
            Some(b"abcd".to_vec())
        );
        assert_eq!(stream.finish(), Some(b"x".to_vec()));
    }
    */
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub field: String,
    pub key: String,
    pub value: Value,
}

pub fn record_prefix(field: &str, key: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut prefix = format!("engine/record/{field}/");
    for byte in Sha256::digest(key.as_bytes()) {
        let _ = write!(&mut prefix, "{byte:02x}");
    }
    prefix.push('/');
    prefix
}

pub fn records(engine: &Engine) -> Result<std::collections::BTreeMap<String, Vec<u8>>, BoundError> {
    engine.records()
}

pub fn insert_record(
    records: &mut std::collections::BTreeMap<String, Vec<u8>>,
    field: &str,
    key: &str,
    value: &impl Serialize,
) -> Result<(), BoundError> {
    insert_record_bounded(records, field, key, value)
}

pub fn insert_record_bounded(
    records: &mut std::collections::BTreeMap<String, Vec<u8>>,
    field: &str,
    key: &str,
    value: &impl Serialize,
) -> Result<(), BoundError> {
    let prefix = record_prefix(field, key);
    let mut writer = CappedWriter::new(MAX_RECORD_BYTES);
    let record = BorrowedRecord { field, key, value };
    if serde_json::to_writer(&mut writer, &record).is_err() || writer.exceeded {
        return Err(BoundError::RecordTooLarge {
            field: field.into(),
            key: key.into(),
            bytes: writer.bytes.len().saturating_add(1),
            limit: MAX_RECORD_BYTES,
        });
    }
    for (index, part) in writer.bytes.chunks(CHUNK_BYTES).enumerate() {
        records.insert(format!("{prefix}{index:08}"), part.to_vec());
    }
    Ok(())
}

#[derive(Serialize)]
struct BorrowedRecord<'a, T: ?Sized> {
    field: &'a str,
    key: &'a str,
    value: &'a T,
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit),
            limit,
            exceeded: false,
        }
    }
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        let take = remaining.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..take]);
        if take != bytes.len() {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "record limit exceeded",
            ));
        }
        Ok(take)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn apply_record(engine: &mut Value, record: Record) {
    if record.value.is_null() {
        if let Some(values) = engine[&record.field].as_object_mut() {
            values.remove(&record.key);
        }
        return;
    }
    match record.field.as_str() {
        "seen_results" => engine["seen_results"]
            .as_array_mut()
            .unwrap()
            .push(Value::String(record.key)),
        "observations_seen" => engine["inbox"][record.key] = record.value,
        _ if record.key.is_empty() && !engine[&record.field].is_object() => {
            engine[record.field] = record.value
        }
        _ => engine[record.field][record.key] = record.value,
    }
}
