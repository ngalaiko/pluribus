//! Streaming decoders for the legacy record migration.
use crate::storage::Record;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::fmt;

const MAX_TEXT: usize = 1024;
const MAX_SOURCES: usize = 8;

#[derive(Clone, Debug, Default)]
struct BoundedSources(Vec<String>);
impl<'de> Deserialize<'de> for BoundedSources {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SourcesVisitor;
        impl<'de> Visitor<'de> for SourcesVisitor {
            type Value = BoundedSources;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array of source IDs")
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(MAX_SOURCES);
                while let Some(value) = seq.next_element::<String>()? {
                    if values.len() == MAX_SOURCES {
                        values.remove(1);
                    }
                    values.push(value);
                }
                Ok(BoundedSources(values))
            }
        }
        deserializer.deserialize_seq(SourcesVisitor)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct JobMeta {
    #[serde(default)]
    id: String,
    #[serde(default)]
    objective: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    sources: BoundedSources,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    incorporated_sequence: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    wait_reason: Option<String>,
    #[serde(default)]
    outstanding_question: Option<String>,
    #[serde(default)]
    recent_reply: Option<String>,
    #[serde(default)]
    cycle_started_ms: i64,
    #[serde(default)]
    retry_count: u32,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(untagged)]
enum YieldId {
    Text(String),
    Number(u64),
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TaskMeta {
    #[serde(default)]
    root: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    parent: Option<(String, YieldId)>,
    #[serde(default)]
    association: Option<String>,
    #[serde(default)]
    context: TaskContextMeta,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    cell_revision: u64,
    #[serde(default)]
    resource_failures: u8,
    #[serde(default)]
    depth: u32,
    #[serde(default)]
    window: Option<crate::engine::Window>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ObservationMeta {
    #[serde(default)]
    id: String,
    #[serde(default)]
    sequence: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    outstanding_question: Option<String>,
    #[serde(default)]
    value: ObservationValueMeta,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TaskContextMeta {
    #[serde(default)]
    question: String,
    #[serde(default)]
    observation: IdentityMeta,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ObservationValueMeta {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    #[serde(rename = "externalSenderId")]
    external_sender_id: String,
    #[serde(default)]
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(default)]
    message: MessageMeta,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct IdentityMeta {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    #[serde(rename = "externalSenderId")]
    external_sender_id: String,
    #[serde(default)]
    #[serde(rename = "conversationId")]
    conversation_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct MessageMeta {
    #[serde(default)]
    text: String,
    #[serde(default)]
    caption: String,
}

fn bounded(text: String) -> String {
    text.chars().take(MAX_TEXT).collect()
}

struct BoundedJsonReader<R> {
    inner: R,
    string: bool,
    copied: usize,
    pending: std::collections::VecDeque<u8>,
}

impl<R: std::io::Read> BoundedJsonReader<R> {
    fn next(&mut self) -> std::io::Result<Option<u8>> {
        let mut byte = [0];
        Ok((self.inner.read(&mut byte)? != 0).then_some(byte[0]))
    }
    fn required(&mut self) -> std::io::Result<u8> {
        self.next()?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete JSON token")
        })
    }
    fn output(&mut self) -> std::io::Result<Option<u8>> {
        loop {
            if let Some(byte) = self.pending.pop_front() {
                return Ok(Some(byte));
            }
            let Some(byte) = self.next()? else {
                return Ok(None);
            };
            if !self.string {
                if byte == b'"' {
                    self.string = true;
                    self.copied = 0;
                }
                return Ok(Some(byte));
            }
            if byte == b'"' {
                self.string = false;
                return Ok(Some(byte));
            }
            // Keep each escape or UTF-8 scalar whole, including surrogate pairs.
            let mut token = [0; 12];
            token[0] = byte;
            let mut len = 1;
            if byte == b'\\' {
                let escape = self.required()?;
                token[len] = escape;
                len += 1;
                if escape == b'u' {
                    for _ in 0..4 {
                        token[len] = self.required()?;
                        len += 1;
                    }
                    let unit = std::str::from_utf8(&token[2..6])
                        .ok()
                        .and_then(|s| u16::from_str_radix(s, 16).ok());
                    if unit.is_some_and(|unit| (0xd800..=0xdbff).contains(&unit)) {
                        for _ in 0..6 {
                            token[len] = self.required()?;
                            len += 1;
                        }
                    }
                }
            } else {
                let remaining = match byte {
                    0xc0..=0xdf => 1,
                    0xe0..=0xef => 2,
                    0xf0..=0xf7 => 3,
                    _ => 0,
                };
                for _ in 0..remaining {
                    token[len] = self.required()?;
                    len += 1;
                }
            }
            // Six encoded bytes per character also bounds escaped identifiers.
            if self.copied < MAX_TEXT * 6 {
                self.copied += len;
                self.pending.extend(&token[..len]);
            }
        }
    }
}
impl<R: std::io::Read> std::io::Read for BoundedJsonReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut count = 0;
        for byte in out {
            let Some(value) = self.output()? else { break };
            *byte = value;
            count += 1;
        }
        Ok(count)
    }
}

fn compact_job(mut job: JobMeta) -> Value {
    job.objective = bounded(job.objective);
    job.origin = bounded(job.origin);
    job.wait_reason = job.wait_reason.map(bounded);
    job.outstanding_question = job.outstanding_question.map(bounded);
    job.recent_reply = job.recent_reply.map(bounded);
    job.sources
        .0
        .iter_mut()
        .for_each(|s| *s = bounded(std::mem::take(s)));
    serde_json::json!({
        "id":job.id,"objective":job.objective,"origin":job.origin,"sources":job.sources.0,
        "revision":job.revision,"incorporated_sequence":job.incorporated_sequence,"status":job.status,
        "wait_reason":job.wait_reason,"outstanding_question":job.outstanding_question,
        "recent_reply":job.recent_reply,"cycle_started_ms":job.cycle_started_ms,"retry_count":job.retry_count,
        "completion_conditions":null,"completed_steps":null,"next_step":null,"blockers":null,
        "notes":null,"checkpoint":null,"checkpoint_version":0,"wake":null
    })
}

fn compact_task(mut task: TaskMeta) -> Value {
    task.root = bounded(task.root);
    task.origin = bounded(task.origin);
    task.association = task.association.map(bounded);
    // Context can contain arbitrary model data; retain only the conversation
    // identity needed by routing and migration.
    let context = serde_json::json!({"observation": {
        "provider":bounded(task.context.observation.provider),
        "externalSenderId":bounded(task.context.observation.external_sender_id),
        "conversationId":bounded(task.context.observation.conversation_id)
    },"question":bounded(task.context.question)});
    let parent = task
        .parent
        .as_ref()
        .map(|(id, value)| serde_json::json!([id, value]));
    serde_json::json!({"root":task.root,"origin":task.origin,"parent":parent,"association":task.association,"context":context,"revision":task.revision,"cell_revision":task.cell_revision,"resource_failures":task.resource_failures,"depth":task.depth,"window":task.window})
}

fn compact_observation(mut observation: ObservationMeta) -> Value {
    observation.id = bounded(observation.id);
    observation.status = bounded(observation.status);
    observation.job = observation.job.map(bounded);
    observation.outstanding_question = observation.outstanding_question.map(bounded);
    let value = observation.value;
    let out = serde_json::json!({"provider":bounded(value.provider),"externalSenderId":bounded(value.external_sender_id),"conversationId":bounded(value.conversation_id),"message":{"text":bounded(value.message.text),"caption":bounded(value.message.caption)}});
    serde_json::json!({"id":observation.id,"sequence":observation.sequence,"status":observation.status,"job":observation.job,"outstanding_question":observation.outstanding_question,"value":out})
}

struct ValueSeed<'a> {
    field: &'a str,
}
impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;
    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        match self.field {
            "jobs" => Option::<JobMeta>::deserialize(deserializer)
                .map(|job| job.map_or(Value::Null, compact_job)),
            "tasks" => Option::<TaskMeta>::deserialize(deserializer)
                .map(|task| task.map_or(Value::Null, compact_task)),
            "inbox" | "observations_seen" => Option::<ObservationMeta>::deserialize(deserializer)
                .map(|observation| observation.map_or(Value::Null, compact_observation)),
            _ => IgnoredAny::deserialize(deserializer).map(|_| Value::Null),
        }
    }
}

struct RecordVisitor;
impl<'de> Visitor<'de> for RecordVisitor {
    type Value = Record;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a legacy record")
    }
    fn visit_map<A>(self, mut map: A) -> Result<Record, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut field: Option<String> = None;
        let mut key: Option<String> = None;
        let mut value = Value::Null;
        while let Some(name) = map.next_key::<String>()? {
            match name.as_str() {
                "field" => field = Some(map.next_value()?),
                "key" => key = Some(map.next_value()?),
                "value" => {
                    let selected = field.as_deref().unwrap_or("");
                    value = map.next_value_seed(ValueSeed { field: selected })?;
                }
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(Record {
            field: field.ok_or_else(|| de::Error::missing_field("field"))?,
            key: key.ok_or_else(|| de::Error::missing_field("key"))?,
            value,
        })
    }
}

pub fn decode<R: std::io::Read>(reader: R) -> Result<Record, serde_json::Error> {
    let reader = BoundedJsonReader {
        inner: reader,
        string: false,
        copied: 0,
        pending: std::collections::VecDeque::new(),
    };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let record = deserializer.deserialize_map(RecordVisitor)?;
    let mut identities = vec![record.key.as_str()];
    for key in ["id", "root", "origin", "job", "association"] {
        if let Some(id) = record.value[key].as_str() {
            identities.push(id);
        }
    }
    for object in [
        &record.value["value"],
        &record.value["context"]["observation"],
    ] {
        for key in ["provider", "externalSenderId", "conversationId"] {
            if let Some(id) = object[key].as_str() {
                identities.push(id);
            }
        }
    }
    if identities.iter().any(|id| id.chars().count() >= MAX_TEXT) {
        return Err(serde_json::Error::io(std::io::Error::other(
            "metadata identity exceeds its bound",
        )));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skips_large_job_fields() {
        let input = serde_json::json!({"field":"jobs","key":"a","value":{"id":"a","origin":"o","status":"running","objective":"x".repeat(1_200_000),"activities":["x".repeat(400_000)],"checkpoint":"x".repeat(400_000)}}).to_string();
        let record = decode(input.as_bytes()).unwrap();
        assert_eq!(record.value["id"], "a");
        assert!(record.value.get("activities").is_none());
        assert!(serde_json::to_vec(&record.value).unwrap().len() < 8 * 1024);
        assert_eq!(
            record.value["objective"].as_str().unwrap().chars().count(),
            MAX_TEXT
        );
        let _: crate::jobs::Job = serde_json::from_value(record.value).unwrap();
    }

    #[test]
    fn all_metadata_records_are_small_and_keep_identity() {
        let task = serde_json::json!({"field":"tasks","key":"t","value":{"root":"r","origin":"o","revision":4,"cell_revision":5,"resource_failures":2,"context":{"observation":{"provider":"telegram","externalSenderId":"u","conversationId":"c"},"checkpoint":{"x":"x".repeat(1_200_000)}}}}).to_string();
        let observation = serde_json::json!({"field":"inbox","key":"o","value":{"id":"o","sequence":2,"status":"new","value":{"provider":"telegram","externalSenderId":"u","conversationId":"c","message":{"text":"hello"},"large":["x".repeat(1_200_000)]}}}).to_string();
        let task = decode(task.as_bytes()).unwrap();
        let observation = decode(observation.as_bytes()).unwrap();
        assert!(serde_json::to_vec(&task.value).unwrap().len() < 8 * 1024);
        assert!(serde_json::to_vec(&observation.value).unwrap().len() < 8 * 1024);
        assert_eq!(task.value["context"]["observation"]["conversationId"], "c");
        assert_eq!(observation.value["value"]["conversationId"], "c");
        assert!(task.value["resource_failures"] == 2);
    }

    #[test]
    fn string_reader_keeps_json_valid_at_unicode_boundary() {
        let input = serde_json::json!({"field":"jobs","key":"a","value":{"id":"a","objective":"😀".repeat(2000)}}).to_string();
        let record = decode(input.as_bytes()).unwrap();
        assert!(record.value["objective"].as_str().unwrap().chars().count() <= MAX_TEXT);
    }

    #[test]
    fn string_reader_drops_partial_escape_atomically() {
        let objective = format!("{}\\u1234\\\"tail", "a".repeat(MAX_TEXT - 1));
        let input = format!(
            "{{\"field\":\"jobs\",\"key\":\"a\",\"value\":{{\"id\":\"a\",\"objective\":{}}}}}",
            serde_json::to_string(&objective).unwrap()
        );
        let record = decode(input.as_bytes()).unwrap();
        assert!(record.value["objective"].is_string());
    }
    #[test]
    fn encoded_unicode_escapes_remain_complete() {
        for prefix in (1017..1025).chain(MAX_TEXT * 6 - 12..MAX_TEXT * 6 + 1) {
            for escaped in [r"\u1234", r"\ud83d\ude00", r#"\""#] {
                let input = format!(
                    r#"{{"field":"jobs","key":"a","value":{{"id":"a","objective":"{}{escaped}tail"}}}}"#,
                    "a".repeat(prefix)
                );
                assert!(
                    decode(input.as_bytes()).is_ok(),
                    "prefix {prefix}: {escaped}"
                );
            }
        }
    }
}
