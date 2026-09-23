use super::*;
use std::collections::BTreeSet;
use std::fmt::Write;

fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!("memory_learning_cases.json")).unwrap()
}

fn completion(event: &CommittedEvent, name: &str, args: &Value) -> Value {
    json!({"call_id":payload(event)["call_id"],"message":{"role":"assistant","content":[
        {"kind":"tool-call","call_id":format!("{}-{name}",event.event_id.as_str()),"name":name,"arguments":args}
    ]},"stop_reason":{"kind":"tool-call"}})
}

fn save_script(case: &Value, initial: bool) -> String {
    let content = if initial {
        &case["initialLesson"]
    } else {
        &case["lesson"]
    };
    let args = json!({"scope":"project:p","kind":"procedure","basis":"explicit","content":content});
    let mut code = format!(
        "const args={args}; args.operationId=context.observationEventId; args.sources=[context.observationEventId];"
    );
    if !initial && case.get("initialLesson").is_some() {
        write!(code,
            "const prior=(await capabilities.invoke('memory.recall',{{scope:'project:p',query:{}}})).output.records[0].record; args.expectedId=prior.id; return (await capabilities.invoke('memory.supersede',args)).output;",
            case["query"]
        ).unwrap();
    } else {
        code += "return (await capabilities.invoke('memory.remember',args)).output;";
    }
    code
}

async fn replay_turn(
    agent: &mut TestAgent,
    store: &Arc<SqliteEventStore<Metadata>>,
    conversation: &str,
    text: &str,
    script: String,
    answer: &Value,
    live: bool,
) -> Vec<CommittedEvent> {
    let mut script = Some(script);
    memory_turn_with(agent, store, conversation, text, |event| {
        if live {
            return tokio::task::block_in_place(|| memory_eval_provider::run(payload(event)));
        }
        if payload(event)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "associate")
        {
            return completion(event, "associate", &json!({"action":"new","jobId":null}));
        }
        if let Some(code) = script.take() {
            completion(event, "js", &json!({"code":code}))
        } else {
            completion(
                event,
                "yield",
                &json!({"action":"complete","reply":answer.to_string()}),
            )
        }
    })
    .await
}

fn score(
    expected: &Value,
    reply: &str,
    writes: usize,
    corrections: usize,
    needs_correction: bool,
    sourced_recall: bool,
) -> Value {
    let decision = serde_json::from_str::<Value>(reply).unwrap_or(Value::Null);
    let correct = decision == *expected;
    json!({
        "decision":decision,
        "correctDecision":correct,
        "durableWrites":writes,
        "corrections":corrections,
        "sourcedRecall":sourced_recall,
        "valid":correct && writes == 1 && (!needs_correction || corrections == 1) && sourced_recall
    })
}

#[allow(clippy::too_many_lines)]
async fn replay(case: &Value, live: bool) -> Value {
    let started = std::time::Instant::now();
    let (mut agent, store) = build(CAPS).await;
    install_memory_reasoning(&mut agent).await;
    // Seed an older version to isolate correction learning from initial extraction.
    if let Some(initial) = case["initialLesson"].as_str() {
        replay_turn(
            &mut agent,
            &store,
            "seed",
            initial,
            save_script(case, true),
            &json!("seeded"),
            false,
        )
        .await;
    }
    let learned = replay_turn(
        &mut agent,
        &store,
        "training",
        case["observation"].as_str().unwrap(),
        save_script(case, false),
        &json!("acknowledged"),
        live,
    )
    .await;
    let source = learned[0].event_id.as_str();
    let writes = learned
        .iter()
        .filter(|e| {
            matches!(
                e.request.event_type.as_str(),
                "memory.remembered" | "memory.superseded"
            )
        })
        .collect::<Vec<_>>();
    let learned_ids = writes
        .iter()
        .filter_map(|e| {
            payload(e)["record"]["record"]["id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let corrections = writes
        .iter()
        .filter(|e| e.request.event_type == "memory.superseded")
        .count();
    drop(agent);
    let mut agent = build_on(&store, CAPS).await;
    install_memory_reasoning(&mut agent).await;
    // A distinct conversation excludes the training turn from recent context.
    let recalled = replay_turn(&mut agent, &store, "application", case["question"].as_str().unwrap(),
        format!("return (await capabilities.invoke('memory.recall',{{scope:'project:p',query:{}}})).output;",case["query"]),
        &case["expected"], live).await;
    let recall_ids = recalled
        .iter()
        .filter(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "memory.recall"
        })
        .map(|e| e.event_id.as_str())
        .collect::<BTreeSet<_>>();
    let sourced_recall = recalled
        .iter()
        .filter(|e| e.request.event_type == "capability.completed")
        .any(|e| {
            let value = payload(e);
            recall_ids.contains(value["requestEventId"].as_str().unwrap_or_default())
                && value["output"]["records"]
                    .as_array()
                    .is_some_and(|records| {
                        records.iter().any(|entry| {
                            let record = &entry["record"];
                            learned_ids.contains(record["id"].as_str().unwrap_or_default())
                                && record["sources"]
                                    .as_array()
                                    .is_some_and(|ids| ids.contains(&json!(source)))
                        })
                    })
        });
    let reply = recalled
        .iter()
        .find_map(|e| {
            let v = payload(e);
            (e.request.event_type == "capability.requested" && v["capability"] == "telegram.reply")
                .then(|| v["arguments"]["text"].as_str().map(str::to_owned))
                .flatten()
        })
        .unwrap_or_default();
    let mut result = score(
        &case["expected"],
        &reply,
        writes.len(),
        corrections,
        case.get("initialLesson").is_some(),
        sourced_recall,
    );
    result["id"] = case["id"].clone();
    result["mode"] = json!(if live { "live" } else { "scripted" });
    result["latencyMs"] = json!(started.elapsed().as_millis());
    result["modelCalls"] = json!(
        learned
            .iter()
            .chain(&recalled)
            .filter(|e| e.request.event_type == "model.requested")
            .count()
    );
    result["recallCalls"] = json!(recall_ids.len());
    result
}

#[test]
fn learning_score_rejects_unsaved_duplicate_stale_and_unretrieved_lessons() {
    let expected = json!({"command":"python3"});
    for (reply, writes, corrections, recalled) in [
        (r#"{"command":"python3"}"#, 0, 1, true),
        (r#"{"command":"python3"}"#, 2, 1, true),
        (r#"{"command":"python3"}"#, 1, 0, true),
        (r#"{"command":"python3"}"#, 1, 1, false),
        (
            r#"{"command":"nix shell nixpkgs#python3 -c python3"}"#,
            1,
            1,
            true,
        ),
        ("Saved.", 1, 1, true),
    ] {
        assert_eq!(
            score(&expected, reply, writes, corrections, true, recalled)["valid"],
            false
        );
    }
    assert_eq!(
        score(&expected, r#"{"command":"python3"}"#, 1, 1, true, true)["valid"],
        true
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_learning_scripted_replay() {
    for case in cases() {
        let result = replay(&case, false).await;
        println!("{result}");
        assert_eq!(result["valid"], true, "{result}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly configured live provider"]
async fn memory_learning_live_replay() {
    if std::env::var("PLURIBUS_MEMORY_EVAL_LIVE").as_deref() != Ok("1") {
        eprintln!("Set PLURIBUS_MEMORY_EVAL_LIVE=1 to enable provider calls.");
        return;
    }
    let path =
        std::env::var("PLURIBUS_MEMORY_EVAL_OUTPUT").expect("set PLURIBUS_MEMORY_EVAL_OUTPUT");
    let mut results = vec![];
    for case in cases() {
        results.push(replay(&case, true).await);
    }
    let report = json!({"cases":results});
    std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    assert!(
        results.iter().all(|result| result["valid"] == true),
        "{report}"
    );
}
