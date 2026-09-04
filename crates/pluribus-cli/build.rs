use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=PLURIBUS_RELEASE_CATALOG");
    let catalog = if let Some(path) = env::var_os("PLURIBUS_RELEASE_CATALOG") {
        let path = PathBuf::from(path)
            .canonicalize()
            .expect("release catalog path");
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(path).expect("release catalog")
    } else {
        "{\"version\":1,\"plugins\":{}}".into()
    };
    let value: serde_json::Value =
        serde_json::from_str(&catalog).expect("valid release catalog JSON");
    assert_eq!(value["version"], 1, "release catalog version");
    let plugins = value["plugins"]
        .as_object()
        .expect("release catalog plugins");
    if !plugins.is_empty() {
        for name in [
            "memory",
            "openai-codex",
            "openrouter",
            "rlm",
            "shell",
            "telegram",
        ] {
            let source = plugins[name]
                .as_object()
                .expect("plugin URL/hash reference");
            assert_eq!(source.len(), 2, "plugin reference fields");
            let url = source["url"].as_str().expect("plugin URL");
            assert!(
                url.starts_with("https://") || url.starts_with("file://"),
                "plugin URL scheme"
            );
            let hash = source["sha256"].as_str().expect("plugin hash");
            assert!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "plugin hash"
            );
        }
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("plugins.json"),
        catalog,
    )
    .expect("write embedded release catalog");
}
