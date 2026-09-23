//! Sandboxed execution for validated Pluribus components.
//!
//! One world, so one instantiation path and one implementation of each host
//! import. A delivery's proposed events, state mutations, and cursor advance
//! commit in a single transaction.

mod runner;
mod wasi;
mod wasi_http;
pub use runner::PluginInstance;

#[cfg(test)]
mod tests;

use pluribus_core::{
    AppendRequest, AuthorityId, BlobError, BlobRef, BlobStore, BlobUploadId, CommittedEvent,
    CursorKey, DeliveryCommit, DeliveryStore, EventId, EventPayload, EventQuery, EventStore,
    EventTypeRegistry, HttpError, HttpGrant, HttpHeader, HttpRequest, HttpStreamProtocol,
    HttpStreamService, PrincipalKind as CorePrincipalKind, PrincipalRef, SecretHandle, StateError,
    StateMutation, StateNamespace, StateStore, StreamError, StreamGrant, StreamId, StreamKind,
    StreamService, validate_blob_ref,
};
use pluribus_plugin_bindings as bindings;
use pluribus_plugin_package::PluginComponent;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime::component::{Component, Linker, Resource};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder};

use bindings::exports::pluribus::plugin::lifecycle as guest;
use bindings::pluribus::plugin::{
    blobs, credentials, events, runtime as execution, socket, state, types,
};

const MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;
const EPOCH_TICK: Duration = Duration::from_millis(10);
const MAX_STATE_KEY_BYTES: usize = 512;
const MAX_STATE_VALUE_BYTES: usize = 1024 * 1024;
const MAX_STATE_MUTATIONS: usize = 256;
const MAX_STATE_SCAN_ENTRIES: u32 = 1000;
const MAX_BLOB_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_EVENT_PAGE: u32 = 1000;
const MAX_OUTCOME_EVENTS: usize = 1000;

const ABI_WORLD: &str = "pluribus:plugin/plugin@3.0.0";

/// Ceilings on one call. Compute is bounded by wall clock, not by an
/// instruction count: epoch interruption stops the same runaway loops that
/// fuel metering would, in the unit an operator can reason about, and
/// without taxing every instruction a guest executes. An interpreter guest
/// costs thousands of instructions per unit of useful work, so an
/// instruction budget is unsettable for it in any case.
#[derive(Clone, Debug)]
pub struct RuntimeLimits {
    pub memory_bytes: usize,
    pub call_timeout: Duration,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 32 * 1024 * 1024,
            call_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrincipalKind {
    Human,
    Agent,
    Node,
    Component,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub id: String,
}

/// Host-assigned provenance for one delivery.
///
/// Only `instance_id`, `agent`, the state checkpoint, `depth` and the deadline
/// cross the ABI. The rest stamps proposed events, so a plugin cannot forge or
/// widen its own provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delivery {
    pub instance_id: String,
    pub agent: Principal,
    pub actor: Principal,
    pub authority_id: String,
    pub activity_id: String,
    pub correlation_id: String,
    pub origin_event_id: String,
    pub depth: u32,
    pub deadline_at_ms: Option<i64>,
    pub visible_blobs: Vec<BlobRef>,
}

/// What one instance is granted: host services, and the ceilings it runs
/// under. An interpreter and a connector have nothing in common here, so
/// the runtime's defaults are a fallback rather than a shared budget.
#[derive(Clone, Default)]
pub struct PluginServices {
    pub credentials: Option<CredentialAccess>,
    /// Default model for requests that omit a selection.
    pub model: Option<String>,
    /// Agent instructions supplied to model requests.
    pub identity: Option<String>,
    pub http: Option<Arc<dyn HttpStreamService>>,
    pub http_grant: Option<HttpGrant>,
    /// Endpoints this component may reach, by the name `socket.connect`
    /// selects them with. A name absent here is denied.
    pub streams: BTreeMap<String, GrantedStream>,
    pub limits: Option<RuntimeLimits>,
}

/// One named endpoint: the transport that reaches it and its ceilings.
///
/// Each endpoint carries its own transport so that a connection ceiling
/// bounds that endpoint rather than the component's traffic as a whole.
#[derive(Clone)]
pub struct GrantedStream {
    pub service: Arc<dyn StreamService>,
    pub grant: StreamGrant,
}

#[derive(Clone)]
pub struct CredentialAccess {
    pub store: Arc<dyn pluribus_core::PluginCredentialStore>,
    pub provider: String,
    pub handles: HashSet<String>,
    pub exports: std::collections::BTreeMap<String, CredentialExport>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialExport {
    pub credential: String,
    pub provider: String,
    pub export: String,
}

impl CredentialAccess {
    pub async fn resolve_export(&self, binding: &str) -> Result<String, types::Error> {
        let grant = self.exports.get(binding).ok_or_else(|| {
            host_error(
                types::ErrorCode::PermissionDenied,
                "credential export not granted",
            )
        })?;
        let unavailable = || {
            host_error(
                types::ErrorCode::Unavailable,
                "credential export unavailable",
            )
        };
        let bytes = self
            .store
            .read_plugin_credential(&SecretHandle::new(&grant.credential), &grant.provider)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let export = &doc["exports"][&grant.export];
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if export["expires_at_ms"].as_i64().ok_or_else(unavailable)? <= now.saturating_add(30_000) {
            return Err(unavailable());
        }
        export["value"]
            .as_str()
            .filter(|v| !v.is_empty() && !v.contains('\0'))
            .map(str::to_owned)
            .ok_or_else(unavailable)
    }
}

/// What one committed delivery produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub events: Vec<CommittedEvent>,
    pub checkpoint: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResourceExhaustion {
    pub resource: String,
    #[serde(rename = "currentBytes")]
    pub current_bytes: usize,
    #[serde(rename = "requestedBytes")]
    pub requested_bytes: usize,
    #[serde(rename = "limitBytes")]
    pub limit_bytes: usize,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    message: String,
    trapped: bool,
    resource_exhaustion: Option<Box<ResourceExhaustion>>,
}

impl RuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            trapped: false,
            resource_exhaustion: None,
        }
    }

    /// Unclassified component termination without a guest outcome.
    fn trap(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            trapped: true,
            resource_exhaustion: None,
        }
    }

    fn resource_limit(resource: ResourceExhaustion, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            trapped: true,
            resource_exhaustion: Some(Box::new(resource)),
        }
    }

    #[must_use]
    pub fn resource_exhaustion(&self) -> Option<&ResourceExhaustion> {
        self.resource_exhaustion.as_deref()
    }

    /// Whether execution terminated without a guest outcome.
    #[must_use]
    pub const fn trapped(&self) -> bool {
        self.trapped
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeError {}

#[derive(Clone)]
pub struct CancellationHandle {
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancellationHandle {
    /// Abandons the delivery in flight. A source loop between deliveries keeps
    /// running; only [`Self::shutdown`] ends it.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake();
    }

    /// Ends the instance: the delivery in flight and the source loop both stop.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Re-arms for the next delivery. Shutdown is terminal and survives it.
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::Release);
    }

    #[cfg(test)]
    fn of(state: &HostState) -> Self {
        Self {
            cancelled: Arc::clone(&state.cancelled),
            shutdown: Arc::clone(&state.shutdown),
            notify: state.cancel_notify.clone(),
        }
    }
}

struct EpochTicker {
    engine: Engine,
}

fn start_epoch_ticker(ticker: &Arc<EpochTicker>) {
    let ticker = Arc::downgrade(ticker);
    thread::spawn(move || {
        loop {
            thread::sleep(EPOCH_TICK);
            if let Some(ticker) = ticker.upgrade() {
                ticker.engine.increment_epoch();
            } else {
                break;
            }
        }
    });
}

#[derive(Clone)]
pub struct Runtime {
    ticker: Arc<EpochTicker>,
    limits: RuntimeLimits,
    state_store: Arc<dyn StateStore>,
    event_store: Arc<dyn EventStore>,
    blob_store: Arc<dyn BlobStore>,
    delivery_store: Arc<dyn DeliveryStore>,
    registry: Arc<EventTypeRegistry>,
    progress: Arc<tokio::sync::Notify>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Runtime {
    /// Builds a runtime over the authoritative stores.
    ///
    /// # Errors
    ///
    /// Returns an error when the Wasmtime engine cannot be configured.
    pub fn new(
        limits: RuntimeLimits,
        state_store: Arc<dyn StateStore>,
        event_store: Arc<dyn EventStore>,
        blob_store: Arc<dyn BlobStore>,
        delivery_store: Arc<dyn DeliveryStore>,
    ) -> Result<Self, RuntimeError> {
        if limits.memory_bytes > MAX_MEMORY_BYTES {
            return Err(RuntimeError::new(
                "configured memory exceeds the ABI maximum",
            ));
        }
        let mut config = Config::new();
        config
            .concurrency_support(true)
            .wasm_component_model(true)
            .wasm_component_model_async(true)
            .epoch_interruption(true);
        let engine = Engine::new(&config)
            .map_err(|error| RuntimeError::new(format!("cannot configure Wasmtime: {error}")))?;
        let ticker = Arc::new(EpochTicker { engine });
        start_epoch_ticker(&ticker);
        Ok(Self {
            ticker,
            limits,
            state_store,
            event_store,
            blob_store,
            delivery_store,
            registry: Arc::new(EventTypeRegistry::core()),
            progress: Arc::new(tokio::sync::Notify::new()),
            clock: Arc::new(system_time_ms),
        })
    }

    /// Sets the clock used by plugin source loops.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Notification shared with the agent driving this runtime.
    #[must_use]
    pub fn progress_notification(&self) -> Arc<tokio::sync::Notify> {
        self.progress.clone()
    }

    /// Compiles and instantiates a validated plugin.
    ///
    /// # Errors
    ///
    /// Rejects a foreign world, invalid configuration, unresolved imports, and
    /// instantiation traps.
    pub async fn instantiate(
        &self,
        package: &PluginComponent,
        config: &Value,
        delivery: Delivery,
        services: PluginServices,
    ) -> Result<PluginInstance, RuntimeError> {
        let expected_principal =
            PrincipalRef::new(CorePrincipalKind::Component, &delivery.instance_id);
        if services
            .http_grant
            .as_ref()
            .is_some_and(|grant| grant.component != expected_principal)
        {
            return Err(RuntimeError::new(
                "host grant belongs to another component instance",
            ));
        }
        let manifest = package.manifest();
        if manifest.world != ABI_WORLD {
            return Err(RuntimeError::new(format!(
                "unsupported runtime world: {}",
                manifest.world
            )));
        }
        package
            .validate_config(config)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        for blob in &delivery.visible_blobs {
            validate_blob_ref(blob)
                .map_err(|error| RuntimeError::new(format!("invalid visible blob: {error}")))?;
        }
        let config_bytes = serde_json::to_vec(config)
            .map_err(|error| RuntimeError::new(format!("cannot encode configuration: {error}")))?;

        let limits = services
            .limits
            .clone()
            .unwrap_or_else(|| self.limits.clone());
        if limits.memory_bytes > MAX_MEMORY_BYTES {
            return Err(RuntimeError::new(
                "configured memory exceeds the ABI maximum",
            ));
        }

        let engine = self.ticker.engine.clone();
        let bytes = package.component().to_vec();
        let component = runtime_io(move || Component::new(&engine, &bytes))
            .await?
            .map_err(|error| RuntimeError::new(format!("cannot compile component: {error}")))?;
        let recipe = Arc::new(InstanceRecipe {
            runtime: self.clone(),
            component,
            delivery,
            services,
            emits: manifest.emits.clone(),
            pinned_session: manifest.pinned_session,
        });
        self.instantiate_recipe(recipe, config_bytes, limits).await
    }

    async fn instantiate_recipe(
        &self,
        recipe: Arc<InstanceRecipe>,
        config_bytes: Vec<u8>,
        limits: RuntimeLimits,
    ) -> Result<PluginInstance, RuntimeError> {
        let delivery = recipe.delivery.clone();
        let services = recipe.services.clone();
        let component = recipe.component.clone();
        let mut linker = Linker::new(&self.ticker.engine);
        wasi_http::add_to_linker(&mut linker)
            .map_err(|error| RuntimeError::new(format!("cannot link host imports: {error}")))?;

        let cancellation = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let instance_id = delivery.instance_id.clone();
        let cursor = CursorKey {
            stream_id: StreamId::new(delivery.agent.id.clone()),
            namespace: StateNamespace::new(instance_id.clone()),
        };
        let delivery_store = self.delivery_store.clone();
        let checkpoint_cursor = cursor.clone();
        let checkpoint = delivery_store
            .checkpoint(&checkpoint_cursor)
            .await
            .map_err(|error| RuntimeError::new(format!("cannot read cursor: {error}")))?;

        let host = HostState::new(
            limits.memory_bytes,
            delivery,
            Arc::clone(&cancellation),
            HostServices {
                state: Arc::clone(&self.state_store),
                events: Arc::clone(&self.event_store),
                blobs: Arc::clone(&self.blob_store),
                registry: Arc::clone(&self.registry),
                progress: self.progress.clone(),
                clock: self.clock.clone(),
            },
            services,
            recipe.emits.clone(),
        );
        let mut host = host;
        host.cancel_notify = cancel_notify.clone();
        host.shutdown = Arc::clone(&shutdown);
        let mut store = Store::new(&self.ticker.engine, host);
        store.limiter(|state| &mut state.limits);
        prepare_call(&mut store, &limits, "startup");
        let instance = match linker.instantiate_async(&mut store, &component).await {
            Ok(instance) => instance,
            Err(error) => {
                return Err(store
                    .data()
                    .resource_error(format!("cannot instantiate component: {error}")));
            }
        };
        let plugin = bindings::Plugin::new(&mut store, &instance)
            .map_err(|error| RuntimeError::new(format!("cannot bind component: {error}")))?;

        Ok(PluginInstance::new(InstanceCore {
            _ticker: Arc::clone(&self.ticker),
            limits,
            store,
            plugin,
            cancellation: CancellationHandle {
                cancelled: cancellation,
                shutdown,
                notify: cancel_notify,
            },
            instance_id,
            cursor,
            checkpoint,
            config: config_bytes,
            delivery_store: Arc::clone(&self.delivery_store),
            interrupted: false,
            run_task: None,
            run_result: None,
            activation: None,
            recipe,
        }))
    }
}

struct InstanceRecipe {
    runtime: Runtime,
    component: Component,
    delivery: Delivery,
    services: PluginServices,
    emits: Vec<String>,
    pinned_session: bool,
}

/// One instantiated plugin. Exported calls are serialized by ownership.
struct InstanceCore {
    recipe: Arc<InstanceRecipe>,
    _ticker: Arc<EpochTicker>,
    limits: RuntimeLimits,
    store: Store<HostState>,
    plugin: bindings::Plugin,
    cancellation: CancellationHandle,
    instance_id: String,
    cursor: CursorKey,
    checkpoint: u64,
    config: Vec<u8>,
    delivery_store: Arc<dyn DeliveryStore>,
    interrupted: bool,
    run_task: Option<wasmtime::component::JoinHandle>,
    run_result: Option<tokio::sync::oneshot::Receiver<Result<(), RuntimeError>>>,
    activation: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Clone, Copy)]
enum CommitPhase {
    Init,
    Handle,
    Stop,
}

impl CommitPhase {
    fn derived_key(self, instance: &str, checkpoint: u64, ordinal: usize) -> String {
        match self {
            Self::Handle => format!("{instance}:{checkpoint}:{ordinal}"),
            Self::Init => format!("{instance}:{checkpoint}:{ordinal}:init"),
            Self::Stop => format!("{instance}:{checkpoint}:{ordinal}:stop"),
        }
    }
}

impl InstanceCore {
    /// Runs setup to the readiness boundary before any delivery.
    ///
    /// # Errors
    ///
    /// Returns an error on a trap, a plugin error, or a rejected commit.
    pub async fn init(&mut self) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.init_inner().await;
        self.interrupted = result.is_err();
        result
    }

    async fn init_inner(&mut self) -> Result<Outcome, RuntimeError> {
        if self.run_task.is_some() {
            return Err(RuntimeError::new("plugin already initialized"));
        }
        let context = self.context();
        prepare_call(&mut self.store, &self.limits, "startup");
        let (startup, ready) = tokio::sync::oneshot::channel();
        let (activation, activated) = tokio::sync::oneshot::channel();
        let (completed, mut result) = tokio::sync::oneshot::channel();
        self.store.data_mut().runner.startup = Some(startup);
        self.store.data_mut().runner.activation = Some(activated);
        self.activation = Some(activation);
        self.run_task = Some(
            self.store
                .spawn(runner::RunTask {
                    lifecycle: self.plugin.pluribus_plugin_lifecycle().clone(),
                    context,
                    config: self.config.clone(),
                    completed,
                    resource_evidence: self.store.data().resource_evidence.clone(),
                })
                .map_err(|e| RuntimeError::trap(e.to_string()))?,
        );
        let startup = tokio::time::timeout(self.limits.call_timeout,
            self.store.run_concurrent(async |_accessor| {
                tokio::select! {
                    biased;
                    finished = &mut result => Err(finished.ok().and_then(Result::err)
                        .unwrap_or_else(|| RuntimeError::trap("run exited before ready"))),
                    outcome = ready => outcome.map_err(|_| RuntimeError::trap("run exited before ready")),
                }
            })
        ).await.map_err(|_| self.store.data().resource_error("startup deadline exceeded"))?
            .map_err(|e| self.store.data().resource_error(format!("startup trapped: {e:#}")))??;
        let origin = EventId::new(self.store.data().delivery.origin_event_id.clone());
        let outcome = self
            .commit(&startup, Some(&origin), CommitPhase::Init)
            .await?;
        self.run_result = Some(result);
        Ok(outcome)
    }

    /// Delivers events in ascending sequence order.
    ///
    /// # Errors
    ///
    /// Returns an error on a trap, a plugin error, or a rejected commit.
    pub async fn handle(&mut self, events: &[CommittedEvent]) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.handle_inner(events).await;
        self.interrupted = result.as_ref().err().is_some_and(RuntimeError::trapped);
        result
    }

    async fn handle_inner(&mut self, events: &[CommittedEvent]) -> Result<Outcome, RuntimeError> {
        if events.is_empty() {
            return Ok(Outcome {
                events: Vec::new(),
                checkpoint: self.checkpoint,
            });
        }
        let highest = events
            .last()
            .map_or(self.checkpoint, |event| event.sequence);
        let lowest = events
            .first()
            .map_or(self.checkpoint, |event| event.sequence);
        let causation = (events.len() == 1).then(|| events[0].event_id.clone());
        let instance = &self.store.data().delivery.instance_id;
        let wit_events = events
            .iter()
            .map(|event| {
                let mut value = wit_event(event);
                if !http_request_visible(event, instance) {
                    value.payload = types::Payload::Json(b"{}".to_vec());
                }
                value
            })
            .collect::<Vec<_>>();
        let context = self.context();
        self.store.data_mut().reveal_event_blobs(events);
        prepare_call(&mut self.store, &self.limits, "delivery");
        let lifecycle = self.plugin.pluribus_plugin_lifecycle();
        let result = tokio::time::timeout(
            self.limits.call_timeout,
            self.store.run_concurrent(async |accessor| {
                lifecycle.call_handle(accessor, context, wit_events).await
            }),
        )
        .await
        .map_err(|_| wasmtime::format_err!("handler deadline exceeded"))
        .and_then(|r| r)
        .and_then(|r| r);
        if result.as_ref().map_or(true, Result::is_err) {
            self.store.data_mut().release_handles();
        }
        let outcome = result
            .map_err(|error| {
                self.store
                    .data()
                    .resource_error(format!("handle trapped: {error:#}"))
            })?
            .map_err(|error| plugin_failure(&error))?;
        if let Some(claimed) = outcome.checkpoint
            && !(lowest..=highest).contains(&claimed)
        {
            return Err(RuntimeError::new(format!(
                "checkpoint {claimed} is outside the delivered batch {lowest}..={highest}"
            )));
        }
        self.commit(&outcome, causation.as_ref(), CommitPhase::Handle)
            .await
    }

    /// Rebuilds a projection from its own mutation events before dispatch.
    /// The replay cursor is separate from the request delivery cursor.
    ///
    /// # Errors
    /// Returns storage, handler, or commit failures without activating the instance.
    pub async fn rebuild(&mut self, event_types: &[String]) -> Result<(), RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.rebuild_inner(event_types).await;
        self.interrupted = false;
        result
    }

    async fn rebuild_inner(&mut self, event_types: &[String]) -> Result<(), RuntimeError> {
        if event_types.is_empty() {
            return Ok(());
        }
        let host = self.store.data();
        let events = Arc::clone(&host.event_store);
        let stream = StreamId::new(host.delivery.agent.id.clone());
        let marker = "__host/rebuild";
        let state = host.state_store.clone();
        let namespace = host.state_namespace.clone();
        let mut after = state
            .get(&namespace, marker)
            .await
            .map_err(|e| RuntimeError::new(e.to_string()))?
            .value
            .map(|v| serde_json::from_slice::<u64>(&v))
            .transpose()
            .map_err(|e| RuntimeError::new(e.to_string()))?
            .unwrap_or(0);
        // The inactive instance cannot append new mutations during this scan.
        let mut high = after;
        loop {
            let page = rebuild_page(&events, &stream, event_types, high, 1000).await?;
            let Some(last) = page.last() else {
                break;
            };
            high = last.sequence;
        }
        while after < high {
            let page = rebuild_page(&events, &stream, event_types, after, 32).await?;
            let page = page
                .into_iter()
                .take_while(|e| e.sequence <= high)
                .collect::<Vec<_>>();
            let Some(last) = page.last() else {
                break;
            };
            let end = last.sequence;
            let own = page
                .iter()
                .filter(|e| {
                    e.request.actor
                        == PrincipalRef::new(CorePrincipalKind::Component, self.instance_id.clone())
                })
                .map(wit_event)
                .collect::<Vec<_>>();
            let mut outcome = guest::Outcome {
                events: vec![],
                mutations: vec![],
                checkpoint: None,
            };
            if !own.is_empty() {
                let context = self.context();
                prepare_call(&mut self.store, &self.limits, "restore");
                self.store.data_mut().replaying = true;
                let lifecycle = self.plugin.pluribus_plugin_lifecycle();
                let result = tokio::time::timeout(
                    self.limits.call_timeout,
                    self.store.run_concurrent(async |accessor| {
                        lifecycle.call_handle(accessor, context, own.clone()).await
                    }),
                )
                .await
                .map_err(|_| wasmtime::format_err!("replay deadline exceeded"))
                .and_then(|r| r)
                .and_then(|r| r);
                self.store.data_mut().replaying = false;
                outcome = result
                    .map_err(|e| {
                        self.store
                            .data()
                            .resource_error(format!("replay trapped: {e:#}"))
                    })?
                    .map_err(|e| plugin_failure(&e))?;
                if !outcome.events.is_empty()
                    || outcome.checkpoint != own.last().map(|e| e.sequence)
                {
                    return Err(RuntimeError::new(
                        "replay must consume its batch without emitting events",
                    ));
                }
            }
            outcome.checkpoint = None;
            outcome
                .mutations
                .push(types::Mutation::Set(types::StateEntry {
                    key: marker.into(),
                    value: end.to_string().into_bytes(),
                }));
            self.commit(&outcome, None, CommitPhase::Handle).await?;
            after = end;
        }
        Ok(())
    }

    /// Advances the delivery cursor past a batch the instance could not
    /// handle, committing no events and no state.
    ///
    /// For a component that trapped: it returned no outcome, so nothing
    /// commits, so the same batch would be delivered again and trap again.
    /// The caller withdraws the instance; this keeps the cursor honest about
    /// what it will never process.
    ///
    /// # Errors
    ///
    /// Returns an error when the commit is rejected.
    pub async fn skip(&mut self, checkpoint: u64) -> Result<(), RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.skip_inner(checkpoint).await;
        self.interrupted = false;
        result
    }

    async fn skip_inner(&mut self, checkpoint: u64) -> Result<(), RuntimeError> {
        self.store.data_mut().release_handles();
        let store = self.delivery_store.clone();
        let transaction = DeliveryCommit {
            cursor: self.cursor.clone(),
            expected_checkpoint: self.checkpoint,
            checkpoint: Some(checkpoint),
            mutations: Vec::new(),
            events: Vec::new(),
        };
        let receipt = store
            .commit(transaction)
            .await
            .map_err(|error| RuntimeError::new(format!("cannot skip delivery: {error}")))?;
        self.checkpoint = receipt.checkpoint;
        Ok(())
    }

    async fn skip_with_events_inner(
        &mut self,
        checkpoint: u64,
        events: Vec<AppendRequest>,
    ) -> Result<Outcome, RuntimeError> {
        self.store.data_mut().release_handles();
        let receipt = self
            .delivery_store
            .commit(DeliveryCommit {
                cursor: self.cursor.clone(),
                expected_checkpoint: self.checkpoint,
                checkpoint: Some(checkpoint),
                mutations: Vec::new(),
                events,
            })
            .await
            .map_err(|error| RuntimeError::new(format!("cannot skip delivery: {error}")))?;
        self.checkpoint = receipt.checkpoint;
        Ok(Outcome {
            events: receipt.events,
            checkpoint: receipt.checkpoint,
        })
    }

    async fn skip_with_events(
        &mut self,
        checkpoint: u64,
        events: Vec<AppendRequest>,
    ) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.skip_with_events_inner(checkpoint, events).await;
        self.interrupted = false;
        result
    }

    /// Final call before teardown. Best-effort: durable correctness must not
    /// depend on it.
    ///
    /// # Errors
    ///
    /// Returns an error on a trap, a plugin error, or a rejected commit.
    pub async fn stop(&mut self, deadline_at_ms: i64) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.stop_inner(deadline_at_ms).await;
        self.interrupted = false;
        result
    }

    async fn stop_inner(&mut self, deadline_at_ms: i64) -> Result<Outcome, RuntimeError> {
        if let Some(task) = self.run_task.take() {
            task.abort();
            self.store
                .run_concurrent(async |_| task.await)
                .await
                .map_err(|e| RuntimeError::trap(e.to_string()))?;
        }
        self.activation.take();
        // Shutdown cancels the delivery in flight; the final call gets its own
        // deadline rather than inheriting that cancellation.
        self.cancellation.reset();
        let context = self.context();
        prepare_call(&mut self.store, &self.limits, "execution");
        let result = self
            .plugin
            .pluribus_plugin_lifecycle()
            .call_stop(&mut self.store, &context, deadline_at_ms)
            .await;
        if result.as_ref().map_or(true, Result::is_err) {
            self.store.data_mut().release_handles();
        }
        let outcome = result
            .map_err(|error| {
                self.store
                    .data()
                    .resource_error(format!("stop trapped: {error:#}"))
            })?
            .map_err(|error| plugin_failure(&error))?;
        self.commit(&outcome, None, CommitPhase::Stop).await
    }

    fn begin_lifecycle(&mut self) -> Result<(), RuntimeError> {
        if self.interrupted {
            return Err(RuntimeError::trap(
                "interrupted lifecycle requires re-instantiation",
            ));
        }
        self.interrupted = true;
        Ok(())
    }

    fn context(&mut self) -> guest::Context {
        let host = self.store.data();
        guest::Context {
            instance_id: self.instance_id.clone(),
            agent: host.wit_agent(),
            state_checkpoint: self.checkpoint,
            depth: host.delivery.depth,
            deadline_at_ms: host.delivery.deadline_at_ms,
        }
    }

    async fn commit(
        &mut self,
        outcome: &guest::Outcome,
        causation: Option<&EventId>,
        phase: CommitPhase,
    ) -> Result<Outcome, RuntimeError> {
        self.store.data_mut().release_handles();

        if outcome.events.len() > MAX_OUTCOME_EVENTS {
            return Err(RuntimeError::new("outcome proposes too many events"));
        }
        let host = self.store.data();
        let mut requests = Vec::with_capacity(outcome.events.len());
        for (ordinal, proposal) in outcome.events.iter().enumerate() {
            let derived = phase.derived_key(&self.instance_id, self.checkpoint, ordinal);
            requests.push(host.append_request(proposal, causation.cloned(), Some(derived))?);
        }
        let mutations = convert_mutations(&outcome.mutations)?;

        let store = self.delivery_store.clone();
        let transaction = DeliveryCommit {
            cursor: self.cursor.clone(),
            expected_checkpoint: self.checkpoint,
            checkpoint: outcome.checkpoint,
            mutations,
            events: requests,
        };
        let receipt = store
            .commit(transaction)
            .await
            .map_err(|error| RuntimeError::new(format!("cannot commit delivery: {error}")))?;
        self.checkpoint = receipt.checkpoint;
        let host = self.store.data_mut();
        host.progress.notify_one();
        Ok(Outcome {
            events: receipt.events,
            checkpoint: receipt.checkpoint,
        })
    }
}

/// Arms the wall-clock ceiling for one call. The epoch ticker interrupts the
/// guest when it runs out.
fn prepare_call(store: &mut Store<HostState>, limits: &RuntimeLimits, phase: &str) {
    store.data_mut().emit_call = false;
    store.data_mut().reset_resource_evidence(phase);
    let deadline = Instant::now() + limits.call_timeout;
    store.data_mut().call_deadline = Some(deadline);
    *store.data().io_completion_deadline.lock().unwrap() = None;
    store.epoch_deadline_callback(move |store| {
        // An idle source handles shutdown through runtime.next; its deadline
        // bounds the unwind. Active deliveries still cancel immediately.
        let host = store.data();
        if host.interrupted() && (!host.runner.active || host.runner.has_pending_delivery()) {
            return Err(wasmtime::Error::msg("call cancelled"));
        }
        if store
            .data()
            .effective_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(wasmtime::Error::msg("call deadline exceeded"));
        }
        Ok(wasmtime::UpdateDeadline::Yield(1))
    });
    store.set_epoch_deadline(1);
}

fn resource_error_from_evidence(
    evidence: &Arc<std::sync::Mutex<Option<ResourceExhaustion>>>,
    message: impl Into<String>,
) -> RuntimeError {
    let message = message.into();
    evidence
        .lock()
        .ok()
        .and_then(|evidence| evidence.clone())
        .map_or_else(
            || RuntimeError::trap(message.clone()),
            |evidence| RuntimeError::resource_limit(evidence, message.clone()),
        )
}

fn convert_mutations(mutations: &[types::Mutation]) -> Result<Vec<StateMutation>, RuntimeError> {
    if mutations.len() > MAX_STATE_MUTATIONS {
        return Err(RuntimeError::new("outcome proposes too many mutations"));
    }
    mutations
        .iter()
        .map(|mutation| match mutation {
            types::Mutation::Set(entry) => {
                if entry.key.len() > MAX_STATE_KEY_BYTES {
                    return Err(RuntimeError::new("state key exceeds 512 bytes"));
                }
                if entry.value.len() > MAX_STATE_VALUE_BYTES {
                    return Err(RuntimeError::new("state value exceeds 1 MiB"));
                }
                Ok(StateMutation::Set {
                    key: entry.key.clone(),
                    value: entry.value.clone(),
                })
            }
            types::Mutation::Delete(key) => {
                if key.len() > MAX_STATE_KEY_BYTES {
                    return Err(RuntimeError::new("state key exceeds 512 bytes"));
                }
                Ok(StateMutation::Delete { key: key.clone() })
            }
        })
        .collect()
}

struct HostServices {
    state: Arc<dyn StateStore>,
    events: Arc<dyn EventStore>,
    blobs: Arc<dyn BlobStore>,
    registry: Arc<EventTypeRegistry>,
    progress: Arc<tokio::sync::Notify>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

struct HostState {
    credentials: Option<CredentialAccess>,
    identity: Option<String>,
    model: Option<String>,
    executor: tokio::runtime::Handle,
    limits: RuntimeLimiter,
    resource_evidence: Arc<std::sync::Mutex<Option<ResourceExhaustion>>>,
    resource_phase: Arc<std::sync::Mutex<String>>,
    delivery: Delivery,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    state_store: Arc<dyn StateStore>,
    state_namespace: StateNamespace,
    replaying: bool,
    event_store: Arc<dyn EventStore>,
    blob_store: Arc<dyn BlobStore>,
    registry: Arc<EventTypeRegistry>,
    progress: Arc<tokio::sync::Notify>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    emits: Vec<String>,
    active_uploads: HashSet<BlobUploadId>,
    visible_blobs: HashSet<BlobRef>,
    append_sequence: u64,
    http: Option<Arc<dyn HttpStreamService>>,
    http_grant: Option<HttpGrant>,
    streams: BTreeMap<String, GrantedStream>,
    call_deadline: Option<Instant>,
    io_completion_deadline: Arc<std::sync::Mutex<Option<Instant>>>,
    transports: Vec<Arc<Transport>>,
    wasi_http: wasmtime_wasi_http::WasiHttpCtx,
    wasi_table: wasmtime::component::ResourceTable,
    wasi_hooks: wasi_http::Hooks,
    runner: runner::HostRunner,
    emit_call: bool,
}

struct RuntimeLimiter {
    limits: StoreLimits,
    evidence: Arc<std::sync::Mutex<Option<ResourceExhaustion>>>,
    phase: Arc<std::sync::Mutex<String>>,
    memory_limit: usize,
    table_limit: usize,
}

impl RuntimeLimiter {
    fn new(
        memory_limit: usize,
        evidence: Arc<std::sync::Mutex<Option<ResourceExhaustion>>>,
        phase: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_limit)
                .memories(4)
                .tables(8)
                .trap_on_grow_failure(true)
                .build(),
            evidence,
            phase,
            memory_limit,
            table_limit: usize::MAX,
        }
    }

    fn record(&self, resource: &str, current: usize, requested: usize, limit: usize) {
        if let Ok(mut evidence) = self.evidence.lock()
            && evidence.is_none()
        {
            let phase = self
                .phase
                .lock()
                .map_or_else(|_| "unknown".into(), |p| p.clone());
            *evidence = Some(ResourceExhaustion {
                resource: resource.into(),
                current_bytes: current,
                requested_bytes: requested,
                limit_bytes: limit,
                phase,
                job_id: None,
                session_id: None,
            });
        }
    }
}

impl ResourceLimiter for RuntimeLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_limit {
            self.record("memory", current, desired, self.memory_limit);
        }
        self.limits.memory_growing(current, desired, maximum)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.table_limit {
            self.record("table", current, desired, self.table_limit);
        }
        self.limits.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.limits.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        32
    }
    fn tables(&self) -> usize {
        8
    }
    fn memories(&self) -> usize {
        4
    }
}

/// One connection and the directions still running over it. The incoming
/// stream and the outgoing stream share it; it closes when both are done,
/// when either fails, or when the delivery ends.
struct Transport {
    service: Arc<dyn StreamService>,
    stream_id: String,
    running: AtomicU8,
    open: AtomicBool,
    failure: std::sync::Mutex<Option<types::Error>>,
}

impl Transport {
    fn new(service: Arc<dyn StreamService>, stream_id: String) -> Self {
        Self {
            service,
            stream_id,
            running: AtomicU8::new(2),
            open: AtomicBool::new(true),
            failure: std::sync::Mutex::new(None),
        }
    }

    /// Records the first failure and tears the connection down.
    fn fail(&self, error: types::Error) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(error);
        }
        self.close();
    }

    /// The first failure on either direction, or success.
    fn outcome(&self) -> Result<(), types::Error> {
        match self.failure.lock() {
            Ok(failure) => failure.clone().map_or(Ok(()), Err),
            Err(_) => Err(host_error(types::ErrorCode::Internal, "transport poisoned")),
        }
    }

    /// Retires one direction, closing the connection once both are done.
    fn finish(&self) {
        if self.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.close();
        }
    }

    fn close(&self) {
        if self.open.swap(false, Ordering::AcqRel) {
            self.service.close(&self.stream_id);
        }
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

impl Drop for HostState {
    fn drop(&mut self) {
        self.release_handles();
    }
}

impl HostState {
    fn new(
        memory_bytes: usize,
        delivery: Delivery,
        cancelled: Arc<AtomicBool>,
        services: HostServices,
        granted: PluginServices,
        emits: Vec<String>,
    ) -> Self {
        let state_namespace = StateNamespace::new(delivery.instance_id.clone());
        let visible_blobs = delivery.visible_blobs.iter().cloned().collect();
        let resource_evidence = Arc::new(std::sync::Mutex::new(None));
        let resource_phase = Arc::new(std::sync::Mutex::new("startup".into()));
        Self {
            credentials: granted.credentials,
            model: granted.model,
            identity: granted.identity,
            executor: tokio::runtime::Handle::current(),
            limits: RuntimeLimiter::new(
                memory_bytes,
                resource_evidence.clone(),
                resource_phase.clone(),
            ),
            resource_evidence,
            resource_phase,
            delivery,
            cancelled,
            shutdown: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            state_store: services.state,
            state_namespace,
            replaying: false,
            event_store: services.events,
            blob_store: services.blobs,
            registry: services.registry,
            progress: services.progress,
            clock: services.clock,
            emits,
            active_uploads: HashSet::new(),
            visible_blobs,
            append_sequence: 0,
            http: granted.http,
            http_grant: granted.http_grant,
            streams: granted.streams,
            call_deadline: None,
            io_completion_deadline: Arc::default(),
            transports: Vec::new(),
            wasi_http: wasmtime_wasi_http::WasiHttpCtx::new(),
            wasi_table: wasi_http::resource_table(),
            wasi_hooks: wasi_http::Hooks,
            runner: runner::HostRunner::default(),
            emit_call: false,
        }
    }

    fn reset_resource_evidence(&mut self, phase: &str) {
        if let Ok(mut evidence) = self.resource_evidence.lock() {
            *evidence = None;
        }
        if let Ok(mut current) = self.resource_phase.lock() {
            *current = phase.into();
        }
    }

    fn resource_error(&self, message: impl Into<String>) -> RuntimeError {
        let message = message.into();
        self.resource_evidence
            .lock()
            .ok()
            .and_then(|evidence| evidence.clone())
            .map_or_else(
                || RuntimeError::trap(message.clone()),
                |evidence| RuntimeError::resource_limit(evidence, message.clone()),
            )
    }

    fn wit_agent(&self) -> types::Principal {
        wit_principal(&self.delivery.agent)
    }

    /// Closes every invocation-scoped handle. A handle never outlives the
    /// delivery that opened it.
    fn release_handles(&mut self) {
        let uploads: Vec<_> = self.active_uploads.drain().collect();
        if !uploads.is_empty() {
            let store = self.blob_store.clone();
            self.executor.spawn(async move {
                for upload_id in uploads {
                    let _ = store.abort_put(&upload_id).await;
                }
            });
        }
        for transport in self.transports.drain(..) {
            transport.close();
        }
    }

    /// Grants read access to the blobs a delivered batch names. An instance
    /// reads a blob it stored itself or one an event handed it, nothing else.
    pub(crate) fn reveal_event_blobs(&mut self, events: &[CommittedEvent]) {
        let instance = self.delivery.instance_id.clone();
        let mut found = Vec::new();
        for event in events
            .iter()
            .filter(|event| http_request_visible(event, &instance))
        {
            referenced_blobs(event, &mut found);
        }
        for blob in found {
            if validate_blob_ref(&blob).is_ok() {
                self.visible_blobs.insert(blob);
            }
        }
    }

    fn authorize_proposal_blobs(&self, payload: &EventPayload) -> Result<(), RuntimeError> {
        authorize_blob_references(payload, &self.visible_blobs)
    }

    fn ensure_visible_blob(&self, blob: &BlobRef) -> Result<(), types::Error> {
        validate_blob_ref(blob).map_err(|error| blob_error(&error))?;
        if self.visible_blobs.contains(blob) {
            Ok(())
        } else {
            Err(host_error(
                types::ErrorCode::PermissionDenied,
                "blob is not visible to this delivery",
            ))
        }
    }

    /// Converts a proposal into an append request, stamping every field a
    /// plugin must not control.
    fn append_request(
        &self,
        proposal: &types::Proposal,
        causation: Option<EventId>,
        derived_key: Option<String>,
    ) -> Result<AppendRequest, RuntimeError> {
        if self.replaying {
            return Err(RuntimeError::new("events cannot be appended during replay"));
        }
        let causation_id = proposal
            .causation_id
            .as_ref()
            .map(|id| EventId::new(id.clone()))
            .or(causation);
        if causation_id.is_none() && !self.emit_call {
            return Err(RuntimeError::new(
                "proposal needs an explicit causation ID when a batch carries several events",
            ));
        }
        let mut payload = core_payload(&proposal.payload).map_err(|error| {
            RuntimeError::new(format!("invalid proposal payload: {}", error.message))
        })?;
        self.authorize_proposal_blobs(&payload)?;
        if proposal.event_type == "model.requested"
            && let EventPayload::CanonicalJson(bytes) = &mut payload
        {
            let mut value: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|error| RuntimeError::new(format!("invalid model request: {error}")))?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| RuntimeError::new("model request must be an object"))?;
            if !object.contains_key("model") {
                let model = self
                    .model
                    .as_ref()
                    .ok_or_else(|| RuntimeError::new("no host model configured"))?;
                object.insert("model".into(), serde_json::Value::String(model.clone()));
            }
            if let Some(identity) = self.identity.as_ref().filter(|text| !text.is_empty()) {
                let messages = object
                    .get_mut("messages")
                    .and_then(serde_json::Value::as_array_mut)
                    .ok_or_else(|| RuntimeError::new("model request requires messages"))?;
                let content = serde_json::json!({"kind":"text","text":identity});
                if messages
                    .first()
                    .is_some_and(|message| message["role"] == "system")
                {
                    messages[0]["content"]
                        .as_array_mut()
                        .ok_or_else(|| RuntimeError::new("system message requires content"))?
                        .insert(0, content);
                } else {
                    messages.insert(0, serde_json::json!({"role":"system","content":[content]}));
                }
            }
            *bytes =
                serde_json::to_vec(&value).map_err(|error| RuntimeError::new(error.to_string()))?;
        }
        let request = AppendRequest {
            stream_id: StreamId::new(self.delivery.agent.id.clone()),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: proposal.event_type.clone(),
            payload_schema: proposal.payload_schema.clone(),
            payload,
            actor: PrincipalRef::new(
                CorePrincipalKind::Component,
                self.delivery.instance_id.clone(),
            ),
            authority_id: Some(AuthorityId::new(self.delivery.authority_id.clone())),
            activity_id: Some(self.delivery.activity_id.clone()),
            correlation_id: Some(self.delivery.correlation_id.clone()),
            causation_id,
            deduplication_key: proposal.idempotency_key.clone().or(derived_key),
        };
        self.registry
            .authorize(&request, &self.emits)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        Ok(request)
    }
}

impl types::Host for HostState {}

impl credentials::Host for HostState {
    async fn resolve_export(&mut self, binding: String) -> Result<String, types::Error> {
        self.credentials
            .as_ref()
            .ok_or_else(|| {
                host_error(
                    types::ErrorCode::PermissionDenied,
                    "credential export not granted",
                )
            })?
            .resolve_export(&binding)
            .await
    }

    async fn get(&mut self, handle: String) -> Result<Option<Vec<u8>>, types::Error> {
        let access = self.credential_access(&handle)?;
        access
            .store
            .read_plugin_credential(&SecretHandle::new(handle), &access.provider)
            .await
            .map_err(|_| {
                host_error(
                    types::ErrorCode::Unavailable,
                    "credential storage unavailable",
                )
            })
    }
    async fn compare_and_swap(
        &mut self,
        handle: String,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, types::Error> {
        let access = self.credential_access(&handle)?;
        if value.len() > 1024 * 1024 || expected.as_ref().is_some_and(|v| v.len() > 1024 * 1024) {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "credential record too large",
            ));
        }
        access
            .store
            .replace_plugin_credential(
                &SecretHandle::new(handle),
                &access.provider,
                expected,
                value,
            )
            .await
            .map_err(|_| {
                host_error(
                    types::ErrorCode::Unavailable,
                    "credential storage unavailable",
                )
            })
    }
}
impl HostState {
    fn credential_access(&self, handle: &str) -> Result<&CredentialAccess, types::Error> {
        self.credentials
            .as_ref()
            .filter(|a| a.handles.contains(handle))
            .ok_or_else(|| host_error(types::ErrorCode::PermissionDenied, "credential not granted"))
    }
}

impl events::Host for HostState {
    async fn append(&mut self, proposal: types::Proposal) -> Result<u64, types::Error> {
        if proposal.idempotency_key.is_none() {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "append requires an idempotency key so a redelivery deduplicates",
            ));
        }
        self.append_sequence += 1;
        let request = self
            .append_request(
                &proposal,
                Some(EventId::new(self.delivery.origin_event_id.clone())),
                None,
            )
            .map_err(|error| host_error(types::ErrorCode::PermissionDenied, &error.to_string()))?;
        let store = self.event_store.clone();
        store
            .append(request)
            .await
            .map(|event| {
                self.progress.notify_one();
                event.sequence
            })
            .map_err(|error| {
                host_error(
                    types::ErrorCode::Internal,
                    &format!("cannot append: {error}"),
                )
            })
    }

    async fn get(&mut self, event_id: String) -> Result<types::Event, types::Error> {
        let store = self.event_store.clone();
        let event = store
            .get(&EventId::new(event_id))
            .await
            .map_err(|error| {
                host_error(types::ErrorCode::Internal, &format!("cannot read: {error}"))
            })?
            .filter(|event| {
                event.request.stream_id == StreamId::new(self.delivery.agent.id.clone())
                    && http_request_visible(event, &self.delivery.instance_id)
            })
            .ok_or_else(|| host_error(types::ErrorCode::NotFound, "no such event"))?;
        self.reveal_event_blobs(std::slice::from_ref(&event));
        Ok(wit_event(&event))
    }

    async fn query(
        &mut self,
        filter: events::Filter,
        limit: u32,
    ) -> Result<events::Page, types::Error> {
        if filter
            .text_query
            .as_ref()
            .is_some_and(|query| query.len() > 4096)
        {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "search text is too long",
            ));
        }
        if limit == 0 {
            return Ok(events::Page {
                events: Vec::new(),
                next_sequence: None,
            });
        }
        let query = EventQuery {
            after_sequence: filter.after_sequence,
            before_sequence: filter.before_sequence,
            event_types: filter.event_types,
            text_query: filter.text_query.clone(),
            descending: filter.descending,
            correlation_id: filter.correlation_id,
            activity_id: filter.activity_id,
            recorded_from_ms: filter.recorded_from_ms,
            recorded_to_ms: filter.recorded_to_ms,
        };
        let searching = filter.text_query.is_some()
            || filter.before_sequence.is_some()
            || filter.conversation_id.is_some()
            || filter.descending;
        let store = self.event_store.clone();
        let stream = StreamId::new(self.delivery.agent.id.clone());
        let events = store
            .query(
                &stream,
                &query,
                if searching {
                    MAX_EVENT_PAGE as usize
                } else {
                    limit.min(MAX_EVENT_PAGE) as usize
                },
            )
            .await
            .map_err(|error| {
                host_error(
                    types::ErrorCode::Internal,
                    &format!("cannot query: {error}"),
                )
            })?;
        let requested = limit.min(MAX_EVENT_PAGE) as usize;
        let selected = if searching {
            let mut selected = Vec::with_capacity(requested);
            for event in &events {
                if !http_request_visible(event, &self.delivery.instance_id)
                    || !filter.conversation_id.as_deref().is_none_or(|wanted| {
                        event_conversation_id(event).as_deref() == Some(wanted)
                    })
                {
                    continue;
                }
                selected.push(event);
                if selected.len() == requested {
                    break;
                }
            }
            selected
        } else {
            events
                .iter()
                .filter(|event| http_request_visible(event, &self.delivery.instance_id))
                .take(requested)
                .collect()
        };
        let next_sequence = (requested > 0)
            .then(|| {
                if searching {
                    selected.last().copied().or_else(|| events.last())
                } else {
                    events.last()
                }
            })
            .flatten()
            .map(|event| {
                self.progress.notify_one();
                event.sequence
            });
        for event in &selected {
            self.reveal_event_blobs(std::slice::from_ref(*event));
        }
        Ok(events::Page {
            events: selected.into_iter().map(wit_event).collect(),
            next_sequence,
        })
    }
}

fn event_conversation_id(event: &CommittedEvent) -> Option<String> {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        return None;
    };
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value["conversationId"]
        .as_str()
        .or_else(|| value["arguments"]["conversationId"].as_str())
        .map(str::to_owned)
}

impl state::Host for HostState {
    async fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, types::Error> {
        validate_state_key(&key)?;
        let store = self.state_store.clone();
        let namespace = self.state_namespace.clone();
        store
            .get(&namespace, &key)
            .await
            .map(|snapshot| snapshot.value)
            .map_err(state_error)
    }

    async fn scan(
        &mut self,
        prefix: String,
        after_key: Option<String>,
        limit: u32,
    ) -> Result<state::Page, types::Error> {
        validate_state_key(&prefix)?;
        if let Some(after_key) = &after_key {
            validate_state_key(after_key)?;
        }
        let store = self.state_store.clone();
        let namespace = self.state_namespace.clone();
        store
            .scan(
                &namespace,
                &prefix,
                after_key.as_deref(),
                limit.min(MAX_STATE_SCAN_ENTRIES) as usize,
            )
            .await
            .map(|page| state::Page {
                entries: page
                    .entries
                    .into_iter()
                    .map(|entry| types::StateEntry {
                        key: entry.key,
                        value: entry.value,
                    })
                    .collect(),
                next_key: page.next_key,
            })
            .map_err(state_error)
    }
}

impl blobs::Host for HostState {
    async fn put(
        &mut self,
        media_type: String,
        bytes: Vec<u8>,
    ) -> Result<types::BlobRef, types::Error> {
        if bytes.len() > MAX_BLOB_CHUNK_BYTES {
            return Err(host_error(
                types::ErrorCode::ResourceExhausted,
                "use open-write for content above 1 MiB",
            ));
        }
        let store = self.blob_store.clone();
        let upload = store
            .begin_put(&media_type, Some(bytes.len() as u64))
            .await
            .map_err(|error| blob_error(&error))?;
        self.active_uploads.insert(upload.clone());
        let result = async {
            store.write(&upload, 0, &bytes).await?;
            store.finish_put(&upload).await
        }
        .await;
        if result.is_err() {
            let _ = store.abort_put(&upload).await;
        }
        self.active_uploads.remove(&upload);
        let blob = result.map_err(|error| blob_error(&error))?;
        self.visible_blobs.insert(blob.clone());
        Ok(wit_blob_ref(&blob))
    }

    async fn open_write(
        &mut self,
        media_type: String,
        expected_size: Option<u64>,
    ) -> Result<String, types::Error> {
        let store = self.blob_store.clone();
        let upload_id = store
            .begin_put(&media_type, expected_size)
            .await
            .map_err(|error| blob_error(&error))?;
        self.active_uploads.insert(upload_id.clone());
        Ok(upload_id.as_str().to_owned())
    }

    async fn write(
        &mut self,
        handle: String,
        offset: u64,
        bytes: Vec<u8>,
    ) -> Result<u64, types::Error> {
        if bytes.len() > MAX_BLOB_CHUNK_BYTES {
            return Err(host_error(
                types::ErrorCode::ResourceExhausted,
                "blob chunk exceeds 1 MiB",
            ));
        }
        let handle = BlobUploadId::new(handle);
        if !self.active_uploads.contains(&handle) {
            return Err(host_error(
                types::ErrorCode::PermissionDenied,
                "upload does not belong to this delivery",
            ));
        }
        let store = self.blob_store.clone();
        store
            .write(&handle, offset, &bytes)
            .await
            .map_err(|error| blob_error(&error))
    }

    async fn finish(&mut self, handle: String) -> Result<types::BlobRef, types::Error> {
        let handle = BlobUploadId::new(handle);
        if !self.active_uploads.remove(&handle) {
            return Err(host_error(
                types::ErrorCode::PermissionDenied,
                "upload does not belong to this delivery",
            ));
        }
        let store = self.blob_store.clone();
        let blob = store
            .finish_put(&handle)
            .await
            .map_err(|error| blob_error(&error))?;
        self.visible_blobs.insert(blob.clone());
        Ok(wit_blob_ref(&blob))
    }

    async fn resolve_visible(&mut self, digest: String) -> Result<types::BlobRef, types::Error> {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "invalid SHA-256 digest",
            ));
        }
        let mut matches = self.visible_blobs.iter().filter(|blob| {
            blob.algorithm == pluribus_core::SHA256_ALGORITHM && blob.digest == digest
        });
        let Some(blob) = matches.next() else {
            return Err(host_error(
                types::ErrorCode::PermissionDenied,
                "blob is not visible to this delivery",
            ));
        };
        if matches.any(|candidate| candidate != blob) {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "visible blob digest is ambiguous",
            ));
        }
        Ok(wit_blob_ref(blob))
    }

    async fn read(
        &mut self,
        blob: types::BlobRef,
        offset: u64,
        max_bytes: u32,
    ) -> Result<types::Chunk, types::Error> {
        let blob = core_blob_ref(blob)?;
        self.ensure_visible_blob(&blob)?;
        let store = self.blob_store.clone();
        store
            .read(
                &blob,
                offset,
                (max_bytes as usize).min(MAX_BLOB_CHUNK_BYTES),
            )
            .await
            .map(|chunk| types::Chunk {
                bytes: chunk.bytes,
                closed: chunk.eof,
            })
            .map_err(|error| blob_error(&error))
    }
}

impl socket::Host for HostState {}

impl HostState {
    fn http_grant(&self) -> Result<&HttpGrant, types::Error> {
        self.http_grant
            .as_ref()
            .ok_or_else(|| host_error(types::ErrorCode::PermissionDenied, "no HTTP grant"))
    }

    /// The flag that ends what the guest is doing now. Cancellation reaches a
    /// delivery; a source loop between deliveries only ends on shutdown.
    fn interrupt_flag(&self) -> &Arc<AtomicBool> {
        if self.runner.active && !self.runner.has_pending_delivery() {
            &self.shutdown
        } else {
            &self.cancelled
        }
    }

    fn interrupted(&self) -> bool {
        self.interrupt_flag().load(Ordering::Acquire)
    }

    fn effective_deadline(&self) -> Option<Instant> {
        self.call_deadline
            .max(*self.io_completion_deadline.lock().unwrap())
    }

    fn call_budget(&self) -> Result<u32, types::Error> {
        if self.interrupted() {
            return Err(host_error(types::ErrorCode::Cancelled, "call cancelled"));
        }
        let remaining = self.effective_deadline().map_or(u32::MAX, |deadline| {
            u32::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u32::MAX)
        });
        if remaining == 0 {
            return Err(host_error(
                types::ErrorCode::DeadlineExceeded,
                "call deadline exceeded",
            ));
        }
        Ok(remaining)
    }

    /// The transport and ceilings behind one granted endpoint name. A name
    /// the operator did not grant is denied, so naming an endpoint never
    /// widens what the component can reach.
    fn stream_access(
        &self,
        endpoint: &str,
    ) -> Result<(Arc<dyn StreamService>, StreamGrant), types::Error> {
        let granted = self.streams.get(endpoint).ok_or_else(stream_denied)?;
        Ok((Arc::clone(&granted.service), granted.grant.clone()))
    }

    /// Milliseconds a connection may take to establish: the grant's ceiling,
    /// the authority deadline, and what is left of the call budget.
    fn connect_budget(&self, grant: &StreamGrant) -> Result<u32, types::Error> {
        let authority_remaining = self.delivery.deadline_at_ms.map_or(u32::MAX, |limit| {
            u32::try_from(limit.saturating_sub(system_time_ms()).max(0)).unwrap_or(u32::MAX)
        });
        let remaining = grant
            .max_timeout_ms
            .min(authority_remaining)
            .min(self.call_budget()?);
        if remaining == 0 {
            return Err(host_error(
                types::ErrorCode::DeadlineExceeded,
                "stream deadline exceeded",
            ));
        }
        Ok(remaining)
    }
}

/// Runs a host operation under the call's cancellation flag and deadline.
async fn cancellable<T>(
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    operation: impl Future<Output = T>,
) -> Result<T, types::Error> {
    tokio::pin!(operation);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(host_error(types::ErrorCode::Cancelled, "call cancelled"));
        }
        if deadline.is_some_and(|d| d <= Instant::now()) {
            return Err(host_error(
                types::ErrorCode::DeadlineExceeded,
                "call deadline exceeded",
            ));
        }
        tokio::select! {
            biased;
            result = &mut operation => return Ok(result),
            () = tokio::time::sleep(EPOCH_TICK) => {}
        }
    }
}

fn system_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn validate_state_key(key: &str) -> Result<(), types::Error> {
    if key.len() > MAX_STATE_KEY_BYTES {
        Err(host_error(
            types::ErrorCode::ResourceExhausted,
            "state key exceeds 512 bytes",
        ))
    } else {
        Ok(())
    }
}

fn http_request_visible(event: &CommittedEvent, instance: &str) -> bool {
    if event.request.event_type != "http.request.received"
        || event.request.actor.id.as_str() == instance
    {
        return true;
    }
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        return false;
    };
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .is_some_and(|v| v["consumer"].as_str() == Some(instance))
}

/// Blobs one delivered event names: its own blob payload, and every blob
/// reference nested in an inline payload. Malformed references are skipped.
fn referenced_blobs(event: &CommittedEvent, found: &mut Vec<BlobRef>) {
    match &event.request.payload {
        EventPayload::Blob(blob) => found.push(blob.clone()),
        EventPayload::CanonicalJson(bytes) => {
            if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
                collect_blob_refs(&value, found);
            }
        }
    }
}

fn collect_blob_refs(value: &Value, found: &mut Vec<BlobRef>) {
    match value {
        Value::Object(map) => {
            if let Some(blob) = blob_ref_from(map) {
                found.push(blob);
            }
            for nested in map.values() {
                collect_blob_refs(nested, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_blob_refs(item, found);
            }
        }
        _ => {}
    }
}

/// A JSON object shaped like a blob reference. Payloads spell the media type
/// either way, so both spellings are accepted.
fn blob_ref_from(map: &serde_json::Map<String, Value>) -> Option<BlobRef> {
    Some(BlobRef {
        algorithm: map.get("algorithm")?.as_str()?.to_owned(),
        digest: map.get("digest")?.as_str()?.to_owned(),
        size: map.get("size")?.as_u64()?,
        media_type: map
            .get("media_type")
            .or_else(|| map.get("media-type"))
            .or_else(|| map.get("mediaType"))?
            .as_str()?
            .to_owned(),
    })
}

fn wit_event(event: &CommittedEvent) -> types::Event {
    types::Event {
        event_id: event.event_id.as_str().to_owned(),
        sequence: event.sequence,
        recorded_at_ms: event.recorded_at_ms,
        event_type: event.request.event_type.clone(),
        payload_schema: event.request.payload_schema.clone(),
        payload: wit_payload(&event.request.payload),
        actor: types::Principal {
            kind: wit_principal_kind(event.request.actor.kind),
            id: event.request.actor.id.as_str().to_owned(),
        },
        authority_id: event
            .request
            .authority_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        activity_id: event.request.activity_id.clone(),
        correlation_id: event.request.correlation_id.clone(),
        causation_id: event
            .request
            .causation_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
    }
}

fn wit_payload(payload: &EventPayload) -> types::Payload {
    match payload {
        EventPayload::CanonicalJson(bytes) => types::Payload::Json(bytes.clone()),
        EventPayload::Blob(blob) => types::Payload::Blob(wit_blob_ref(blob)),
    }
}

fn authorize_blob_references(
    payload: &EventPayload,
    visible_blobs: &HashSet<BlobRef>,
) -> Result<(), RuntimeError> {
    let mut found = Vec::new();
    match payload {
        EventPayload::Blob(blob) => found.push(blob.clone()),
        EventPayload::CanonicalJson(bytes) => {
            let value: Value = serde_json::from_slice(bytes)
                .map_err(|error| RuntimeError::new(format!("invalid proposal JSON: {error}")))?;
            collect_blob_refs(&value, &mut found);
        }
    }
    found.retain(|blob| validate_blob_ref(blob).is_ok());
    if found.iter().any(|blob| !visible_blobs.contains(blob)) {
        return Err(RuntimeError::new(
            "proposal references a blob not visible to this delivery",
        ));
    }
    Ok(())
}

fn core_payload(payload: &types::Payload) -> Result<EventPayload, types::Error> {
    match payload {
        types::Payload::Json(bytes) => Ok(EventPayload::CanonicalJson(bytes.clone())),
        types::Payload::Blob(blob) => core_blob_ref(blob.clone()).map(EventPayload::Blob),
    }
}

fn wit_principal(principal: &Principal) -> types::Principal {
    types::Principal {
        kind: match principal.kind {
            PrincipalKind::Human => types::PrincipalKind::Human,
            PrincipalKind::Agent => types::PrincipalKind::Agent,
            PrincipalKind::Node => types::PrincipalKind::Node,
            PrincipalKind::Component => types::PrincipalKind::Component,
            PrincipalKind::External => types::PrincipalKind::External,
        },
        id: principal.id.clone(),
    }
}

fn wit_principal_kind(kind: CorePrincipalKind) -> types::PrincipalKind {
    match kind {
        CorePrincipalKind::Human => types::PrincipalKind::Human,
        CorePrincipalKind::Agent => types::PrincipalKind::Agent,
        CorePrincipalKind::Node => types::PrincipalKind::Node,
        CorePrincipalKind::Component => types::PrincipalKind::Component,
        CorePrincipalKind::External => types::PrincipalKind::External,
    }
}

fn core_blob_ref(blob: types::BlobRef) -> Result<BlobRef, types::Error> {
    let blob = BlobRef {
        algorithm: blob.algorithm,
        digest: blob.digest,
        size: blob.size,
        media_type: blob.media_type,
    };
    validate_blob_ref(&blob).map_err(|error| blob_error(&error))?;
    Ok(blob)
}

fn wit_blob_ref(blob: &BlobRef) -> types::BlobRef {
    types::BlobRef {
        algorithm: blob.algorithm.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        media_type: blob.media_type.clone(),
    }
}

fn host_error(code: types::ErrorCode, message: &str) -> types::Error {
    types::Error {
        code,
        message: message.to_owned(),
        retryable: matches!(
            code,
            types::ErrorCode::Unavailable | types::ErrorCode::ResourceExhausted
        ),
        details: None,
    }
}

fn blob_error(error: &BlobError) -> types::Error {
    let code = match error {
        BlobError::NotFound => types::ErrorCode::NotFound,
        BlobError::Conflict { .. } => types::ErrorCode::Conflict,
        BlobError::Invalid(_) => types::ErrorCode::InvalidArgument,
        BlobError::ResourceExhausted(_) => types::ErrorCode::ResourceExhausted,
        BlobError::Corrupt(_) | BlobError::Storage(_) => types::ErrorCode::Internal,
    };
    host_error(code, &error.to_string())
}

fn state_error(error: StateError) -> types::Error {
    match error {
        StateError::Conflict { expected, actual } => host_error(
            types::ErrorCode::Conflict,
            &format!("state revision conflict: expected {expected}, actual {actual}"),
        ),
        StateError::Invalid(message) => host_error(types::ErrorCode::InvalidArgument, &message),
        StateError::Storage(message) => types::Error {
            code: types::ErrorCode::Internal,
            message,
            retryable: true,
            details: None,
        },
    }
}

fn stream_denied() -> types::Error {
    host_error(
        types::ErrorCode::PermissionDenied,
        "local streams require an endpoint grant",
    )
}

fn stream_plugin_error(error: StreamError) -> types::Error {
    match error {
        StreamError::Cancelled => host_error(types::ErrorCode::Cancelled, "stream cancelled"),
        StreamError::DeadlineExceeded => host_error(
            types::ErrorCode::DeadlineExceeded,
            "stream deadline exceeded",
        ),
        StreamError::LimitExceeded => host_error(
            types::ErrorCode::ResourceExhausted,
            "stream exceeded its granted transfer limit",
        ),
        StreamError::Denied(message) => host_error(types::ErrorCode::PermissionDenied, &message),
        StreamError::Unavailable(message) => host_error(types::ErrorCode::Unavailable, &message),
    }
}

fn plugin_failure(error: &types::Error) -> RuntimeError {
    let message = format!("{}: {}", error_code(error.code), error.message);
    if error.code == types::ErrorCode::ResourceExhausted
        && let Some(details) = error
            .details
            .as_ref()
            .and_then(|details| serde_json::from_slice::<ResourceExhaustion>(details).ok())
    {
        return RuntimeError {
            message,
            trapped: false,
            resource_exhaustion: Some(Box::new(details)),
        };
    }
    RuntimeError::new(message)
}

fn error_code(code: types::ErrorCode) -> &'static str {
    match code {
        types::ErrorCode::InvalidArgument => "invalid-argument",
        types::ErrorCode::NotFound => "not-found",
        types::ErrorCode::PermissionDenied => "permission-denied",
        types::ErrorCode::Unsupported => "unsupported",
        types::ErrorCode::Conflict => "conflict",
        types::ErrorCode::Unavailable => "unavailable",
        types::ErrorCode::ResourceExhausted => "resource-exhausted",
        types::ErrorCode::Cancelled => "cancelled",
        types::ErrorCode::DeadlineExceeded => "deadline-exceeded",
        types::ErrorCode::Internal => "internal",
    }
}

async fn runtime_io<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RuntimeError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| RuntimeError::new(format!("storage task failed: {e}")))
}

async fn rebuild_page(
    events: &Arc<dyn EventStore>,
    stream: &StreamId,
    event_types: &[String],
    after: u64,
    limit: usize,
) -> Result<Vec<CommittedEvent>, RuntimeError> {
    let events = events.clone();
    let stream = stream.clone();
    let query = EventQuery {
        after_sequence: Some(after),
        event_types: event_types.to_vec(),
        ..EventQuery::default()
    };
    events
        .query(&stream, &query, limit)
        .await
        .map_err(|e| RuntimeError::new(e.to_string()))
}
