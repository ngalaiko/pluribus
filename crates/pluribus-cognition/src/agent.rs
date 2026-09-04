//! The agent loop.
//!
//! One pass fires due timers, gates new requests against authority, then
//! delivers pending events to each instance. Every step reads its position
//! from the log, so a restart resumes rather than replaying or skipping.

use crate::dispatch::{
    PendingTimer, Registration, Routed, Router, RouterError, Subscriptions, fire_timer,
};
use pluribus_core::{
    Authority, CommittedEvent, ConstraintPolicy, EventId, EventStore, PrincipalRef, StreamId,
};
use pluribus_plugin_package::{PluginComponent, PluginPackage};
use pluribus_runtime_wasm::{CancellationHandle, Outcome};
use pluribus_runtime_wasm::{Delivery, PluginInstance, PluginServices, Runtime, RuntimeError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::JoinHandle;

const MAX_COGNITION_BATCH: usize = 16;

/// Supplies the authority an activity carries. Keeping this out of the loop
/// leaves standing policy replaceable.
#[async_trait::async_trait]
pub trait AuthorityResolver: Send + Sync {
    /// Authority for the activity that produced this request event.
    ///
    /// # Errors
    ///
    /// Returns an error when no authority can be derived, which denies.
    async fn resolve(&self, event: &CommittedEvent) -> Result<Authority, String>;
}

/// What one pass accomplished. All zero means the agent is idle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    pub timers_fired: usize,
    pub requests_gated: usize,
    pub requests_denied: usize,
    pub deliveries: usize,
    pub events_committed: usize,
}

impl Progress {
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.timers_fired == 0
            && self.requests_gated == 0
            && self.deliveries == 0
            && self.events_committed == 0
    }
}

/// One agent: its instances, its router, and its position in the log.
pub struct Agent<P, R> {
    router: Router<P>,
    external: BTreeSet<String>,
    workers: BTreeMap<String, Worker>,
    cancellations: BTreeMap<String, CancellationHandle>,
    stopped: Arc<AtomicBool>,
    authority: R,
    runtime: Runtime,
    instances: BTreeMap<String, PluginInstance>,
    events: Arc<dyn EventStore>,
    stream_id: StreamId,
    principal: PrincipalRef,
    /// Highest sequence already passed through the request gate.
    gated_through: u64,
    admission_now_ms: i64,
    batch: usize,
    /// Instances withdrawn after a trap, and why. They receive no further
    /// deliveries; restoring one is an operator decision.
    failed: BTreeMap<String, String>,
    backoff: BTreeMap<String, (u32, i64)>,
    health_through: u64,
}

impl<P, R> Drop for Agent<P, R> {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for cancellation in self.cancellations.values() {
            cancellation.cancel();
        }
        for worker in self.workers.values() {
            worker.handle.abort();
        }
    }
}

#[derive(Clone, Default)]
pub struct ComponentInstall {
    pub services: PluginServices,
    pub models: Vec<String>,
}

struct StagedComponent {
    instance: PluginInstance,
    registration: Registration,
    rebuilds: Vec<String>,
}

struct Worker {
    batch: Vec<CommittedEvent>,
    handle: JoinHandle<(PluginInstance, Result<Outcome, RuntimeError>)>,
}

impl<P: ConstraintPolicy, R: AuthorityResolver> Agent<P, R> {
    #[must_use]
    pub fn new(
        router: Router<P>,
        authority: R,
        runtime: Runtime,
        events: Arc<dyn EventStore>,
        stream_id: StreamId,
        principal: PrincipalRef,
    ) -> Self {
        Self {
            router,
            external: BTreeSet::new(),
            workers: BTreeMap::new(),
            cancellations: BTreeMap::new(),
            stopped: Arc::new(AtomicBool::new(false)),
            authority,
            runtime,
            instances: BTreeMap::new(),
            events,
            stream_id,
            principal,
            gated_through: 0,
            admission_now_ms: 0,
            batch: 100,
            failed: BTreeMap::new(),
            backoff: BTreeMap::new(),
            health_through: 0,
        }
    }

    /// Events delivered to one instance per pass.
    #[must_use]
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// Validates and instantiates every component before lifecycle calls or activation.
    ///
    /// # Errors
    /// Returns configuration, compilation, registration, or lifecycle failures.
    /// Lifecycle commits remain durable if a later lifecycle call fails.
    #[allow(clippy::needless_pass_by_value)]
    pub async fn install_package(
        &mut self,
        package: &PluginPackage,
        config: &Value,
        delivery: Delivery,
        mut components: BTreeMap<String, ComponentInstall>,
    ) -> Result<Vec<String>, AgentError> {
        let config = &package.resolve_config(config);
        package
            .validate_config(config)
            .map_err(|e| AgentError::Storage(e.to_string()))?;
        if !components.keys().eq(package.components().keys()) {
            return Err(AgentError::Storage(
                "component installation settings must match the package".into(),
            ));
        }
        let mut staged = Vec::new();
        let mut registrations = self.router.registrations().to_vec();
        for (name, component) in package.components() {
            let setup = components.remove(name).ok_or_else(|| {
                AgentError::Storage(format!("missing component settings: {name}"))
            })?;
            let mut component_delivery = delivery.clone();
            component_delivery.instance_id =
                pluribus_plugin_package::component_id(&delivery.instance_id, name);
            let projected = component
                .project_config(config)
                .map_err(|e| AgentError::Storage(e.to_string()))?;
            let prepared = self
                .stage_component(
                    component,
                    projected,
                    component_delivery,
                    setup.services,
                    &setup.models,
                    &registrations,
                )
                .await?;
            registrations.push(prepared.registration.clone());
            staged.push(prepared);
        }
        for prepared in &mut staged {
            prepared
                .instance
                .init()
                .await
                .map_err(AgentError::Runtime)?;
            prepared
                .instance
                .rebuild(&prepared.rebuilds)
                .await
                .map_err(AgentError::Runtime)?;
        }
        let ids = staged
            .iter()
            .map(|s| s.registration.instance_id.clone())
            .collect();
        for prepared in staged {
            self.activate_component(prepared);
        }
        Ok(ids)
    }

    /// Installs one explicitly selected component.
    ///
    /// # Errors
    /// Returns configuration, compilation, registration, or lifecycle failures.
    pub async fn install_component(
        &mut self,
        component: &PluginComponent,
        config: &Value,
        delivery: Delivery,
        services: PluginServices,
        models: &[String],
    ) -> Result<(), AgentError> {
        let mut staged = self
            .stage_component(
                component,
                config,
                delivery,
                services,
                models,
                self.router.registrations(),
            )
            .await?;
        staged.instance.init().await.map_err(AgentError::Runtime)?;
        staged
            .instance
            .rebuild(&staged.rebuilds)
            .await
            .map_err(AgentError::Runtime)?;
        self.activate_component(staged);
        Ok(())
    }

    async fn stage_component(
        &self,
        component: &PluginComponent,
        config: &Value,
        delivery: Delivery,
        services: PluginServices,
        models: &[String],
        registrations: &[Registration],
    ) -> Result<StagedComponent, AgentError> {
        let instance_id = delivery.instance_id.clone();
        if registrations.iter().any(|r| r.instance_id == instance_id)
            || self.instances.contains_key(&instance_id)
            || self.workers.contains_key(&instance_id)
        {
            return Err(AgentError::Storage(format!(
                "component instance already installed: {instance_id}"
            )));
        }
        let manifest = component.manifest();
        let subscriptions = Subscriptions::from_manifest(manifest, models);
        if subscriptions.whole_stream && registrations.iter().any(|r| r.subscriptions.whole_stream)
        {
            return Err(AgentError::Storage("one cognition writer per agent".into()));
        }
        for name in &subscriptions.capabilities {
            if name.starts_with("memory.")
                && registrations
                    .iter()
                    .any(|r| r.subscriptions.capabilities.contains(name))
            {
                return Err(AgentError::Storage(format!(
                    "duplicate memory provider: {name}"
                )));
            }
        }
        if !manifest.rebuilds.is_empty()
            && manifest.imports.iter().any(|import| {
                ![
                    "pluribus:plugin/events@1.0.0",
                    "pluribus:plugin/state@1.0.0",
                ]
                .contains(&import.as_str())
            })
        {
            return Err(AgentError::Storage(
                "rebuild providers may import only events and state".into(),
            ));
        }
        for kind in &manifest.rebuilds {
            if !manifest.subscribes.contains(kind)
                || !manifest.emits.contains(kind)
                || kind.contains('*')
                || kind == "capability.requested"
            {
                return Err(AgentError::Storage(
                    "rebuild types must be subscribed mutation emissions".into(),
                ));
            }
        }
        let instance = self
            .runtime
            .instantiate(component, config, delivery, services)
            .await
            .map_err(AgentError::Runtime)?;
        Ok(StagedComponent {
            instance,
            registration: Registration {
                instance_id,
                subscriptions,
            },
            rebuilds: manifest.rebuilds.clone(),
        })
    }

    fn activate_component(&mut self, staged: StagedComponent) {
        let instance_id = staged.registration.instance_id.clone();
        if !staged.registration.subscriptions.whole_stream {
            self.external.insert(instance_id.clone());
        }
        self.router.register(staged.registration);
        self.cancellations
            .insert(instance_id.clone(), staged.instance.cancellation());
        self.instances.insert(instance_id, staged.instance);
    }

    /// Stops an instance and unregisters it.
    ///
    /// # Errors
    ///
    /// Returns an error when `stop` or its commit fails.
    pub async fn remove(
        &mut self,
        instance_id: &str,
        deadline_at_ms: i64,
    ) -> Result<(), AgentError> {
        if let Some(handle) = self.cancellations.remove(instance_id) {
            handle.cancel();
        }
        if let Some(worker) = self.workers.get_mut(instance_id) {
            let joined = (&mut worker.handle).await;
            self.workers.remove(instance_id);
            let (instance, _) =
                joined.map_err(|_| AgentError::Storage("provider worker panicked".into()))?;
            self.instances.insert(instance_id.into(), instance);
        }
        self.external.remove(instance_id);
        let stopped = if let Some(instance) = self.instances.get_mut(instance_id) {
            instance.stop(deadline_at_ms).await
        } else {
            self.router.unregister(instance_id);
            return Ok(());
        };
        self.instances.remove(instance_id);
        self.router.unregister(instance_id);
        stopped.map(|_| ()).map_err(AgentError::Runtime)
    }

    #[must_use]
    pub fn instance_ids(&self) -> Vec<String> {
        self.instances
            .keys()
            .chain(self.workers.keys())
            .cloned()
            .collect()
    }

    /// Instances withdrawn after a trap, mapped to the reason.
    #[must_use]
    pub const fn failed(&self) -> &BTreeMap<String, String> {
        &self.failed
    }

    /// Runs one pass.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read or a delivery fails.
    pub async fn tick(&mut self, now_ms: i64) -> Result<Progress, AgentError> {
        self.admission_now_ms = now_ms;
        self.refresh_health().await?;
        let mut progress = Progress::default();
        self.collect_workers(&mut progress, false).await?;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(progress);
        }

        for timer in self.due_timers(now_ms).await? {
            fire_timer(
                self.events.as_ref(),
                &self.stream_id,
                &self.principal,
                &timer,
            )
            .await
            .map_err(AgentError::Router)?;
            progress.timers_fired += 1;
        }

        let cognition: Vec<_> = self
            .router
            .registrations()
            .iter()
            .filter(|r| r.subscriptions.whole_stream)
            .map(|r| r.instance_id.clone())
            .collect();
        for id in &cognition {
            if let Some(count) = self.deliver(id).await? {
                progress.deliveries += 1;
                progress.events_committed += count;
            }
        }
        let (gated, denied) = self.gate_new_requests(now_ms).await?;
        progress.requests_gated = gated;
        progress.requests_denied = denied;

        for instance_id in self.instance_ids() {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            if cognition.contains(&instance_id) || self.workers.contains_key(&instance_id) {
                continue;
            }
            let committed = self.deliver(&instance_id).await?;
            if let Some(count) = committed {
                progress.deliveries += 1;
                progress.events_committed += count;
            }
        }

        Ok(progress)
    }

    /// Runs a pass and joins its providers. Deterministic drivers use this boundary.
    ///
    /// # Errors
    /// Returns storage or provider failures.
    pub async fn tick_wait(&mut self, now_ms: i64) -> Result<Progress, AgentError> {
        let mut progress = self.tick(now_ms).await?;
        self.collect_workers(&mut progress, true).await?;
        Ok(progress)
    }

    /// Waits for local progress, bounded to poll external writers and timers.
    pub async fn wait_for_progress(&self, maximum: std::time::Duration) {
        let progress = self.runtime.progress_notification();
        let _ = tokio::time::timeout(maximum, progress.notified()).await;
    }

    /// Independent admission latch for the emergency-stop monitor.
    pub fn stop_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopped)
    }

    pub fn cancellation_handles(&self) -> Vec<CancellationHandle> {
        self.cancellations.values().cloned().collect()
    }

    async fn collect_workers(
        &mut self,
        progress: &mut Progress,
        wait: bool,
    ) -> Result<(), AgentError> {
        let ready: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| wait || w.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let joined = (&mut self.workers.get_mut(&id).unwrap().handle).await;
            let worker = self.workers.remove(&id).unwrap();
            let (instance, result) =
                joined.map_err(|_| AgentError::Storage("provider worker panicked".into()))?;
            self.instances.insert(id.clone(), instance);
            match result {
                Ok(outcome) => {
                    if !self.stopped.load(Ordering::Acquire)
                        && let Some(handle) = self.cancellations.get(&id)
                    {
                        handle.reset();
                    }
                    self.recovered(&id).await?;
                    progress.events_committed += outcome.events.len();
                }
                Err(error) if error.trapped() => {
                    self.quarantine(
                        &id,
                        worker.batch.last().unwrap().sequence,
                        &worker.batch,
                        &error,
                    )
                    .await?;
                }
                Err(error) => self.defer(&id, &error).await?,
            }
        }
        Ok(())
    }

    async fn refresh_health(&mut self) -> Result<(), AgentError> {
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &pluribus_core::EventQuery {
                        after_sequence: Some(self.health_through),
                        event_types: vec![
                            "component.backoff".into(),
                            "component.recovered".into(),
                            "component.failed".into(),
                        ],
                        ..Default::default()
                    },
                    self.batch,
                )
                .await
                .map_err(|e| AgentError::Storage(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            for event in page {
                self.health_through = event.sequence;
                if event.request.actor.kind != pluribus_core::PrincipalKind::Node
                    || ![
                        self.principal.id.as_str(),
                        &format!("operator:{}", self.principal.id.as_str()),
                    ]
                    .contains(&event.request.actor.id.as_str())
                {
                    continue;
                }
                let pluribus_core::EventPayload::CanonicalJson(bytes) = event.request.payload
                else {
                    continue;
                };
                let payload: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| AgentError::Storage(e.to_string()))?;
                let Some(id) = payload["instanceId"].as_str() else {
                    continue;
                };
                match event.request.event_type.as_str() {
                    "component.backoff" => {
                        self.backoff.insert(
                            id.into(),
                            (
                                payload["failures"].as_u64().unwrap_or(1).min(32) as u32,
                                payload["retryAtMs"].as_i64().unwrap_or(0),
                            ),
                        );
                    }
                    "component.recovered" => {
                        self.backoff.remove(id);
                        self.failed.remove(id);
                    }
                    "component.failed" => {
                        self.failed.insert(
                            id.into(),
                            payload["reason"]
                                .as_str()
                                .unwrap_or("component failed")
                                .into(),
                        );
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn defer(&mut self, id: &str, error: &RuntimeError) -> Result<(), AgentError> {
        let failures = self
            .backoff
            .get(id)
            .map_or(1, |(n, _)| n.saturating_add(1).min(32));
        let delay = (1000_i64 << failures.saturating_sub(1).min(6)).min(60_000);
        let retry_at = self.admission_now_ms.saturating_add(delay);
        self.router.append_health("component.backoff", serde_json::json!({
            "instanceId": id, "reason": error.to_string(), "failures": failures, "retryAtMs": retry_at
        })).await.map_err(AgentError::Router)?;
        self.backoff.insert(id.into(), (failures, retry_at));
        Ok(())
    }

    async fn recovered(&mut self, id: &str) -> Result<(), AgentError> {
        if self.backoff.contains_key(id) {
            self.router
                .append_health("component.recovered", serde_json::json!({"instanceId": id}))
                .await
                .map_err(AgentError::Router)?;
            self.backoff.remove(id);
        }
        Ok(())
    }

    async fn observations_delivered(&self) -> Result<bool, AgentError> {
        for registration in self
            .router
            .registrations()
            .iter()
            .filter(|r| r.subscriptions.whole_stream)
        {
            let checkpoint = self
                .instances
                .get(&registration.instance_id)
                .map_or(0, PluginInstance::checkpoint);
            let pending = self
                .events
                .query(
                    &self.stream_id,
                    &pluribus_core::EventQuery {
                        after_sequence: Some(checkpoint),
                        event_types: vec!["observation.received".into()],
                        ..pluribus_core::EventQuery::default()
                    },
                    1,
                )
                .await
                .map_err(|e| AgentError::Storage(e.to_string()))?;
            if !pending.is_empty() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn due_timers(&self, now_ms: i64) -> Result<Vec<PendingTimer>, AgentError> {
        Ok(self
            .router
            .timers(self.batch)
            .await
            .map_err(AgentError::Router)?
            .into_iter()
            .filter(|timer| timer.due_at_ms <= now_ms)
            .collect())
    }

    /// Runs the authority gate over requests committed since the last pass.
    ///
    /// A refused request gets a terminal `capability.denied`, so the requester
    /// resumes instead of waiting for a result that never arrives.
    async fn gate_new_requests(&mut self, now_ms: i64) -> Result<(usize, usize), AgentError> {
        let requests = self
            .events
            .read(&self.stream_id, self.gated_through, self.batch)
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        let mut gated = 0;
        let mut denied = 0;
        for event in requests {
            self.gated_through = self.gated_through.max(event.sequence);
            if event.request.event_type == "cognition.cancel-requested" {
                if let Some(request) = crate::payload_field(&event, "requestEventId") {
                    for (id, worker) in &self.workers {
                        if worker.batch.iter().any(|r| {
                            r.event_id.as_str() == request && r.request.actor == event.request.actor
                        }) && let Some(handle) = self.cancellations.get(id)
                        {
                            handle.cancel();
                        }
                    }
                }
                continue;
            }
            if event.request.event_type != "capability.requested" {
                continue;
            }
            if self
                .router
                .result_for(&event.event_id)
                .await
                .map_err(AgentError::Router)?
                .is_some()
            {
                continue;
            }
            gated += 1;
            let authority = match self.authority.resolve(&event).await {
                Ok(authority) => authority,
                Err(reason) => {
                    self.router
                        .append_denial(&event, &reason)
                        .await
                        .map_err(AgentError::Router)?;
                    denied += 1;
                    continue;
                }
            };
            if crate::payload_field(&event, "capability")
                .is_some_and(|name| name.starts_with("memory."))
                && (!authority.origin.trusted || authority.current_depth != 0)
            {
                self.router
                    .append_denial(&event, "memory requires a trusted root origin")
                    .await
                    .map_err(AgentError::Router)?;
                denied += 1;
                continue;
            }
            let routed = self
                .router
                .route_request(&event, &authority, &self.principal, now_ms)
                .await
                .map_err(AgentError::Router)?;
            match routed {
                Routed::Deliver { .. } => {}
                Routed::NoProvider { reason } | Routed::Denied { reason } => {
                    self.router
                        .append_denial(&event, &reason)
                        .await
                        .map_err(AgentError::Router)?;
                    denied += 1;
                }
            }
        }
        Ok((gated, denied))
    }

    /// Delivers one instance's pending events. Returns the number of events it
    /// committed, or `None` when it had nothing to handle.
    #[allow(clippy::too_many_lines)]
    async fn deliver(&mut self, instance_id: &str) -> Result<Option<usize>, AgentError> {
        if self.failed.contains_key(instance_id)
            || self
                .backoff
                .get(instance_id)
                .is_some_and(|(_, until)| *until > self.admission_now_ms)
        {
            return Ok(None);
        }
        let mut checkpoint = self
            .instances
            .get(instance_id)
            .ok_or_else(|| AgentError::UnknownInstance(instance_id.to_owned()))?
            .checkpoint();
        let whole_stream = self
            .router
            .registrations()
            .iter()
            .any(|r| r.instance_id == instance_id && r.subscriptions.whole_stream);
        let pending = loop {
            let pending = self
                .router
                .poll(
                    instance_id,
                    checkpoint,
                    if self.external.contains(instance_id) {
                        1
                    } else if whole_stream {
                        self.batch.min(MAX_COGNITION_BATCH)
                    } else {
                        self.batch
                    },
                )
                .await
                .map_err(AgentError::Router)?;
            if pending.is_empty() {
                self.recovered(instance_id).await?;
                return Ok(None);
            }
            checkpoint = pending.last().unwrap().sequence;
            if whole_stream {
                break pending;
            }
            let mut authorized = Vec::new();
            for event in pending {
                if event.request.event_type != "capability.requested"
                    || self
                        .router
                        .authorized_for(&event.event_id, instance_id)
                        .await
                        .map_err(AgentError::Router)?
                {
                    authorized.push(event);
                }
            }
            let authorized = self.drop_answered_requests(authorized).await?;
            if !authorized.is_empty() {
                break authorized;
            }
        };
        if self.stopped.load(Ordering::Acquire) {
            return Ok(None);
        }
        if self.external.contains(instance_id) {
            let activity = |events: &[CommittedEvent]| {
                events
                    .iter()
                    .any(|e| e.request.event_type == "capability.requested")
            };
            if activity(&pending)
                && self.workers.values().filter(|w| activity(&w.batch)).count() >= 4
            {
                return Ok(None);
            }
            if pending
                .iter()
                .any(|e| e.request.event_type == "model.requested")
                && self
                    .workers
                    .values()
                    .filter(|w| {
                        w.batch
                            .iter()
                            .any(|e| e.request.event_type == "model.requested")
                    })
                    .count()
                    >= 5
            {
                return Ok(None);
            }
            if pending.iter().any(|e| {
                matches!(
                    e.request.event_type.as_str(),
                    "capability.requested" | "code.evaluate-requested"
                )
            }) && !self.observations_delivered().await?
            {
                return Ok(None);
            }
            for request in &pending {
                if self
                    .router
                    .expired(request, self.admission_now_ms)
                    .await
                    .map_err(AgentError::Router)?
                {
                    self.instances
                        .get_mut(instance_id)
                        .unwrap()
                        .skip(checkpoint)
                        .await
                        .map_err(AgentError::Runtime)?;
                    return Ok(Some(0));
                }
                if request.request.event_type == "code.evaluate-requested"
                    && !self
                        .router
                        .revision_current(request)
                        .await
                        .map_err(AgentError::Router)?
                {
                    self.router
                        .reject_stale_code(request)
                        .await
                        .map_err(AgentError::Router)?;
                    self.instances
                        .get_mut(instance_id)
                        .unwrap()
                        .skip(checkpoint)
                        .await
                        .map_err(AgentError::Runtime)?;
                    return Ok(Some(0));
                }
                if request.request.event_type == "capability.requested" {
                    let permitted = if let Ok(authority) = self.authority.resolve(request).await {
                        matches!(self.router.route_request(request,&authority,&self.principal,self.admission_now_ms).await,Ok(Routed::Deliver { instance_id: ref provider }) if provider == instance_id)
                    } else {
                        false
                    };
                    if !permitted
                        || !self
                            .router
                            .revision_current(request)
                            .await
                            .map_err(AgentError::Router)?
                    {
                        self.router
                            .append_denial(
                                request,
                                "authority or job revision changed before admission",
                            )
                            .await
                            .map_err(AgentError::Router)?;
                        self.instances
                            .get_mut(instance_id)
                            .unwrap()
                            .skip(checkpoint)
                            .await
                            .map_err(AgentError::Runtime)?;
                        return Ok(Some(0));
                    }
                }
                if matches!(
                    request.request.event_type.as_str(),
                    "model.requested" | "capability.requested" | "code.evaluate-requested"
                ) && !self
                    .router
                    .admit_attempt(request, instance_id)
                    .await
                    .map_err(AgentError::Router)?
                {
                    self.instances
                        .get_mut(instance_id)
                        .unwrap()
                        .skip(checkpoint)
                        .await
                        .map_err(AgentError::Runtime)?;
                    return Ok(Some(0));
                }
            }
            if self.stopped.load(Ordering::Acquire) {
                return Ok(None);
            }
            let mut instance = self.instances.remove(instance_id).unwrap();
            let batch = pending.clone();
            let progress = self.runtime.progress_notification();
            let handle = tokio::spawn(async move {
                let result = instance.handle(&batch).await;
                progress.notify_one();
                (instance, result)
            });
            self.workers.insert(
                instance_id.into(),
                Worker {
                    batch: pending,
                    handle,
                },
            );
            return Ok(Some(0));
        }
        let handled = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| AgentError::UnknownInstance(instance_id.to_owned()))?
            .handle(&pending)
            .await;
        match handled {
            Ok(outcome) => {
                self.recovered(instance_id).await?;
                Ok(Some(outcome.events.len()))
            }
            // A reported failure leaves the instance alive and the batch
            // unchecked, so it is retried. A trap is not retryable.
            Err(error) if error.trapped() => {
                self.quarantine(instance_id, checkpoint, &pending, &error)
                    .await?;
                Ok(None)
            }
            Err(error) => {
                self.defer(instance_id, &error).await?;
                Ok(None)
            }
        }
    }

    /// Withdraws a component that died mid-delivery and records why.
    ///
    /// The component cannot report this itself: a trap leaves no outcome to
    /// return, and `component.failed` is core-owned. Nothing committed, so the
    /// cursor still points at the batch that killed it; redelivering would
    /// trap again, which is why the instance goes before the cursor moves.
    async fn quarantine(
        &mut self,
        instance_id: &str,
        checkpoint: u64,
        batch: &[CommittedEvent],
        error: &RuntimeError,
    ) -> Result<(), AgentError> {
        let reason = error.to_string();
        tracing::warn!(
            instance = instance_id,
            checkpoint,
            reason,
            "component quarantined"
        );
        for request in batch {
            if matches!(
                request.request.event_type.as_str(),
                "capability.requested" | "model.requested" | "code.evaluate-requested"
            ) && self
                .router
                .result_for(&request.event_id)
                .await
                .map_err(AgentError::Router)?
                .is_none()
            {
                self.router
                    .admit_attempt(request, instance_id)
                    .await
                    .map_err(AgentError::Router)?;
            }
        }
        let mut instance = self.instances.remove(instance_id);
        self.router.unregister(instance_id);
        self.failed.insert(instance_id.to_owned(), reason.clone());
        self.router
            .append_component_failure(instance_id, batch, &reason)
            .await
            .map_err(AgentError::Router)?;
        if let Some(instance) = instance.as_mut()
            && !instance.requires_reinstantiation()
        {
            instance
                .skip(checkpoint)
                .await
                .map_err(AgentError::Runtime)?;
        }
        Ok(())
    }

    /// Removes requests that already have a terminal result, so a provider
    /// does not execute an effect twice and a denied request never runs.
    async fn drop_answered_requests(
        &self,
        events: Vec<CommittedEvent>,
    ) -> Result<Vec<CommittedEvent>, AgentError> {
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            if matches!(
                event.request.event_type.as_str(),
                "capability.requested" | "model.requested" | "code.evaluate-requested"
            ) && self
                .router
                .result_for(&event.event_id)
                .await
                .map_err(AgentError::Router)?
                .is_some()
            {
                continue;
            }
            kept.push(event);
        }
        Ok(kept)
    }

    /// Finds the terminal result for a request.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be read.
    pub async fn result_for(
        &self,
        request: &EventId,
    ) -> Result<Option<CommittedEvent>, AgentError> {
        self.router
            .result_for(request)
            .await
            .map_err(AgentError::Router)
    }
}

#[derive(Debug)]
pub enum AgentError {
    Runtime(RuntimeError),
    Router(RouterError),
    Storage(String),
    UnknownInstance(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Router(error) => write!(formatter, "{error}"),
            Self::Storage(message) => write!(formatter, "agent storage failed: {message}"),
            Self::UnknownInstance(id) => write!(formatter, "unknown instance: {id}"),
        }
    }
}

impl Error for AgentError {}
