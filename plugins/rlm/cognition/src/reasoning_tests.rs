#[test]
fn reasoning_rejects_model_scheduled_continuations() {
    assert!(validate_control(&json!({"action":"continue"}), false).is_err());
    assert!(!model_tools(false).to_string().contains("continue"));
}

#[test]
fn reasoning_empty_outputs_request_execution_then_report_stall() {
    let mut e = Engine::default();
    let c = config();
    let mut request = observe(&mut e, &c, "one", json!({})).remove(0);
    for attempt in 1..=3 {
        let out = e.event(&c, &format!("empty-{attempt}"), "model.completed",
            &completion(request.payload["call_id"].as_str().unwrap(), json!([])), None);
        assert!(!out.iter().any(|d| d.kind == "timer.set"));
        if attempt < 3 {
            request = out.into_iter().find(|d| d.kind == "model.requested").unwrap();
            assert!(turn(&request)["trigger"]["error"].as_str().unwrap().contains("executable JS"));
        } else {
            assert_eq!(e.jobs["one"].status, "failed");
            assert!(out.iter().any(|d| d.kind == "capability.requested" && d.payload["arguments"]["text"].as_str().is_some_and(|s| s.contains("stalled"))));
        }
    }
}

#[test]
fn reasoning_budget_pause_preserves_session_and_uses_fixed_delay() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c, "js", "model.completed", &completion(first.payload["call_id"].as_str().unwrap(),
        json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"state.rows = [1,2,3]; return state.rows.length;"}}])), None);
    e.budgets.insert("one".into(), 32);
    e.jobs.get_mut("one").unwrap().retry_count = 7;
    let out = e.event(&c, "cell", "code.completed", &json!({"sessionId":"one","value":3}), None);
    assert_eq!(e.jobs["one"].status, "paused-budget");
    assert!(e.tasks["one"].cell_started);
    assert_eq!(e.jobs["one"].retry_count, 7);
    assert!(!out.iter().any(|d| d.kind == "code.close-requested"));
    let timer = out.iter().find(|d| d.kind == "timer.set").unwrap();
    assert_eq!(timer.payload["dueAtMs"], e.now_ms + 60_000);
    e.event(&c, "timer", "timer.set", &timer.payload, None);
    e.now_ms += 60_000;
    let wake = e.event(&c, "wake", "timer.fired", &json!({"requestEventId":"timer"}), None);
    let request = wake.iter().find(|d| d.kind == "model.requested").unwrap();
    let out = e.event(&c, "js2", "model.completed", &completion(request.payload["call_id"].as_str().unwrap(),
        json!([{"kind":"tool-call","name":"js","call_id":"js2","arguments":{"code":"return state.rows;"}}])), None);
    assert_eq!(out[0].payload["requiresSession"], true);
}

#[test]
fn reasoning_repeated_cells_report_stall() {
    let mut e = Engine::default();
    let c = config();
    let mut request = observe(&mut e, &c, "one", json!({})).remove(0);
    for i in 0..3 {
        e.event(&c, &format!("m{i}"), "model.completed", &completion(request.payload["call_id"].as_str().unwrap(),
            json!([{"kind":"tool-call","name":"js","call_id":format!("js{i}"),"arguments":{"code":"return 'I will implement it';"}}])), None);
        let out = e.event(&c, &format!("cell{i}"), "code.completed", &json!({"sessionId":"one","value":"I will implement it"}), None);
        e = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        if i < 2 {
            request = out.into_iter().find(|d| d.kind == "model.requested").unwrap();
        } else {
            assert_eq!(e.jobs["one"].status, "failed");
            assert!(out.iter().any(|d| d.kind == "capability.requested"));
        }
    }
}

#[test]
fn reasoning_rejects_expired_waits_and_unfocused_children() {
    let mut e = Engine::default();
    let c = config();
    e.now_ms = 100;
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    let out = answer(&mut e, &c, &first, r#"{"transition":"wait","dueAtMs":99}"#);
    assert!(out.iter().any(|d| d.kind == "model.requested"));
    assert!(!out.iter().any(|d| d.kind == "timer.set"));
    for args in [json!({"context":[1,2]}), json!({"question":"  "})] {
        let out = e.event(&c, &format!("child-{args}"), "code.yielded", &json!({"sessionId":"one","id":1,"method":"rlm.query","args":args}), None);
        assert_eq!(out[0].kind, "code.resumed");
        assert!(out[0].payload["response"]["error"].is_string());
        assert!(!e.tasks.contains_key("one:1"));
    }
}

#[test]
fn reasoning_changed_output_is_progress_and_js_resets_corrections() {
    let mut e = Engine::default();
    let c = config();
    let mut request = observe(&mut e, &c, "one", json!({})).remove(0);
    for i in 0..4 {
        e.tasks.get_mut("one").unwrap().control_errors = 2;
        e.event(&c, &format!("m{i}"), "model.completed", &completion(request.payload["call_id"].as_str().unwrap(),
            json!([{"kind":"tool-call","name":"js","call_id":format!("js{i}"),"arguments":{"code":"console.log(++state.n);"}}])), None);
        assert_eq!(e.tasks["one"].control_errors, 0);
        let out = e.event(&c, &format!("cell{i}"), "code.completed", &json!({"sessionId":"one","value":null,"log":[format!("log: {i}")]}), None);
        request = out.into_iter().find(|d| d.kind == "model.requested").unwrap();
    }
    assert_eq!(e.jobs["one"].status, "running");
}

#[test]
fn plugin_boundary_uses_only_generic_capability_dispatch() {
    assert!(!PROMPT.contains("memory."));
    assert!(PROMPT.contains("Use yield reply for user-facing text; invoke media capabilities through JS only for requested file or media sends."));
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({}));
    let out = e.event(&c, "legacy", "code.yielded", &json!({"sessionId":"one","id":1,"method":"memory.recall","args":{}}), None);
    assert_eq!(out[0].kind, "code.resumed");
    assert!(out[0].payload["response"]["error"].is_string());
}
