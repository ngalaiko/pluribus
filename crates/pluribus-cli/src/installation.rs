use pluribus_plugin_package::PluginPackage;
use serde_json::{Value, json};
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn development_root(executable: &Path) -> Option<PathBuf> {
    let parent = executable.parent()?;
    let profile = if parent.file_name()? == "deps" {
        parent.parent()?
    } else {
        parent
    };
    if !["debug", "release"]
        .iter()
        .any(|name| profile.file_name().is_some_and(|part| part == *name))
    {
        return None;
    }
    let target = profile
        .ancestors()
        .skip(1)
        .take(2)
        .find(|path| path.file_name().is_some_and(|part| part == "target"))?;
    let root = target.parent()?;
    (root.join("Cargo.toml").is_file() && root.join("rust-toolchain.toml").is_file())
        .then(|| root.to_path_buf())
}

fn root_for(executable: &Path, override_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = override_root {
        if !root.is_dir() {
            return Err(
                format!("PLURIBUS_PLUGIN_DIR is not a directory: {}", root.display()).into(),
            );
        }
        return Ok(root.canonicalize()?);
    }
    let root = executable
        .parent()
        .and_then(Path::parent)
        .map(|prefix| prefix.join("share/pluribus/plugins"));
    if let Some(root) = root.filter(|root| root.is_dir()) {
        return Ok(root);
    }
    if let Some(root) = development_root(executable) {
        return Ok(root.join("target/plugins"));
    }
    Err("bundled plugins not found; install the complete Pluribus distribution or set PLURIBUS_PLUGIN_DIR".into())
}

pub fn package_root() -> Result<PathBuf> {
    root_for(
        &env::current_exe()?.canonicalize()?,
        env::var_os("PLURIBUS_PLUGIN_DIR").map(PathBuf::from),
    )
}

/// The package directories an installation carries, sorted.
pub fn installed(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// An instance for `package`, with the access its manifest asks for.
///
/// HTTP origins and methods come from the component's requested capabilities.
/// A component wanting an endpoint gets one in the data directory, answered by
/// this account: the bridge or executor a person starts runs as them.
pub(crate) fn instance(
    package: &PluginPackage,
    id: &str,
    runtime: &Path,
    source: &crate::package_source::PackageSource,
) -> Value {
    let mut components = serde_json::Map::new();
    for (name, component) in package.components() {
        let manifest = component.manifest();
        let mut access = serde_json::Map::new();
        let mut streams = serde_json::Map::new();
        for requested in &manifest.requested_capabilities {
            match requested.name.as_str() {
                "net.http" | "host.http" => {
                    // A model provider streams completions, so it needs more
                    // room and time than an ordinary call.
                    let (bytes, timeout) = if manifest.model_provider.is_some() {
                        (16 * 1024 * 1024, 300_000)
                    } else {
                        (1024 * 1024, 30_000)
                    };
                    access.insert(
                        "http".into(),
                        json!({
                            "origins": requested.constraints["origins"],
                            "methods": requested.constraints["methods"],
                            "max_request_bytes": requested.constraints["max_request_bytes"].as_u64().unwrap_or(bytes),
                            "max_response_bytes": requested.constraints["max_response_bytes"].as_u64().unwrap_or(bytes),
                            "max_timeout_ms": requested.constraints["max_timeout_ms"].as_u64().unwrap_or(timeout),
                        }),
                    );
                }
                "net.tls" => {
                    // The plugin names an endpoint, not a destination: the
                    // name selects among what the operator granted.
                    //
                    // A remote endpoint is usually a session held open for
                    // hours, so its byte budget is closer to a connection
                    // lifetime than to a per-exchange ceiling: exhausting it
                    // ends the connection and the plugin reconnects. The
                    // local-socket default is far too small for that.
                    let mut tls = json!({
                        "hostname": requested.constraints["hostname"],
                        "port": requested.constraints["port"],
                    });
                    if let Some(preamble) = requested.constraints.get("starttls") {
                        tls["starttls"] = preamble.clone();
                    }
                    streams.insert(
                        endpoint_name(requested),
                        json!({
                            "tls": tls,
                            "max_bytes": requested.constraints["max_bytes"].as_u64().unwrap_or(1024 * 1024 * 1024),
                            "max_timeout_ms": requested.constraints["max_timeout_ms"].as_u64().unwrap_or(15_000),
                            // One session per endpoint. A component that
                            // wants more must say so in its manifest.
                            "max_connections": requested.constraints["max_connections"].as_u64().unwrap_or(1),
                        }),
                    );
                }
                "host.stream" => {
                    let socket = match (name.is_empty(), endpoint_name(requested).as_str()) {
                        (true, "default") => format!("{id}.sock"),
                        (false, "default") => format!("{id}-{name}.sock"),
                        (true, endpoint) => format!("{id}-{endpoint}.sock"),
                        (false, endpoint) => format!("{id}-{name}-{endpoint}.sock"),
                    };
                    streams.insert(
                        endpoint_name(requested),
                        json!({
                            "socket": runtime.join(socket),
                            "peer_uids": [own_uid()],
                        }),
                    );
                }
                _ => {}
            }
        }
        if !streams.is_empty() {
            access.insert("stream".into(), Value::Object(streams));
        }
        components.insert(name.clone(), Value::Object(access));
    }
    json!({
        "package": source,
        "config": {},
        "components": components,
    })
}

pub(crate) fn resolve_instance(
    package: &PluginPackage,
    id: &str,
    runtime: &Path,
    instance: &mut crate::registry::PluginInstance,
) -> Result<()> {
    let runtime = std::path::absolute(runtime)?;
    let defaults = self::instance(package, id, &runtime, &instance.package);
    let mut access = defaults["components"].clone();
    if let Some(overrides) = &instance.access {
        if package.components().len() != 1 || !instance.overrides.is_empty() {
            return Err("access requires one component and no components overrides".into());
        }
        let name = package.components().keys().next().unwrap();
        pluribus_plugin_package::merge_config(&mut access[name], overrides);
    }
    for (name, overrides) in &instance.overrides {
        let Some(base) = access.get_mut(name) else {
            return Err(format!("unknown component: {id}/{name}").into());
        };
        pluribus_plugin_package::merge_config(base, overrides);
    }
    instance.components = serde_json::from_value(access)?;
    instance.config = package.resolve_config(&instance.config);
    if !package.manifest().credentials.is_empty() {
        let credentials = instance
            .config
            .as_object_mut()
            .ok_or("plugin config must be an object")?
            .entry("credentials")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("credentials must be an object")?;
        for credential in &package.manifest().credentials {
            credentials
                .entry(credential.id.clone())
                .or_insert_with(|| json!(format!("{id}:{}", credential.id)));
        }
    }
    Ok(())
}

/// The name `socket.connect` selects this endpoint with. A manifest asking
/// for one endpoint need not name it; `default` is what the guest then passes.
fn endpoint_name(requested: &pluribus_plugin_package::RequestedCapability) -> String {
    requested.constraints["name"]
        .as_str()
        .unwrap_or("default")
        .to_owned()
}

fn own_uid() -> u32 {
    // The endpoint a person starts answers as that person.
    rustix::process::geteuid().as_raw()
}

/// Adds an instance for the package at `reference`: a package directory, or a
/// pinned archive with `--sha256`.
pub async fn install(
    data: &crate::Paths,
    reference: &str,
    sha256: Option<String>,
    id: Option<String>,
) -> Result<()> {
    add(data, reference_source(reference, sha256).await?, id).await
}

/// Adds an instance for a package this build ships, referenced by name so an
/// upgrade that replaces the installation keeps resolving it.
pub async fn install_named(data: &crate::Paths, name: &str) -> Result<()> {
    add(
        data,
        crate::package_source::bundled(name),
        Some(name.to_owned()),
    )
    .await
}

async fn add(
    data: &crate::Paths,
    source: crate::package_source::PackageSource,
    id: Option<String>,
) -> Result<()> {
    let resolved = source.resolve(&data.cache, false).await?;
    let package = PluginPackage::load(&resolved)?;
    let manifest_id = &package.manifest().id;
    let id = id.unwrap_or_else(|| {
        manifest_id
            .rsplit('.')
            .next()
            .unwrap_or(manifest_id)
            .to_owned()
    });
    let mut config = crate::load_config(data)?;
    if config.plugin_instances.contains_key(&id) {
        return Err(format!("instance {id} is already configured").into());
    }
    let value = json!({"package": source});
    config
        .plugin_instances
        .insert(id.clone(), serde_json::from_value(value)?);
    config.validate()?;
    crate::save_config(data, &config)?;
    tracing::info!(instance = %id, plugin = %package.manifest().id, "configured instance");
    println!("Installed {id}.");
    for credential in &package.manifest().credentials {
        println!(
            "  Authorize {} with `pluribus{} plugins auth {id}`.",
            credential.display_name,
            data.cli_options()
        );
    }
    Ok(())
}

/// What the operator named: a package directory, or an archive. An archive
/// without a digest is pinned to what arrives, and every later fetch checks it.
async fn reference_source(
    reference: &str,
    sha256: Option<String>,
) -> Result<crate::package_source::PackageSource> {
    if let Some(sha256) = sha256 {
        return Ok(crate::package_source::PackageSource::Archive(
            crate::package_source::ArchiveSource {
                url: reference.to_owned(),
                sha256,
            },
        ));
    }
    if reference.ends_with(".tar.gz") {
        let pinned = crate::package_source::pin(reference).await?;
        println!("Pinned {reference} to sha256:{}.", pinned.sha256);
        return Ok(crate::package_source::PackageSource::Archive(pinned));
    }
    if reference.starts_with("file://") {
        return Ok(crate::package_source::PackageSource::File(
            reference.to_owned(),
        ));
    }
    Ok(Path::new(reference).canonicalize()?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_slots_preserve_bindings_and_default_missing_handles() {
        let package = PluginPackage::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        for (config, expected) in [
            (json!({}), "personal:bot-token"),
            (
                json!({"credentials":{"bot-token":"shared:telegram"}}),
                "shared:telegram",
            ),
        ] {
            let mut instance = serde_json::from_value(json!({
                "package":"bundled:telegram", "config":config
            }))
            .unwrap();
            resolve_instance(&package, "personal", root.path(), &mut instance).unwrap();
            assert_eq!(instance.config["credentials"]["bot-token"], expected);
        }
        let mut instance = serde_json::from_value(json!({
            "package":"bundled:telegram", "config":{"credentials":"invalid"}
        }))
        .unwrap();
        assert!(resolve_instance(&package, "personal", root.path(), &mut instance).is_err());
    }

    fn load(name: &str) -> PluginPackage {
        PluginPackage::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/plugins")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn a_tls_request_installs_every_endpoint_its_manifest_names() {
        let package = load("email");
        assert_eq!(
            package
                .components()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [""]
        );
        assert_eq!(package.manifest().credentials[0].components, [""]);
        let root = tempfile::tempdir().unwrap();
        let mut instance = serde_json::from_value(json!({
            "package":"bundled:email", "config":{}
        }))
        .unwrap();
        resolve_instance(&package, "mail", root.path(), &mut instance).unwrap();
        let streams = &instance.components[""].stream;
        assert_eq!(streams.keys().collect::<Vec<_>>(), ["imap", "smtp"]);

        let imap = &streams["imap"];
        let tls = imap.tls.as_ref().unwrap();
        assert_eq!(tls.hostname, "imap.mail.me.com");
        assert_eq!(tls.port, 993);
        assert!(!tls.allow_private_network);
        assert!(tls.starttls.is_none());
        // A session held open for hours needs a budget sized as a connection
        // lifetime, not as one request/response exchange.
        assert_eq!(imap.max_bytes, 1024 * 1024 * 1024);
        assert_eq!(imap.max_connections, 1);
        // The plugin names no destination, so no local socket is provisioned.
        assert!(imap.socket.is_none());
        assert!(imap.peer_uids.is_empty());
        imap.validate().unwrap();

        // Submission upgrades from plaintext, and the host owns the upgrade.
        let smtp = &streams["smtp"];
        let tls = smtp.tls.as_ref().unwrap();
        assert_eq!(tls.hostname, "smtp.mail.me.com");
        assert_eq!(tls.port, 587);
        assert!(matches!(
            tls.starttls,
            Some(crate::registry::StartTlsAccess::Smtp)
        ));
        assert_eq!(smtp.max_bytes, 32 * 1024 * 1024);
        smtp.validate().unwrap();
    }

    /// A local endpoint and a network endpoint are separate authorities: a
    /// manifest asking for one must not be handed the other.
    #[test]
    fn an_endpoint_grant_cannot_exceed_the_manifest() {
        let email = load("email");
        let connector = &email.components()[""];
        let shell = load("shell");
        let main = &shell.components()["main"];
        let access = |value: Value| -> crate::registry::StreamAccess {
            serde_json::from_value(value).unwrap()
        };

        let granted = access(json!({"tls":{"hostname":"imap.mail.me.com","port":993}}));
        assert!(crate::allows_stream(connector, "imap", &granted));
        // The manifest asks for that endpoint under one name and no other.
        assert!(!crate::allows_stream(connector, "smtp", &granted));
        assert!(!crate::allows_stream(connector, "default", &granted));
        assert!(
            !crate::allows_stream(main, "default", &granted),
            "host.stream must not reach the network"
        );

        for elsewhere in [
            json!({"tls":{"hostname":"imap.example.com","port":993}}),
            json!({"tls":{"hostname":"imap.mail.me.com","port":143}}),
            // The preamble is part of the endpoint: a grant may not drop it.
            json!({"tls":{"hostname":"smtp.mail.me.com","port":587}}),
        ] {
            let name = if elsewhere["tls"]["port"] == 587 {
                "smtp"
            } else {
                "imap"
            };
            assert!(
                !crate::allows_stream(connector, name, &access(elsewhere.clone())),
                "{elsewhere} is a different endpoint"
            );
        }
        assert!(crate::allows_stream(
            connector,
            "smtp",
            &access(json!({"tls":{
                "hostname":"smtp.mail.me.com","port":587,"starttls":"smtp"
            }}))
        ));

        let socket = access(json!({"socket":"/run/pluribus/e.sock","peer_uids":[1001]}));
        assert!(!crate::allows_stream(connector, "imap", &socket));
        assert!(crate::allows_stream(main, "default", &socket));
    }

    #[test]
    fn the_email_component_asks_for_no_http() {
        let email = load("email");
        let manifest = email.components()[""].manifest();
        for required in [
            "pluribus:plugin/socket@3.0.0",
            "pluribus:plugin/credentials@3.0.0",
            "pluribus:plugin/state@3.0.0",
            "pluribus:plugin/blobs@3.0.0",
        ] {
            assert!(
                manifest.imports.iter().any(|import| import == required),
                "{required} is missing"
            );
        }
        // Mail arrives over IMAP. No HTTP grant is requested, so none is
        // installed.
        assert!(
            !manifest
                .requested_capabilities
                .iter()
                .any(|request| matches!(request.name.as_str(), "net.http" | "host.http"))
        );
    }

    #[test]
    fn explicit_override_never_falls_back() {
        assert!(
            root_for(
                Path::new("/bin/pluribus"),
                Some(PathBuf::from("/missing-pluribus-packages"))
            )
            .is_err()
        );
    }
}
