#[cfg(test)]
mod fixtures;
mod installation;
mod package_source;
mod registry;
mod stop;

use clap::{Parser, Subcommand};
use pluribus_cognition::{Agent, ComponentInstall, Router};
use pluribus_core::{
    BlobStore, DeliveryStore, EventId, EventMetadataSource, EventStore, EventTypeRegistry,
    OAuthCredentialStore, PrincipalKind, PrincipalRef, SecretHandle, StateStore, StreamId,
};
use pluribus_host_oauth::{
    CredentialEnrollment, DeviceCodeStatus, EnrollmentPolicy, ReqwestOAuthTransport, SystemClock,
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
use tracing::{info, warn};

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
    #[arg(long, short = 'd', global = true, value_name = "PATH")]
    data_dir: Option<PathBuf>,
    /// Directory holding `config.json`.
    #[arg(long, global = true, value_name = "PATH")]
    config_dir: Option<PathBuf>,
    /// Directory holding packages fetched by digest.
    #[arg(long, global = true, value_name = "PATH")]
    cache_dir: Option<PathBuf>,
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
        // Naming a directory says where this agent lives, all of it. The
        // platform decides only what nothing else names.
        let Some(state) = cli.data_dir.clone() else {
            return Self {
                state: pluribus_paths::state(),
                config: cli
                    .config_dir
                    .clone()
                    .unwrap_or_else(pluribus_paths::config),
                cache: cli.cache_dir.clone().unwrap_or_else(pluribus_paths::cache),
                runtime: pluribus_paths::runtime(),
            };
        };
        Self {
            config: cli.config_dir.clone().unwrap_or_else(|| state.clone()),
            cache: cli.cache_dir.clone().unwrap_or_else(|| state.clone()),
            runtime: state.clone(),
            state,
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
}

#[derive(Debug, Subcommand)]
enum Command {
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
        /// Use cached packages or local file URLs without network downloads.
        #[arg(long)]
        offline: bool,
    },
    /// Configure provider credentials.
    Auth {
        /// Configured instance ID or alias.
        #[arg(value_name = "INSTANCE")]
        plugin: String,
        /// Adopt the installed refresh recipe without signing in again.
        #[arg(long)]
        adopt_recipe: bool,
    },
    /// Configure a plugin from a package directory or archive.
    Install {
        /// Package directory, or an archive URL with `--sha256`.
        package: String,
        /// Digest of an archive package.
        #[arg(long)]
        sha256: Option<String>,
        /// Instance name. Defaults to the plugin name.
        #[arg(long)]
        id: Option<String>,
    },
    /// Halt cognition and cancel active shell commands.
    Stop,
}

async fn run_command() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let paths = Paths::resolve(&cli);
    let data = &paths;
    match cli.command {
        Command::Init { example } => {
            initialize(data, example).await?;
            if example {
                example_configuration(data).await?;
            }
            Ok(())
        }
        Command::Run { resume, offline } => {
            stop::prepare(data, resume)?;
            run_agent(data, offline).await
        }
        Command::Auth {
            plugin,
            adopt_recipe,
        } => authenticate(data, &plugin, adopt_recipe).await,
        Command::Install {
            package,
            sha256,
            id,
        } => installation::install(data, &package, sha256, id).await,
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
    // `pluribus auth openrouter` replaces it with one of your own.
    const DEMO_KEY: &str =
        "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";

    for name in ["cli", "shell", "openrouter", "rlm"] {
        installation::install_named(data, name).await?;
    }

    let mut config = load_config(data)?;
    config.identity = IDENTITY.into();
    config.model = MODEL.into();
    config.model_instance = Some("openrouter/main".into());
    config.capability_instances = vec!["shell/main".into()];
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
        json!({"credential": "openrouter:personal", "models": [MODEL]}),
    )?;
    set(&mut config, "rlm", json!({}))?;
    save_config(data, &config)?;
    enroll(data, "openrouter", false, json!({"api_key": DEMO_KEY})).await?;

    // The flag is noise when the data directory is the one every command uses
    // without being told.
    let where_ = if data.state == pluribus_paths::state() && data.config == pluribus_paths::config()
    {
        String::new()
    } else {
        format!(" --data-dir {}", data.state.display())
    };
    println!("\nConfigured cli, shell, openrouter and rlm, on a shared key with no");
    println!("budget. Replace it with `pluribus{where_} auth openrouter`.");
    println!("\nStart each of these, in its own terminal:");
    println!("  pluribus-cli-bridge{where_}");
    println!("  pluribus-shell-executor{where_}");
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

async fn authenticate(
    data: &Paths,
    plugin_name: &str,
    adopt_recipe: bool,
) -> Result<(), Box<dyn Error>> {
    let config = load_config(data)?;
    let target = credential_target(data, &config, plugin_name).await?;
    let package = PluginPackage::load(&target.package)?;
    let descriptors = credential_descriptors(&package, &target)?;
    let descriptor = select_credential_descriptor(&descriptors)?;
    let input = prompt_credential_input(&descriptor.input_schema)?;
    enroll(data, plugin_name, adopt_recipe, input).await
}

/// Stores one credential for a configured instance.
async fn enroll(
    data: &Paths,
    plugin_name: &str,
    adopt_recipe: bool,
    input: Value,
) -> Result<(), Box<dyn Error>> {
    let config = load_config(data)?;
    let database = open_database(&data.state).await?;
    let target = credential_target(data, &config, plugin_name).await?;
    let package = PluginPackage::load(&target.package)?;
    let descriptors = credential_descriptors(&package, &target)?;
    let descriptor = select_credential_descriptor(&descriptors)?;
    let handle = target
        .config
        .pointer(&descriptor.config_pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("credential config pointer does not resolve to a handle")?;
    let policy = EnrollmentPolicy {
        components: descriptor.components.clone(),
        enrollment_origins: descriptor.enrollment_origins.clone(),
        injection_origins: descriptor.injection_origins.clone(),
    };
    let store: Arc<dyn OAuthCredentialStore> = database;
    let transport = Arc::new(ReqwestOAuthTransport::new()?);
    let enrollment = CredentialEnrollment::new(store, transport, Arc::new(SystemClock));
    if adopt_recipe {
        if descriptor.flow_schema != "pluribus:credential/oauth-device@1" {
            return Err("recipe adoption requires an oauth-device@1 declaration".into());
        }
        enrollment
            .adopt_device_recipe(
                &descriptor.input_schema,
                &descriptor.flow,
                &input,
                &SecretHandle::new(handle),
                &policy,
            )
            .await
            .map_err(|_| {
                warn!(
                    instance = %target.instance_id,
                    %handle,
                    "credential refresh recipe adoption failed"
                );
                "recipe adoption failed; reauthorize with auth without --adopt-recipe"
            })?;
        println!("{} refresh recipe adopted.", descriptor.display_name);
        return Ok(());
    }
    match descriptor.flow_schema.as_str() {
        "pluribus:credential/static-http@1" => {
            enrollment
                .enroll_static(
                    &descriptor.input_schema,
                    &descriptor.flow,
                    &input,
                    &SecretHandle::new(handle),
                    &policy,
                )
                .await?;
        }
        "pluribus:credential/oauth-device@1" => {
            CredentialEnrollment::validate_device_flow(&descriptor.flow_schema, &descriptor.flow)?;
            let mut session = enrollment
                .begin_device(
                    &descriptor.input_schema,
                    &descriptor.flow,
                    &input,
                    SecretHandle::new(handle),
                    policy,
                )
                .await?;
            println!("\n{}", descriptor.display_name);
            println!("1. Open {}", session.prompt().verification_url);
            println!("2. Enter {}", session.prompt().user_code);
            print!("Waiting for approval");
            io::stdout().flush()?;
            let mut interval = session.prompt().interval;
            loop {
                match enrollment.poll_device(&mut session).await? {
                    DeviceCodeStatus::Pending => {
                        print!(".");
                        io::stdout().flush()?;
                        tokio::time::sleep(interval).await;
                    }
                    DeviceCodeStatus::SlowDown => {
                        interval += Duration::from_secs(5);
                        tokio::time::sleep(interval).await;
                    }
                    DeviceCodeStatus::Authorized => break,
                }
            }
        }
        schema => return Err(format!("unsupported credential flow: {schema}").into()),
    }
    info!(
        instance = %target.instance_id,
        %handle,
        flow = %descriptor.flow_schema,
        "configured credential handle"
    );
    println!("\n{} configured.", descriptor.display_name);
    Ok(())
}

struct CredentialTarget {
    package: PathBuf,
    instance_id: String,
    config: Value,
    enrollment_origins: Vec<String>,
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
    Ok(CredentialTarget {
        package: path,
        instance_id: id.into(),
        config: instance.config.clone(),
        enrollment_origins: instance.enrollment_origins.clone(),
        components: instance.components.clone(),
    })
}

/// Credential enrollment a plugin declares.
///
/// Read from the manifest: enrollment is static data, so discovering it no
/// longer instantiates the component.
struct CredentialDescriptor {
    enrollment_origins: Vec<String>,
    components: Vec<PrincipalRef>,
    injection_origins: Vec<String>,
    display_name: String,
    config_pointer: String,
    input_schema: Value,
    flow_schema: String,
    flow: Value,
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
            let consumers = declaration
                .components
                .iter()
                .map(|name| {
                    target.components.get(name).ok_or_else(|| {
                        format!("credential references unconfigured component: {name}")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let injection_origins = consumers
                .first()
                .map(|access| {
                    access
                        .http
                        .origins
                        .iter()
                        .filter(|origin| {
                            consumers
                                .iter()
                                .all(|consumer| consumer.http.origins.contains(origin))
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let enrollment_origins = target
                .enrollment_origins
                .iter()
                .filter(|origin| {
                    declaration
                        .components
                        .iter()
                        .all(|name| allows_http(&package.components()[name], origin, "POST"))
                })
                .cloned()
                .collect();
            Ok(CredentialDescriptor {
                enrollment_origins,
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
                injection_origins,
                display_name: declaration.display_name.clone(),
                config_pointer: declaration.config_pointer.clone(),
                input_schema: package_json(package, &declaration.input_schema)?,
                flow_schema: declaration.flow_schema.clone(),
                flow: package_json(package, &declaration.flow)?,
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
            return Err("multiple credential fields require an interactive terminal".into());
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
        let value = if secret {
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
    for origin in &target.enrollment_origins {
        if !package
            .components()
            .values()
            .any(|component| allows_http(component, origin, "POST"))
        {
            return Err(format!(
                "enrollment grant exceeds manifest for {}",
                target.instance_id
            )
            .into());
        }
    }
    Ok(())
}

/// Supplies core capability and trust context to RLM instances.
async fn refresh_cognition_tools(data: &Paths, config: &mut Config) -> Result<(), Box<dyn Error>> {
    let mut rlm_instances = Vec::new();
    let mut tools = Vec::new();
    let grants = config.trusted_grants()?;
    for (id, instance) in &config.plugin_instances {
        let package = PluginPackage::load(resolve_plugin(data, &instance.package).await?)?;
        if package.manifest().id == "dev.pluribus.rlm" {
            rlm_instances.push(id.clone());
        }
        for (name, component) in package.components() {
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
    for id in rlm_instances {
        let instance = config
            .plugin_instances
            .get_mut(&id)
            .expect("resolved RLM instance");
        instance.config["tools"] = json!(tools);
    }
    Ok(())
}

/// Every way in and out the configuration installs.
///
/// A component that emits observations is one provider's senses; the component
/// answering for it is the one providing `<provider>.reply`. The provider is
/// the plugin name: the last segment of its manifest ID.
async fn connectors(
    data: &Paths,
    config: &Config,
) -> Result<Vec<pluribus_cognition::Connector>, Box<dyn Error>> {
    let mut ingress: BTreeMap<String, String> = BTreeMap::new();
    let mut replies: BTreeMap<String, (String, Vec<pluribus_core::CapabilityName>)> =
        BTreeMap::new();
    for (id, instance) in &config.plugin_instances {
        let package = PluginPackage::load(resolve_plugin(data, &instance.package).await?)?;
        let manifest_id = &package.manifest().id;
        let provider = manifest_id
            .rsplit('.')
            .next()
            .unwrap_or(manifest_id)
            .to_owned();
        let reply = format!("{provider}.reply");
        for (name, component) in package.components() {
            if !instance.components.contains_key(name) {
                continue;
            }
            let selector = pluribus_plugin_package::component_id(id, name);
            let manifest = component.manifest();
            if manifest
                .emits
                .iter()
                .any(|event| event == "observation.received")
            {
                ingress.insert(provider.clone(), selector.clone());
            }
            if manifest
                .provides
                .iter()
                .any(|capability| capability.capability == reply)
            {
                let capabilities = manifest
                    .provides
                    .iter()
                    .map(|capability| pluribus_core::CapabilityName::new(&capability.capability))
                    .collect();
                replies.insert(provider.clone(), (selector, capabilities));
            }
        }
    }
    Ok(ingress
        .into_iter()
        .map(|(provider, ingress)| {
            let (reply, reply_capabilities) = replies.remove(&provider).unwrap_or_default();
            pluribus_cognition::Connector {
                provider,
                ingress,
                reply,
                reply_capabilities,
            }
        })
        .collect())
}

const IDLE_MIN: Duration = Duration::from_millis(50);
const IDLE_MAX: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_lines)]
async fn run_agent(data: &Paths, offline: bool) -> Result<(), Box<dyn Error>> {
    let config = package_source::prepare(data, &load_config(data)?, offline, true).await?;
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

    let mut enrollment_policies = Vec::new();
    for id in config.plugin_instances.keys() {
        let target = credential_target(data, &config, id).await?;
        let package = PluginPackage::load(&target.package)?;
        validate_instance_package(&package, &target)?;
        for name in package.components().keys() {
            enrollment_policies.push((
                PrincipalRef::new(
                    PrincipalKind::Component,
                    pluribus_plugin_package::component_id(id, name),
                ),
                target
                    .enrollment_origins
                    .iter()
                    .filter(|origin| allows_http(&package.components()[name], origin, "POST"))
                    .cloned()
                    .collect(),
            ));
        }
    }
    let credentials = Arc::new(
        pluribus_host_oauth::RefreshingCredentialStore::new(
            Arc::clone(&database) as Arc<dyn OAuthCredentialStore>,
            Arc::new(ReqwestOAuthTransport::new()?),
            Arc::new(SystemClock),
        )
        .with_enrollment_policies(enrollment_policies),
    );
    let http: Arc<dyn pluribus_core::HttpStreamService> = Arc::new(
        pluribus_host_http::PolicyHttpService::new(Arc::clone(&blobs), credentials),
    );
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&database) as Arc<dyn StateStore>,
        Arc::clone(&database) as Arc<dyn EventStore>,
        Arc::clone(&blobs),
        Arc::clone(&database) as Arc<dyn DeliveryStore>,
    )?;
    let connectors = connectors(data, &config).await?;
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

    let mut credential_handles = std::collections::HashSet::new();
    let mut cognition = false;
    for (id, instance) in &config.plugin_instances {
        let package = PluginPackage::load(resolve_plugin(data, &instance.package).await?)?;
        cognition |= package.components().iter().any(|(name, component)| {
            component.manifest().subscribes_to_stream() && instance.components.contains_key(name)
        });
        for declaration in &package.manifest().credentials {
            if let Some(handle) = instance
                .config
                .pointer(&declaration.config_pointer)
                .and_then(Value::as_str)
            {
                credential_handles.insert(SecretHandle::new(handle));
            }
        }
        let mut installs = BTreeMap::new();
        for (name, component) in package.components() {
            let access = &instance.components[name];
            let component_id = pluribus_plugin_package::component_id(id, name);
            let projected = component.project_config(&instance.config)?;
            let services = PluginServices {
                model: Some(config.model.clone()),
                identity: Some(config.identity.clone()),
                http: Some(Arc::clone(&http)),
                http_grant: Some(access.http.grant(&component_id)),
                stream: if access.stream.is_some() {
                    Some(Arc::new(pluribus_host_stream::LocalStreamService::new(
                        &data.state,
                    )?))
                } else {
                    None
                },
                stream_grant: access.stream.as_ref().map(StreamAccess::grant),
                limits: access.limits.map(InstanceLimits::runtime),
            };
            let models = component
                .manifest()
                .model_provider
                .as_ref()
                .map(|provider| {
                    projected
                        .pointer(&provider.models_pointer)
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|v| v.as_str().or_else(|| v["id"].as_str()))
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default();
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
    let mut credential_cursor = 0;
    loop {
        if stop::is_stopped(data) {
            info!(agent = %config.agent_id, reason = %"emergency_stop", "stopping agent");
            println!("Emergency stop is set; halting.");
            return Ok(());
        }
        publish_credential_lifecycle(
            &database,
            &config.agent_id,
            &credential_handles,
            &mut credential_cursor,
        )
        .await?;
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

async fn publish_credential_lifecycle(
    database: &SqliteEventStore<SystemMetadata>,
    agent_id: &str,
    handles: &std::collections::HashSet<SecretHandle>,
    cursor: &mut u64,
) -> Result<(), Box<dyn Error>> {
    let registry = EventTypeRegistry::core();
    for entry in database.lifecycle_after(*cursor, 100).await? {
        if handles.contains(&entry.handle) {
            let request = pluribus_core::AppendRequest {
                stream_id: StreamId::new(agent_id),
                stream_kind: pluribus_core::StreamKind::Agent,
                observed_at_ms: Some(entry.at_ms),
                event_type: "credential.lifecycle".into(),
                payload_schema: "pluribus.credential-lifecycle/1".into(),
                payload: pluribus_core::EventPayload::CanonicalJson(serde_json::to_vec(&json!({
                    "sourceSequence":entry.sequence,
                    "handle":entry.handle.as_str(),
                    "generation":entry.generation,
                    "outcome":entry.outcome,
                    "atMs":entry.at_ms,
                    "deadlineMs":entry.deadline_ms,
                }))?),
                actor: PrincipalRef::new(PrincipalKind::Node, "credential-host"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: Some(format!("credential-lifecycle:{}", entry.sequence)),
            };
            registry.validate(&request)?;
            let committed = database.append(request.clone()).await?;
            if committed.request != request {
                return Err("credential lifecycle deduplication collision".into());
            }
        }
        *cursor = entry.sequence;
    }
    Ok(())
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
        refresh_cognition_tools(&Paths::under(Path::new("/tmp")), &mut config)
            .await
            .unwrap();
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
        refresh_cognition_tools(&Paths::under(Path::new("/tmp")), &mut config)
            .await
            .unwrap();
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
                "components":{"main":{"http": {"origins": ["https://mail.example.com"], "methods": ["POST"]}}},
                "enrollment_origins": ["https://login.example.com"]
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
    fn data_directory_is_a_global_flag() {
        let cli = Cli::try_parse_from(["pluribus", "auth", "telegram", "--data-dir", "/tmp/agent"])
            .unwrap();

        assert_eq!(cli.data_dir, Some(PathBuf::from("/tmp/agent")));
        // Without the flag, an agent lives where the specification says.
        let default = Cli::try_parse_from(["pluribus", "run"]).unwrap();
        assert_eq!(default.data_dir, None);
        // Each kind of thing has its own directory, named for this program.
        let paths = Paths::resolve(&default);
        for directory in [&paths.config, &paths.state, &paths.cache, &paths.runtime] {
            assert!(directory.is_absolute(), "{}", directory.display());
            assert!(directory.ends_with("pluribus"), "{}", directory.display());
        }
        assert!(matches!(
            cli.command,
            Command::Auth { plugin, adopt_recipe: false } if plugin == "telegram"
        ));
    }

    #[test]
    fn auth_accepts_plugin_ids_without_core_subcommands() {
        let cli = Cli::try_parse_from(["pluribus", "auth", "dev.example.provider"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Auth { plugin, adopt_recipe: false } if plugin == "dev.example.provider"
        ));
    }
}

#[cfg(test)]
mod credential_health_tests {
    use super::*;

    #[tokio::test]
    async fn credential_audit_projection_is_scoped_and_deduplicates_restart() {
        let database = SqliteEventStore::open_in_memory(SystemMetadata::default())
            .await
            .unwrap();
        let credential = pluribus_core::HttpCredential {
            headers: vec![pluribus_core::SecretHeader {
                name: "authorization".into(),
                value: b"Bearer fixture-secret".to_vec(),
            }],
            path_prefix: None,
            allowed_origins: vec!["https://example.com".into()],
            allowed_components: vec![PrincipalRef::new(PrincipalKind::Component, "fixture")],
        };
        let handle = SecretHandle::new("fixture");
        database.put_http(&handle, &credential).await.unwrap();
        database
            .put_http(&SecretHandle::new("unrelated"), &credential)
            .await
            .unwrap();
        let handles = std::collections::HashSet::from([handle]);
        publish_credential_lifecycle(&database, "personal", &handles, &mut 0)
            .await
            .unwrap();
        publish_credential_lifecycle(&database, "personal", &handles, &mut 0)
            .await
            .unwrap();
        let events = database
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request.event_type, "credential.lifecycle");
        let pluribus_core::EventPayload::CanonicalJson(bytes) = &events[0].request.payload else {
            panic!("inline audit required")
        };
        let value: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(value["handle"], "fixture");
        assert_eq!(value["outcome"], "enrolled");
        assert!(!String::from_utf8_lossy(bytes).contains("fixture-secret"));
        assert!(
            EventTypeRegistry::core()
                .authorize(&events[0].request, &["*".into()])
                .is_err()
        );
    }

    #[tokio::test]
    async fn credential_audit_dedup_collision_is_not_silently_consumed() {
        let database = SqliteEventStore::open_in_memory(SystemMetadata::default())
            .await
            .unwrap();
        let handle = SecretHandle::new("fixture");
        database
            .put_http(
                &handle,
                &pluribus_core::HttpCredential {
                    headers: vec![pluribus_core::SecretHeader {
                        name: "authorization".into(),
                        value: b"Bearer fixture-secret".to_vec(),
                    }],
                    path_prefix: None,
                    allowed_origins: vec!["https://example.com".into()],
                    allowed_components: vec![PrincipalRef::new(
                        PrincipalKind::Component,
                        "fixture",
                    )],
                },
            )
            .await
            .unwrap();
        let sequence = database.lifecycle_after(0, 1).await.unwrap()[0].sequence;
        database
            .append(pluribus_core::AppendRequest {
                stream_id: StreamId::new("personal"),
                stream_kind: pluribus_core::StreamKind::Agent,
                observed_at_ms: None,
                event_type: "cognition.completed".into(),
                payload_schema: "test/1".into(),
                payload: pluribus_core::EventPayload::CanonicalJson(b"{}".to_vec()),
                actor: PrincipalRef::new(PrincipalKind::Component, "fixture"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: Some(format!("credential-lifecycle:{sequence}")),
            })
            .await
            .unwrap();
        let mut cursor = 0;
        assert!(
            publish_credential_lifecycle(
                &database,
                "personal",
                &std::collections::HashSet::from([handle]),
                &mut cursor
            )
            .await
            .is_err()
        );
        assert_eq!(cursor, 0);
    }

    #[test]
    fn recipe_adoption_is_an_explicit_auth_option() {
        assert!(Cli::try_parse_from(["pluribus", "auth", "codex", "--adopt-recipe"]).is_ok());
    }
}
