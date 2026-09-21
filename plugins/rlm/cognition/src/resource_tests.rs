fn resource_cell(e: &mut Engine, c: &Config, request: &Draft, id: &str) {
    let output = e.event(c, id, "model.completed", &completion(
        request.payload["call_id"].as_str().unwrap(),
        json!([{"kind":"tool-call","name":"js","call_id":id,
            "arguments":{"code":format!("await history.read({{limit: {}}}); // {id}", 32 - id.len())}}]),
    ), None);
    let code = output
        .iter()
        .find(|d| d.kind == "code.evaluate-requested")
        .unwrap();
    e.event(c, id, "code.evaluate-requested", &code.payload, None);
}

fn memory_failure(session: &str, request: &str) -> Value {
    json!({"sessionId":session,"requestEventId":request,"code":"resource-exhausted",
        "resourceError":{"resource":"memory","currentBytes":32768000,
            "requestedBytes":34078720,"limitBytes":33554432,"phase":"execution"},
        "effectStatus":"outcome-unknown","reason":"memory limit exceeded"})
}

#[test]
fn resource_failure_replans_with_limits_and_committed_checkpoint() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    resource_cell(&mut e, &c, &first, "cell");
    let checkpoint = json!({"version":1,"mode":"explicit","state":{"offset":10}});
    let task = e.tasks.get_mut("one").unwrap();
    task.context["checkpoint"] = checkpoint.clone();
    task.pending = Some(json!({"request":"external","yield":1}));
    let out = e.event(
        &c,
        "oom",
        "code.failed",
        &memory_failure("one", "cell"),
        None,
    );
    assert!(out.iter().any(|d| d.kind == "model.requested"));
    let task = &e.tasks["one"];
    assert!(
        !task.cell_started,
        "resource failure loses the guest session"
    );
    assert!(task.pending.is_none());
    assert_eq!(task.context["checkpoint"], checkpoint);
    assert_eq!(task.context["resourceRecovery"]["remainingAttempts"], 2);
    assert_eq!(
        task.context["resourceRecovery"]["effectStatus"],
        "outcome-unknown"
    );
    assert_eq!(task.context["resourceRecovery"]["readLimit"], 16);
}

#[test]
fn resource_retry_allowance_survives_restart_and_ends_only_affected_job() {
    let c = config();
    let mut e = Engine::default();
    let mut request = observe(&mut e, &c, "one", json!({})).remove(0);
    for n in 0..3 {
        let cell = format!("cell-{n}");
        resource_cell(&mut e, &c, &request, &cell);
        let out = e.event(
            &c,
            &format!("oom-{n}"),
            "code.failed",
            &memory_failure("one", &cell),
            None,
        );
        if n < 2 {
            request = out
                .into_iter()
                .find(|d| d.kind == "model.requested")
                .unwrap();
            e = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
        } else {
            assert!(!out.iter().any(|d| d.kind == "model.requested"));
            assert!(out.iter().any(|d| d.kind == "cognition.failed"));
            assert_eq!(e.jobs["one"].status, "failed");
        }
    }
    assert!(
        observe(&mut e, &c, "two", json!({"conversationId":"chat:2"}))
            .iter()
            .any(|d| d.kind == "model.requested")
    );
}

#[test]
fn deferred_cognition_failure_replaces_lost_turn_once() {
    let c = config();
    let mut e = Engine::default();
    observe(&mut e, &c, "one", json!({}));
    let failure = json!({"jobId":"one","sessionId":"one","inputEventId":"oversized",
        "requestEventId":"oversized","code":"resource-exhausted","deferred":true,
        "resource":{"resource":"memory","limitBytes":33554432,"phase":"delivery"},
        "effectStatus":"outcome-unknown"});
    let out = e.event(
        &c,
        "host-oom",
        "cognition.resource-exhausted",
        &failure,
        None,
    );
    let request = out
        .iter()
        .find(|d| d.kind == "model.requested")
        .expect("deferred job needs a bounded recovery turn");
    assert!(request.payload.to_string().contains("resource-exhausted"));
    assert_eq!(
        e.tasks["one"].context["resourceRecovery"]["remainingAttempts"],
        2
    );
    assert!(
        e.event(
            &c,
            "duplicate",
            "cognition.resource-exhausted",
            &failure,
            None
        )
        .is_empty()
    );
}

#[test]
fn resource_recovery_rejects_identical_cell_without_execution() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    resource_cell(&mut e, &c, &first, "cell");
    let source = e.tasks["one"].cell_source.clone();
    let retry = e
        .event(
            &c,
            "oom",
            "code.failed",
            &memory_failure("one", "cell"),
            None,
        )
        .into_iter()
        .find(|d| d.kind == "model.requested")
        .unwrap();
    let output = e.event(
        &c,
        "same",
        "model.completed",
        &completion(
            retry.payload["call_id"].as_str().unwrap(),
            json!([{"kind":"tool-call","name":"js","call_id":"again","arguments":{"code":source}}]),
        ),
        None,
    );
    assert!(!output.iter().any(|d| d.kind == "code.evaluate-requested"));
    assert!(output.iter().any(|d| d.kind == "model.requested"));
}

#[test]
fn unloaded_scheduler_entries_are_not_discarded() {
    let mut e = Engine::default();
    e.queue.push_back("unloaded-job".into());
    e.event(
        &config(),
        "unrelated",
        "memory.remembered",
        &json!({}),
        None,
    );
    assert_eq!(e.queue.front().map(String::as_str), Some("unloaded-job"));
}

#[test]
fn oversized_selected_state_recovers_without_losing_new_observation() {
    let c = config();
    let mut e = Engine::default();
    observe(&mut e, &c, "oversized", json!({}));
    e.queue_resource_error(
        "oversized",
        json!({"resource":"state","limitBytes":262144,"phase":"restore"}),
    );
    let out = observe(&mut e, &c, "new", json!({"conversationId":"separate"}));
    assert!(e.inbox.contains_key("new"));
    assert_eq!(e.tasks["oversized"].resource_failures, 1);
    assert!(out.iter().any(|d| d.kind == "model.requested"));
    assert_eq!(
        e.tasks["oversized"].context["resourceRecovery"]["sessionStatus"],
        "lost"
    );
}

#[test]
fn large_observation_is_referenced_in_bounded_durable_state() {
    let mut e = Engine::default();
    let out = observe(
        &mut e,
        &config(),
        "large",
        json!({"archive":"x".repeat(600_000)}),
    );
    assert!(out.iter().any(|d| d.kind == "model.requested"));
    assert!(crate::storage::records(&e).is_ok());
    assert_eq!(e.inbox["large"].value["archive"]["eventId"], "large");
}

#[test]
fn deferred_new_observation_is_admitted_once_with_resource_context() {
    let mut e = Engine::default();
    let c = config();
    let payload = json!({"inputEventId":"original","requestEventId":"original","deferred":true,
        "input":{"eventId":"original","eventType":"observation.received","sequence":1,
            "payload":{"provider":"telegram","externalSenderId":"u","conversationId":"c","message":{"text":"work"}}},
        "resource":{"resource":"memory","phase":"delivery","limitBytes":33554432}});
    let out = e.event(
        &c,
        "deferred",
        "cognition.resource-exhausted",
        &payload,
        None,
    );
    assert_eq!(
        out.iter().filter(|d| d.kind == "model.requested").count(),
        1
    );
    assert_eq!(e.tasks["original"].resource_failures, 1);
    assert!(
        e.event(
            &c,
            "duplicate",
            "cognition.resource-exhausted",
            &payload,
            None
        )
        .is_empty()
    );
}

#[test]
fn oversized_child_restore_preserves_scope_and_parent_yield() {
    let mut e = Engine::default();
    let c = config();
    observe(&mut e, &c, "root", json!({}));
    let summary = json!({"root":"root","origin":"root","parent":["root",7],"depth":1,
        "revision":2,"cell_revision":2,"window":{"after":4,"limit":8},
        "context":{"question":"Find the referenced fact"}});
    e.restore_resource_task(&c, "child", &summary);
    let task = &e.tasks["child"];
    assert_eq!(task.parent, Some(("root".into(), json!(7))));
    assert_eq!(
        task.window.as_ref().map(|w| (w.after, w.limit)),
        Some((4, 8))
    );
    assert_eq!(task.context["question"], "Find the referenced fact");
}

#[test]
fn unknown_resource_result_cannot_consume_a_jobs_retry_budget() {
    let c = config();
    let mut e = Engine::default();
    observe(&mut e, &c, "one", json!({}));
    let out = e.event(
        &c,
        "unknown",
        "code.failed",
        &memory_failure("one", "not-admitted"),
        None,
    );
    assert!(out.is_empty());
    assert_eq!(e.tasks["one"].resource_failures, 0);
}

#[test]
fn resource_failure_after_resume_reaches_the_current_cell() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    resource_cell(&mut e, &c, &first, "cell");
    e.event(
        &c,
        "resumed",
        "code.resumed",
        &json!({"sessionId":"one","response":{"id":1,"ok":true,"value":[]}}),
        None,
    );
    e.event(
        &c,
        "oom",
        "code.failed",
        &memory_failure("one", "resumed"),
        None,
    );
    assert_eq!(e.tasks["one"].resource_failures, 1);
}
