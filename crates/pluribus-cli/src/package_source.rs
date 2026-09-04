use pluribus_plugin_package::PluginPackage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tracing::{error, info};
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PackageSource {
    File(String),
    Archive(ArchiveSource),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSource {
    pub url: String,
    pub sha256: String,
}

const CATALOG: &str = include_str!(concat!(env!("OUT_DIR"), "/plugins.json"));

/// Prefix of a package reference naming a package of this installation.
const BUNDLED: &str = "bundled:";

/// A reference resolved through the installation rather than pinned to a
/// directory, so replacing the installation keeps it loadable.
#[must_use]
pub fn bundled(name: &str) -> PackageSource {
    PackageSource::File(format!("{BUNDLED}{name}"))
}

fn bundled_name(value: &str) -> Option<&str> {
    value.strip_prefix(BUNDLED)
}

fn check_bundled_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("bundled package name must be alphanumeric, '-', or '_'".into());
    }
    Ok(())
}

/// Where this build gets a package it knows by name.
///
/// What is installed beside the binary comes first, then a source checkout,
/// then the release assets this build was built against — the network last.
fn default_source(name: &str) -> Result<PackageSource> {
    if let Ok(root) = crate::installation::package_root() {
        let directory = root.join(name);
        if directory.join("plugin.toml").is_file() {
            return Ok(directory.into());
        }
    }
    let catalog: serde_json::Value =
        serde_json::from_str(CATALOG).expect("embedded release catalog");
    let reference = catalog["plugins"][name].clone();
    if reference.is_null() {
        return Err(format!(
            "no source for package {name}: install it beside the binary, set PLURIBUS_PLUGIN_DIR, or name its archive"
        )
        .into());
    }
    Ok(PackageSource::Archive(serde_json::from_value(reference)?))
}

impl std::fmt::Display for PackageSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(value) => formatter.write_str(value),
            Self::Archive(source) => write!(formatter, "{} sha256:{}", source.url, source.sha256),
        }
    }
}

impl From<PathBuf> for PackageSource {
    fn from(path: PathBuf) -> Self {
        Self::File(
            Url::from_directory_path(path)
                .expect("absolute package path")
                .into(),
        )
    }
}

impl PackageSource {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::File(value) => match bundled_name(value) {
                Some(name) => check_bundled_name(name),
                None => file_directory(value).map(|_| ()),
            },
            Self::Archive(source) => source.parsed_url().map(|_| ()),
        }
    }

    pub async fn resolve(&self, data: &Path, offline: bool) -> Result<PathBuf> {
        match self {
            Self::File(value) => match bundled_name(value) {
                // A name is resolved where the running binary finds packages,
                // so the reference outlives the directory it lands in.
                Some(name) => {
                    check_bundled_name(name)?;
                    Box::pin(default_source(name)?.resolve(data, offline)).await
                }
                None => file_directory(value),
            },
            Self::Archive(source) => source.resolve(data, offline).await,
        }
    }
}

fn file_directory(value: &str) -> Result<PathBuf> {
    if !value.starts_with("file://") {
        return Err(
            "package requires a file:// directory URL or a pinned file:// or https:// archive"
                .into(),
        );
    }
    let url = Url::parse(value)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err("file URL must not contain a query or fragment".into());
    }
    url.to_file_path()
        .map_err(|()| "file URL requires an absolute local path".into())
}

impl ArchiveSource {
    fn parsed_url(&self) -> Result<Url> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("package sha256 must contain 64 lowercase hexadecimal characters".into());
        }
        let url = Url::parse(&self.url)?;
        if !matches!(url.scheme(), "https" | "file") {
            return Err("package URL must use https:// or file://".into());
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err("package URL must not contain credentials or a fragment".into());
        }
        if url.scheme() == "file" {
            if url.query().is_some() || url.to_file_path().is_err() {
                return Err("file URL must identify an absolute local archive path".into());
            }
        } else if url.host_str().is_none() {
            return Err("HTTPS package URL requires a host".into());
        }
        Ok(url)
    }

    async fn resolve(&self, data: &Path, offline: bool) -> Result<PathBuf> {
        let url = self.parsed_url()?;
        let cache = data.join("packages/sha256");
        let destination = cache.join(&self.sha256);
        let expected = self.sha256.clone();
        let cached = destination.clone();
        match blocking(move || {
            if cached.try_exists()? {
                verify_cached(&cached, &expected)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
        .await
        {
            Ok(true) => {
                info!(digest = %self.sha256, "served package from cache");
                return Ok(destination.join("package"));
            }
            Ok(false) => {}
            Err(error) => {
                error!(digest = %self.sha256, "cached package failed verification");
                return Err(error);
            }
        }
        if offline && url.scheme() != "file" {
            return Err(format!(
                "package {} is not cached; offline download disabled",
                self.sha256
            )
            .into());
        }
        let stage = blocking(move || {
            fs::create_dir_all(&cache)?;
            Ok(tempfile::Builder::new()
                .prefix(".package-")
                .tempdir_in(&cache)?)
        })
        .await?;
        let archive = stage.path().join("archive.tar.gz");
        download(&url, &archive).await?;
        let expected = self.sha256.clone();
        match blocking(move || {
            verify_hash(&archive, &expected)?;
            let package = stage.path().join("package");
            fs::create_dir(&package)?;
            unpack(&archive, &package, false)?;
            PluginPackage::load(&package)?;
            if let Err(error) = fs::rename(stage.path(), &destination) {
                if !destination.exists() {
                    return Err(error.into());
                }
                verify_cached(&destination, &expected)?;
            }
            Ok(destination.join("package"))
        })
        .await
        {
            Ok(package) => {
                info!(
                    host = %url.host_str().unwrap_or("file"),
                    digest = %self.sha256,
                    "fetched package archive"
                );
                Ok(package)
            }
            Err(error) => {
                error!(digest = %self.sha256, "fetched package failed verification");
                Err(error)
            }
        }
    }
}

/// Downloads an archive to learn its digest, so a reference can be pinned to
/// what arrived. Later fetches verify against that pin.
pub async fn pin(url: &str) -> Result<ArchiveSource> {
    let parsed = Url::parse(url)?;
    let stage = blocking(|| Ok(tempfile::Builder::new().prefix(".package-").tempdir()?)).await?;
    let archive = stage.path().join("archive.tar.gz");
    download(&parsed, &archive).await?;
    let sha256 = blocking(move || hash(File::open(&archive)?)).await?;
    Ok(ArchiveSource {
        url: url.to_owned(),
        sha256,
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || work().map_err(|error| error.to_string()))
        .await?
        .map_err(Into::into)
}

fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_mins(2))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("pluribus/", env!("CARGO_PKG_VERSION")))
}

async fn download(url: &Url, destination: &Path) -> Result<()> {
    if url.scheme() == "file" {
        let path = url.to_file_path().map_err(|()| "invalid file URL")?;
        let destination = destination.to_owned();
        blocking(move || {
            if !fs::metadata(&path)?.is_file() {
                return Err("file URL must identify a regular archive file".into());
            }
            let file = File::open(path)?;
            if file.metadata()?.len() > MAX_ARCHIVE {
                return Err("package archive exceeds size limit".into());
            }
            copy_archive(file, &destination, MAX_ARCHIVE)
        })
        .await
    } else {
        download_https(&http_client().build()?, url, destination).await
    }
}

async fn download_https(client: &reqwest::Client, url: &Url, destination: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_ARCHIVE)
    {
        return Err("package archive exceeds size limit".into());
    }
    let mut file = tokio::fs::File::create(destination).await?;
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or("package size overflow")?;
        if size > MAX_ARCHIVE {
            return Err("package archive exceeds size limit".into());
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

fn copy_archive(reader: impl Read, destination: &Path, limit: u64) -> Result<()> {
    let mut file = File::create(destination)?;
    if io::copy(&mut reader.take(limit + 1), &mut file)? > limit {
        return Err("package archive exceeds size limit".into());
    }
    file.sync_all()?;
    Ok(())
}

fn hash(mut reader: impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut hex, "{byte:02x}")?;
    }
    Ok(hex)
}

fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    regular_file(path)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_ARCHIVE || hash(file)? != expected {
        return Err("package archive SHA-256 mismatch".into());
    }
    Ok(())
}

fn regular_file(path: &Path) -> Result<()> {
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err("package contains a non-regular file".into());
    }
    Ok(())
}

fn safe_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().len() > 4096 || path.components().count() > 32 {
        return Err("package path exceeds length or depth limit".into());
    }
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                let name = name.to_str().ok_or("package path must be UTF-8")?;
                if name.contains(['\\', ':']) {
                    return Err("unsafe package path".into());
                }
                result.push(name);
            }
            Component::CurDir => {}
            _ => return Err("unsafe package path".into()),
        }
    }
    Ok(result)
}

fn tree_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    if !root.symlink_metadata()?.file_type().is_dir() {
        return Err("package directory must not be a symlink".into());
    }
    let mut paths = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            for child in tree_files(&entry.path())? {
                paths.insert(PathBuf::from(entry.file_name()).join(child));
            }
        } else if kind.is_file() {
            paths.insert(PathBuf::from(entry.file_name()));
        } else {
            return Err("package contains a link or special file".into());
        }
    }
    Ok(paths)
}

fn unpack(archive: &Path, root: &Path, verify: bool) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
    let mut tar = tar::Archive::new(decoder.take(MAX_EXPANDED + 1));
    let mut seen = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut expanded = 0_u64;
    for (index, entry) in tar.entries()?.raw(true).enumerate() {
        if index >= MAX_ENTRIES {
            return Err("too many package entries".into());
        }
        let mut entry = entry?;
        let path = safe_path(&entry.path()?)?;
        if !seen.insert(path.clone()) {
            return Err("duplicate package path".into());
        }
        let kind = entry.header().entry_type();
        if !(kind.is_dir() || kind.is_file()) {
            return Err("unsupported package archive entry".into());
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or("package size overflow")?;
        if expanded > MAX_EXPANDED {
            return Err("expanded package exceeds size limit".into());
        }
        let target = root.join(&path);
        if kind.is_dir() {
            if entry.size() != 0 {
                return Err("nonempty archive directory".into());
            }
            if !verify {
                fs::create_dir_all(&target)?;
            }
        } else {
            if path.as_os_str().is_empty() {
                return Err("empty package file path".into());
            }
            files.insert(path);
            if verify {
                regular_file(&target)?;
                if hash(&mut entry)? != hash(File::open(&target)?)? {
                    return Err("cached package differs from pinned archive".into());
                }
            } else {
                fs::create_dir_all(target.parent().ok_or("invalid package path")?)?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                io::copy(&mut entry, &mut file)?;
            }
        }
    }
    // Consume the gzip trailer so truncated streams and checksum errors fail.
    let mut remaining = tar.into_inner();
    io::copy(&mut remaining, &mut io::sink())?;
    if remaining.limit() == 0 {
        return Err("expanded package exceeds size limit".into());
    }
    if verify && files != tree_files(root)? {
        return Err("cached package file set changed".into());
    }
    Ok(())
}

fn verify_cached(root: &Path, sha256: &str) -> Result<()> {
    tree_files(root)?;
    verify_hash(&root.join("archive.tar.gz"), sha256)?;
    unpack(&root.join("archive.tar.gz"), &root.join("package"), true)?;
    PluginPackage::load(root.join("package"))?;
    Ok(())
}

pub async fn prepare(
    data: &crate::Paths,
    config: &crate::Config,
    offline: bool,
    cognition: bool,
) -> Result<crate::Config> {
    let mut resolved = config.clone();
    let mut prepared = BTreeSet::new();
    resolve_instances(data, &mut resolved, &mut prepared, offline).await?;
    if cognition {
        crate::refresh_cognition_tools(data, &mut resolved).await?;
        resolve_instances(data, &mut resolved, &mut prepared, offline).await?;
    }
    resolved.validate_resolved()?;
    for id in resolved.plugin_instances.keys() {
        let target = crate::credential_target(data, &resolved, id).await?;
        blocking(move || {
            let package = PluginPackage::load(&target.package)?;
            crate::validate_instance_package(&package, &target)
        })
        .await?;
    }
    Ok(resolved)
}

async fn resolve_instances(
    data: &crate::Paths,
    config: &mut crate::Config,
    prepared: &mut BTreeSet<String>,
    offline: bool,
) -> Result<()> {
    use futures_util::{StreamExt as _, TryStreamExt as _};
    let sources: Vec<_> = config
        .plugin_instances
        .iter()
        .filter(|(id, _)| !prepared.contains(*id))
        .map(|(id, instance)| (id.clone(), instance.package.clone()))
        .collect();
    let resolved: Vec<_> = futures_util::stream::iter(sources)
        .map(|(id, source)| async move {
            let path = source.resolve(&data.cache, offline).await?;
            blocking(move || {
                let package = PluginPackage::load(&path)?;
                Ok((id, path.canonicalize()?, package))
            })
            .await
        })
        .buffer_unordered(4)
        .try_collect()
        .await?;
    for (id, path, package) in resolved {
        let instance = config
            .plugin_instances
            .get_mut(&id)
            .ok_or("missing plugin instance")?;
        instance.package = path.into();
        crate::installation::resolve_instance(&package, &id, &data.runtime, instance)?;
        prepared.insert(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
