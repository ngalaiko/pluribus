use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fmt::Write as _,
    fs::{self, File},
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

struct Runner(Child);
impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn configured_file_archive_allows_concurrent_runners_without_its_source() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("plugin archive.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo");
    for entry in fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "plugin.toml" {
            // Subscribe the fixture to bypass the default cognition package.
            let manifest = fs::read_to_string(entry.path())
                .unwrap()
                .lines()
                .map(|line| {
                    if line.starts_with("subscribes =") {
                        "subscribes = [\"*\"]"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut header = tar::Header::new_ustar();
            header.set_size(manifest.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, &name, manifest.as_bytes())
                .unwrap();
        } else if entry.file_type().unwrap().is_dir() {
            tar.append_dir_all(&name, entry.path()).unwrap();
        } else {
            tar.append_path_with_name(entry.path(), &name).unwrap();
        }
    }
    tar.into_inner().unwrap().finish().unwrap();
    let mut hash = String::new();
    for byte in Sha256::digest(fs::read(&archive).unwrap()) {
        write!(&mut hash, "{byte:02x}").unwrap();
    }
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let mut config = json!({
        "agent_id": "test", "identity": "test", "model": "test",
        "maximum_blob_bytes": 1_048_576,
        "plugin_instances": {
            "echo": {"package": {"url": url::Url::from_file_path(&archive).unwrap().as_str(), "sha256": hash},
                "config": {}}
        }
    });
    let mut runners = Vec::new();
    for attempt in 0..2 {
        if attempt == 1 {
            fs::remove_file(&archive).unwrap();
            config["plugin_instances"]["echo"]["package"]["url"] =
                json!("https://example.invalid/elsewhere");
        }
        fs::write(
            data.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let output = temp.path().join(format!("run-{attempt}.log"));
        let errors = temp.path().join(format!("run-{attempt}.err"));
        let mut runner = Runner(
            Command::new(env!("CARGO_BIN_EXE_pluribus"))
                .args(["-d", data.to_str().unwrap(), "run", "--offline", "--resume"])
                .stdout(File::create(&output).unwrap())
                .stderr(File::create(&errors).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let log = fs::read_to_string(&output).unwrap();
            if log.contains("Running. Interrupt to stop.") {
                let logged = fs::read_to_string(&errors).unwrap();
                assert!(logged.contains("started component"), "{logged}");
                assert!(logged.contains("instance=echo"), "{logged}");
                break;
            }
            assert!(
                runner.0.try_wait().unwrap().is_none(),
                "runner exited before startup: {log} {}",
                fs::read_to_string(&errors).unwrap()
            );
            assert!(Instant::now() < deadline, "startup timed out: {log}");
            thread::sleep(Duration::from_millis(50));
        }
        runners.push(runner);
    }
    fs::write(data.join("STOP"), b"").unwrap();
    for mut runner in runners {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = runner.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "runner did not stop");
            thread::sleep(Duration::from_millis(50));
        }
    }
}
