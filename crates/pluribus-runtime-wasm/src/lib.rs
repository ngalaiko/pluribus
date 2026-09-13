//! Sandboxed execution for validated Pluribus components.
//!
//! One world, so one instantiation path and one implementation of each host
//! import. A delivery's proposed events, state mutations, and cursor advance
//! commit in a single transaction.

mod subscription;

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
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wasmtime::component::{Component, HasSelf, Linker, Resource};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use bindings::exports::pluribus::plugin::lifecycle as guest;
use bindings::pluribus::plugin::{
    blobs, credentials, events, http, reader, socket, state, types, writer,
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

const ABI_WORLD: &str = "pluribus:plugin/plugin@1.0.0";

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
    pub stream: Option<Arc<dyn StreamService>>,
    pub stream_grant: Option<StreamGrant>,
    pub limits: Option<RuntimeLimits>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    message: String,
    trapped: bool,
}

impl RuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            trapped: false,
        }
    }

    /// The component died mid-call. It returned no outcome, and the same
    /// input would kill it again.
    fn trap(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            trapped: true,
        }
    }

    /// Whether the component died rather than reporting a failure. A
    /// reported failure is worth retrying; a trap is not.
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
pub struct CancellationHandle(Arc<AtomicBool>);

impl CancellationHandle {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
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
            .async_support(true)
            .wasm_component_model(true)
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
        })
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
        if manifest.world != ABI_WORLD && manifest.world != "pluribus:plugin/source@1.0.0" {
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
        bindings::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|error| RuntimeError::new(format!("cannot link host imports: {error}")))?;

        let cancellation = Arc::new(AtomicBool::new(false));
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
            },
            services,
            recipe.emits.clone(),
        );
        let mut store = Store::new(&self.ticker.engine, host);
        store.limiter(|state| &mut state.limits);
        prepare_call(&mut store, &limits);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .map_err(|error| RuntimeError::new(format!("cannot instantiate component: {error}")))?;
        let plugin = bindings::Plugin::new(&mut store, &instance)
            .map_err(|error| RuntimeError::new(format!("cannot bind component: {error}")))?;
        let ingress = instance
            .get_export_index(&mut store, None, "pluribus:plugin/ingress@1.0.0")
            .and_then(|interface| {
                instance.get_export_index(&mut store, Some(&interface), "receive")
            })
            .map(|function| {
                instance
                    .get_typed_func::<(Vec<u8>,), (Result<types::IngressOutcome, types::Error>,)>(
                        &mut store, function,
                    )
            })
            .transpose()
            .map_err(|e| RuntimeError::new(format!("invalid ingress export: {e}")))?;
        store.data_mut().ingress_supported = ingress.is_some();

        Ok(PluginInstance {
            _ticker: Arc::clone(&self.ticker),
            limits,
            store,
            plugin,
            ingress,
            cancellation: CancellationHandle(cancellation),
            instance_id,
            cursor,
            checkpoint,
            config: config_bytes,
            delivery_store: Arc::clone(&self.delivery_store),
            pinned_session: recipe.pinned_session,
            interrupted: false,
            recipe,
        })
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

type IngressFunction =
    wasmtime::component::TypedFunc<(Vec<u8>,), (Result<types::IngressOutcome, types::Error>,)>;

/// One instantiated plugin. Exported calls are serialized by ownership.
pub struct PluginInstance {
    recipe: Arc<InstanceRecipe>,
    _ticker: Arc<EpochTicker>,
    limits: RuntimeLimits,
    store: Store<HostState>,
    plugin: bindings::Plugin,
    ingress: Option<IngressFunction>,
    cancellation: CancellationHandle,
    instance_id: String,
    cursor: CursorKey,
    checkpoint: u64,
    config: Vec<u8>,
    delivery_store: Arc<dyn DeliveryStore>,
    pinned_session: bool,
    interrupted: bool,
}

#[derive(Clone, Copy)]
enum CommitPhase {
    Init,
    Handle,
    Stop,
    Ingress([u8; 16]),
}

impl CommitPhase {
    fn derived_key(self, instance: &str, checkpoint: u64, ordinal: usize) -> String {
        match self {
            Self::Handle => format!("{instance}:{checkpoint}:{ordinal}"),
            Self::Init => format!("{instance}:{checkpoint}:{ordinal}:init"),
            Self::Stop => format!("{instance}:{checkpoint}:{ordinal}:stop"),
            Self::Ingress(id) => format!("{instance}:ingress:{id:02x?}:{ordinal}"),
        }
    }
}

impl PluginInstance {
    /// Creates fresh linear memory using the original grants and configuration.
    ///
    /// # Errors
    /// Returns cursor, linking, or instantiation failures.
    pub async fn restart(&self) -> Result<Self, RuntimeError> {
        self.recipe
            .runtime
            .instantiate_recipe(
                self.recipe.clone(),
                self.config.clone(),
                self.limits.clone(),
            )
            .await
    }

    /// Whether a dropped lifecycle future requires a fresh instance.
    #[must_use]
    pub fn requires_reinstantiation(&self) -> bool {
        self.interrupted
    }

    #[must_use]
    pub fn cancellation(&self) -> CancellationHandle {
        self.cancellation.clone()
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// A pinned session keeps its linear memory for one activity because its
    /// continuation is a suspended call stack. Losing the instance fails the
    /// activity rather than resuming against a fresh one.
    #[must_use]
    pub fn pinned_session(&self) -> bool {
        self.pinned_session
    }

    #[must_use]
    pub fn checkpoint(&self) -> u64 {
        self.checkpoint
    }

    /// Whether a subscription has uncommitted input ready for its callback.
    pub fn has_stream_input(&mut self) -> bool {
        let host = self.store.data_mut();
        if host.stream_input.is_none() {
            host.stream_input = host
                .subscription
                .as_mut()
                .and_then(|s| s.inbox.try_recv().ok());
        }
        host.stream_input.is_some()
    }

    /// Processes transient transport input. Only plugin outcomes enter the log.
    ///
    /// # Errors
    /// Returns callback, transport, or commit failures.
    pub async fn receive_input(&mut self, now_ms: i64) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.receive_input_inner(now_ms).await;
        self.interrupted = false;
        result
    }

    async fn receive_input_inner(&mut self, now_ms: i64) -> Result<Outcome, RuntimeError> {
        let input = self
            .store
            .data()
            .stream_input
            .as_ref()
            .ok_or_else(|| RuntimeError::new("no subscription input"))?;
        let id = input.id;
        let mut payload = input.payload.clone();
        payload["receivedAtMs"] = Value::from(now_ms);
        let bytes = serde_json::to_vec(&payload).unwrap();
        let blob = input.blob.clone();
        prepare_call(&mut self.store, &self.limits);
        self.store.data_mut().ingress_call = true;
        if let Some(blob) = blob {
            self.store.data_mut().visible_blobs.insert(blob);
        }
        let function = self
            .ingress
            .ok_or_else(|| RuntimeError::new("missing receive export"))?;
        let (outcome,) = function
            .call_async(&mut self.store, (bytes,))
            .await
            .map_err(|e| RuntimeError::trap(format!("receive trapped: {e:#}")))?;
        function
            .post_return_async(&mut self.store)
            .await
            .map_err(|e| RuntimeError::trap(format!("receive post-return trapped: {e:#}")))?;
        let outcome = outcome.map_err(|e| plugin_failure(&e))?;
        let outcome = guest::Outcome {
            events: outcome.events,
            mutations: outcome.mutations,
            checkpoint: None,
        };
        let result = self
            .commit(&outcome, None, CommitPhase::Ingress(id))
            .await?;
        if let Some(input) = self.store.data_mut().stream_input.take() {
            let _ = input.committed.send(());
        }
        Ok(result)
    }

    /// Calls `init` once, before any delivery.
    ///
    /// # Errors
    ///
    /// Returns an error on a trap, a plugin error, or a rejected commit.
    pub async fn init(&mut self) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.init_inner().await;
        self.interrupted = false;
        result
    }

    async fn init_inner(&mut self) -> Result<Outcome, RuntimeError> {
        let config = self.config.clone();
        let context = self.context();
        prepare_call(&mut self.store, &self.limits);
        let result = self
            .plugin
            .pluribus_plugin_lifecycle()
            .call_init(&mut self.store, &context, &config)
            .await;
        if result.as_ref().map_or(true, Result::is_err) {
            self.store.data_mut().release_handles();
        }
        let outcome = result
            .map_err(|error| RuntimeError::trap(format!("init trapped: {error:#}")))?
            .map_err(|error| plugin_failure(&error))?;
        let origin = EventId::new(self.store.data().delivery.origin_event_id.clone());
        self.commit(&outcome, Some(&origin), CommitPhase::Init)
            .await
    }

    /// Delivers events in ascending sequence order.
    ///
    /// # Errors
    ///
    /// Returns an error on a trap, a plugin error, or a rejected commit.
    pub async fn handle(&mut self, events: &[CommittedEvent]) -> Result<Outcome, RuntimeError> {
        self.begin_lifecycle()?;
        let result = self.handle_inner(events).await;
        self.interrupted = false;
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
        prepare_call(&mut self.store, &self.limits);
        let result = self
            .plugin
            .pluribus_plugin_lifecycle()
            .call_handle(&mut self.store, &context, &wit_events)
            .await;
        if result.as_ref().map_or(true, Result::is_err) {
            self.store.data_mut().release_handles();
        }
        let outcome = result
            .map_err(|error| RuntimeError::trap(format!("handle trapped: {error:#}")))?
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
                prepare_call(&mut self.store, &self.limits);
                self.store.data_mut().replaying = true;
                let result = self
                    .plugin
                    .pluribus_plugin_lifecycle()
                    .call_handle(&mut self.store, &context, &own)
                    .await;
                self.store.data_mut().replaying = false;
                outcome = result
                    .map_err(|e| RuntimeError::trap(format!("replay trapped: {e:#}")))?
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
        self.store.data_mut().subscription = None;
        self.store.data_mut().stream_input = None;
        let context = self.context();
        prepare_call(&mut self.store, &self.limits);
        let result = self
            .plugin
            .pluribus_plugin_lifecycle()
            .call_stop(&mut self.store, &context, deadline_at_ms)
            .await;
        if result.as_ref().map_or(true, Result::is_err) {
            self.store.data_mut().release_handles();
        }
        let outcome = result
            .map_err(|error| RuntimeError::trap(format!("stop trapped: {error:#}")))?
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
        if matches!(phase, CommitPhase::Stop) {
            host.subscription = None;
        } else if let Some(request) = host.pending_subscription.take() {
            host.subscription = None;
            host.subscription = Some(subscription::Subscription::start(host, request));
        }
        host.progress.notify_one();
        Ok(Outcome {
            events: receipt.events,
            checkpoint: receipt.checkpoint,
        })
    }
}

/// Arms the wall-clock ceiling for one call. The epoch ticker interrupts the
/// guest when it runs out.
fn prepare_call(store: &mut Store<HostState>, limits: &RuntimeLimits) {
    store.data_mut().pending_subscription = None;
    store.data_mut().ingress_call = false;
    let deadline = Instant::now() + limits.call_timeout;
    store.data_mut().call_deadline = Some(deadline);
    store.data_mut().stream_deadline = store
        .data()
        .stream_grant
        .as_ref()
        .map(|grant| Instant::now() + Duration::from_millis(u64::from(grant.max_timeout_ms)));
    let cancelled = Arc::clone(&store.data().cancelled);
    store.epoch_deadline_callback(move |_| {
        if cancelled.load(Ordering::Acquire) {
            return Err(wasmtime::Error::msg("call cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(wasmtime::Error::msg("call deadline exceeded"));
        }
        Ok(wasmtime::UpdateDeadline::Yield(1))
    });
    store.set_epoch_deadline(1);
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
}

struct HostState {
    credentials: Option<CredentialAccess>,
    identity: Option<String>,
    model: Option<String>,
    executor: tokio::runtime::Handle,
    limits: StoreLimits,
    delivery: Delivery,
    cancelled: Arc<AtomicBool>,
    state_store: Arc<dyn StateStore>,
    state_namespace: StateNamespace,
    replaying: bool,
    event_store: Arc<dyn EventStore>,
    blob_store: Arc<dyn BlobStore>,
    registry: Arc<EventTypeRegistry>,
    progress: Arc<tokio::sync::Notify>,
    emits: Vec<String>,
    active_uploads: HashSet<BlobUploadId>,
    visible_blobs: HashSet<BlobRef>,
    append_sequence: u64,
    http: Option<Arc<dyn HttpStreamService>>,
    http_grant: Option<HttpGrant>,
    stream: Option<Arc<dyn StreamService>>,
    stream_grant: Option<StreamGrant>,
    stream_deadline: Option<Instant>,
    call_deadline: Option<Instant>,
    transports: HashMap<u32, Transport>,
    readers: HashMap<u32, u32>,
    writers: HashMap<u32, u32>,
    handle_sequence: u32,
    pending_subscription: Option<subscription::Request>,
    stream_input: Option<subscription::Input>,
    ingress_supported: bool,
    ingress_call: bool,
    subscription: Option<subscription::Subscription>,
}

/// One connection and the halves the guest still holds. Reader and writer
/// share it; it closes when the last half goes or the delivery ends.
struct Transport {
    kind: TransportKind,
    halves: u8,
}

enum TransportKind {
    /// Receive-only HTTP response body. Buffered because the service
    /// hands over whole frames while a reader asks for bytes.
    Http {
        stream_id: String,
        buffer: VecDeque<u8>,
        ended: bool,
    },
    Socket {
        stream_id: String,
    },
}

/// A transport resolved without holding a borrow on the registry.
enum Target {
    Http(String),
    Socket(String),
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
        let stream_deadline = granted
            .stream_grant
            .as_ref()
            .map(|grant| Instant::now() + Duration::from_millis(u64::from(grant.max_timeout_ms)));
        Self {
            credentials: granted.credentials,
            model: granted.model,
            identity: granted.identity,
            executor: tokio::runtime::Handle::current(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_bytes)
                .memories(4)
                .tables(8)
                .instances(32)
                .trap_on_grow_failure(true)
                .build(),
            delivery,
            cancelled,
            state_store: services.state,
            state_namespace,
            replaying: false,
            event_store: services.events,
            blob_store: services.blobs,
            registry: services.registry,
            progress: services.progress,
            emits,
            active_uploads: HashSet::new(),
            visible_blobs,
            append_sequence: 0,
            http: granted.http,
            http_grant: granted.http_grant,
            stream: granted.stream,
            stream_grant: granted.stream_grant,
            stream_deadline,
            call_deadline: None,
            transports: HashMap::new(),
            readers: HashMap::new(),
            writers: HashMap::new(),
            handle_sequence: 0,
            pending_subscription: None,
            stream_input: None,
            ingress_supported: false,
            ingress_call: false,
            subscription: None,
        }
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
        for id in self.transports.keys().copied().collect::<Vec<_>>() {
            self.close_transport(id);
        }
        self.readers.clear();
        self.writers.clear();
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
        if causation_id.is_none() && !self.ingress_call {
            return Err(RuntimeError::new(
                "proposal needs an explicit causation ID when a batch carries several events",
            ));
        }
        let mut payload = core_payload(&proposal.payload).map_err(|error| {
            RuntimeError::new(format!("invalid proposal payload: {}", error.message))
        })?;
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

    async fn now_ms(&mut self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
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
    async fn random_bytes(&mut self, length: u32) -> Result<Vec<u8>, types::Error> {
        if length > 1024 {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "random request too large",
            ));
        }
        let mut bytes = vec![0; length as usize];
        getrandom::fill(&mut bytes)
            .map_err(|_| host_error(types::ErrorCode::Unavailable, "randomness unavailable"))?;
        Ok(bytes)
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
        store
            .get(&EventId::new(event_id))
            .await
            .map_err(|error| {
                host_error(types::ErrorCode::Internal, &format!("cannot read: {error}"))
            })?
            .as_ref()
            .filter(|event| {
                event.request.stream_id == StreamId::new(self.delivery.agent.id.clone())
                    && http_request_visible(event, &self.delivery.instance_id)
            })
            .map(wit_event)
            .ok_or_else(|| host_error(types::ErrorCode::NotFound, "no such event"))
    }

    async fn query(
        &mut self,
        filter: events::Filter,
        limit: u32,
    ) -> Result<events::Page, types::Error> {
        let query = EventQuery {
            after_sequence: filter.after_sequence,
            event_types: filter.event_types,
            correlation_id: filter.correlation_id,
            activity_id: filter.activity_id,
            recorded_from_ms: filter.recorded_from_ms,
            recorded_to_ms: filter.recorded_to_ms,
        };
        let store = self.event_store.clone();
        let stream = StreamId::new(self.delivery.agent.id.clone());
        let events = store
            .query(&stream, &query, limit.min(MAX_EVENT_PAGE) as usize)
            .await
            .map_err(|error| {
                host_error(
                    types::ErrorCode::Internal,
                    &format!("cannot query: {error}"),
                )
            })?;
        let next_sequence = events.last().map(|event| {
            self.progress.notify_one();
            event.sequence
        });
        Ok(events::Page {
            events: events
                .iter()
                .filter(|event| http_request_visible(event, &self.delivery.instance_id))
                .map(wit_event)
                .collect(),
            next_sequence,
        })
    }
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

impl http::Host for HostState {
    async fn exchange(
        &mut self,
        request: http::InlineRequest,
    ) -> Result<http::InlineResponse, types::Error> {
        let mut grant = self.http_grant()?.clone();
        grant.max_request_bytes = grant.max_request_bytes.min(1024 * 1024);
        grant.max_response_bytes = grant.max_response_bytes.min(1024 * 1024);
        let timeout = request.timeout_ms.min(self.call_budget()?);
        let result = self
            .http_wait(pluribus_host_http::exchange_inline(
                &grant,
                &request.method,
                &request.url,
                request
                    .headers
                    .into_iter()
                    .map(|h| HttpHeader {
                        name: h.name,
                        value: h.value,
                    })
                    .collect(),
                request.body,
                timeout,
            ))
            .await?;
        Ok(http::InlineResponse {
            status: result.0,
            body: result.1,
        })
    }

    async fn subscribe(&mut self, request: http::Request) -> Result<(), types::Error> {
        if !self.ingress_supported || self.replaying || self.cancelled.load(Ordering::Acquire) {
            return Err(stream_denied());
        }
        self.http_service()?;
        self.http_grant()?;
        let timeout = request.timeout_ms.min(self.http_grant()?.max_timeout_ms);
        let mut request = self.http_request(request)?;
        request.timeout_ms = timeout;
        self.pending_subscription = Some(subscription::Request::Http(request));
        Ok(())
    }

    async fn send(&mut self, request: http::Request) -> Result<http::Response, types::Error> {
        let request = self.http_request(request)?;
        let service = self.http_service()?;
        let grant = self.http_grant()?;
        let response = self.http_wait(service.send(grant, &request)).await?;
        self.visible_blobs.insert(response.body.clone());
        Ok(http::Response {
            status: response.status,
            headers: response
                .headers
                .into_iter()
                .map(|header| http::Header {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
            body: wit_blob_ref(&response.body),
        })
    }

    async fn sse(
        &mut self,
        request: http::Request,
    ) -> Result<Resource<reader::Reader>, types::Error> {
        let request = self.http_request(request)?;
        let stream_id = self
            .http_wait(self.http_service()?.open_stream(
                self.http_grant()?,
                HttpStreamProtocol::Bytes,
                &request,
            ))
            .await?;
        Ok(self.register_read_only(TransportKind::Http {
            stream_id,
            buffer: VecDeque::new(),
            ended: false,
        }))
    }
}

impl socket::Host for HostState {
    async fn subscribe(&mut self, request: Vec<u8>) -> Result<(), types::Error> {
        let (_, grant) = self.stream_access()?;
        if !self.ingress_supported || self.replaying || self.cancelled.load(Ordering::Acquire) {
            return Err(stream_denied());
        }
        if request.len() as u64 > grant.max_bytes || request.len() > 32 * 1024 {
            return Err(host_error(
                types::ErrorCode::ResourceExhausted,
                "subscription request exceeds limit",
            ));
        }
        self.pending_subscription = Some(subscription::Request::Socket(request));
        Ok(())
    }

    async fn connect(
        &mut self,
    ) -> Result<(Resource<reader::Reader>, Resource<writer::Writer>), types::Error> {
        let (service, grant) = self.stream_access()?;
        let deadline = self.stream_deadline.ok_or_else(stream_denied)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(host_error(types::ErrorCode::Cancelled, "stream cancelled"));
        }
        let remaining = self.stream_budget(deadline)?;
        let grant = StreamGrant {
            max_timeout_ms: grant.max_timeout_ms.min(remaining),
            ..grant
        };
        let stream_id = self
            .cancellable(service.open(&grant))
            .await?
            .map_err(stream_plugin_error)?;
        Ok(self.register_duplex(TransportKind::Socket { stream_id }))
    }
}

impl reader::Host for HostState {}

impl reader::HostReader for HostState {
    async fn receive(
        &mut self,
        read: Resource<reader::Reader>,
        max_bytes: u32,
        timeout_ms: u32,
    ) -> Result<types::Chunk, types::Error> {
        if max_bytes == 0 {
            return Err(host_error(
                types::ErrorCode::InvalidArgument,
                "max bytes must be positive",
            ));
        }
        let timeout_ms = timeout_ms.min(self.call_budget()?);
        let (id, target) = self.read_target(&read)?;
        match target {
            Target::Http(stream_id) => {
                self.receive_http(id, &stream_id, max_bytes, timeout_ms)
                    .await
            }
            Target::Socket(stream_id) => {
                self.receive_socket(id, &stream_id, max_bytes, timeout_ms)
                    .await
            }
        }
    }

    async fn drop(&mut self, read: Resource<reader::Reader>) -> wasmtime::Result<()> {
        if let Some(id) = self.readers.remove(&read.rep()) {
            self.release_half(id);
        }
        Ok(())
    }
}

impl writer::Host for HostState {}

impl writer::HostWriter for HostState {
    async fn send(
        &mut self,
        write: Resource<writer::Writer>,
        bytes: Vec<u8>,
    ) -> Result<(), types::Error> {
        let (id, target) = self.write_target(&write)?;
        match target {
            Target::Socket(stream_id) => {
                let (service, _) = self.stream_access()?;
                self.cancellable(service.send(&stream_id, &bytes))
                    .await?
                    .map_err(|error| {
                        self.close_transport(id);
                        stream_plugin_error(error)
                    })
            }
            // No opener hands out a writer over HTTP yet; a future
            // `http.websocket` would route here.
            Target::Http(_) => Err(host_error(
                types::ErrorCode::Unsupported,
                "HTTP channels are receive-only",
            )),
        }
    }

    /// Half-closes: the peer reads EOF while the reader stays usable.
    async fn drop(&mut self, write: Resource<writer::Writer>) -> wasmtime::Result<()> {
        let Some(id) = self.writers.remove(&write.rep()) else {
            return Ok(());
        };
        if let Some(Transport {
            kind: TransportKind::Socket { stream_id },
            ..
        }) = self.transports.get(&id)
        {
            let stream_id = stream_id.clone();
            if let Some(service) = self.stream.clone() {
                service.shutdown_write(&stream_id);
            }
        }
        self.release_half(id);
        Ok(())
    }
}

impl HostState {
    async fn http_wait<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, HttpError>>,
    ) -> Result<T, types::Error> {
        self.cancellable(operation)
            .await?
            .map_err(|e| http_error(&e))
    }

    async fn cancellable<T>(
        &self,
        operation: impl std::future::Future<Output = T>,
    ) -> Result<T, types::Error> {
        tokio::pin!(operation);
        loop {
            self.call_budget()?;
            tokio::select! {
                biased;
                result = &mut operation => return Ok(result),
                () = tokio::time::sleep(EPOCH_TICK) => {}
            }
        }
    }

    fn http_service(&self) -> Result<&dyn HttpStreamService, types::Error> {
        self.http
            .as_deref()
            .ok_or_else(|| host_error(types::ErrorCode::Unavailable, "HTTP service is unavailable"))
    }

    fn http_grant(&self) -> Result<&HttpGrant, types::Error> {
        self.http_grant
            .as_ref()
            .ok_or_else(|| host_error(types::ErrorCode::PermissionDenied, "no HTTP grant"))
    }

    fn http_request(&self, request: http::Request) -> Result<HttpRequest, types::Error> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(host_error(types::ErrorCode::Cancelled, "call cancelled"));
        }
        let body = request.body.map(core_blob_ref).transpose()?;
        if let Some(body) = &body {
            self.ensure_visible_blob(body)?;
        }
        Ok(HttpRequest {
            method: request.method,
            url: request.url,
            headers: request
                .headers
                .into_iter()
                .map(|header| HttpHeader {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
            body,
            credential_handle: request.credential.map(SecretHandle::new),
            timeout_ms: request.timeout_ms.min(self.call_budget()?),
        })
    }

    fn call_budget(&self) -> Result<u32, types::Error> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(host_error(types::ErrorCode::Cancelled, "call cancelled"));
        }
        let remaining = self.call_deadline.map_or(u32::MAX, |deadline| {
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

    fn stream_access(&self) -> Result<(Arc<dyn StreamService>, StreamGrant), types::Error> {
        let service = self.stream.clone().ok_or_else(stream_denied)?;
        let grant = self.stream_grant.clone().ok_or_else(stream_denied)?;
        Ok((service, grant))
    }

    /// Milliseconds left before the grant lifetime or the authority deadline.
    fn stream_budget(&self, deadline: Instant) -> Result<u32, types::Error> {
        let authority_remaining = self.delivery.deadline_at_ms.map_or(u32::MAX, |limit| {
            u32::try_from(limit.saturating_sub(system_time_ms()).max(0)).unwrap_or(u32::MAX)
        });
        let remaining = u32::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(u32::MAX)
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

    fn next_handle(&mut self) -> u32 {
        self.handle_sequence += 1;
        self.handle_sequence
    }

    fn register_read_only(&mut self, kind: TransportKind) -> Resource<reader::Reader> {
        let id = self.next_handle();
        self.transports.insert(id, Transport { kind, halves: 1 });
        let read = self.next_handle();
        self.readers.insert(read, id);
        Resource::new_own(read)
    }

    fn register_duplex(
        &mut self,
        kind: TransportKind,
    ) -> (Resource<reader::Reader>, Resource<writer::Writer>) {
        let id = self.next_handle();
        self.transports.insert(id, Transport { kind, halves: 2 });
        let read = self.next_handle();
        self.readers.insert(read, id);
        let write = self.next_handle();
        self.writers.insert(write, id);
        (Resource::new_own(read), Resource::new_own(write))
    }

    fn read_target(&self, read: &Resource<reader::Reader>) -> Result<(u32, Target), types::Error> {
        let id = *self.readers.get(&read.rep()).ok_or_else(closed_channel)?;
        Ok((id, self.target(id)?))
    }

    fn write_target(
        &self,
        write: &Resource<writer::Writer>,
    ) -> Result<(u32, Target), types::Error> {
        let id = *self.writers.get(&write.rep()).ok_or_else(closed_channel)?;
        Ok((id, self.target(id)?))
    }

    fn target(&self, id: u32) -> Result<Target, types::Error> {
        match self.transports.get(&id) {
            Some(Transport {
                kind: TransportKind::Http { stream_id, .. },
                ..
            }) => Ok(Target::Http(stream_id.clone())),
            Some(Transport {
                kind: TransportKind::Socket { stream_id },
                ..
            }) => Ok(Target::Socket(stream_id.clone())),
            None => Err(closed_channel()),
        }
    }

    /// Drops one half. The transport closes when the last one goes.
    fn release_half(&mut self, id: u32) {
        let done = match self.transports.get_mut(&id) {
            Some(transport) => {
                transport.halves = transport.halves.saturating_sub(1);
                transport.halves == 0
            }
            None => false,
        };
        if done {
            self.close_transport(id);
        }
    }

    /// Closes the transport whatever the guest still holds. Both halves
    /// then report the channel closed.
    fn close_transport(&mut self, id: u32) {
        match self.transports.remove(&id) {
            Some(Transport {
                kind: TransportKind::Http { stream_id, .. },
                ..
            }) => {
                if let (Some(service), Some(grant)) = (&self.http, &self.http_grant) {
                    service.close_stream(grant, &stream_id);
                }
            }
            Some(Transport {
                kind: TransportKind::Socket { stream_id },
                ..
            }) => {
                if let Some(service) = &self.stream {
                    service.close(&stream_id);
                }
            }
            None => {}
        }
    }

    fn http_buffer(&mut self, id: u32) -> Result<(&mut VecDeque<u8>, &mut bool), types::Error> {
        match self.transports.get_mut(&id) {
            Some(Transport {
                kind: TransportKind::Http { buffer, ended, .. },
                ..
            }) => Ok((buffer, ended)),
            Some(Transport {
                kind: TransportKind::Socket { .. },
                ..
            })
            | None => Err(closed_channel()),
        }
    }

    async fn receive_http(
        &mut self,
        id: u32,
        stream_id: &str,
        max_bytes: u32,
        timeout_ms: u32,
    ) -> Result<types::Chunk, types::Error> {
        let (buffer, ended) = self.http_buffer(id)?;
        if buffer.is_empty() && !*ended {
            let page = self
                .http_wait(self.http_service()?.receive(
                    self.http_grant()?,
                    stream_id,
                    1,
                    timeout_ms,
                ))
                .await?;
            let (buffer, ended) = self.http_buffer(id)?;
            buffer.extend(page.frames.into_iter().flat_map(|frame| frame.data));
            *ended = page.closed;
        }
        let (buffer, ended) = self.http_buffer(id)?;
        let count = buffer
            .len()
            .min(usize::try_from(max_bytes).unwrap_or(usize::MAX))
            .min(MAX_BLOB_CHUNK_BYTES);
        let bytes = buffer.drain(..count).collect();
        let closed = *ended && buffer.is_empty();
        if closed {
            self.close_transport(id);
        }
        Ok(types::Chunk { bytes, closed })
    }

    async fn receive_socket(
        &mut self,
        id: u32,
        stream_id: &str,
        max_bytes: u32,
        timeout_ms: u32,
    ) -> Result<types::Chunk, types::Error> {
        let (service, _) = self.stream_access()?;
        let deadline = self.stream_deadline.ok_or_else(stream_denied)?;
        let timeout_ms = timeout_ms.min(self.stream_budget(deadline)?);
        match self
            .cancellable(service.receive(stream_id, max_bytes, timeout_ms, &self.cancelled))
            .await?
        {
            Ok(page) => {
                if page.closed {
                    self.close_transport(id);
                }
                Ok(types::Chunk {
                    bytes: page.bytes,
                    closed: page.closed,
                })
            }
            Err(error) => {
                self.close_transport(id);
                Err(stream_plugin_error(error))
            }
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

fn http_error(error: &HttpError) -> types::Error {
    let code = match error {
        HttpError::Invalid(_) => types::ErrorCode::InvalidArgument,
        HttpError::Unsupported(_) => types::ErrorCode::Unsupported,
        HttpError::PermissionDenied(_) | HttpError::AuthenticationRequired => {
            types::ErrorCode::PermissionDenied
        }
        HttpError::NotFound(_) => types::ErrorCode::NotFound,
        HttpError::ResourceExhausted(_) => types::ErrorCode::ResourceExhausted,
        HttpError::Timeout => types::ErrorCode::DeadlineExceeded,
        HttpError::Cancelled => types::ErrorCode::Cancelled,
        HttpError::Unavailable(_) => types::ErrorCode::Unavailable,
        HttpError::Internal(_) => types::ErrorCode::Internal,
    };
    host_error(code, &error.to_string())
}

fn closed_channel() -> types::Error {
    host_error(types::ErrorCode::InvalidArgument, "channel is closed")
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
        StreamError::Unavailable(message) => host_error(types::ErrorCode::Unavailable, &message),
    }
}

fn plugin_failure(error: &types::Error) -> RuntimeError {
    RuntimeError::new(format!("{}: {}", error_code(error.code), error.message))
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
