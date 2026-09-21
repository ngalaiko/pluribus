use super::*;
use std::collections::BTreeMap;

#[test]
fn package_sources_require_explicit_url_schemes() {
    for value in [
        "builtin:telegram",
        "plugins/telegram/dist",
        "/plugins/telegram/dist",
        "http://example.org/plugin",
        "file:/plugins/telegram/dist",
        "file://remote/plugins/telegram/dist",
        "file:///plugins/telegram/dist?query",
        "file:///plugins/telegram/dist#fragment",
    ] {
        let source: PackageSource = serde_json::from_value(serde_json::json!(value)).unwrap();
        assert!(source.validate().is_err(), "accepted {value}");
    }
}

#[tokio::test]
async fn file_directory_url_resolves_without_a_package_root() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("a package");
    fs::create_dir(&directory).unwrap();
    let source: PackageSource = serde_json::from_value(serde_json::json!(
        Url::from_directory_path(&directory).unwrap().as_str()
    ))
    .unwrap();
    source.validate().unwrap();
    assert_eq!(source.resolve(temp.path(), true).await.unwrap(), directory);
}

#[test]
fn a_bundled_name_is_a_valid_reference_of_one_segment() {
    let source = bundled("openai-codex");
    source.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&source).unwrap(),
        "bundled:openai-codex"
    );
    let parsed: PackageSource =
        serde_json::from_value(serde_json::json!("bundled:openai-codex")).unwrap();
    parsed.validate().unwrap();
    for value in ["bundled:", "bundled:../etc", "bundled:a/b", "bundled:a b"] {
        let source: PackageSource = serde_json::from_value(serde_json::json!(value)).unwrap();
        assert!(source.validate().is_err(), "accepted {value}");
    }
}

fn fixture(root: &Path) -> ArchiveSource {
    package_archive(
        root,
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/memory"),
    )
}

fn package_archive(root: &Path, package: &Path) -> ArchiveSource {
    let archive = root.join("a package.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    tar.append_dir_all(".", package).unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    ArchiveSource {
        url: Url::from_file_path(&archive).unwrap().into(),
        sha256: hash(File::open(archive).unwrap()).unwrap(),
    }
}

#[tokio::test]
async fn file_archive_is_pinned_and_reusable_offline_from_another_url() {
    let temp = tempfile::tempdir().unwrap();
    let mut source = fixture(temp.path());
    let data = temp.path().join("data");
    let path = source.resolve(&data, true).await.unwrap();
    PluginPackage::load(&path).unwrap();
    fs::remove_file(Url::parse(&source.url).unwrap().to_file_path().unwrap()).unwrap();
    source.url = "https://example.invalid/any/layout/package".into();
    assert_eq!(source.resolve(&data, true).await.unwrap(), path);
    fs::write(path.join("config.schema.json"), b"{}").unwrap();
    assert!(
        source
            .resolve(&data, true)
            .await
            .unwrap_err()
            .to_string()
            .contains("differs")
    );
}

#[tokio::test]
async fn wrong_hash_never_installs() {
    let temp = tempfile::tempdir().unwrap();
    let mut source = fixture(temp.path());
    source.sha256 = "0".repeat(64);
    let data = temp.path().join("data");
    assert!(
        source
            .resolve(&data, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("SHA-256 mismatch")
    );
    assert!(!data.join("packages/sha256").join(&source.sha256).exists());
}

#[tokio::test]
async fn validates_url_and_hash_before_download() {
    for url in [
        "http://example.org/p",
        "ftp://example.org/p",
        "file://remote/path",
        "https://user:secret@example.org/p",
        "https://example.org/p#fragment",
    ] {
        assert!(
            ArchiveSource {
                url: url.into(),
                sha256: "a".repeat(64)
            }
            .parsed_url()
            .is_err(),
            "{url}"
        );
    }
    for sha256 in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
        assert!(
            ArchiveSource {
                url: "https://example.org/p".into(),
                sha256: sha256.into()
            }
            .parsed_url()
            .is_err()
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let source = ArchiveSource {
        url: "https://example.invalid/p".into(),
        sha256: "a".repeat(64),
    };
    assert!(
        source
            .resolve(temp.path(), true)
            .await
            .unwrap_err()
            .to_string()
            .contains("offline")
    );
}

#[test]
fn unsafe_paths_and_links_are_rejected() {
    for path in [
        "../outside",
        "/absolute",
        "a/../../outside",
        "C:/drive",
        "a\\b",
    ] {
        assert!(safe_path(Path::new(path)).is_err());
    }
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_link(&mut header, "link", "../../outside")
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let root = temp.path().join("out");
    fs::create_dir(&root).unwrap();
    assert!(
        unpack(&archive, &root, false)
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
}

#[test]
fn duplicate_entries_and_truncated_gzip_fail() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("bad.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    for _ in 0..2 {
        let mut header = tar::Header::new_ustar();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "same", &b"x"[..]).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    let out = temp.path().join("out");
    fs::create_dir(&out).unwrap();
    assert!(
        unpack(&archive, &out, false)
            .unwrap_err()
            .to_string()
            .contains("duplicate")
    );
    let source = fixture(temp.path());
    let file = Url::parse(&source.url).unwrap().to_file_path().unwrap();
    let f = fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_len(f.metadata().unwrap().len() - 5).unwrap();
    assert!(unpack(&file, &temp.path().join("truncated"), false).is_err());
}

#[tokio::test]
async fn concurrent_installs_share_one_verified_cache() {
    let temp = tempfile::tempdir().unwrap();
    let source = fixture(temp.path());
    let data = temp.path().join("data");
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let source = source.clone();
            let data = data.clone();
            tokio::spawn(async move { source.resolve(&data, true).await.unwrap() })
        })
        .collect();
    for worker in workers {
        assert!(worker.await.unwrap().is_dir());
    }
    assert_eq!(
        fs::read_dir(data.join("packages/sha256")).unwrap().count(),
        1
    );
}

fn configuration(source: &ArchiveSource) -> crate::Config {
    crate::Config {
        plugin_instances: BTreeMap::from([(
            "echo".into(),
            serde_json::from_value(serde_json::json!({
                "package": source, "config": {}, "components": {"main": {}}
            }))
            .unwrap(),
        )]),
        model_instance: None,
        ..crate::Config::default()
    }
}

#[tokio::test]
async fn package_preparation_does_not_create_metadata_locks() {
    let temp = tempfile::tempdir().unwrap();
    let source = fixture(temp.path());
    let data = temp.path().join("data");
    prepare(
        &crate::Paths::under(&data),
        &configuration(&source),
        true,
        false,
    )
    .await
    .unwrap();
    assert!(!data.join("plugins.lock").exists());
    assert!(!data.join("plugins.previous.lock").exists());
    assert!(!data.join("PACKAGES.lock").exists());
    assert!(!data.join("packages/locks").exists());
}

#[tokio::test]
async fn configuration_controls_package_selection_and_hash_validation() {
    let temp = tempfile::tempdir().unwrap();
    let source = fixture(temp.path());
    let data = temp.path().join("data");
    let mut config = configuration(&source);
    prepare(&crate::Paths::under(&data), &config, true, false)
        .await
        .unwrap();
    config.plugin_instances.get_mut("echo").unwrap().config = serde_json::json!({"invalid": true});
    assert!(
        prepare(&crate::Paths::under(&data), &config, true, false)
            .await
            .is_err()
    );
    config.plugin_instances.get_mut("echo").unwrap().config = serde_json::json!({});
    let mut moved = source.clone();
    moved.url = "https://example.invalid/a/different/layout".into();
    config.plugin_instances.get_mut("echo").unwrap().package = PackageSource::Archive(moved);
    prepare(&crate::Paths::under(&data), &config, true, false)
        .await
        .unwrap();
    let PackageSource::Archive(reference) =
        &mut config.plugin_instances.get_mut("echo").unwrap().package
    else {
        panic!()
    };
    reference.sha256 = "0".repeat(64);
    assert!(
        prepare(&crate::Paths::under(&data), &config, true, false)
            .await
            .is_err()
    );
}

fn changed_archive(root: &Path, original: &ArchiveSource, from: &str, to: &str) -> ArchiveSource {
    let path = root.join("changed.tar.gz");
    let input = File::open(Url::parse(&original.url).unwrap().to_file_path().unwrap()).unwrap();
    let mut input = tar::Archive::new(flate2::read::GzDecoder::new(input));
    let encoder =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::fast());
    let mut output = tar::Builder::new(encoder);
    for entry in input.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().into_owned();
        let mut header = entry.header().clone();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name.file_name().is_some_and(|name| name == "plugin.toml") {
            bytes = String::from_utf8(bytes)
                .unwrap()
                .replace(from, to)
                .into_bytes();
        }
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        output.append_data(&mut header, name, &bytes[..]).unwrap();
    }
    output.into_inner().unwrap().finish().unwrap();
    ArchiveSource {
        url: Url::from_file_path(&path).unwrap().into(),
        sha256: hash(File::open(path).unwrap()).unwrap(),
    }
}

#[tokio::test]
async fn major_updates_and_rollbacks_follow_source_and_hash() {
    let temp = tempfile::tempdir().unwrap();
    let source = fixture(temp.path());
    let data = temp.path().join("data");
    let config = configuration(&source);
    prepare(&crate::Paths::under(&data), &config, true, false)
        .await
        .unwrap();
    let update = changed_archive(
        temp.path(),
        &source,
        "manifest_version = 1",
        "manifest_version = 1\n# metadata-marker",
    );
    prepare(
        &crate::Paths::under(&data),
        &configuration(&update),
        true,
        false,
    )
    .await
    .unwrap();
    prepare(&crate::Paths::under(&data), &config, true, false)
        .await
        .unwrap();
}

#[test]
fn bounded_copy_rejects_oversize_streams() {
    let temp = tempfile::tempdir().unwrap();
    assert!(copy_archive(&b"123456"[..], &temp.path().join("archive"), 5).is_err());
}
/// Loopback HTTPS fixture. The adjacent key is public test data.
#[cfg(test)]
mod tls_fixture {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    /// A loopback endpoint and the certificate a client must trust to reach it.
    pub struct Fixture {
        pub port: u16,
        pub certificate: Vec<u8>,
    }

    /// Serves the routes the download checks exercise from a fresh self-signed
    /// certificate, so no key material is committed.
    pub async fn serve(payload: Vec<u8>) -> Fixture {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.subject_alt_names = vec![rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let certificate_der = CertificateDer::from(certificate.der().to_vec());

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate_der.clone()],
                PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let payload = payload.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut request = [0u8; 1024];
                    let Ok(read) = stream.read(&mut request).await else {
                        return;
                    };
                    let path = String::from_utf8_lossy(&request[..read])
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let _ = stream.write_all(&response(&path, &payload)).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Fixture {
            port,
            certificate: certificate_der.to_vec(),
        }
    }

    fn response(path: &str, payload: &[u8]) -> Vec<u8> {
        let redirect = |location: &str| {
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
                .into_bytes()
        };
        match path {
            "/redirect" => redirect("/archive"),
            "/loop" => redirect("/loop"),
            "/downgrade" => redirect("http://127.0.0.1:1/archive"),
            // A declared length far past the cap, with no body behind it.
            "/oversize" => b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n".to_vec(),
            "/archive" | "/truncated" => {
                let body = if path == "/truncated" {
                    &payload[..4]
                } else {
                    payload
                };
                let mut bytes = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    payload.len()
                )
                .into_bytes();
                bytes.extend_from_slice(body);
                bytes
            }
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        }
    }
}

#[tokio::test]
#[ignore = "binds loopback sockets"]
async fn https_download_checks_tls_redirects_status_and_size() {
    let temp = tempfile::tempdir().unwrap();
    let source = fixture(temp.path());
    let archive_path = Url::parse(&source.url).unwrap().to_file_path().unwrap();
    let endpoint = tls_fixture::serve(fs::read(&archive_path).unwrap()).await;

    let port = endpoint.port;
    let url = |path: &str| Url::parse(&format!("https://127.0.0.1:{port}{path}")).unwrap();
    let client = http_client()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_der(&endpoint.certificate).unwrap())
        .build()
        .unwrap();
    let archive = temp.path().join("download.tar.gz");
    download_https(&client, &url("/redirect"), &archive)
        .await
        .unwrap();
    verify_hash(&archive, &source.sha256).unwrap();
    let package = temp.path().join("package");
    fs::create_dir(&package).unwrap();
    unpack(&archive, &package, false).unwrap();
    PluginPackage::load(&package).unwrap();
    for path in ["/missing", "/oversize", "/truncated", "/loop", "/downgrade"] {
        assert!(
            download_https(&client, &url(path), &archive).await.is_err(),
            "{path}"
        );
    }
    assert!(
        download_https(
            &http_client().no_proxy().build().unwrap(),
            &url("/archive"),
            &archive
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn custom_cognition_named_rlm_keeps_its_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let source = package_archive(
        temp.path(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/shell"),
    );
    let source = changed_archive(
        temp.path(),
        &source,
        "[components.main]",
        "[components.main]\nsubscribes = [\"*\"]",
    );
    let variant = temp.path().join("variant");
    fs::create_dir(&variant).unwrap();
    let source = changed_archive(&variant, &source, "components.main", "components.cognition");
    let mut config = configuration(&source);
    let mut instance = config.plugin_instances.remove("echo").unwrap();
    let access = instance.overrides.remove("main").unwrap();
    instance.overrides.insert("cognition".into(), access);
    config.plugin_instances.insert("rlm".into(), instance);
    let resolved = prepare(
        &crate::Paths::under(&temp.path().join("data")),
        &config,
        true,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        resolved.plugin_instances["rlm"].config,
        serde_json::json!({})
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires loopback sockets"]
async fn stalled_download_yields_and_cancellation_disconnects() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/archive",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (started, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(socket.read_u8().await.unwrap());
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
            .await
            .unwrap();
        started.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("archive");
    let download = tokio::spawn(async move {
        download_https(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &url,
            &archive,
        )
        .await
        .map_err(|error| error.to_string())
    });
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!download.is_finished());
    download.abort();
    assert!(download.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn omitted_components_use_package_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let source = package_archive(
        temp.path(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo"),
    );
    let mut config = configuration(&source);
    let instance = serde_json::to_value(&config.plugin_instances["echo"]).unwrap();
    let mut instance = instance.as_object().unwrap().clone();
    instance.remove("components");
    config.plugin_instances.insert(
        "echo".into(),
        serde_json::from_value(serde_json::Value::Object(instance)).unwrap(),
    );
    config.capability_instances.push("echo".into());
    config.validate().unwrap();
    let resolved = prepare(
        &crate::Paths::under(&temp.path().join("data")),
        &config,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(
        resolved.plugin_instances["echo"]
            .components
            .contains_key("")
    );
    assert!(
        serde_json::to_value(&resolved.plugin_instances["echo"])
            .unwrap()
            .get("components")
            .is_none()
    );
}

#[tokio::test]
async fn manifest_access_and_credentials_are_defaults_not_saved_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let source = package_archive(
        temp.path(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex"),
    );
    let mut config = configuration(&source);
    config.plugin_instances.insert(
        "echo".into(),
        serde_json::from_value(serde_json::json!({
            "package": source, "components": {"main": {"http": {"max_timeout_ms": 12000}}}
        }))
        .unwrap(),
    );
    let resolved = prepare(
        &crate::Paths::under(&temp.path().join("data")),
        &config,
        true,
        false,
    )
    .await
    .unwrap();
    let instance = &resolved.plugin_instances["echo"];
    assert_eq!(instance.components["main"].http.max_timeout_ms, 12000);
    assert!(
        instance.components["main"]
            .http
            .origins
            .contains(&"https://chatgpt.com".into())
    );
    assert!(
        serde_json::to_value(instance)
            .unwrap()
            .get("config")
            .is_none()
    );
    assert_eq!(
        instance.config["credentials"]["subscription"],
        "echo:subscription"
    );
    assert_eq!(
        instance.config["models"],
        serde_json::json!(["gpt-5.6-luna"])
    );
    assert_eq!(
        serde_json::to_value(instance).unwrap()["components"],
        serde_json::json!({"main":{"http":{"max_timeout_ms":12000}}})
    );

    config.plugin_instances.insert(
        "echo".into(),
        serde_json::from_value(serde_json::json!({
            "package": source, "components": {"main": {"http": {"origins": []}}}
        }))
        .unwrap(),
    );
    let resolved = prepare(
        &crate::Paths::under(&temp.path().join("other")),
        &config,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(
        resolved.plugin_instances["echo"].components["main"]
            .http
            .origins
            .is_empty()
    );
}

#[tokio::test]
async fn single_component_access_needs_no_component_key() {
    let temp = tempfile::tempdir().unwrap();
    let source = package_archive(
        temp.path(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo"),
    );
    let mut config = configuration(&source);
    config.plugin_instances.insert(
        "echo".into(),
        serde_json::from_value(serde_json::json!({
            "package": source, "access": {"limits": {"call_timeout_ms": 5000}}
        }))
        .unwrap(),
    );
    let resolved = prepare(
        &crate::Paths::under(&temp.path().join("data")),
        &config,
        true,
        false,
    )
    .await
    .unwrap();
    assert_eq!(resolved.component("echo").unwrap().0, "echo");
    assert_eq!(
        resolved.plugin_instances["echo"].components[""]
            .limits
            .unwrap()
            .call_timeout_ms,
        5000
    );
    let saved = serde_json::to_value(&resolved.plugin_instances["echo"]).unwrap();
    assert!(saved.get("components").is_none());
    assert_eq!(
        saved["access"],
        serde_json::json!({"limits":{"call_timeout_ms":5000}})
    );
}

#[tokio::test]
async fn assembling_an_agent_needs_only_package_sources() {
    let temp = tempfile::tempdir().unwrap();
    let packages = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    let instances = ["openai-codex", "rlm", "telegram"].into_iter().map(|name| {
        (name.to_owned(), serde_json::json!({"package": Url::from_directory_path(packages.join(name)).unwrap().as_str()}))
    }).collect::<serde_json::Map<_, _>>();
    let config: crate::Config =
        serde_json::from_value(serde_json::json!({"plugin_instances":instances})).unwrap();
    let resolved = prepare(&crate::Paths::under(temp.path()), &config, true, true)
        .await
        .unwrap();
    assert_eq!(resolved.plugin_instances["rlm"].components.len(), 2);
    assert!(
        resolved.plugin_instances["rlm"].config["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "telegram.reply")
    );
    assert_eq!(
        resolved.plugin_instances["telegram"].config["credentials"]["bot-token"],
        "telegram:bot-token"
    );
    for instance in resolved.plugin_instances.values() {
        assert_eq!(
            serde_json::to_value(instance)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }
}
