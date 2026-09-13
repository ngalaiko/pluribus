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
                "host.stream" => {
                    access.insert(
                        "stream".into(),
                        json!({
                            "socket": runtime.join(if name.is_empty() { format!("{id}.sock") } else { format!("{id}-{name}.sock") }),
                            "peer_uids": [own_uid()],
                        }),
                    );
                }
                _ => {}
            }
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
    if instance.enrollment_overrides.is_none() {
        instance.enrollment_origins = package
            .manifest()
            .credentials
            .iter()
            .flat_map(|credential| credential.enrollment_origins.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
    }
    Ok(())
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
            "  Authorize {} with `pluribus auth {id}`.",
            credential.display_name
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
