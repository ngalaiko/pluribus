fn topic_observation(topic: i64) -> Value {
    json!({"provider":"telegram","externalSenderId":"7","conversationId":format!("chat:1:thread:{topic}"),"message":{"chat":{"id":1},"message_thread_id":topic,"text":"work"}})
}

#[test]
fn conversation_boundaries_isolate_routing_and_explicit_amendments() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", topic_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    assert_eq!(
        e.routing_context(&c, &topic_observation(10))["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (index, mut foreign) in [topic_observation(20), topic_observation(10)]
        .into_iter()
        .enumerate()
    {
        if foreign["message"]["message_thread_id"] == 10 {
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
fn conversation_clarifications_stay_in_their_topic() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", topic_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "question", topic_observation(10)).remove(0);
    let out = answer(&mut e, &c, &classify, r#"{"action":"clarify"}"#);
    assert_eq!(
        out[0].payload["arguments"]["conversationId"],
        "chat:1:thread:10"
    );
    assert_eq!(
        e.routing_context(&c, &topic_observation(10))["recentClarifications"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        e.routing_context(&c, &topic_observation(20))["recentClarifications"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn conversation_rejects_foreign_clarification_resolution() {
    let c = config();
    let mut e = Engine::default();
    let first = observe(&mut e, &c, "one", topic_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &first,
        r#"{"transition":"wait","waitFor":"input"}"#,
    );
    let classify = observe(&mut e, &c, "question", topic_observation(10)).remove(0);
    answer(&mut e, &c, &classify, r#"{"action":"clarify"}"#);
    let mut foreign = e.inbox["question"].clone();
    foreign.id = "foreign-question".into();
    foreign.value = topic_observation(20);
    e.inbox.insert(foreign.id.clone(), foreign);
    let classify = observe(&mut e, &c, "response", topic_observation(10)).remove(0);
    answer(
        &mut e,
        &c,
        &classify,
        r#"{"action":"cancel","jobId":"one","resolvesObservationId":"foreign-question"}"#,
    );
    assert_ne!(e.jobs["one"].status, "cancelled");
    assert_eq!(e.inbox["foreign-question"].status, "waiting-input");
}
