use serde_json::json;
use std::{fs, process::Command};

#[test]
fn list_reads_configured_instances_without_resolving_packages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let config = serde_json::to_vec(&json!({
        "plugin_instances": {
            "work": {"package": "file:///missing/work"},
            "personal": {"package": "bundled:telegram"}
        }
    }))
    .unwrap();
    fs::write(&path, &config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pluribus"))
        .arg("--config-dir")
        .arg(directory.path())
        .args(["plugins", "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("personal\tbundled:telegram"), "{stdout}");
    assert!(stdout.contains("work\tfile:///missing/work"), "{stdout}");
    assert_eq!(fs::read(path).unwrap(), config);
}
