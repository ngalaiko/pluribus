fn turn(request: &Draft) -> Value {
    serde_json::from_str(
        request.payload["messages"][1]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn first_turn_exposes_input_tools_and_job_without_js() {
    let mut c = config();
    c.tools = vec![
        json!({"name":"memory.recall","parameters":{"required":["scope","query"]},"configuredConstraints":{"scopes":["agent:personal"]}}),
    ];
    let mut e = Engine::default();
    let request = observe(
        &mut e,
        &c,
        "one",
        json!({"message":{"chat":{"id":1},"text":"What is my secret word?"}}),
    )
    .remove(0);
    let value = turn(&request);
    assert_eq!(value["input"]["message"]["text"], "What is my secret word?");
    assert_eq!(value["tools"], json!(c.tools));
    assert_eq!(value["job"]["objective"], "What is my secret word?");
    assert_eq!(value["trigger"]["kind"], "observation");
    assert_eq!(value, e.tasks["one"].context["turn"]);
}

#[test]
fn child_turn_exposes_selected_data_but_no_root_capabilities() {
    let mut e = Engine::default();
    let c = config();
    e.start(
        &c,
        "child",
        "root",
        "origin",
        json!({"question":"Sum these","context":{"rows":[1,2,3]}}),
        Some(("root".into(), json!("yield"))),
        1,
    );
    e.budgets.insert("root".into(), 0);
    e.scheduling = true;
    let request = e.request(&c, "child", "cause").remove(0);
    let value = turn(&request);
    assert_eq!(value["question"], "Sum these");
    assert_eq!(value["context"]["rows"], json!([1, 2, 3]));
    assert!(value.get("tools").is_none());
    assert_eq!(value, e.tasks["child"].context["turn"]);
}

#[test]
fn correction_turn_exposes_reason_and_preserves_feedback() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    let out = e.event(
        &c,
        "bad",
        "model.completed",
        &completion(
            first.payload["call_id"].as_str().unwrap(),
            json!([{"kind":"text","text":"done"}]),
        ),
        None,
    );
    let request = out.iter().find(|d| d.kind == "model.requested").unwrap();
    assert_eq!(turn(request)["trigger"]["kind"], "correction");
    assert_eq!(turn(request)["trigger"]["attempt"], 1);
    assert!(request.payload["messages"].as_array().unwrap().len() > 2);
}

#[test]
fn routing_and_amendment_turns_share_the_envelope() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let routing = observe(
        &mut e,
        &c,
        "two",
        json!({"message":{"chat":{"id":1},"text":"Use repository X"}}),
    )
    .remove(0);
    assert_eq!(turn(&routing)["trigger"]["kind"], "routing");
    assert_eq!(turn(&routing)["observation"]["text"], "Use repository X");
    assert_eq!(turn(&routing), e.tasks["associate:two"].context["turn"]);
    let out = answer(&mut e, &c, &routing, r#"{"action":"amend","jobId":"one"}"#);
    let amended = out.iter().find(|d| d.kind == "model.requested").unwrap();
    assert_eq!(turn(amended)["trigger"]["kind"], "amendment");
    assert_eq!(turn(amended)["message"], "Use repository X");
    assert_eq!(turn(amended)["job"]["revision"], 1);
}

#[test]
fn scheduled_and_retry_turns_explain_the_wake_after_restore() {
    for retry in [false, true] {
        let mut e = Engine::default();
        let c = config();
        let first = observe(&mut e, &c, "one", json!({})).remove(0);
        if retry {
            e.event(
                &c,
                "failed",
                "model.failed",
                &json!({"call_id":first.payload["call_id"],"code":"temporary"}),
                None,
            );
        } else {
            answer(
                &mut e,
                &c,
                &first,
                r#"{"transition":"wait","dueAtMs":60000,"nextStep":"Check progress"}"#,
            );
        }
        e.event(
            &c,
            "timer",
            "timer.set",
            &json!({"jobId":"one","wakeToken":e.jobs["one"].wake.as_ref().unwrap().token}),
            None,
        );
        let mut e: Engine = serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap();
        e.now_ms = 60000;
        let out = e.event(
            &c,
            "wake",
            "timer.fired",
            &json!({"requestEventId":"timer"}),
            None,
        );
        let request = out.iter().find(|d| d.kind == "model.requested").unwrap();
        let value = turn(request);
        assert_eq!(
            value["trigger"]["kind"],
            if retry { "retry" } else { "scheduled-wake" }
        );
        assert_eq!(value["trigger"]["scheduled"]["due_at_ms"], 60000);
        assert_eq!(value["nowMs"], 60000);
        assert_eq!(value, e.tasks["one"].context["turn"]);
    }
}

#[test]
fn tool_result_turn_preserves_native_result_without_copying_it_into_envelope() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({})).remove(0);
    e.event(&c,"model","model.completed",&completion(first.payload["call_id"].as_str().unwrap(),json!([{"kind":"tool-call","name":"js","call_id":"js","arguments":{"code":"return 42"}}])),None);
    let out = e.event(
        &c,
        "cell",
        "code.completed",
        &json!({"sessionId":"one","value":42}),
        None,
    );
    let request = out.iter().find(|d| d.kind == "model.requested").unwrap();
    assert_eq!(turn(request)["trigger"]["kind"], "tool-result");
    assert_eq!(
        request.payload["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"][0]["output"]["value"],
        42
    );
    assert!(!turn(request).to_string().contains("42"));
}

#[test]
fn large_transport_data_stays_referenced_without_hiding_message() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(&mut e, &c, "one", json!({"rawMetadata":"x".repeat(100000)})).remove(0);
    let value = turn(&first);
    assert_eq!(value["message"], "work");
    assert_eq!(value["input"]["contextPointer"], "/observation");
    assert_eq!(
        e.tasks["one"]
            .context
            .pointer(value["input"]["contextPointer"].as_str().unwrap())
            .unwrap()["rawMetadata"]
            .as_str()
            .unwrap()
            .len(),
        100000
    );
    assert!(value.to_string().len() < 26000);
}

#[test]
fn new_turn_includes_recent_same_conversation_exchange_only() {
    let mut e = Engine::default();
    let c = config();
    let first = observe(
        &mut e,
        &c,
        "one",
        json!({"message":{"chat":{"id":1},"text":"Remember blue"}}),
    )
    .remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"complete","reply":"Saved blue"}"#,
    );
    let other=observe(&mut e,&c,"other",json!({"provider":"telegram","externalSenderId":"8","conversationId":"chat:2","message":{"chat":{"id":2},"text":"private other conversation"}})).remove(0);
    answer(
        &mut e,
        &c,
        &other,
        r#"{"transition":"complete","reply":"Other reply"}"#,
    );
    let next = observe(
        &mut e,
        &c,
        "two",
        json!({"message":{"chat":{"id":1},"text":"What color?"}}),
    )
    .remove(0);
    let recent = turn(&next)["recentConversation"].clone();
    assert!(recent.to_string().contains("Saved blue"));
    assert!(!recent.to_string().contains("Other reply"));
    assert!(!recent.to_string().contains("What color?"));
}

#[test]
fn metadata_cannot_crowd_out_capability_schemas() {
    let mut e = Engine::default();
    let mut c = config();
    c.tools = vec![json!({"name":"memory.recall","parameters":{"description":"x".repeat(5000)}})];
    let first = observe(&mut e, &c, "one", json!({"rawMetadata":"x".repeat(22000)})).remove(0);
    assert_eq!(turn(&first)["tools"], json!(c.tools));
    assert_eq!(turn(&first)["input"]["contextPointer"], "/observation");
}

#[test]
fn router_never_receives_js_only_references() {
    let mut e = Engine::default();
    let c = config();
    e.start(&c, "route", "route", "origin", json!({}), None, 0);
    let task = e.tasks.get_mut("route").unwrap();
    task.association = Some("origin".into());
    task.context["routing"] = json!({"observation":{"text":"Which one?"},"jobs":[{"objective":"x".repeat(30000)}],"recentClarifications":[]});
    assert!(turn_context(task, 0)["jobs"].is_array());
}

#[test]
fn history_uses_conversation_identity_without_provider_thread_fields() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", json!({"message":{"text":"previous", "message_thread_id":1}})).remove(0);
    answer(&mut e, &c, &first, r#"{"transition":"complete","reply":"done"}"#);
    let next = observe(&mut e, &c, "two", json!({"message":{"text":"next", "message_thread_id":2}})).remove(0);
    assert_eq!(turn(&next)["recentConversation"]["entries"][0]["message"], "previous");
}

fn attachment(kind: &str, status: &str, media_type: &str, size: u64) -> Value {
    json!({"kind":kind,"status":status,"fileName":"attachment","metadata":{},
        "blob":{"algorithm":"sha256","digest":"a".repeat(64),"size":size,"mediaType":media_type}})
}

#[test]
fn photo_caption_and_ready_images_reach_the_model() {
    let mut e = Engine::default();
    let c = config();
    let request = observe(
        &mut e,
        &c,
        "one",
        json!({
            "message":{"chat":{"id":1},"caption":"what is this"},
            "media":[
                attachment("photo", "ready", "image/jpeg", 11),
                attachment("photo", "pending", "image/jpeg", 12),
                attachment("photo", "failed", "image/jpeg", 13),
                attachment("document", "ready", "application/pdf", 14),
                attachment("photo", "ready", "image/png", 20 * 1024 * 1024 + 1),
            ],
        }),
    )
    .remove(0);
    let content = request.payload["messages"][1]["content"]
        .as_array()
        .unwrap();
    assert_eq!(turn(&request)["message"], "what is this");
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[1],
        json!({"kind":"image","blob":{"algorithm":"sha256","digest":"a".repeat(64),"size":11,"media_type":"image/jpeg"}})
    );
    assert_eq!(request.payload["required_features"], json!(["vision"]));
    assert_eq!(e.jobs["one"].objective, "what is this");
}

#[test]
fn attached_images_are_capped_per_turn() {
    let mut e = Engine::default();
    let c = config();
    let media: Vec<Value> = (0..MAX_TURN_IMAGES + 3)
        .map(|_| attachment("photo", "ready", "image/png", 11))
        .collect();
    let request = observe(
        &mut e,
        &c,
        "one",
        json!({"message":{"chat":{"id":1},"caption":"many"},"media":media}),
    )
    .remove(0);
    assert_eq!(
        request.payload["messages"][1]["content"]
            .as_array()
            .unwrap()
            .len(),
        MAX_TURN_IMAGES + 1
    );
}

#[test]
fn a_text_observation_requests_no_vision() {
    let mut e = Engine::default();
    let c = config();
    let request = observe(&mut e, &c, "one", json!({})).remove(0);
    assert_eq!(
        request.payload["messages"][1]["content"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(request.payload.get("required_features").is_none());
}
