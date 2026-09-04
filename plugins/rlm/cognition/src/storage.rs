use crate::engine::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CHUNK_BYTES: usize = 128 * 1024;

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

pub fn records(
    engine: &Engine,
) -> Result<std::collections::BTreeMap<String, Vec<u8>>, serde_json::Error> {
    let value = serde_json::to_value(engine)?;
    let mut records = std::collections::BTreeMap::new();
    for (field, value) in value.as_object().unwrap() {
        let entries: Vec<(String, Value)> = if field == "seen_results" {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|key| (key.as_str().unwrap().to_owned(), Value::Bool(true)))
                .collect()
        } else if let Some(values) = value.as_object() {
            values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        } else {
            vec![(String::new(), value.clone())]
        };
        for (key, value) in entries {
            let field = if field == "inbox" && value["value"].is_null() {
                "observations_seen"
            } else {
                field.as_str()
            };
            let prefix = record_prefix(field, &key);
            let record = Record {
                field: field.into(),
                key,
                value,
            };
            for (index, part) in serde_json::to_vec(&record)?.chunks(CHUNK_BYTES).enumerate() {
                records.insert(format!("{prefix}{index:08}"), part.to_vec());
            }
        }
    }
    Ok(records)
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
