fn delegated_child(e: &mut Engine, c: &Config, parent: &str, range: Value) -> Vec<Draft> {
    e.event(c, &format!("{parent}-child"), "code.yielded", &json!({"sessionId":parent,"id":1,"method":"rlm.query","args":{"question":"q","range":range}}), None)
}

#[test]
fn child_untrusted_root_cannot_delegate_history() {
    let c = config();
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    e.tasks.get_mut("o").unwrap().context["observation"]["externalSenderId"] = json!("untrusted");
    delegated_child(&mut e, &c, "o", json!({"after":10,"limit":20}));
    assert!(e.history_window(&c, "o:1").is_none());
}

#[test]
fn child_nested_history_is_intersected_with_parent() {
    let c = config();
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    delegated_child(&mut e, &c, "o", json!({"after":10,"limit":20}));
    delegated_child(&mut e, &c, "o:1", json!({"after":0,"limit":100}));
    let window = e.history_window(&c, "o:1:1").unwrap();
    assert_eq!((window.after, window.limit), (10, 20));
    assert_eq!(
        e.tasks["o:1:1"].context["range"],
        json!({"after":10,"limit":20})
    );
}

#[test]
fn child_inherits_amended_job_revision_for_js() {
    let c = config();
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    let requests = observe(
        &mut e,
        &c,
        "amend",
        json!({"jobId":"o","message":{"text":"amend"}}),
    );
    assert_eq!(e.jobs["o"].revision, 1);
    let request = requests
        .iter()
        .find(|d| d.kind == "model.requested")
        .unwrap();
    e.event(&c, "root-js", "model.completed", &completion(request.payload["call_id"].as_str().unwrap(), json!([{"kind":"tool-call","name":"js","call_id":"root-js","arguments":{"code":"return 1"}}])), None);
    let requests = delegated_child(&mut e, &c, "o", Value::Null);
    let request = requests
        .iter()
        .find(|d| d.kind == "model.requested")
        .unwrap();
    let out = e.event(&c, "child-js", "model.completed", &completion(request.payload["call_id"].as_str().unwrap(), json!([{"kind":"tool-call","name":"js","call_id":"child-js","arguments":{"code":"return 1"}}])), None);
    let code = out
        .iter()
        .find(|d| d.kind == "code.evaluate-requested")
        .unwrap();
    assert_eq!(code.payload["revision"], 1);
}

#[test]
fn child_background_inference_consumes_hourly_allowance() {
    let c = config();
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    e.tasks.get_mut("o").unwrap().background = true;
    let requests = delegated_child(&mut e, &c, "o", Value::Null);
    assert!(requests.iter().any(|d| d.kind == "model.requested"));
    assert_eq!(e.background_calls.len(), 1);
}

#[test]
fn child_background_inference_pauses_at_hourly_limit() {
    let mut c = config();
    c.background_model_calls_per_hour = 1;
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    e.tasks.get_mut("o").unwrap().background = true;
    e.background_calls.push(e.now_ms);
    let requests = delegated_child(&mut e, &c, "o", Value::Null);
    assert!(!requests.iter().any(|d| d.kind == "model.requested"));
    assert_eq!(e.jobs["o"].status, "paused-budget");
    assert!(requests.iter().any(|d| d.kind == "timer.set"));
}

#[test]
fn child_disjoint_and_overflowing_ranges_grant_no_history() {
    let c = config();
    for range in [
        json!({"after":30,"limit":20}),
        json!({"after":u64::MAX-1,"limit":20}),
    ] {
        let mut e = Engine::default();
        root_with_a_cell(&mut e, &c);
        delegated_child(&mut e, &c, "o", json!({"after":10,"limit":20}));
        delegated_child(&mut e, &c, "o:1", range);
        assert!(e.history_window(&c, "o:1:1").is_none());
        assert!(e.tasks["o:1:1"].context["range"].is_null());
    }
}

#[test]
fn child_budget_pause_settles_root_tool_call() {
    let mut c = config();
    c.background_model_calls_per_hour = 1;
    let mut e = Engine::default();
    root_with_a_cell(&mut e, &c);
    e.tasks.get_mut("o").unwrap().background = true;
    e.background_calls.push(e.now_ms);
    let requests = delegated_child(&mut e, &c, "o", Value::Null);
    let timer = requests.iter().find(|d| d.kind == "timer.set").unwrap();
    e.event(&c, "budget-timer", "timer.set", &timer.payload, None);
    let mut e: Engine = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
    e.now_ms = 3_600_000;
    let requests = e.event(
        &c,
        "budget-wake",
        "timer.fired",
        &json!({"requestEventId":"budget-timer"}),
        None,
    );
    assert!(requests.iter().any(|d| d.kind == "model.requested"));
    assert!(
        e.tasks["o"]
            .messages
            .iter()
            .any(|m| m["role"] == "tool" && m["content"][0]["call_id"] == "js1")
    );
}
