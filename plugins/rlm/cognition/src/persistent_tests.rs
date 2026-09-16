fn observe(e: &mut Engine, c: &Config, id: &str, extra: Value) -> Vec<Draft> {
    let mut value = json!({"provider":"telegram","externalSenderId":"7","conversationId":"chat:1","message":{"chat":{"id":1},"text":"work"}});
    for (key, v) in extra.as_object().unwrap() {
        value[key] = v.clone();
    }
    e.event(c, id, "observation.received", &value, None)
}
fn answer(e: &mut Engine, c: &Config, request: &Draft, text: &str) -> Vec<Draft> {
    let content = if request.payload["tools"].as_array().is_some_and(|tools| tools.iter().any(|t| t["name"] == "yield")) {
        let child = request.payload["tools"][1]["input_schema"]["required"][0] == "result";
        if child { return e.event(c,"answer","model.completed", &completion(request.payload["call_id"].as_str().unwrap(), json!([{"kind":"tool-call","name":"yield","call_id":"control","arguments":{"result":text}}])),None); }
        let mut value: Value = serde_json::from_str(text).unwrap();
        value["action"] = value.as_object_mut().unwrap().remove("transition").unwrap_or(json!("complete"));
        if value["waitFor"] == "input" { value["question"] = json!("Which repository should I use?"); }
        json!([{"kind":"tool-call","name":"yield","call_id":"control","arguments":value}])
    } else {
        let mut value: Value = serde_json::from_str(text).unwrap();
        if value.get("jobId").is_none() { value["jobId"] = Value::Null; }
        if value["action"] == "clarify" { value["question"] = json!("Should I change the memory task or the routing task?"); }
        json!([{"kind":"tool-call","name":"associate","call_id":"association","arguments":value}])
    };
    e.event(
        c,
        "answer",
        "model.completed",
        &completion(
            request.payload["call_id"].as_str().unwrap(),
            content,
        ),
        None,
    )
}
#[test]
fn waiting_jobs_and_sources_survive_serialization() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    assert!(
        answer(
            &mut e,
            &c,
            &first,
            r#"{"transition":"wait","waitFor":"input","note":"Need credentials"}"#
        )
        .iter().any(|d|d.kind=="capability.requested")
    );
    let mut e: Engine = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
    assert_eq!(e.jobs["one"].status, "waiting-input");
    for n in 0..10 {
        assert!(
            e.event(
                &c,
                &format!("irrelevant{n}"),
                "memory.remembered",
                &json!({}),
                None
            )
            .is_empty()
        );
    }
    let next = observe(&mut e, &c, "two", json!({"jobId":"one"}));
    assert_eq!(e.jobs.len(), 1);
    assert_eq!(e.jobs["one"].sources, vec!["one", "two"]);
    assert_eq!(e.jobs["one"].revision, 1);
    assert_eq!(
        next.iter().filter(|d| d.kind == "model.requested").count(),
        1
    );
}
#[test]
fn correction_discards_an_inflight_effect_proposal() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    assert!(
        !observe(
            &mut e,
            &c,
            "two",
            json!({"jobId":"one","constraint":"Leave authentication alone"})
        )
        .iter()
        .any(|d| d.kind == "model.requested")
    );
    let result=e.event(&c,"model","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"await capabilities.invoke('shell.execute',{});"}}])),None);
    assert!(!result.iter().any(|d| d.kind == "code.evaluate-requested"));
    assert!(result.iter().any(|d| d.kind == "model.requested"));
}
#[test]
fn cancelled_job_ignores_late_model_result_and_duplicate_observation() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    observe(&mut e, &c, "stop", json!({"jobId":"one","intent":"cancel"}));
    assert!(answer(&mut e, &c, &first, r#"{"note":"done","reply":"sent"}"#).is_empty());
    assert!(observe(&mut e, &c, "one", json!({})).is_empty());
    assert_eq!(e.jobs["one"].status, "cancelled");
}
#[test]
fn scheduled_wait_uses_owned_revision_after_restart() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    let timer = answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","dueAtMs":60000,"note":"Check external deadline"}"#,
    )
    .remove(0);
    e.event(&c, "timer1", "timer.set", &timer.payload, None);
    e.now_ms = 60_000;
    let next = e
        .event(
            &c,
            "wake1",
            "timer.fired",
            &json!({"requestEventId":"timer1"}),
            None,
        )
        .remove(0);
    assert!(
        e.event(
            &c,
            "duplicate",
            "timer.fired",
            &json!({"requestEventId":"timer1"}),
            None
        )
        .is_empty()
    );
    let timer = answer(&mut e, &c, &next, r#"{"transition":"wait","dueAtMs":180000}"#).remove(0);
    e.event(&c, "timer2", "timer.set", &timer.payload, None);
    let mut e: Engine = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
    e.now_ms = 180_000;
    let resumed = e.event(
        &c,
        "wake2",
        "timer.fired",
        &json!({"requestEventId":"timer2"}),
        None,
    );
    assert_eq!(e.jobs["one"].status, "running");
    assert!(resumed.iter().any(|d| d.kind == "model.requested"));
}
#[test]
fn association_model_separates_independent_work() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "two", json!({})).remove(0);
    assert!(e.inbox["two"].job.is_none());
    let request = answer(&mut e, &c, &classify, r#"{"action":"new"}"#);
    assert_eq!(e.jobs.len(), 2);
    assert_eq!(e.inbox["two"].job.as_deref(), Some("two"));
    assert_eq!(request[0].kind, "model.requested");
}
#[test]
fn another_conversation_cannot_amend_a_job() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({}));
    observe(
        &mut e,
        &c,
        "attack",
        json!({"jobId":"one","conversationId":"chat:other"}),
    );
    assert_eq!(e.jobs["one"].revision, 0);
    assert_eq!(e.inbox["attack"].status, "waiting-input");
}

#[test]
fn another_runnable_job_precedes_a_continuing_model_turn() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    observe(&mut e, &c, "two", json!({"provider":"telegram","externalSenderId":"8","conversationId":"chat:8"}));
    let code=e.event(&c,"m","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"return 1;"}}])),None);
    assert!(
        code.iter()
            .any(|d| d.kind == "model.requested" && d.payload["jobId"] == "two")
    );
}

#[test]
fn duplicate_cell_result_cannot_schedule_an_extra_model_decision() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"m","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"return 1;"}}])),None);
    let next = e
        .event(
            &c,
            "cell",
            "code.completed",
            &json!({"sessionId":"one","value":1}),
            None,
        )
        .remove(0);
    assert!(
        e.event(
            &c,
            "cell",
            "code.completed",
            &json!({"sessionId":"one","value":1}),
            None
        )
        .is_empty()
    );
    assert!(e.queue.is_empty());
    answer(
        &mut e,
        &c,
        &next,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    assert_eq!(e.jobs["one"].status, "waiting-input");
}

#[test]
fn ambiguous_association_preserves_input_and_existing_wait() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "two", json!({})).remove(0);
    let response = answer(&mut e, &c, &classify, r#"{"action":"clarify"}"#);
    assert_eq!(e.inbox["two"].status, "waiting-input");
    assert_eq!(e.jobs["one"].status, "waiting-input");
    assert!(!response.iter().any(|d| d.kind == "model.requested"));
}
#[test]
fn elapsed_cycle_pauses_instead_of_admitting_more_inference() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"m","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"return 1"}}])),None);
    e.now_ms = 1_800_000;
    let out = e.event(
        &c,
        "cell",
        "code.completed",
        &json!({"sessionId":"one","value":1}),
        None,
    );
    assert_eq!(e.jobs["one"].status, "paused-budget");
    assert!(out.iter().any(|d| d.kind == "timer.set"));
    assert!(!out.iter().any(|d| d.kind == "model.requested"));
}
#[test]
fn cancelled_children_cannot_resume_their_parent() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"m","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"await rlm.query({question:'q'});"}}])),None);
    let child = e
        .event(
            &c,
            "yield",
            "code.yielded",
            &json!({"sessionId":"one","id":1,"method":"rlm.query","args":{"question":"q"}}),
            None,
        )
        .remove(0);
    observe(&mut e, &c, "stop", json!({"jobId":"one","intent":"cancel"}));
    let result = answer(&mut e, &c, &child, "late answer");
    assert!(result.is_empty());
    assert!(e.tasks.is_empty());
}

#[test]
fn omitted_progress_fields_preserve_completed_steps() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input","completedSteps":["reproduced"],"completionConditions":["tests pass"]}"#,
    );
    let next = observe(&mut e, &c, "two", json!({"jobId":"one"}))
        .into_iter()
        .find(|d| d.kind == "model.requested")
        .unwrap();
    answer(
        &mut e,
        &c,
        &next,
        r#"{"transition":"wait","dueAtMs":180000,"note":"Run tests"}"#,
    );
    assert_eq!(e.jobs["one"].completed_steps, json!(["reproduced"]));
    assert_eq!(e.jobs["one"].completion_conditions, json!(["tests pass"]));
}

#[test]
fn oversized_context_cannot_leave_a_phantom_coordinator_call() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"m","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"return 1;"}}])),None);
    e.tasks.get_mut("one").unwrap().messages =
        vec![json!({"role":"system","content":[]}),json!({"role":"user","content":[{"kind":"text","text":"Initial input"}]})];
    e.event(
        &c,
        "cell",
        "code.completed",
        &json!({"sessionId":"one","value":"x".repeat(70*1024)}),
        None,
    );
    let next = observe(&mut e, &c, "two", json!({"provider":"telegram","externalSenderId":"8","conversationId":"chat:8"}));
    assert!(next.iter().any(|d| d.kind == "model.requested"));
}

#[test]
fn oversized_job_notes_are_excluded_from_router_context() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    e.jobs.get_mut("one").unwrap().notes = json!("x".repeat(70 * 1024));
    observe(&mut e, &c, "two", json!({}));
    assert_eq!(e.inbox["two"].status, "pending");
    assert_eq!(e.calls.len(), 1);
}

#[test]
fn operator_cancel_stops_running_job_and_invalidates_requests() {
    let mut e = Engine::default();
    let c = config();
    let request = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c, "request", "model.requested", &request.payload, None);
    let out = e.event(&c, "operator-cancel", "operator.job-control", &json!({"version":1,"jobId":"one","action":"cancel","revision":0,"reason":"stop"}), None);
    assert_eq!(e.jobs["one"].status, "cancelled");
    assert!(out.iter().any(|d| d.kind == "cognition.cancel-requested"));
    assert!(!e.tasks.contains_key("one"));
    assert!(e.calls.is_empty());
}

#[test]
fn operator_resume_requires_reconciliation_and_preserves_origin() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({}));
    e.calls.clear();
    e.jobs.get_mut("one").unwrap().status = "waiting-input".into();
    e.jobs.get_mut("one").unwrap().activities.insert("effect".into(), json!({"status":"capability.failed","result":{"code":"outcome-unknown"}}));
    let control = json!({"version":1,"jobId":"one","action":"resume","revision":0,"reason":"continue"});
    assert!(e.event(&c, "resume", "operator.job-control", &control, None).is_empty());
    assert_eq!(e.jobs["one"].status, "waiting-input");
    let origin = e.jobs["one"].origin.clone();
    assert!(e.event(&c, "reconciled", "operator.attempt-reconciled", &json!({"version":1,"requestEventId":"effect","outcome":"completed","output":{"ok":true},"reason":"verified"}), None).is_empty());
    assert_eq!(e.jobs["one"].status, "waiting-input");
    let out = e.event(&c, "resume2", "operator.job-control", &control, None);
    assert!(out.iter().any(|d| d.kind == "model.requested"));
    assert_eq!(out.iter().find(|d| d.kind == "model.requested").unwrap().cause, origin);
    assert_eq!(e.jobs["one"].origin, origin);
    assert_eq!(e.tasks["one"].origin, origin);
    assert_eq!(e.jobs["one"].revision, 1);
}

#[test]
fn retired_jobs_release_context_but_duplicate_observations_stay_inert() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({"message":{"chat":{"id":1},"text":"x".repeat(20000)}}));
    e.jobs.get_mut("one").unwrap().status = "completed".into();
    e.tasks.clear();
    e.calls.clear();
    for n in 0..65 {
        let mut job = e.jobs["one"].clone();
        job.id = format!("later-{n}");
        job.incorporated_sequence = n + 1;
        e.jobs.insert(job.id.clone(), job);
    }
    e.retire();
    assert!(!e.jobs.contains_key("one"));
    assert!(e.inbox["one"].value.is_null());
    assert!(observe(&mut e, &c, "one", json!({})).is_empty());
}

#[test]
fn legacy_connector_events_do_not_mutate_observations() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({"update_id":47,"media":[{"status":"ready"}]}));
    let mut ready = e.inbox["one"].value.clone();
    ready["observationDeduplicationKey"] = json!("telegram:update:47");
    ready["media"] = json!([{"status":"failed"}]);
    assert!(e.event(&c, "media", "telegram.media-ready", &ready, None).is_empty());
    assert_eq!(e.jobs.len(), 1);
    assert_eq!(e.inbox["one"].value["media"][0]["status"], "ready");
    assert_eq!(e.tasks["one"].context["observation"]["media"][0]["status"], "ready");
}

#[test]
fn queued_model_request_preserves_its_observation_authority() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    observe(&mut e, &c, "two", json!({"provider":"telegram","externalSenderId":"8","conversationId":"chat:2","message":{"chat":{"id":2},"text":"other"}}));
    let out = answer(&mut e, &c, &first, r#"{"transition":"complete","reply":null}"#);
    let second = out.iter().find(|d| d.kind == "model.requested" && d.payload["jobId"] == "two").unwrap();
    assert_eq!(second.cause, "two");
}

#[test]
fn legacy_retirement_bounds_each_delivery() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "one", json!({}));
    e.jobs.get_mut("one").unwrap().status = "completed".into();
    e.tasks.clear();
    e.calls.clear();
    for index in 0..200 {
        let mut job = e.jobs["one"].clone();
        job.id = format!("old-{index}");
        e.jobs.insert(job.id.clone(), job);
    }
    let before = crate::storage::records(&e).unwrap();
    e.retire();
    let after = crate::storage::records(&e).unwrap();
    let changed = before.iter().filter(|(key, value)| after.get(*key) != Some(*value)).count()
        + after.keys().filter(|key| !before.contains_key(*key)).count();
    assert!(changed <= 64, "retirement changed {changed} records");
}

#[test]
fn amendment_closes_the_interrupted_tool_call() {
    let mut e = Engine::default(); let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"model","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"blocked","arguments":{"code":"await capabilities.invoke('shell.execute',{});"}}])),None);
    e.event(&c,"old-shell","capability.requested",&json!({"jobId":"one","revision":0,"capability":"shell.execute"}),None);
    e.event(&c,"old-code","code.evaluate-requested",&json!({"jobId":"one","revision":0,"sessionId":"one"}),None);
    e.tasks.get_mut("one").unwrap().pending=Some(json!({"request":"old-shell","yield":1}));
    let classify=observe(&mut e,&c,"update",json!({"message":{"text":"also inspect deployment"}})).into_iter().find(|d|d.kind=="model.requested").unwrap();
    let out=e.event(&c,"amend","model.completed",&completion(classify.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"associate","call_id":"associate-update","arguments":{"action":"amend","jobId":"one"}}])),None);
    let request=out.iter().find(|d| d.kind=="model.requested").unwrap();
    let messages=request.payload["messages"].as_array().unwrap();
    let result=messages.iter().flat_map(|m|m["content"].as_array().unwrap()).find(|x|x["kind"]=="tool-result" && x["call_id"]=="blocked");
    assert!(result.is_some(),"amendment must close the pending model tool call");
    assert_eq!(result.unwrap()["output"]["outcome"],"unknown");
    let late=e.event(&c,"late","capability.completed",&json!({"requestEventId":"old-shell","output":{"exit_code":0}}),None);
    assert!(!late.iter().any(|d| d.kind=="code.resumed"));
    assert_eq!(e.jobs["one"].activities["old-shell"]["result"]["output"]["exit_code"],0);
    let count=e.tasks["one"].messages.len();
    e.event(&c,"late-code","code.completed",&json!({"sessionId":"one","requestEventId":"old-code","value":"late"}),None);
    assert_eq!(e.tasks["one"].messages.len(),count,"late results must not alter the amended conversation");

}

#[test]
fn malformed_tool_history_fails_locally_without_model_retry() {
    let mut e = Engine::default(); let c=config();
    let first=observe(&mut e,&c,"one",json!({})).remove(0);
    e.event(&c,"model","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"call","arguments":{"code":"return 1;"}}])),None);
    e.tasks.get_mut("one").unwrap().messages.push(json!({"role":"user","content":[{"kind":"text","text":"interleaved"}]}));
    let out=e.event(&c,"result","code.completed",&json!({"sessionId":"one","requestEventId":"request","value":1}),None);
    assert!(!out.iter().any(|d|d.kind=="model.requested"));
    assert_eq!(e.jobs["one"].status,"failed");
    assert!(out.iter().any(|d|d.kind=="capability.requested" && d.payload["arguments"]["text"].as_str().is_some_and(|s|s.contains("tool"))));
}
