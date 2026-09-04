use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub type Result<T> = std::result::Result<T, String>;
pub const KINDS: &[&str] = &["fact", "preference", "belief", "goal", "procedure"];

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub max_records: usize,
    pub max_content_bytes: usize,
    pub max_sources: usize,
    pub max_result_bytes: usize,
    pub max_results: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_records: 10_000,
            max_content_bytes: 4096,
            max_sources: 16,
            max_result_bytes: 16_384,
            max_results: 20,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.max_records == 0
            || self.max_records > 10_000
            || self.max_content_bytes == 0
            || self.max_content_bytes > 4096
            || self.max_sources == 0
            || self.max_sources > 16
            || self.max_result_bytes < 256
            || self.max_result_bytes > 16_384
            || self.max_results == 0
            || self.max_results > 20
        {
            return Err("invalid-argument".into());
        }
        Ok(())
    }
}

pub trait Store {
    fn get(&self, key: &str) -> Result<Option<Value>>;
    fn scan(&self, prefix: &str, after: Option<&str>, limit: usize)
    -> Result<Vec<(String, Value)>>;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub scope: String,
    pub sources: Vec<String>,
    pub basis: String,
    pub created_at_ms: i64,
    pub supersedes: Option<String>,
    pub expires_at_ms: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredRecord {
    pub record: Record,
    pub root: String,
    pub terms: BTreeSet<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub operation_id: String,
    pub digest: String,
    pub receipt: Value,
    pub record: Option<StoredRecord>,
    pub forgotten_root: Option<String>,
}

pub struct Engine<'a, S> {
    pub store: &'a S,
    pub changes: BTreeMap<String, Value>,
    pub config: &'a Config,
    pub instance: &'a str,
}
impl<'a, S: Store> Engine<'a, S> {
    pub fn new(store: &'a S, config: &'a Config, instance: &'a str) -> Self {
        Self {
            store,
            config,
            instance,
            changes: BTreeMap::new(),
        }
    }
    fn get(&self, key: &str) -> Result<Option<Value>> {
        if let Some(value) = self.changes.get(key) {
            return Ok(Some(value.clone()));
        }
        self.store.get(key)
    }
    fn record(&self, id: &str) -> Result<Option<StoredRecord>> {
        self.get(&key("record", id))?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| "storage-error".into())
    }
    fn current(&self, stored: &StoredRecord) -> Result<bool> {
        Ok(self.get(&key("head", &stored.root))? == Some(json!(stored.record.id)))
    }
    fn active(&self, stored: &StoredRecord, scope: &str, now: i64) -> Result<bool> {
        Ok(stored.record.scope == scope
            && self.current(stored)?
            && stored
                .record
                .expires_at_ms
                .is_none_or(|expiry| expiry > now))
    }
    fn bounded(&self, value: Value, bytes: usize) -> Result<Value> {
        if serde_json::to_vec(&value)
            .map_err(|_| "storage-error")?
            .len()
            > bytes
        {
            return Err("result-too-large".into());
        }
        Ok(value)
    }
    pub fn execute(
        &mut self,
        operation: &str,
        args: &Value,
        now: i64,
        sequence: u64,
        mut source_readable: impl FnMut(&str) -> bool,
    ) -> Result<(Value, Option<Change>)> {
        let scope = string(args, "scope", 256)?;
        match operation {
            "recall" => self.recall(args, scope, now, sequence).map(|v| (v, None)),
            "get" => self.read_ids(args, scope, now).map(|v| (v, None)),
            "remember" | "supersede" | "forget" => {
                let fields = if operation == "forget" {
                    vec!["scope", "operationId", "expectedId"]
                } else {
                    let mut fields = vec![
                        "scope",
                        "operationId",
                        "kind",
                        "content",
                        "sources",
                        "basis",
                        "expiresAtMs",
                    ];
                    if operation == "supersede" {
                        fields.push("expectedId");
                    }
                    fields
                };
                only(args, &fields)?;
                let operation_id = string(args, "operationId", 128)?;
                let digest = hash(&json!([operation, args]));
                if let Some(receipt) = self.get(&key("operation", operation_id))? {
                    if receipt["digest"] != digest {
                        return Err("operation-conflict".into());
                    }
                    return Ok((receipt["receipt"].clone(), None));
                }
                let old = if operation == "remember" {
                    None
                } else {
                    let id = string(args, "expectedId", 512)?;
                    let old = self.record(id)?.ok_or("conflict")?;
                    if old.record.scope != scope || !self.current(&old)? {
                        return Err("conflict".into());
                    }
                    Some(old)
                };
                let (receipt, record, forgotten_root) = if operation == "forget" {
                    let old = old.as_ref().unwrap();
                    let mut ids = vec![];
                    let mut next = Some(old.record.id.clone());
                    while let Some(id) = next {
                        let record = self.record(&id)?.ok_or("storage-error")?;
                        next = record.record.supersedes;
                        ids.push(id);
                        if ids.len() > self.config.max_records {
                            return Err("storage-error".into());
                        }
                    }
                    (json!({"forgottenIds":ids}), None, Some(old.root.clone()))
                } else {
                    let count = self.get("count")?.and_then(|v| v.as_u64()).unwrap_or(0);
                    if count >= self.config.max_records as u64 {
                        return Err("capacity-exceeded".into());
                    }
                    let kind = string(args, "kind", 32)?;
                    if !KINDS.contains(&kind) {
                        return Err("invalid-argument".into());
                    }
                    let content = string(args, "content", self.config.max_content_bytes)?;
                    let basis = string(args, "basis", 16)?;
                    if !["explicit", "inferred"].contains(&basis) {
                        return Err("invalid-argument".into());
                    }
                    let sources = strings(args, "sources", self.config.max_sources, 256)?;
                    let expires = match args.get("expiresAtMs") {
                        None | Some(Value::Null) => None,
                        Some(v) => Some(v.as_i64().filter(|t| *t > now).ok_or("invalid-argument")?),
                    };
                    if sources.iter().any(|id| !source_readable(id)) {
                        return Err("source-unavailable".into());
                    }
                    let id = format!("memory:{}", hash(&json!([self.instance, operation_id])));
                    let predecessor = old.as_ref().map(|s| s.record.id.clone());
                    // Every accepted chain must fit a later forget receipt.
                    let mut ids = vec![id.clone()];
                    let mut next = predecessor.clone();
                    while let Some(previous) = next {
                        next = self
                            .record(&previous)?
                            .ok_or("storage-error")?
                            .record
                            .supersedes;
                        ids.push(previous);
                        if ids.len() > self.config.max_records {
                            return Err("storage-error".into());
                        }
                    }
                    self.bounded(json!({"forgottenIds":ids}), self.config.max_result_bytes)?;
                    let receipt = if let Some(previous) = &predecessor {
                        json!({"id":id,"supersedes":previous})
                    } else {
                        json!({"id":id})
                    };
                    let stored = StoredRecord {
                        root: old.as_ref().map_or_else(|| id.clone(), |s| s.root.clone()),
                        terms: terms(content),
                        record: Record {
                            id,
                            kind: kind.into(),
                            content: content.into(),
                            scope: scope.into(),
                            sources,
                            basis: basis.into(),
                            created_at_ms: now,
                            supersedes: predecessor,
                            expires_at_ms: expires,
                        },
                    };
                    (receipt, Some(stored), None)
                };
                let receipt = self.bounded(receipt, self.config.max_result_bytes)?;
                let change = Change {
                    operation_id: operation_id.into(),
                    digest,
                    receipt: receipt.clone(),
                    record,
                    forgotten_root,
                };
                self.apply(&change)?;
                Ok((receipt, Some(change)))
            }
            _ => Err("invalid-argument".into()),
        }
    }
    pub fn apply(&mut self, change: &Change) -> Result<()> {
        let op_key = key("operation", &change.operation_id);
        if let Some(existing) = self.get(&op_key)? {
            if existing["digest"] == change.digest && existing["receipt"] == change.receipt {
                return Ok(());
            }
            return Err("operation-conflict".into());
        }
        if let Some(stored) = &change.record {
            let count = self.get("count")?.and_then(|v| v.as_u64()).unwrap_or(0);
            self.changes.insert("count".into(), json!(count + 1));
            self.changes.insert(
                key("record", &stored.record.id),
                serde_json::to_value(stored).unwrap(),
            );
            self.changes
                .insert(key("head", &stored.root), json!(stored.record.id));
        }
        if let Some(root) = &change.forgotten_root {
            self.changes.insert(key("head", root), Value::Null);
        }
        self.changes.insert(
            op_key,
            json!({"digest":change.digest,"receipt":change.receipt}),
        );
        Ok(())
    }
    fn read_ids(&self, args: &Value, scope: &str, now: i64) -> Result<Value> {
        only(args, &["ids", "scope"])?;
        let ids = strings(args, "ids", self.config.max_results, 512)?;
        let mut records = vec![];
        let mut unavailable = vec![];
        for id in ids {
            if let Some(stored) = self.record(&id)?
                && self.active(&stored, scope, now)?
            {
                records.push(stored.record);
            } else {
                unavailable.push(id);
            }
        }
        self.bounded(
            json!({"records":records,"unavailableIds":unavailable}),
            self.config.max_result_bytes,
        )
    }
    fn recall(&self, args: &Value, scope: &str, now: i64, sequence: u64) -> Result<Value> {
        only(args, &["query", "scope", "kinds", "limit", "maxBytes"])?;
        let query = terms(string(args, "query", 512)?);
        let limit = number(
            args,
            "limit",
            8.min(self.config.max_results),
            self.config.max_results,
        )?;
        let bytes = number(
            args,
            "maxBytes",
            6000.min(self.config.max_result_bytes),
            self.config.max_result_bytes,
        )?;
        let kinds = if args.get("kinds").is_some() {
            strings(args, "kinds", KINDS.len(), 32)?
        } else {
            vec![]
        };
        if kinds.iter().any(|k| !KINDS.contains(&k.as_str())) {
            return Err("invalid-argument".into());
        }
        let mut candidates = vec![];
        let mut after = None;
        loop {
            let page = self.store.scan("record/", after.as_deref(), 100)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|(k, _)| k.clone());
            for (key, value) in page {
                let value = self.changes.get(&key).unwrap_or(&value);
                self.candidate(value, &query, scope, &kinds, now, &mut candidates)?;
            }
        }
        for (key, value) in &self.changes {
            if key.starts_with("record/") && self.store.get(key)?.is_none() {
                self.candidate(value, &query, scope, &kinds, now, &mut candidates)?;
            }
        }
        candidates.sort_by(|(a, ta), (b, tb)| {
            tb.len()
                .cmp(&ta.len())
                .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = candidates.len();
        let mut records = vec![];
        for (record, matched_terms) in candidates.into_iter().take(limit) {
            records.push(json!({"record":record,"matchedTerms":matched_terms}));
            let output = json!({"records":records,"truncated":true,"asOfSequence":sequence});
            if serde_json::to_vec(&output).unwrap().len() > bytes {
                records.pop();
                if records.is_empty() {
                    return Err("result-too-large".into());
                }
                break;
            }
        }
        self.bounded(
            json!({"truncated":records.len()<total,"records":records,"asOfSequence":sequence}),
            bytes,
        )
    }
    fn candidate(
        &self,
        value: &Value,
        query: &BTreeSet<String>,
        scope: &str,
        kinds: &[String],
        now: i64,
        candidates: &mut Vec<(Record, Vec<String>)>,
    ) -> Result<()> {
        let stored: StoredRecord =
            serde_json::from_value(value.clone()).map_err(|_| "storage-error")?;
        if !self.active(&stored, scope, now)?
            || (!kinds.is_empty() && !kinds.contains(&stored.record.kind))
        {
            return Ok(());
        }
        let matched = query
            .intersection(&stored.terms)
            .cloned()
            .collect::<Vec<_>>();
        if !matched.is_empty() {
            candidates.push((stored.record, matched));
        }
        Ok(())
    }
}
fn only(value: &Value, fields: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or("invalid-argument")?;
    if object.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err("invalid-argument".into());
    }
    Ok(())
}
fn string<'a>(args: &'a Value, field: &str, max: usize) -> Result<&'a str> {
    args[field]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= max)
        .ok_or_else(|| "invalid-argument".into())
}
fn strings(args: &Value, field: &str, max: usize, max_bytes: usize) -> Result<Vec<String>> {
    let values = args[field]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= max)
        .ok_or("invalid-argument")?;
    let mut result = vec![];
    for value in values {
        let value = value
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= max_bytes)
            .ok_or("invalid-argument")?
            .to_owned();
        if result.contains(&value) {
            return Err("invalid-argument".into());
        }
        result.push(value);
    }
    Ok(result)
}
fn number(args: &Value, field: &str, default: usize, max: usize) -> Result<usize> {
    match args.get(field) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| *n > 0 && *n <= max as u64)
            .map(|n| n as usize)
            .ok_or_else(|| "invalid-argument".into()),
    }
}
fn terms(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
fn hash(value: &Value) -> String {
    Sha256::digest(serde_json::to_vec(value).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn key(prefix: &str, id: &str) -> String {
    format!("{prefix}/{}", hash(&json!(id)))
}

#[cfg(test)]
mod tests;
