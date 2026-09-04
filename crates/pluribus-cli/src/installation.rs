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
fn instance(
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
                "net.http" => {
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
                            "max_request_bytes": bytes,
                            "max_response_bytes": bytes,
                            "max_timeout_ms": timeout,
                        }),
                    );
                }
                "host.stream" => {
                    access.insert(
                        "stream".into(),
                        json!({
                            "socket": runtime.join(format!("{id}-{name}.sock")),
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
    // An endpoint socket must be absolute, and the data directory is where
    // this instance's endpoint belongs.
    let runtime = data.runtime.canonicalize()?;
    let value = instance(&package, &id, &runtime, &source);
    config
        .plugin_instances
        .insert(id.clone(), serde_json::from_value(value)?);
    config.validate()?;
    crate::save_config(data, &config)?;
    tracing::info!(instance = %id, plugin = %package.manifest().id, "configured instance");
    println!("Installed {id}.");
    for credential in &package.manifest().credentials {
        println!(
            "  {} needs a handle in config and `pluribus auth {id}`.",
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
