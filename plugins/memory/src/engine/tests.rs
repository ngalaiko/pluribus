use super::*;
#[derive(Default)]
struct Data(BTreeMap<String, Value>);
impl Store for Data {
    fn get(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.0.get(key).cloned())
    }
    fn scan(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Value)>> {
        Ok(self
            .0
            .iter()
            .filter(|(k, _)| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a))
            .take(limit)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}
fn args(op: &str, text: &str) -> Value {
    json!({"operationId":op,"kind":"procedure","content":text,"scope":"project:p","sources":["source"],"basis":"explicit"})
}
#[test]
fn corrections_forgetting_and_receipts_survive_rebuild() {
    let data = Data::default();
    let config = Config::default();
    let mut e = Engine::new(&data, &config, "memory");
    let a = args("a", "Use Jujutsu");
    let (first, c1) = e.execute("remember", &a, 1, 1, |_| true).unwrap();
    let mut b = args("b", "Use Jujutsu, never push");
    b["expectedId"] = first["id"].clone();
    let (second, c2) = e.execute("supersede", &b, 2, 2, |_| true).unwrap();
    let mut stale = b.clone();
    stale["operationId"] = json!("c");
    assert_eq!(
        e.execute("supersede", &stale, 3, 3, |_| true).unwrap_err(),
        "conflict"
    );
    let forget = json!({"operationId":"forget","expectedId":second["id"],"scope":"project:p"});
    let (receipt, c3) = e.execute("forget", &forget, 3, 3, |_| true).unwrap();
    assert_eq!(receipt["forgottenIds"], json!([second["id"], first["id"]]));
    let mut rebuilt = Engine::new(&data, &config, "memory");
    for change in [c1, c2, c3].into_iter().flatten() {
        rebuilt.apply(&change).unwrap();
        rebuilt.apply(&change).unwrap();
    }
    assert_eq!(e.changes, rebuilt.changes);
    let read = json!({"query":"Jujutsu","scope":"project:p"});
    assert_eq!(
        rebuilt.execute("recall", &read, 4, 4, |_| true).unwrap().0["records"],
        json!([])
    );
    let retry = rebuilt.execute("remember", &a, 4, 4, |_| false).unwrap();
    assert_eq!(retry.0, first);
    assert!(retry.1.is_none());
    let mut changed = a.clone();
    changed["content"] = json!("Other");
    assert_eq!(
        rebuilt
            .execute("remember", &changed, 4, 4, |_| true)
            .unwrap_err(),
        "operation-conflict"
    );
}
#[test]
fn lexical_ranking_filters_expiry_scope_and_bounds() {
    let data = Data::default();
    let config = Config::default();
    let mut e = Engine::new(&data, &config, "m");
    let (a, _) = e
        .execute("remember", &args("a", "Jujutsu workflow"), 1, 1, |_| true)
        .unwrap();
    e.execute("remember", &args("b", "Jujutsu"), 2, 2, |_| true)
        .unwrap();
    let mut expired = args("expired", "Jujutsu workflow");
    expired["expiresAtMs"] = json!(3);
    e.execute("remember", &expired, 2, 2, |_| true).unwrap();
    let mut foreign = args("foreign", "Jujutsu workflow");
    foreign["scope"] = json!("other");
    e.execute("remember", &foreign, 2, 2, |_| true).unwrap();
    let read = json!({"query":"JUJUTSU workflow","scope":"project:p","limit":1});
    let result = e.execute("recall", &read, 3, 5, |_| true).unwrap().0;
    assert_eq!(result["records"][0]["record"]["id"], a["id"]);
    assert_eq!(
        result["records"][0]["matchedTerms"],
        json!(["jujutsu", "workflow"])
    );
    assert_eq!(result["truncated"], true);
    let mut small = read;
    small["maxBytes"] = json!(10);
    assert_eq!(
        e.execute("recall", &small, 3, 5, |_| true).unwrap_err(),
        "result-too-large"
    );
    let get = json!({"ids":[a["id"],"missing"],"scope":"other"});
    assert_eq!(
        e.execute("get", &get, 3, 5, |_| true).unwrap().0["unavailableIds"],
        get["ids"]
    );
}
#[test]
fn invalid_sources_capacity_and_arguments_leave_no_partial_writes() {
    let data = Data::default();
    let config = Config {
        max_records: 1,
        ..Config::default()
    };
    let mut e = Engine::new(&data, &config, "m");
    let a = args("a", "Fact");
    assert_eq!(
        e.execute("remember", &a, 1, 1, |_| false).unwrap_err(),
        "source-unavailable"
    );
    assert!(e.changes.is_empty());
    let mut invalid = a.clone();
    invalid["injected"] = json!(true);
    assert_eq!(
        e.execute("remember", &invalid, 1, 1, |_| true).unwrap_err(),
        "invalid-argument"
    );
    e.execute("remember", &a, 1, 1, |_| true).unwrap();
    let before = e.changes.clone();
    assert_eq!(
        e.execute("remember", &args("b", "Other"), 1, 1, |_| true)
            .unwrap_err(),
        "capacity-exceeded"
    );
    assert_eq!(before, e.changes);
}
#[test]
fn recall_scans_multiple_pages_and_uses_committed_state() {
    let mut data = Data::default();
    let config = Config::default();
    let mut e = Engine::new(&data, &config, "m");
    for n in 0..205 {
        e.execute(
            "remember",
            &args(&n.to_string(), "needle"),
            n,
            n as u64,
            |_| true,
        )
        .unwrap();
    }
    data.0 = e.changes;
    let mut e = Engine::new(&data, &config, "m");
    let result = e
        .execute(
            "recall",
            &json!({"scope":"project:p","query":"needle"}),
            300,
            300,
            |_| true,
        )
        .unwrap()
        .0;
    assert_eq!(result["records"][0]["record"]["createdAtMs"], 204);
    assert_eq!(result["records"].as_array().unwrap().len(), 8);
    assert_eq!(result["truncated"], true);
}

#[test]
fn a_complete_result_fits_its_exact_serialized_budget() {
    let data = Data::default();
    let config = Config::default();
    let mut e = Engine::new(&data, &config, "m");
    e.execute("remember", &args("a", "needle"), 1, 1, |_| true)
        .unwrap();
    let mut read = json!({"scope":"project:p","query":"needle"});
    let result = e.execute("recall", &read, 2, 2, |_| true).unwrap().0;
    read["maxBytes"] = json!(serde_json::to_vec(&result).unwrap().len());
    assert_eq!(
        e.execute("recall", &read, 2, 2, |_| true).unwrap().0,
        result
    );
}

#[test]
fn denied_writes_do_not_block_identical_authorized_retries() {
    let data = Data::default();
    let config = Config::default();
    let mut e = Engine::new(&data, &config, "m");
    let a = args("a", "needle");
    assert!(e.execute("remember", &a, 1, 1, |_| false).is_err());
    assert!(e.execute("remember", &a, 1, 1, |_| true).is_ok());
}

#[test]
fn correction_chains_remain_forgettable_within_response_limits() {
    let data = Data::default();
    let config = Config {
        max_result_bytes: 256,
        ..Config::default()
    };
    let mut e = Engine::new(&data, &config, "m");
    let mut id = e
        .execute("remember", &args("0", "Fact"), 1, 1, |_| true)
        .unwrap()
        .0["id"]
        .clone();
    for n in 1..10 {
        let mut a = args(&n.to_string(), "Correction");
        a["expectedId"] = id.clone();
        match e.execute("supersede", &a, n, n as u64, |_| true) {
            Ok((result, _)) => id = result["id"].clone(),
            Err(code) => {
                assert_eq!(code, "result-too-large");
                break;
            }
        }
    }
    e.execute(
        "forget",
        &json!({"operationId":"forget","expectedId":id,"scope":"project:p"}),
        20,
        20,
        |_| true,
    )
    .unwrap();
}
