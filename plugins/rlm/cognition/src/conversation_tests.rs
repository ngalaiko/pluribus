fn conversation_observation(conversation: i64) -> Value {
    json!({"provider":"telegram","externalSenderId":"7","conversationId":format!("conversation:{conversation}"),"message":{"text":"work"}})
}

#[test]
fn conversation_boundaries_isolate_routing_and_explicit_amendments() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", conversation_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    assert_eq!(
        e.routing_context(&c, &conversation_observation(10))["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (index, mut foreign) in [conversation_observation(20), conversation_observation(10)]
        .into_iter()
        .enumerate()
    {
        if foreign["conversationId"] == "conversation:10" {
            foreign["conversationId"] = json!("different-conversation");
        }
        assert!(
            e.routing_context(&c, &foreign)["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        foreign["jobId"] = json!("one");
        observe(&mut e, &c, &format!("foreign-{index}"), foreign);
        assert_eq!(e.jobs["one"].revision, 0);
    }
}

#[test]
fn conversation_clarifications_stay_in_their_conversation() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", conversation_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "question", conversation_observation(10)).remove(0);
    let out = answer(&mut e, &c, &classify, r#"{"action":"clarify"}"#);
    assert_eq!(
        out[0].payload["arguments"]["conversationId"],
        "conversation:10"
    );
    assert_eq!(
        e.routing_context(&c, &conversation_observation(10))["recentClarifications"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        e.routing_context(&c, &conversation_observation(20))["recentClarifications"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn conversation_rejects_foreign_clarification_resolution() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", conversation_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "question", conversation_observation(10)).remove(0);
    answer(&mut e, &c, &classify, r#"{"action":"clarify"}"#);
    let mut foreign = e.inbox["question"].clone();
    foreign.id = "foreign-question".into();
    foreign.value = conversation_observation(20);
    e.inbox.insert(foreign.id.clone(), foreign);
    let classify = observe(&mut e, &c, "response", conversation_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &classify,
        r#"{"action":"cancel","jobId":"one","resolvesObservationId":"foreign-question"}"#,
    );
    assert_ne!(e.jobs["one"].status, "cancelled");
    assert_eq!(e.inbox["foreign-question"].status, "waiting-input");
}
