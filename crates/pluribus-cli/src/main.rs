#[cfg(test)]
mod fixtures;
mod installation;
mod logs;
mod package_source;
mod registry;
mod stop;

use clap::{Parser, Subcommand};
use pluribus_cognition::{Agent, ComponentInstall, Router};
use pluribus_core::{
    BlobStore, DeliveryStore, EventId, EventMetadataSource, EventStore, EventTypeRegistry,
    PrincipalKind, PrincipalRef, SecretHandle, StateStore, StreamId,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal as RuntimePrincipal, PrincipalKind as RuntimePrincipalKind,
    Runtime, RuntimeLimits,
};
use pluribus_store_fs::FileBlobStore;
use pluribus_store_sqlite::SqliteEventStore;
use registry::{ComponentAccess, Config, InstanceLimits, StreamAccess};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::info;

const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Default)]
struct SystemMetadata {
    sequence: AtomicU64,
}

impl EventMetadataSource for SystemMetadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!(
            "{:x}-{:x}-{:x}",
            system_now_ns(),
            std::process::id(),
            self.sequence.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn now_ms(&self) -> i64 {
        system_now_ms()
    }
}

#[tokio::main]
async fn main() {
    pluribus_log::init();
    if let Err(error) = run_command().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "pluribus",
    version,
    about = "A small, plugin-based agent runtime",
    arg_required_else_help = true
)]
struct Cli {
    /// Agent state: the event store, blobs, and the emergency stop.
    #[arg(long, short = 'd', global = true, value_name = "PATH", default_value_os_t = pluribus_paths::state())]
    data_dir: PathBuf,
    /// Directory holding `config.json`.
    #[arg(long, global = true, value_name = "PATH", default_value_os_t = pluribus_paths::config())]
    config_dir: PathBuf,
    /// Directory holding packages fetched by digest.
    #[arg(long, global = true, value_name = "PATH", default_value_os_t = pluribus_paths::cache())]
    cache_dir: PathBuf,
    /// Directory holding local plugin endpoints.
    #[arg(long, global = true, value_name = "PATH", default_value_os_t = pluribus_paths::runtime())]
    runtime_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// Where this agent keeps each kind of thing, after the flags have their say.
#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub runtime: PathBuf,
}

impl Paths {
    fn resolve(cli: &Cli) -> Self {
        Self {
            state: cli.data_dir.clone(),
            config: cli.config_dir.clone(),
            cache: cli.cache_dir.clone(),
            runtime: cli.runtime_dir.clone(),
        }
    }

    /// Every kind of thing in one directory, the way a test or a single
    /// self-contained installation wants it.
    #[must_use]
    pub fn under(directory: &Path) -> Self {
        Self {
            state: directory.to_owned(),
            config: directory.to_owned(),
            cache: directory.to_owned(),
            runtime: directory.to_owned(),
        }
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.json")
    }

    fn cli_options(&self) -> String {
        let mut options = String::new();
        for (flag, path, default) in [
            ("data-dir", &self.state, pluribus_paths::state()),
            ("config-dir", &self.config, pluribus_paths::config()),
            ("cache-dir", &self.cache, pluribus_paths::cache()),
            ("runtime-dir", &self.runtime, pluribus_paths::runtime()),
        ] {
            if path != &default {
                options.push_str(" --");
                options.push_str(flag);
                options.push(' ');
                options.push_str(&shell_path(path));
            }
        }
        options
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show stored events for the configured agent as JSON lines.
    Logs {
        /// Keep printing new events until interrupted.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Number of recent events to print before following.
        #[arg(long, short = 'n', default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// Create an agent data directory.
    Init {
        /// Configure the packages this build ships: a terminal to talk to, a
        /// shell to run commands, and free-tier cognition.
        #[arg(long)]
        example: bool,
    },
    /// Run the agent until interrupted.
    Run {
        /// Clear the emergency stop before starting.
        #[arg(long)]
        resume: bool,
    },
    /// List, authenticate, and install plugin instances.
    Plugins {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Halt cognition and cancel active shell commands.
    Stop,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// List configured instances and their package sources.
    List,
    /// Enroll credentials for a configured instance.
    Auth {
        /// Configured instance ID or alias.
        #[arg(value_name = "INSTANCE")]
        plugin: String,
    },
    /// Add a plugin instance to config.json.
    Install {
        /// Package directory or archive URL.
        package: String,
        /// Expected archive digest; defaults to the downloaded archive's digest.
        #[arg(long)]
        sha256: Option<String>,
        /// Instance name. Defaults to the plugin name.
        #[arg(long)]
        id: Option<String>,
    },
}

async fn run_command() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let paths = Paths::resolve(&cli);
    let data = &paths;
    match cli.command {
        Command::Logs { follow, limit } => logs::show(data, limit as usize, follow).await,
        Command::Init { example } => {
            initialize(data, example).await?;
            if example {
                example_configuration(data).await?;
            }
            Ok(())
        }
        Command::Run { resume } => {
            stop::prepare(data, resume)?;
            run_agent(data).await
        }
        Command::Plugins { command } => match command {
            PluginCommand::List => {
                let config = load_config(data)?;
                if config.plugin_instances.is_empty() {
                    println!("No plugin instances are configured.");
                } else {
                    println!("INSTANCE\tPACKAGE");
                    for (id, instance) in &config.plugin_instances {
                        let source = match &instance.package {
                            package_source::PackageSource::File(source) => source,
                            package_source::PackageSource::Archive(source) => &source.url,
                        };
                        println!("{id}\t{source}");
                    }
                }
                Ok(())
            }
            PluginCommand::Auth { plugin } => authenticate(data, &plugin).await,
            PluginCommand::Install {
                package,
                sha256,
                id,
            } => installation::install(data, &package, sha256, id).await,
        },
        Command::Stop => stop::request(data),
    }
}

async fn initialize(data: &Paths, quiet: bool) -> Result<(), Box<dyn Error>> {
    // An agent is created where it will run, so say which directory refused.
    let private = |directory: &Path| -> Result<(), Box<dyn Error>> {
        let at = |error: io::Error| format!("{}: {error}", directory.display());
        fs::create_dir_all(directory).map_err(at)?;
        // State and credentials live here, and an endpoint grant refuses a
        // runtime whose data anyone else can read.
        fs::set_permissions(
            directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .map_err(at)?;
        Ok(())
    };
    for directory in [&data.state, &data.config, &data.cache, &data.runtime] {
        private(directory)?;
    }
    let config_path = data.config_file();
    if config_path.exists() {
        return Err(format!("{} already exists", config_path.display()).into());
    }
    fs::write(&config_path, serde_json::to_vec_pretty(&Config::default())?)?;
    open_database(&data.state).await?;
    FileBlobStore::open(data.state.join("blobs"), MAX_BLOB_BYTES).await?;
    println!("Created {}", config_path.display());
    if !quiet {
        print_unconfigured(&config_path);
    }
    Ok(())
}

/// An agent without plugins runs nothing, so say what is missing and what is
/// available to configure.
fn print_unconfigured(config_path: &Path) {
    println!("No plugin instances are configured; the agent has nothing to run.");
    match installation::package_root().and_then(|root| {
        let names = installation::installed(&root)?;
        Ok((root, names))
    }) {
        Ok((root, names)) if !names.is_empty() => {
            println!(
                "Installed packages in {}: {}.",
                root.display(),
                names.join(", ")
            );
        }
        _ => {}
    }
    println!("Add instances to {}.", config_path.display());
}

/// The agent this build can run with what it ships: a terminal connector, a
/// shell, and `OpenRouter`'s free tier for cognition.
async fn example_configuration(data: &Paths) -> Result<(), Box<dyn Error>> {
    const MODEL: &str = "openrouter/free";
    const IDENTITY: &str = "You are a local assistant. Answer tersely.";
    // A shared key with no budget, so the example runs without an account.
    // `pluribus plugins auth openrouter` replaces it with one of your own.
    const DEMO_KEY: &str =
        "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";

    for name in ["cli", "shell", "openrouter", "rlm", "scheduler"] {
        installation::install_named(data, name).await?;
    }

    let mut config = load_config(data)?;
    config.identity = IDENTITY.into();
    config.model = MODEL.into();
    config.model_instance = Some("openrouter/main".into());
    config.capability_instances = vec!["shell/main".into(), "scheduler".into()];
    let schedule_capabilities: Vec<String> = [
        "create", "list", "get", "update", "pause", "resume", "delete",
    ]
    .into_iter()
    .map(|method| format!("schedule.{method}"))
    .collect();
    config.trusted_constraints.insert(
        "scheduler".into(),
        schedule_capabilities
            .iter()
            .map(|capability| (capability.clone(), json!({})))
            .collect(),
    );
    config
        .trusted_capabilities
        .insert("scheduler".into(), schedule_capabilities);
    config
        .trusted_capabilities
        .insert("shell/main".into(), vec!["shell.execute".into()]);
    config.trusted_constraints.insert(
        "shell/main".into(),
        BTreeMap::from([("shell.execute".into(), json!({}))]),
    );

    set(
        &mut config,
        "cli",
        json!({"conversation_id": "local", "sender": "operator"}),
    )?;
    set(
        &mut config,
        "openrouter",
        json!({"credentials": {"api-key": "openrouter:personal"}, "models": [MODEL]}),
    )?;
    set(&mut config, "rlm", json!({}))?;
    set(&mut config, "scheduler", json!({}))?;
    save_config(data, &config)?;
    enroll(data, "openrouter", "api-key", json!({"api_key": DEMO_KEY})).await?;

    let where_ = data.cli_options();
    println!("\nConfigured cli, shell, openrouter, rlm and scheduler, on a shared key with no");
    println!("budget. Replace it with `pluribus{where_} plugins auth openrouter`.");
    println!("\nStart each of these, in its own terminal:");
    println!(
        "  pluribus-cli-bridge --socket {}",
        shell_path(&data.runtime.join("cli-main.sock"))
    );
    println!(
        "  pluribus-shell-executor --socket {}",
        shell_path(&data.runtime.join("shell-main.sock"))
    );
    println!("  pluribus{where_} run");
    Ok(())
}

fn set(config: &mut Config, id: &str, value: Value) -> Result<(), Box<dyn Error>> {
    let instance = config
        .plugin_instances
        .get_mut(id)
        .ok_or_else(|| format!("{id} is not configured"))?;
    instance.config = value.clone();
    instance.config_overrides = value;
    Ok(())
}

async fn authenticate(data: &Paths, plugin_name: &str) -> Result<(), Box<dyn Error>> {
    let config = load_config(data)?;
    let target = credential_target(data, &config, plugin_name).await?;
    let package = PluginPackage::load(&target.package)?;
    let descriptors = credential_descriptors(&package, &target)?;
    let descriptor = select_credential_descriptor(&descriptors)?;
    let credential_id = descriptor.id.clone();
    let input = prompt_credential_input(&descriptor.input_schema)?;
    enroll(data, plugin_name, &credential_id, input).await
}

/// Stores one credential for a configured instance.
async fn enroll(
    data: &Paths,
    plugin_name: &str,
    credential_id: &str,
    input: Value,
) -> Result<(), Box<dyn Error>> {
    let config = load_config(data)?;
    let database = open_database(&data.state).await?;
    let target = credential_target(data, &config, plugin_name).await?;
    let package = PluginPackage::load(&target.package)?;
    let descriptors = credential_descriptors(&package, &target)?;
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.id == credential_id)
        .ok_or("credential declaration changed during enrollment")?;
    let handle = target.config["credentials"]
        .get(&descriptor.id)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("credential slot does not resolve to a handle")?;
    if !package
        .manifest()
        .credentials
        .iter()
        .any(|declared| declared.id == descriptor.id && declared.access)
    {
        return Err("credential is not readable by any component".into());
    }
    // The host speaks no protocol here, so there is nothing to render: the
    // validated input is the record, and the component reads it back.
    if descriptor.flow_schema == "pluribus:credential/static-plugin@1" {
        if !jsonschema::is_valid(&descriptor.input_schema, &input) {
            return Err("invalid credential input".into());
        }
        store_plugin_credential(database.as_ref(), handle, &package.manifest().id, &input).await?;
        println!("{} stored.", descriptor.display_name);
        return Ok(());
    }
    if descriptor.flow_schema == "pluribus:credential/plugin@1" {
        let [component] = descriptor.components.as_slice() else {
            return Err("plugin enrollment requires one component".into());
        };
        if !jsonschema::is_valid(&descriptor.input_schema, &input) {
            return Err("invalid credential input".into());
        }
        let enrollment_id = SystemMetadata::default()
            .next_event_id()
            .as_str()
            .to_owned();
        stage_plugin_enrollment(
            database.as_ref(),
            handle,
            &package.manifest().id,
            &enrollment_id,
            input,
        )
        .await?;
        let request = database
            .append(pluribus_core::AppendRequest {
                stream_id: StreamId::new(&config.agent_id),
                stream_kind: pluribus_core::StreamKind::Agent,
                observed_at_ms: None,
                event_type: "credential.enrollment.requested".into(),
                payload_schema: "pluribus.credential.enrollment.requested/1".into(),
                payload: pluribus_core::EventPayload::CanonicalJson(serde_json::to_vec(
                    &json!({"component":component.id.as_str(),"credential":handle,"enrollment":enrollment_id}),
                )?),
                actor: PrincipalRef::new(PrincipalKind::Node, "credential-cli"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            })
            .await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut after = request.sequence;
        loop {
            let replies = database
                .query(
                    &StreamId::new(&config.agent_id),
                    &pluribus_core::EventQuery {
                        after_sequence: Some(after),
                        event_types: vec!["credential.enrollment.started".into()],
                        correlation_id: None,
                        activity_id: None,
                        recorded_from_ms: None,
                        recorded_to_ms: None,
                        ..pluribus_core::EventQuery::default()
                    },
                    100,
                )
                .await?;
            for reply in replies {
                after = reply.sequence;
                if reply.request.actor != *component
                    || reply.request.causation_id.as_ref() != Some(&request.event_id)
                {
                    continue;
                }
                let pluribus_core::EventPayload::CanonicalJson(bytes) = reply.request.payload
                else {
                    continue;
                };
                let value: Value = serde_json::from_slice(&bytes)?;
                for line in enrollment_display_lines(&value)? {
                    println!("{line}");
                }
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("enrollment timed out; ensure pluribus run is active".into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    Err(format!("unsupported credential flow: {}", descriptor.flow_schema).into())
}

fn enrollment_display_lines(value: &Value) -> Result<Vec<String>, Box<dyn Error>> {
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or("plugin enrollment unavailable")?;
    let mut lines = vec![format!("Open {url}")];
    match value.get("userCode") {
        None | Some(Value::Null) => {}
        Some(code) => lines.push(format!(
            "User code: {}",
            code.as_str()
                .filter(|code| !code.is_empty())
                .ok_or("plugin enrollment unavailable")?
        )),
    }
    Ok(lines)
}

struct CredentialTarget {
    package: PathBuf,
    instance_id: String,
    config: Value,
    components: BTreeMap<String, ComponentAccess>,
}

async fn credential_target(
    data: &Paths,
    config: &Config,
    plugin: &str,
) -> Result<CredentialTarget, Box<dyn Error>> {
    let (id, instance) = config.instance(plugin)?;
    let mut instance = instance.clone();
    let path = resolve_plugin(data, &instance.package).await?;
    let package = PluginPackage::load(&path)?;
    installation::resolve_instance(&package, id, &data.runtime, &mut instance)?;
    Ok(credential_target_from_instance(id, path, &instance))
}

fn credential_target_from_instance(
    id: &str,
    path: PathBuf,
    instance: &crate::registry::PluginInstance,
) -> CredentialTarget {
    CredentialTarget {
        package: path,
        instance_id: id.into(),
        config: instance.config.clone(),
        components: instance.components.clone(),
    }
}

/// Credential enrollment a plugin declares.
///
/// Read from the manifest: enrollment is static data, so discovering it no
/// longer instantiates the component.
struct CredentialDescriptor {
    components: Vec<PrincipalRef>,
    display_name: String,
    id: String,
    input_schema: Value,
    flow_schema: String,
}

fn credential_descriptors(
    package: &PluginPackage,
    target: &CredentialTarget,
) -> Result<Vec<CredentialDescriptor>, Box<dyn Error>> {
    validate_instance_package(package, target)?;
    let declarations = &package.manifest().credentials;
    if declarations.is_empty() {
        return Err("plugin declares no credentials".into());
    }
    declarations
        .iter()
        .map(|declaration| {
            for name in &declaration.components {
                if !target.components.contains_key(name) {
                    return Err(
                        format!("credential references unconfigured component: {name}").into(),
                    );
                }
            }
            Ok(CredentialDescriptor {
                components: declaration
                    .components
                    .iter()
                    .map(|name| {
                        PrincipalRef::new(
                            PrincipalKind::Component,
                            pluribus_plugin_package::component_id(&target.instance_id, name),
                        )
                    })
                    .collect(),
                display_name: declaration.display_name.clone(),
                id: declaration.id.clone(),
                input_schema: package_json(package, &declaration.input_schema)?,
                flow_schema: declaration.flow_schema.clone(),
            })
        })
        .collect()
}

/// Reads a package-relative JSON file named by the manifest.
fn package_json(package: &PluginPackage, relative: &str) -> Result<Value, Box<dyn Error>> {
    let path = package.root().join(relative);
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()).into())
}

fn select_credential_descriptor(
    descriptors: &[CredentialDescriptor],
) -> Result<&CredentialDescriptor, Box<dyn Error>> {
    match descriptors {
        [] => Err("plugin declares no credentials".into()),
        [descriptor] => Ok(descriptor),
        _ if !io::stdin().is_terminal() => {
            Err("plugin declares several credentials; use an interactive terminal".into())
        }
        _ => {
            println!("Credential:");
            for (index, descriptor) in descriptors.iter().enumerate() {
                println!("  {}. {}", index + 1, descriptor.display_name);
            }
            loop {
                let choice = prompt_line("Choice", "1")?;
                if let Ok(index) = choice.parse::<usize>()
                    && let Some(descriptor) = index
                        .checked_sub(1)
                        .and_then(|index| descriptors.get(index))
                {
                    return Ok(descriptor);
                }
                println!("Enter a listed number.");
            }
        }
    }
}

/// Replaces the whole record with the enrolled input. Re-running `auth`
/// rotates the secret rather than merging into what is there.
async fn store_plugin_credential(
    store: &dyn pluribus_core::PluginCredentialStore,
    handle: &str,
    provider: &str,
    input: &Value,
) -> Result<(), Box<dyn Error>> {
    let handle = SecretHandle::new(handle);
    let previous = store.read_plugin_credential(&handle, provider).await?;
    let bytes = serde_json::to_vec(input)?;
    if bytes.len() > 1024 * 1024 {
        return Err("credential record too large".into());
    }
    if !store
        .replace_plugin_credential(&handle, provider, previous, bytes)
        .await?
    {
        return Err("credential changed during enrollment; retry".into());
    }
    Ok(())
}

async fn stage_plugin_enrollment(
    store: &dyn pluribus_core::PluginCredentialStore,
    handle: &str,
    provider: &str,
    id: &str,
    input: Value,
) -> Result<(), Box<dyn Error>> {
    let handle = SecretHandle::new(handle);
    let previous = store.read_plugin_credential(&handle, provider).await?;
    let mut record: Value = previous
        .as_ref()
        .map(|v| serde_json::from_slice(v))
        .transpose()
        .map_err(|_| "invalid credential record")?
        .unwrap_or_else(|| json!({}));
    let object = record.as_object_mut().ok_or("invalid credential record")?;
    object.insert(
        "enrollment".into(),
        json!({"id":id,"input":input,"expires_at_ms":system_now_ms()+60_000}),
    );
    let bytes = serde_json::to_vec(&record)?;
    if bytes.len() > 1024 * 1024 {
        return Err("credential record too large".into());
    }
    if !store
        .replace_plugin_credential(&handle, provider, previous, bytes)
        .await?
    {
        return Err("credential changed during enrollment; retry".into());
    }
    Ok(())
}

fn prompt_credential_input(schema: &Value) -> Result<Value, Box<dyn Error>> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if properties.is_empty() {
        return Ok(json!({}));
    }
    if !io::stdin().is_terminal() {
        if properties.len() != 1 {
            let mut bytes = String::new();
            io::stdin().take(64 * 1024 + 1).read_to_string(&mut bytes)?;
            if bytes.len() > 64 * 1024 {
                return Err("credential input too large".into());
            }
            return serde_json::from_str(&bytes)
                .map_err(|_| "invalid credential input JSON".into());
        }
        let name = properties.keys().next().expect("one property").clone();
        let mut value = String::new();
        io::stdin().take(64 * 1024).read_to_string(&mut value)?;
        return Ok(Value::Object(Map::from_iter([(
            name,
            Value::String(value.trim().into()),
        )])));
    }
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let mut input = Map::new();
    for (name, field) in properties {
        let label = field.get("title").and_then(Value::as_str).unwrap_or(&name);
        let default = field.get("default").and_then(Value::as_str).unwrap_or("");
        let secret = field
            .get("writeOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let from_file = field
            .get("x-input-file")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let value = if from_file {
            let path = prompt_line(label, default)?;
            let mut contents = String::new();
            fs::File::open(path.trim())
                .map_err(|_| "cannot open credential file")?
                .take(64 * 1024 + 1)
                .read_to_string(&mut contents)
                .map_err(|_| "cannot read credential file")?;
            if contents.len() > 64 * 1024 {
                return Err("credential file too large".into());
            }
            contents
        } else if secret {
            rpassword::prompt_password(format!("{label}: "))?
        } else {
            prompt_line(label, default)?
        };
        let value = value.trim().to_owned();
        if !value.is_empty() || required.contains(&name.as_str()) {
            input.insert(name, Value::String(value));
        }
    }
    Ok(Value::Object(input))
}

fn allows_http(
    component: &pluribus_plugin_package::PluginComponent,
    origin: &str,
    method: &str,
) -> bool {
    component
        .manifest()
        .requested_capabilities
        .iter()
        .any(|request| {
            matches!(request.name.as_str(), "host.http" | "net.http")
                && request
                    .constraints
                    .get("origins")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(origin)))
                && request
                    .constraints
                    .get("methods")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(method)))
        })
}

/// Whether the manifest asks for the endpoint the configuration grants under
/// `endpoint`.
///
/// The two endpoint kinds are separate capabilities: a manifest asking for a
/// local socket does not thereby reach the network, and one asking for a named
/// TLS endpoint reaches that endpoint and no other. The name is matched too,
/// so a second endpoint cannot be smuggled in under the first one's request.
fn allows_stream(
    component: &pluribus_plugin_package::PluginComponent,
    endpoint: &str,
    access: &crate::registry::StreamAccess,
) -> bool {
    component
        .manifest()
        .requested_capabilities
        .iter()
        .filter(|request| {
            request.constraints.get("name").and_then(Value::as_str) == Some(endpoint)
                || (endpoint == "default" && request.constraints.get("name").is_none())
        })
        .any(|request| match (request.name.as_str(), &access.tls) {
            ("host.stream", None) => true,
            ("net.tls", Some(tls)) => {
                request.constraints.get("hostname").and_then(Value::as_str) == Some(&tls.hostname)
                    && request.constraints.get("port").and_then(Value::as_u64)
                        == Some(u64::from(tls.port))
                    && request.constraints.get("starttls").and_then(Value::as_str)
                        == tls.starttls.map(starttls_name)
            }
            _ => false,
        })
}

const fn starttls_name(preamble: crate::registry::StartTlsAccess) -> &'static str {
    match preamble {
        crate::registry::StartTlsAccess::Smtp => "smtp",
    }
}

fn validate_instance_package(
    package: &PluginPackage,
    target: &CredentialTarget,
) -> Result<(), Box<dyn Error>> {
    package.validate_config(&target.config)?;
    if package.components().keys().ne(target.components.keys()) {
        return Err(format!(
            "configured components differ from package {}",
            target.instance_id
        )
        .into());
    }
    for (name, access) in &target.components {
        let component = &package.components()[name];
        for (endpoint, stream) in &access.stream {
            stream.validate()?;
            if !allows_stream(component, endpoint, stream) {
                return Err(format!(
                    "endpoint grant exceeds manifest for {}/{name} endpoint {endpoint}",
                    target.instance_id
                )
                .into());
            }
        }
        for origin in &access.http.origins {
            for method in &access.http.methods {
                if !allows_http(component, origin, method) {
                    return Err(format!(
                        "HTTP grant exceeds manifest for {}/{name}",
                        target.instance_id
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

/// Supplies capability and trust context to declared catalog consumers.
fn refresh_cognition_tools(
    config: &mut Config,
    packages: &package_source::ResolvedPackages,
) -> Result<(), Box<dyn Error>> {
    let mut catalog_consumers = Vec::new();
    let mut components = Vec::new();
    let mut tools = Vec::new();
    let grants = config.trusted_grants()?;
    for id in config.plugin_instances.keys() {
        let package = packages.get(id)?;
        for (name, component) in package.components() {
            if !config.plugin_instances[id].components.contains_key(name) {
                continue;
            }
            let manifest = component.manifest();
            if manifest.catalog_injection.is_some() {
                catalog_consumers.push((id.clone(), name.to_owned()));
            }
            components.push(json!({
                "instanceId":pluribus_plugin_package::component_id(id, name),
                "plugin":package.manifest().id,
                "subscribes":manifest.subscribes,
                "emits":manifest.emits,
                "capabilities":manifest.provides.iter().map(|p| p.capability.clone()).collect::<Vec<_>>()
            }));
            for capability in &component.manifest().provides {
                let schema: Value = serde_json::from_slice(&fs::read(
                    package.root().join(&capability.arguments_schema),
                )?)?;
                let provider = PrincipalRef::new(
                    PrincipalKind::Component,
                    pluribus_plugin_package::component_id(id, name),
                );
                let constraints: Vec<Value> = grants
                    .get(&pluribus_core::CapabilityName::new(&capability.capability))
                    .into_iter()
                    .flatten()
                    .filter(|grant| grant.provider.as_ref() == Some(&provider))
                    .map(|grant| serde_json::from_slice(grant.constraints.as_bytes()))
                    .collect::<Result<_, _>>()?;
                tools.push(json!({"name":capability.capability,"description":capability.description,"parameters":schema,"configuredConstraints":constraints}));
            }
        }
    }
    for (id, component_name) in catalog_consumers {
        let instance = config
            .plugin_instances
            .get_mut(&id)
            .expect("resolved catalog consumer");
        let injection = packages
            .get(&id)?
            .component(&component_name)
            .unwrap()
            .manifest()
            .catalog_injection
            .as_ref()
            .unwrap();
        if injection.tools_pointer == injection.components_pointer {
            return Err(format!("catalog pointers collide for {id}/{component_name}").into());
        }
        set_config_pointer(&mut instance.config, &injection.tools_pointer, json!(tools))?;
        set_config_pointer(
            &mut instance.config,
            &injection.components_pointer,
            json!(components),
        )?;
    }
    Ok(())
}

fn set_config_pointer(
    config: &mut Value,
    pointer: &str,
    value: Value,
) -> Result<(), Box<dyn Error>> {
    if let Some(target) = config.pointer_mut(pointer) {
        *target = value;
        return Ok(());
    }
    let (parent, key) = pointer
        .rsplit_once('/')
        .ok_or("catalog pointer must name a field")?;
    let key = key.replace("~1", "/").replace("~0", "~");
    let parent = config
        .pointer_mut(parent)
        .ok_or("catalog pointer parent missing from config")?;
    parent
        .as_object_mut()
        .ok_or("catalog pointer parent is not an object")?
        .insert(key, value);
    Ok(())
}

/// Every way in and out the configuration installs.
///
/// Connector authority comes from public manifest declarations.
fn connectors(
    config: &Config,
    packages: &package_source::ResolvedPackages,
) -> Result<Vec<pluribus_cognition::Connector>, Box<dyn Error>> {
    let mut connectors = Vec::new();
    for (id, instance) in &config.plugin_instances {
        let package = packages.get(id)?;
        for (name, component) in package.components() {
            if !instance.components.contains_key(name) {
                continue;
            }
            let selector = pluribus_plugin_package::component_id(id, name);
            let manifest = component.manifest();
            if let Some(declaration) = &manifest.connector {
                if !manifest
                    .emits
                    .iter()
                    .any(|event| event == "observation.received")
                {
                    return Err(format!("connector {} does not emit observations", selector).into());
                }
                let (reply, reply_capabilities) =
                    if let Some(reply_name) = &declaration.reply_component {
                        if !instance.components.contains_key(reply_name) {
                            return Err(format!(
                                "connector {} has unconfigured reply component {}",
                                selector, reply_name
                            )
                            .into());
                        }
                        let reply_manifest = package
                            .component(reply_name)
                            .ok_or_else(|| {
                                format!(
                                    "connector {} references missing reply component {}",
                                    selector, reply_name
                                )
                            })?
                            .manifest();
                        (
                            pluribus_plugin_package::component_id(id, reply_name),
                            reply_manifest
                                .provides
                                .iter()
                                .map(|capability| {
                                    pluribus_core::CapabilityName::new(&capability.capability)
                                })
                                .collect(),
                        )
                    } else {
                        (String::new(), Vec::new())
                    };
                connectors.push(pluribus_cognition::Connector {
                    provider: declaration.provider.clone(),
                    inherits_origin: declaration.inherits_origin,
                    ingress: selector,
                    reply,
                    reply_capabilities,
                });
            }
        }
    }
    Ok(connectors)
}

fn selected_model_component(config: &Config) -> Result<Option<String>, String> {
    config
        .model_instance
        .as_deref()
        .map(|selector| config.component(selector).map(|(id, _, _)| id))
        .transpose()
}

fn configured_models(
    component: &pluribus_plugin_package::PluginComponent,
    config: &Value,
    selected: bool,
) -> Vec<String> {
    if !selected {
        return Vec::new();
    }
    component
        .manifest()
        .model_provider
        .as_ref()
        .and_then(|provider| config.pointer(&provider.models_pointer))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().or_else(|| v["id"].as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

const IDLE_MIN: Duration = Duration::from_millis(50);
const IDLE_MAX: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_lines)]
async fn run_agent(data: &Paths) -> Result<(), Box<dyn Error>> {
    let (config, packages) =
        package_source::prepare_registry(data, &load_config(data)?, false, true).await?;
    info!(
        state = %data.state.display(),
        config_dir = %data.config.display(),
        cache = %data.cache.display(),
        runtime = %data.runtime.display(),
        instances = config.plugin_instances.len(),
        "starting agent"
    );
    let database = open_database(&data.state).await?;
    let blobs: Arc<dyn BlobStore> =
        Arc::new(FileBlobStore::open(data.state.join("blobs"), config.maximum_blob_bytes).await?);
    let agent_principal = PrincipalRef::new(PrincipalKind::Agent, &config.agent_id);
    let stream_id = StreamId::new(config.agent_id.clone());

    let http: Arc<dyn pluribus_core::HttpStreamService> = Arc::new(
        pluribus_host_http::PolicyHttpService::new(Arc::clone(&blobs)),
    );
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&database) as Arc<dyn StateStore>,
        Arc::clone(&database) as Arc<dyn EventStore>,
        Arc::clone(&blobs),
        Arc::clone(&database) as Arc<dyn DeliveryStore>,
    )?;
    let connectors = connectors(&config, &packages)?;
    info!(
        connectors = %connectors
            .iter()
            .map(|connector| format!("{}:{}", connector.provider, connector.ingress))
            .collect::<Vec<_>>()
            .join(","),
        model_instance = %config.model_instance.as_deref().unwrap_or("none"),
        "resolved connectors"
    );
    let router = Router::new(
        stream_id.clone(),
        agent_principal.clone(),
        Arc::clone(&database) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        pluribus_cognition::OriginConstraints,
    );
    let mut agent = Agent::new(
        router,
        pluribus_cognition::OriginAuthority {
            agent: agent_principal.clone(),
            grants: config.trusted_grants().map_err(|error| error.clone())?,
            max_depth: 4,
            events: Arc::clone(&database) as Arc<dyn EventStore>,
            connectors,
        },
        runtime,
        Arc::clone(&database) as Arc<dyn EventStore>,
        stream_id,
        agent_principal.clone(),
    );

    let model_component = selected_model_component(&config)?;
    let mut cognition = false;
    for (id, instance) in &config.plugin_instances {
        let package = packages.get(id)?;
        cognition |= package.components().iter().any(|(name, component)| {
            component.manifest().subscribes_to_stream() && instance.components.contains_key(name)
        });
        let mut installs = BTreeMap::new();
        for (name, component) in package.components() {
            let access = &instance.components[name];
            let component_id = pluribus_plugin_package::component_id(id, name);
            let services = PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    exports: serde_json::from_value(
                        instance
                            .config
                            .get("credential_exports")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    )?,
                    store: database.clone(),
                    provider: package.manifest().id.clone(),
                    handles: package
                        .manifest()
                        .credentials
                        .iter()
                        .filter(|c| c.access && c.components.contains(name))
                        .filter_map(|c| {
                            instance.config["credentials"][&c.id]
                                .as_str()
                                .map(str::to_owned)
                        })
                        .collect(),
                }),
                model: Some(config.model.clone()),
                identity: Some(config.identity.clone()),
                http: Some(Arc::clone(&http)),
                http_grant: Some(access.http.grant(&component_id)),
                // Each endpoint gets its own transport, so a connection
                // ceiling bounds that endpoint rather than the component's
                // traffic as a whole.
                streams: access
                    .stream
                    .iter()
                    .map(|(endpoint, stream)| {
                        let service: Arc<dyn pluribus_core::StreamService> = if stream.is_local() {
                            // A local endpoint requires the private runtime
                            // directory; a remote one has no local socket to
                            // protect.
                            Arc::new(pluribus_host_stream::LocalStreamService::new(&data.state)?)
                        } else {
                            Arc::new(pluribus_host_stream::LocalStreamService::remote())
                        };
                        Ok((
                            endpoint.clone(),
                            pluribus_runtime_wasm::GrantedStream {
                                service,
                                grant: StreamAccess::grant(stream)?,
                            },
                        ))
                    })
                    .collect::<Result<_, Box<dyn Error>>>()?,
                limits: access.limits.map(InstanceLimits::runtime),
            };
            let models = configured_models(
                component,
                &instance.config,
                model_component.as_deref() == Some(component_id.as_str()),
            );
            installs.insert(name.clone(), ComponentInstall { services, models });
        }
        agent
            .install_package(
                &package,
                &instance.config,
                Delivery {
                    instance_id: id.clone(),
                    agent: RuntimePrincipal {
                        kind: RuntimePrincipalKind::Agent,
                        id: config.agent_id.clone(),
                    },
                    actor: RuntimePrincipal {
                        kind: RuntimePrincipalKind::Agent,
                        id: config.agent_id.clone(),
                    },
                    authority_id: format!("standing:{id}"),
                    activity_id: format!("install:{id}"),
                    correlation_id: format!("install:{id}"),
                    origin_event_id: format!("install:{id}"),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: Vec::new(),
                },
                installs,
            )
            .await?;
        for name in package.components().keys() {
            info!(instance = %id, component = %name, "started component");
        }
    }
    if !cognition {
        return Err(format!(
            "no cognition component is configured in {}: the agent would receive events and act on none",
            data.config_file().display()
        )
        .into());
    }

    let _monitor = stop::Monitor::new(
        data.state.join("STOP"),
        agent.stop_signal(),
        agent.cancellation_handles(),
    );
    println!("Running. Interrupt to stop.");
    let mut idle = IDLE_MIN;
    loop {
        if stop::is_stopped(data) {
            info!(agent = %config.agent_id, reason = %"emergency_stop", "stopping agent");
            println!("Emergency stop is set; halting.");
            return Ok(());
        }
        let progress = match agent.tick(system_now_ms()).await {
            Ok(progress) => progress,
            Err(error) => {
                info!(agent = %config.agent_id, reason = %"tick_failed", "stopping agent");
                return Err(error.into());
            }
        };
        if progress.is_idle() {
            agent.wait_for_progress(idle).await;
            idle = (idle * 2).min(IDLE_MAX);
        } else {
            idle = IDLE_MIN;
        }
    }
}

async fn open_database(
    data: &Path,
) -> Result<Arc<SqliteEventStore<SystemMetadata>>, Box<dyn Error>> {
    fs::create_dir_all(data)?;
    Ok(Arc::new(
        SqliteEventStore::open(data.join("pluribus.sqlite3"), SystemMetadata::default()).await?,
    ))
}

fn load_config(paths: &Paths) -> Result<Config, Box<dyn Error>> {
    let bytes = fs::read(paths.config_file())?;
    let config: Config = serde_json::from_slice(&bytes)?;
    config.validate()?;
    Ok(config)
}

fn save_config(paths: &Paths, config: &Config) -> Result<(), Box<dyn Error>> {
    config.validate()?;
    let destination = paths.config_file();
    let temporary = paths.config.join("config.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(config)?)?;
    fs::rename(temporary, destination)?;
    Ok(())
}

fn prompt_line(label: &str, default: &str) -> Result<String, Box<dyn Error>> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value)? == 0 {
        return Err("input ended".into());
    }
    let value = value.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

async fn resolve_plugin(
    data: &Paths,
    configured: &package_source::PackageSource,
) -> Result<PathBuf, Box<dyn Error>> {
    configured.resolve(&data.cache, false).await
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn system_now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
        std::fs::create_dir_all(destination).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn package_path(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/plugins")
            .join(name)
            .canonicalize()
            .map(|source| {
                let target = root.join(name);
                copy_tree(&source, &target);
                target
            })
            .unwrap()
    }

    fn set_package(config: &mut crate::registry::Config, instance: &str, path: &std::path::Path) {
        config.plugin_instances.get_mut(instance).unwrap().package =
            package_source::PackageSource::File(
                url::Url::from_directory_path(path).unwrap().to_string(),
            );
    }

    #[tokio::test]
    async fn connectors_keep_same_plugin_instances_paired() {
        let mut config = crate::fixtures::config();
        let telegram = config.plugin_instances["telegram-1"].clone();
        config
            .plugin_instances
            .insert("telegram-2".into(), telegram);
        for instance in config.plugin_instances.values_mut() {
            instance.aliases.clear();
        }
        let temp = tempfile::tempdir().unwrap();
        let (config, packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &config,
            true,
            false,
        )
        .await
        .unwrap();
        let resolved = super::connectors(&config, &packages).unwrap();
        assert_eq!(resolved.len(), 2);
        for id in ["telegram-1", "telegram-2"] {
            assert!(resolved.iter().any(|c| {
                c.ingress == format!("{id}/receive") && c.reply == format!("{id}/send")
            }));
        }
    }

    #[tokio::test]
    async fn connector_discovery_reuses_loaded_packages() {
        let mut config = crate::fixtures::config();
        let temp = tempfile::tempdir().unwrap();
        let data = super::Paths::under(temp.path());
        let (_, packages) = crate::package_source::prepare_registry(&data, &config, true, false)
            .await
            .unwrap();
        for instance in config.plugin_instances.values_mut() {
            instance.package =
                package_source::PackageSource::File("file:///package-removed-after-load".into());
        }
        let resolved = super::connectors(&config, &packages).unwrap();
        assert!(!resolved.is_empty());
    }

    #[tokio::test]
    async fn renamed_manifest_ids_keep_explicit_connector_and_catalog_contracts() {
        let temp = tempfile::tempdir().unwrap();
        let telegram_path = package_path(temp.path(), "telegram");
        let manifest_path = telegram_path.join("plugin.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("dev.pluribus.telegram", "org.example.chat");
        std::fs::write(manifest_path, manifest).unwrap();
        let mut config = crate::fixtures::config();
        set_package(&mut config, "telegram-1", &telegram_path);
        let (mut config, packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &config,
            true,
            false,
        )
        .await
        .unwrap();
        let connectors = super::connectors(&config, &packages).unwrap();
        assert!(
            connectors
                .iter()
                .any(|connector| connector.provider == "telegram"
                    && connector.ingress == "telegram-1/receive"
                    && connector.reply == "telegram-1/send")
        );

        let rlm_path = package_path(temp.path(), "rlm");
        let manifest_path = rlm_path.join("plugin.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("dev.pluribus.rlm", "org.example.reasoner");
        std::fs::write(manifest_path, manifest).unwrap();
        set_package(&mut config, "rlm", &rlm_path);
        let (mut config, packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &config,
            true,
            false,
        )
        .await
        .unwrap();
        super::refresh_cognition_tools(&mut config, &packages).unwrap();
        assert!(config.plugin_instances["rlm"].config["tools"].is_array());
        assert!(
            config.plugin_instances["rlm"].config["components"]
                .as_array()
                .unwrap()
                .iter()
                .any(|component| component["plugin"] == "org.example.reasoner")
        );
    }

    #[tokio::test]
    async fn connector_targets_and_catalog_pointers_are_validated() {
        let temp = tempfile::tempdir().unwrap();
        let telegram_path = package_path(temp.path(), "telegram");
        let manifest_path = telegram_path.join("plugin.toml");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap().replace(
            "reply_component = \"send\"",
            "reply_component = \"missing\"",
        );
        std::fs::write(manifest_path, manifest).unwrap();
        let mut config = crate::fixtures::config();
        set_package(&mut config, "telegram-1", &telegram_path);
        let (mut config, packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &config,
            true,
            false,
        )
        .await
        .unwrap();
        assert!(super::connectors(&config, &packages).is_err());

        let mut disabled = crate::fixtures::config();
        set_package(&mut disabled, "telegram-1", &telegram_path);
        let (mut disabled, disabled_packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &disabled,
            true,
            false,
        )
        .await
        .unwrap();
        disabled
            .plugin_instances
            .get_mut("telegram-1")
            .unwrap()
            .components
            .remove("send");
        assert!(super::connectors(&disabled, &disabled_packages).is_err());

        let rlm_path = package_path(temp.path(), "rlm");
        let manifest_path = rlm_path.join("plugin.toml");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap().replace(
            "components_pointer = \"/components\"",
            "components_pointer = \"/tools\"",
        );
        std::fs::write(manifest_path, manifest).unwrap();
        set_package(&mut config, "rlm", &rlm_path);
        let (mut config, packages) = crate::package_source::prepare_registry(
            &super::Paths::under(temp.path()),
            &config,
            true,
            false,
        )
        .await
        .unwrap();
        assert!(super::refresh_cognition_tools(&mut config, &packages).is_err());
    }

    #[test]
    fn model_selector_routes_only_selected_provider() {
        use pluribus_core::{
            AppendRequest, CommittedEvent, EventId, EventPayload, PrincipalKind, PrincipalRef,
            StreamId, StreamKind,
        };

        let mut config = crate::fixtures::config();
        config.model_instance = Some("codex/main".into());
        assert_eq!(
            super::selected_model_component(&config).unwrap().as_deref(),
            Some("codex-1/main")
        );
        let request = CommittedEvent {
            schema: CommittedEvent::SCHEMA.into(),
            event_id: EventId::new("request"),
            sequence: 1,
            recorded_at_ms: 0,
            request: AppendRequest {
                stream_id: StreamId::new("personal"),
                stream_kind: StreamKind::Agent,
                observed_at_ms: None,
                event_type: "model.requested".into(),
                payload_schema: "pluribus.model-request/1".into(),
                payload: EventPayload::CanonicalJson(
                    serde_json::to_vec(&serde_json::json!({"model":"gpt-5.6-luna"})).unwrap(),
                ),
                actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            },
        };
        let load = |name: &str| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/plugins")
                .join(name);
            pluribus_plugin_package::PluginPackage::load(path).unwrap()
        };
        let codex = load("openai-codex");
        let codex_component = &codex.components()["main"];
        let codex_models = super::configured_models(
            codex_component,
            &config.plugin_instances["codex-1"].config,
            true,
        );
        let codex_subscriptions = pluribus_cognition::Subscriptions::from_manifest(
            codex_component.manifest(),
            &codex_models,
        );
        assert!(codex_subscriptions.accepts(&request));

        let openrouter = load("openrouter");
        let openrouter_component = &openrouter.components()["main"];
        let other_models = super::configured_models(
            openrouter_component,
            &serde_json::json!({"models":["gpt-5.6-luna"]}),
            false,
        );
        let other_subscriptions = pluribus_cognition::Subscriptions::from_manifest(
            openrouter_component.manifest(),
            &other_models,
        );
        assert!(!other_subscriptions.accepts(&request));
    }
    /// A static plugin credential is the record, not a staging area: the
    /// component reads exactly what was enrolled, and re-enrolling replaces it.
    #[tokio::test]
    async fn a_static_plugin_credential_is_stored_whole_and_rotates() {
        use pluribus_core::{InMemoryCredentialStore, PluginCredentialStore, SecretHandle};
        let store = InMemoryCredentialStore::default();
        let handle = SecretHandle::new("mail:account");
        let read = async |provider: &str| {
            store
                .read_plugin_credential(&handle, provider)
                .await
                .unwrap()
        };
        super::store_plugin_credential(
            &store,
            "mail:account",
            "dev.pluribus.email",
            &json!({"username":"a@b.c","password":"first"}),
        )
        .await
        .unwrap();
        let record: Value =
            serde_json::from_slice(&read("dev.pluribus.email").await.unwrap()).unwrap();
        assert_eq!(record["username"], "a@b.c");
        assert_eq!(record["password"], "first");
        // No staging envelope: the component reads the enrolled object itself.
        assert!(record.get("enrollment").is_none());

        super::store_plugin_credential(
            &store,
            "mail:account",
            "dev.pluribus.email",
            &json!({"username":"a@b.c","password":"second"}),
        )
        .await
        .unwrap();
        let rotated: Value =
            serde_json::from_slice(&read("dev.pluribus.email").await.unwrap()).unwrap();
        assert_eq!(rotated["password"], "second");
        // The record is scoped to its provider.
        assert!(read("dev.pluribus.other").await.is_none());
    }

    #[tokio::test]
    async fn plugin_enrollment_stages_input_in_private_storage() {
        use pluribus_core::{InMemoryCredentialStore, PluginCredentialStore, SecretHandle};
        let store = InMemoryCredentialStore::default();
        let handle = SecretHandle::new("test");
        store
            .replace_plugin_credential(&handle, "provider", None, br#"{"app":{"id":7}}"#.to_vec())
            .await
            .unwrap();
        super::stage_plugin_enrollment(
            &store,
            "test",
            "provider",
            "enrollment-1",
            serde_json::json!({"private_key":"fixture-key"}),
        )
        .await
        .unwrap();
        let bytes = store
            .read_plugin_credential(&handle, "provider")
            .await
            .unwrap()
            .unwrap();
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["app"]["id"], 7);
        assert_eq!(record["enrollment"]["id"], "enrollment-1");
        assert_eq!(record["enrollment"]["input"]["private_key"], "fixture-key");
        assert!(record["enrollment"]["expires_at_ms"].as_i64().unwrap() > super::system_now_ms());
        assert!(
            store
                .read_plugin_credential(&handle, "other")
                .await
                .unwrap()
                .is_none()
        );
    }

    use super::*;
    use clap::error::ErrorKind;
    use pluribus_cognition::AuthorityResolver;
    use pluribus_core::{CapabilityName, CommittedEvent, ConstraintSet, Grant};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn requests_without_an_origin_have_no_standing_grants() {
        let resolver = pluribus_cognition::OriginAuthority {
            agent: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            grants: BTreeMap::from([(
                CapabilityName::new("shell.execute"),
                vec![Grant {
                    capability: CapabilityName::new("shell.execute"),
                    provider: None,
                    constraints: ConstraintSet::canonical_json(b"{}"),
                }],
            )]),
            max_depth: 4,
            events: Arc::new(
                SqliteEventStore::open_in_memory(SystemMetadata::default())
                    .await
                    .unwrap(),
            ),
            connectors: vec![pluribus_cognition::Connector {
                provider: "telegram".into(),
                inherits_origin: false,
                ingress: "telegram-1/receive".into(),
                reply: "telegram-1/send".into(),
                reply_capabilities: vec![pluribus_core::CapabilityName::new("telegram.reply")],
            }],
        };
        let event = CommittedEvent {
            schema: "pluribus.event/1".into(),
            event_id: pluribus_core::EventId::new("request"),
            sequence: 1,
            recorded_at_ms: 0,
            request: pluribus_core::AppendRequest {
                stream_id: StreamId::new("personal"),
                stream_kind: pluribus_core::StreamKind::Agent,
                observed_at_ms: None,
                event_type: "capability.requested".into(),
                payload_schema: "test/1".into(),
                payload: pluribus_core::EventPayload::CanonicalJson(b"{}".to_vec()),
                actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            },
        };
        assert!(
            resolver
                .resolve(&event)
                .await
                .map_or(true, |a| !a.permits(&CapabilityName::new("shell.execute")))
        );
    }

    #[tokio::test]
    async fn an_empty_configuration_gains_no_instances() {
        let mut config = Config::default();
        refresh_cognition_tools(&mut config, &package_source::ResolvedPackages::default()).unwrap();
        assert!(config.plugin_instances.is_empty());
    }

    #[tokio::test]
    async fn cognition_catalog_includes_configured_provider_constraints() {
        if !fixtures::packages_are_built() {
            return;
        }
        let mut config = fixtures::config();
        config.capability_instances.push("telegram/send".into());
        config
            .trusted_capabilities
            .insert("telegram/send".into(), vec!["telegram.react".into()]);
        config.trusted_constraints.insert(
            "telegram/send".into(),
            BTreeMap::from([("telegram.react".into(), json!({"conversationId":"chat:1"}))]),
        );
        let (resolved, _) = crate::package_source::prepare_registry(
            &Paths::under(Path::new("/tmp")),
            &config,
            true,
            true,
        )
        .await
        .unwrap();
        config = resolved;
        assert!(
            serde_json::to_value(&config.plugin_instances["rlm"]).unwrap()["config"]
                .get("tools")
                .is_none()
        );
        let tools = config.plugin_instances["rlm"].config["tools"]
            .as_array()
            .unwrap();
        let react = tools
            .iter()
            .find(|tool| tool["name"] == "telegram.react")
            .unwrap();
        assert_eq!(
            react["configuredConstraints"],
            json!([{"conversationId":"chat:1"}])
        );
        assert!(react["parameters"].is_object());
        let components = config.plugin_instances["rlm"].config["components"]
            .as_array()
            .unwrap();
        let receive = components
            .iter()
            .find(|c| c["instanceId"] == "telegram-1/receive")
            .unwrap();
        assert!(
            receive["emits"]
                .as_array()
                .unwrap()
                .contains(&json!("observation.received"))
        );
        assert_eq!(receive["subscribes"], json!([]));
        assert!(
            serde_json::to_value(&config.plugin_instances["rlm"]).unwrap()["config"]
                .get("components")
                .is_none()
        );
    }

    #[test]
    fn default_uses_luna() {
        assert_eq!(Config::default().model, "gpt-5.6-luna");
    }

    #[tokio::test]
    async fn auth_resolves_an_external_instance_from_configuration() {
        let package =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex");
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value["plugin_instances"] = json!({
            "mail-work": {
                "package": url::Url::from_directory_path(&package).unwrap().as_str(),
                "config": {"auth": {"handle": "mail:work"}},
                "components":{"main":{"http": {"origins": ["https://mail.example.com"], "methods": ["POST"]}}}
            }
        });
        let config: Config = serde_json::from_value(value).unwrap();
        let target =
            credential_target(&Paths::under(Path::new("/tmp/agent")), &config, "mail-work")
                .await
                .unwrap();

        assert_eq!(
            target.package.canonicalize().unwrap(),
            package.canonicalize().unwrap()
        );
        assert_eq!(target.instance_id, "mail-work");
        assert_eq!(
            target.config.pointer("/auth/handle"),
            Some(&json!("mail:work"))
        );
        assert_eq!(
            target.components["main"].http.origins,
            ["https://mail.example.com"]
        );
    }

    #[test]
    fn help_is_parsed_before_any_command_runs() {
        let error = Cli::try_parse_from(["pluribus", "--help"]).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    }

    #[test]
    fn data_directory_does_not_override_other_directories() {
        let defaults = Paths::resolve(&Cli::parse_from(["pluribus", "run"]));
        let paths = Paths::resolve(&Cli::parse_from([
            "pluribus",
            "--data-dir",
            "/tmp/isolated-state",
            "run",
        ]));
        assert_eq!(paths.state, PathBuf::from("/tmp/isolated-state"));
        assert_eq!(paths.config, defaults.config);
        assert_eq!(paths.cache, defaults.cache);
        assert_eq!(paths.runtime, defaults.runtime);
    }

    #[test]
    fn help_displays_default_directories() {
        let help = Cli::try_parse_from(["pluribus", "--help"])
            .unwrap_err()
            .to_string();
        for path in [
            pluribus_paths::state(),
            pluribus_paths::config(),
            pluribus_paths::cache(),
            pluribus_paths::runtime(),
        ] {
            assert!(
                help.contains(&format!("[default: {}]", path.display()))
                    || help.contains(&format!("[default: \"{}\"]", path.display())),
                "{help}"
            );
        }
    }

    #[test]
    fn data_directory_is_a_global_flag() {
        let cli = Cli::try_parse_from([
            "pluribus",
            "plugins",
            "auth",
            "telegram",
            "--data-dir",
            "/tmp/agent",
        ])
        .unwrap();

        assert_eq!(cli.data_dir, PathBuf::from("/tmp/agent"));
        // Without the flag, an agent lives where the specification says.
        let default = Cli::try_parse_from(["pluribus", "run"]).unwrap();
        assert_eq!(default.data_dir, pluribus_paths::state());
        // Each kind of thing has its own directory, named for this program.
        let paths = Paths::resolve(&default);
        for directory in [&paths.config, &paths.state, &paths.cache, &paths.runtime] {
            assert!(directory.is_absolute(), "{}", directory.display());
            assert!(directory.ends_with("pluribus"), "{}", directory.display());
        }
        assert!(matches!(
            cli.command,
            Command::Plugins { command: PluginCommand::Auth { plugin } } if plugin == "telegram"
        ));
    }

    #[test]
    fn auth_accepts_plugin_ids_without_core_subcommands() {
        let cli =
            Cli::try_parse_from(["pluribus", "plugins", "auth", "dev.example.provider"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Plugins { command: PluginCommand::Auth { plugin } } if plugin == "dev.example.provider"
        ));
    }

    #[test]
    fn enrollment_display_accepts_url_only_and_rejects_invalid_prompts() {
        assert_eq!(
            enrollment_display_lines(&json!({"url": "https://login.example"})).unwrap(),
            ["Open https://login.example"]
        );
        for prompt in [
            json!({}),
            json!({"url": ""}),
            json!({"url": "https://login.example", "userCode": 123}),
            json!({"url": "https://login.example", "userCode": ""}),
        ] {
            assert!(enrollment_display_lines(&prompt).is_err());
        }
    }

    #[test]
    fn enrollment_display_includes_optional_user_code() {
        assert_eq!(
            enrollment_display_lines(&json!({
                "url": "https://login.example/device",
                "userCode": "ABCD-EFGH"
            }))
            .unwrap(),
            [
                "Open https://login.example/device".to_owned(),
                "User code: ABCD-EFGH".to_owned()
            ]
        );
    }
}
