use serde_json::{Value, json};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

pub fn run(mut body: Value) -> Value {
    let command = std::env::var("PLURIBUS_MEMORY_EVAL_PROVIDER_CMD").unwrap();
    body["model"] =
        json!(std::env::var("PLURIBUS_MEMORY_EVAL_MODEL").expect("set PLURIBUS_MEMORY_EVAL_MODEL"));
    let call_id = body["call_id"].clone();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        serde_json::to_writer(&mut input, &body).unwrap();
        input.write_all(b"\n").unwrap();
    });
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |stream: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stream.take(1_048_577).read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("provider bridge exceeded 120 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    writer.join().unwrap();
    let output = out.join().unwrap();
    let error = err.join().unwrap();
    assert!(
        status.success(),
        "provider bridge failed: {}",
        String::from_utf8_lossy(&error)
    );
    assert!(
        output.len() <= 1_048_576,
        "provider completion exceeds 1 MiB"
    );
    let completion: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        completion["call_id"], call_id,
        "provider returned wrong call_id"
    );
    let _: pluribus_model::Completion =
        serde_json::from_value(completion.clone()).expect("canonical provider completion");
    completion
}
