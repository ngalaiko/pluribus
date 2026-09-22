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
use std::future::Future;
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
    reduced_batches: BTreeSet<String>,
    health_through: u64,
}

impl<P, R> Drop for Agent<P, R> {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for cancellation in self.cancellations.values() {
            cancellation.shutdown();
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
            reduced_batches: BTreeSet::new(),
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
            let prepared = self
                .stage_component(
                    component,
                    config,
                    component_delivery,
                    setup.services,
                    &setup.models,
                    &registrations,
                )
                .await?;
            registrations.push(prepared.registration.clone());
            staged.push(prepared);
        }
        staged = run_independent(staged, |mut prepared| async move {
            let result = prepared.instance.init().await.map(|_| ());
            (prepared, result)
        })
        .await
        .map_err(AgentError::Runtime)?;
        staged = run_independent(staged, |mut prepared| async move {
            let result = prepared
                .instance
                .rebuild(&prepared.rebuilds)
                .await
                .map(|_| ());
            (prepared, result)
        })
        .await
        .map_err(AgentError::Runtime)?;
        let ids = staged
            .iter()
            .map(|s| s.registration.instance_id.clone())
            .collect();
        for mut prepared in staged {
            self.recover_installed_session(&mut prepared.instance)
                .await?;
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
        self.recover_installed_session(&mut staged.instance).await?;
        self.activate_component(staged);
        Ok(())
    }

    async fn recover_installed_session(
        &mut self,
        instance: &mut PluginInstance,
    ) -> Result<(), AgentError> {
        let mut after = 0;
        let mut failure = None;
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &pluribus_core::EventQuery {
                        after_sequence: Some(after),
                        event_types: vec!["component.failed".into(), "component.recovered".into()],
                        ..Default::default()
                    },
                    100,
                )
                .await
                .map_err(|e| AgentError::Storage(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            for event in page {
                after = event.sequence;
                if event.request.actor.kind != pluribus_core::PrincipalKind::Node
                    || ![
                        self.principal.id.as_str(),
                        &format!("operator:{}", self.principal.id.as_str()),
                    ]
                    .contains(&event.request.actor.id.as_str())
                {
                    continue;
                }
                if crate::payload_field(&event, "instanceId").as_deref()
                    != Some(instance.instance_id())
                {
                    continue;
                }
                failure = if event.request.event_type == "component.failed" {
                    Some(event)
                } else {
                    None
                };
            }
        }
        if let Some(failure) = failure {
            let mut batch = Vec::new();
            if let Some(id) = &failure.request.causation_id
                && let Some(event) = self
                    .events
                    .get(id)
                    .await
                    .map_err(|e| AgentError::Storage(e.to_string()))?
            {
                batch.push(event);
            }
            if instance.pinned_session() {
                self.router
                    .fail_sessions(instance.instance_id(), &batch)
                    .await
                    .map_err(AgentError::Router)?;
            } else {
                let Some(request) = batch.first() else {
                    return Ok(());
                };
                if !self.cancelled_before(request, failure.sequence).await? {
                    return Ok(());
                }
                self.router
                    .cancel_attempt(request)
                    .await
                    .map_err(AgentError::Router)?;
            }
            let checkpoint = batch.last().map_or(instance.checkpoint(), |e| {
                e.sequence.max(instance.checkpoint())
            });
            instance
                .skip(checkpoint)
                .await
                .map_err(AgentError::Runtime)?;
            self.router
                .append_health(
                    "component.recovered",
                    serde_json::json!({"instanceId":instance.instance_id()}),
                )
                .await
                .map_err(AgentError::Router)?;
        }
        Ok(())
    }

    async fn cancelled_before(
        &mut self,
        request: &CommittedEvent,
        before: u64,
    ) -> Result<bool, AgentError> {
        let mut after = request.sequence;
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &pluribus_core::EventQuery {
                        after_sequence: Some(after),
                        event_types: vec!["cognition.cancel-requested".into()],
                        ..Default::default()
                    },
                    100,
                )
                .await
                .map_err(|e| AgentError::Storage(e.to_string()))?;
            if page.is_empty() {
                return Ok(false);
            }
            for event in page {
                if event.sequence >= before {
                    return Ok(false);
                }
                after = event.sequence;
                if event.request.actor == request.request.actor
                    && crate::payload_field(&event, "requestEventId").as_deref()
                        == Some(request.event_id.as_str())
                {
                    return Ok(true);
                }
            }
        }
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
                    "pluribus:plugin/events@3.0.0",
                    "pluribus:plugin/state@3.0.0",
                    "pluribus:plugin/runtime@3.0.0",
                ]
                .contains(&import.as_str())
            })
        {
            return Err(AgentError::Storage(
                "rebuild providers may import only events, state, and runtime".into(),
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

    fn activate_component(&mut self, mut staged: StagedComponent) {
        staged.instance.start();
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
            handle.shutdown();
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
                    self.recovered(&id, Some(&worker.batch)).await?;
                    progress.events_committed += outcome.events.len();
                }
                Err(_)
                    if self
                        .cancellations
                        .get(&id)
                        .is_some_and(CancellationHandle::is_cancelled) =>
                {
                    self.cancel_worker(&id, &worker.batch).await?;
                }
                Err(error) if error.trapped() && error.resource_exhaustion().is_none() => {
                    self.quarantine(
                        &id,
                        worker
                            .batch
                            .last()
                            .map_or(self.instances[&id].checkpoint(), |event| event.sequence),
                        &worker.batch,
                        &error,
                    )
                    .await?;
                }
                Err(error) if error.resource_exhaustion().is_some() => {
                    if let Err(restart) = self
                        .restart_after_resource(&id, &worker.batch, error.resource_exhaustion())
                        .await
                    {
                        self.mark_resource_restart_failure(&id, &restart).await?;
                        continue;
                    }
                    self.defer_delivery(&id, &worker.batch, &error).await?;
                }
                Err(error) => self.defer_delivery(&id, &worker.batch, &error).await?,
            }
        }
        Ok(())
    }

    async fn cancel_worker(
        &mut self,
        id: &str,
        batch: &[CommittedEvent],
    ) -> Result<(), AgentError> {
        for request in batch {
            self.router
                .cancel_attempt(request)
                .await
                .map_err(AgentError::Router)?;
        }
        let old = self
            .instances
            .remove(id)
            .ok_or_else(|| AgentError::UnknownInstance(id.into()))?;
        if old.pinned_session() {
            self.router
                .fail_sessions(id, &[])
                .await
                .map_err(AgentError::Router)?;
        }
        let mut fresh = old.restart().await.map_err(AgentError::Runtime)?;
        fresh.init().await.map_err(AgentError::Runtime)?;
        fresh
            .skip(
                batch
                    .last()
                    .map_or(old.checkpoint(), |event| event.sequence),
            )
            .await
            .map_err(AgentError::Runtime)?;
        fresh.start();
        self.cancellations.insert(id.into(), fresh.cancellation());
        self.instances.insert(id.into(), fresh);
        self.recovered(id, None).await?;
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
                        let request_id = payload["requestEventId"].as_str();
                        let key = request_id
                            .map_or_else(|| id.to_owned(), |request| format!("{id}\n{request}"));
                        if request_id.is_some() {
                            self.reduced_batches.insert(id.to_owned());
                        }
                        let failures = payload["failures"]
                            .as_u64()
                            .unwrap_or(1)
                            .min(if request_id.is_some() { 3 } else { 32 });
                        self.backoff.insert(
                            key,
                            (
                                u32::try_from(failures).unwrap_or(32),
                                payload["retryAtMs"].as_i64().unwrap_or(0),
                            ),
                        );
                    }
                    "component.recovered" => {
                        self.backoff.remove(id);
                        if let Some(requests) = payload["requestEventIds"].as_array() {
                            for request in requests.iter().filter_map(Value::as_str) {
                                self.backoff.remove(&format!("{id}\n{request}"));
                            }
                        } else {
                            self.backoff
                                .retain(|key, _| key != id && !key.starts_with(&format!("{id}\n")));
                            self.reduced_batches.remove(id);
                        }
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

    async fn defer_delivery(
        &mut self,
        id: &str,
        batch: &[CommittedEvent],
        error: &RuntimeError,
    ) -> Result<(), AgentError> {
        let resource = error.resource_exhaustion();
        let request_id = resource.and_then(|_| batch.first().map(|event| event.event_id.as_str()));
        let key = request_id.map_or_else(|| id.to_owned(), |request| format!("{id}\n{request}"));
        let failures = self.backoff.get(&key).map_or(1, |(n, _)| {
            n.saturating_add(1)
                .min(if resource.is_some() { 3 } else { 32 })
        });
        if resource.is_some() {
            self.reduced_batches.insert(id.to_owned());
        }
        if resource.is_some() && batch.is_empty() && failures >= 3 {
            self.mark_resource_restart_failure(id, error).await?;
            self.backoff.remove(&key);
            return Ok(());
        }
        if let Some(resource) = resource
            && failures >= 3
            && batch.len() == 1
        {
            let source = &batch[0];
            let effect_status = if matches!(
                source.request.event_type.as_str(),
                "model.requested" | "capability.requested" | "code.evaluate-requested"
            ) {
                "outcome-unknown"
            } else {
                "not-started"
            };
            let deferred = self
                .router
                .resource_exhaustion_request(
                    id,
                    source,
                    0,
                    serde_json::json!({
                        "resource": resource.resource,
                        "currentBytes": resource.current_bytes,
                        "requestedBytes": resource.requested_bytes,
                        "limitBytes": resource.limit_bytes,
                        "phase": resource.phase,
                    }),
                    effect_status,
                    "deferred-before-cursor",
                    resource.job_id.clone(),
                    resource.session_id.clone(),
                )
                .map_err(AgentError::Router)?;
            self.instances
                .get_mut(id)
                .ok_or_else(|| AgentError::UnknownInstance(id.to_owned()))?
                .skip_with_events(source.sequence, vec![deferred])
                .await
                .map_err(AgentError::Runtime)?;
            self.backoff.remove(&key);
            return Ok(());
        }
        let delay = (1000_i64 << failures.saturating_sub(1).min(6)).min(60_000);
        let retry_at = self.admission_now_ms.saturating_add(delay);
        self.router
            .append_health(
                "component.backoff",
                serde_json::json!({
                    "instanceId": id, "requestEventId": request_id,
                "phase": resource.map(|r| r.phase.clone()), "reason": error.to_string(),
                    "failures": failures, "retryAtMs": retry_at
                }),
            )
            .await
            .map_err(AgentError::Router)?;
        self.backoff.insert(key, (failures, retry_at));
        Ok(())
    }

    async fn restart_after_resource(
        &mut self,
        id: &str,
        batch: &[CommittedEvent],
        resource: Option<&pluribus_runtime_wasm::ResourceExhaustion>,
    ) -> Result<(), AgentError> {
        let old = self
            .instances
            .remove(id)
            .ok_or_else(|| AgentError::UnknownInstance(id.to_owned()))?;
        let pinned = old.pinned_session();
        let checkpoint = if pinned {
            batch
                .last()
                .map_or(old.checkpoint(), |event| event.sequence)
        } else {
            old.checkpoint()
        };
        if pinned {
            self.router
                .fail_sessions_with_resource(
                    id,
                    batch,
                    resource.map(|resource| serde_json::to_value(resource).unwrap_or_default()),
                )
                .await
                .map_err(AgentError::Router)?;
        }
        let mut fresh = old.restart().await.map_err(AgentError::Runtime)?;
        fresh.init().await.map_err(AgentError::Runtime)?;
        fresh.skip(checkpoint).await.map_err(AgentError::Runtime)?;
        fresh.start();
        self.cancellations
            .insert(id.to_owned(), fresh.cancellation());
        self.instances.insert(id.to_owned(), fresh);
        Ok(())
    }

    async fn mark_resource_restart_failure(
        &mut self,
        id: &str,
        error: &(dyn fmt::Display + Sync),
    ) -> Result<(), AgentError> {
        self.router
            .append_health(
                "component.failed",
                serde_json::json!({
                    "instanceId": id,
                    "reason": error.to_string(),
                    "code": "resource-exhausted",
                    "health": "unhealthy",
                }),
            )
            .await
            .map_err(AgentError::Router)?;
        self.failed.insert(id.to_owned(), error.to_string());
        Ok(())
    }

    async fn recovered(
        &mut self,
        id: &str,
        batch: Option<&[CommittedEvent]>,
    ) -> Result<(), AgentError> {
        let request_ids: Vec<_> = batch
            .unwrap_or_default()
            .iter()
            .map(|event| event.event_id.as_str().to_owned())
            .collect();
        let has_matching = self.backoff.contains_key(id)
            || request_ids
                .iter()
                .any(|request| self.backoff.contains_key(&format!("{id}\n{request}")));
        if has_matching {
            self.router
                .append_health(
                    "component.recovered",
                    serde_json::json!({"instanceId": id, "requestEventIds": request_ids}),
                )
                .await
                .map_err(AgentError::Router)?;
            self.backoff.remove(id);
            for request in request_ids {
                self.backoff.remove(&format!("{id}\n{request}"));
            }
        }
        Ok(())
    }

    async fn observations_delivered(&mut self) -> Result<bool, AgentError> {
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

    async fn due_timers(&mut self, now_ms: i64) -> Result<Vec<PendingTimer>, AgentError> {
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
            .query(
                &self.stream_id,
                &pluribus_core::EventQuery {
                    after_sequence: Some(self.gated_through),
                    event_types: vec![
                        "capability.requested".into(),
                        "cognition.cancel-requested".into(),
                    ],
                    ..Default::default()
                },
                self.batch,
            )
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
            || self.backoff.iter().any(|(key, (_, until))| {
                (key == instance_id || key.starts_with(&format!("{instance_id}\n")))
                    && *until > self.admission_now_ms
            })
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
                    if self.reduced_batches.contains(instance_id)
                        || self.external.contains(instance_id)
                    {
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
                if self
                    .instances
                    .get_mut(instance_id)
                    .unwrap()
                    .has_background_output()
                {
                    let mut instance = self.instances.remove(instance_id).unwrap();
                    let progress = self.runtime.progress_notification();
                    let handle = tokio::spawn(async move {
                        let result = instance.background_output();
                        progress.notify_one();
                        (instance, result)
                    });
                    self.workers.insert(
                        instance_id.into(),
                        Worker {
                            batch: Vec::new(),
                            handle,
                        },
                    );
                    return Ok(Some(0));
                }
                self.recovered(instance_id, Some(&pending)).await?;
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
                self.recovered(instance_id, Some(&pending)).await?;
                Ok(Some(outcome.events.len()))
            }
            // A reported failure leaves the instance alive and the batch
            // unchecked, so it is retried. A trap is not retryable.
            Err(error) if error.trapped() && error.resource_exhaustion().is_none() => {
                self.quarantine(instance_id, checkpoint, &pending, &error)
                    .await?;
                Ok(None)
            }
            Err(error) if error.resource_exhaustion().is_some() => {
                if let Err(restart) = self
                    .restart_after_resource(instance_id, &pending, error.resource_exhaustion())
                    .await
                {
                    self.mark_resource_restart_failure(instance_id, &restart)
                        .await?;
                    return Ok(None);
                }
                self.defer_delivery(instance_id, &pending, &error).await?;
                Ok(None)
            }
            Err(error) => {
                self.defer_delivery(instance_id, &pending, &error).await?;
                Ok(None)
            }
        }
    }

    /// Fails lost sessions and replaces their component without replaying the
    /// poisoned batch. Components without pinned sessions stay quarantined.
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
        if !self
            .instances
            .get(instance_id)
            .is_some_and(PluginInstance::pinned_session)
        {
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
        }
        let mut instance = self.instances.remove(instance_id);
        self.router
            .append_component_failure(instance_id, batch, &reason)
            .await
            .map_err(AgentError::Router)?;
        if let Some(old) = instance.as_mut().filter(|i| i.pinned_session()) {
            self.router
                .fail_sessions(instance_id, batch)
                .await
                .map_err(AgentError::Router)?;
            let restart = old.restart();
            let restarted = async move {
                let mut fresh = restart.await?;
                fresh.init().await?;
                fresh.skip(checkpoint).await?;
                fresh.start();
                Ok::<_, RuntimeError>(fresh)
            }
            .await;
            match restarted {
                Ok(fresh) => {
                    self.cancellations
                        .insert(instance_id.into(), fresh.cancellation());
                    self.instances.insert(instance_id.into(), fresh);
                    self.router
                        .append_health(
                            "component.recovered",
                            serde_json::json!({"instanceId":instance_id}),
                        )
                        .await
                        .map_err(AgentError::Router)?;
                    self.failed.remove(instance_id);
                    self.backoff.remove(instance_id);
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(instance = instance_id, %error, "component restart failed");
                }
            }
        }
        self.router.unregister(instance_id);
        self.failed.insert(instance_id.to_owned(), reason.clone());
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
        &mut self,
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

/// Runs independent lifecycle work concurrently while retaining input order.
async fn run_independent<T, E, F, Fut>(items: Vec<T>, run: F) -> Result<Vec<T>, E>
where
    F: Fn(T) -> Fut,
    Fut: Future<Output = (T, Result<(), E>)>,
{
    let results = futures_util::future::join_all(items.into_iter().map(run)).await;
    let mut completed = Vec::with_capacity(results.len());
    let mut first_error = None;
    for (item, result) in results {
        completed.push(item);
        if first_error.is_none() {
            first_error = result.err();
        }
    }
    first_error.map_or(Ok(completed), Err)
}

#[cfg(test)]
mod independent_tests {
    use super::run_independent;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn independent_lifecycle_work_overlaps_and_keeps_each_order() {
        let phases = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(2));
        let initialized = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run_independent(vec![1_u8, 2], {
                let barrier = Arc::clone(&barrier);
                let phases = Arc::clone(&phases);
                move |id| {
                    let barrier = Arc::clone(&barrier);
                    let phases = Arc::clone(&phases);
                    async move {
                        phases.lock().unwrap().push((id, "init"));
                        barrier.wait().await;
                        (id, Ok::<_, ()>(()))
                    }
                }
            }),
        )
        .await
        .expect("independent work should overlap")
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run_independent(initialized, {
                let barrier = Arc::clone(&barrier);
                let phases = Arc::clone(&phases);
                move |id| {
                    let barrier = Arc::clone(&barrier);
                    let phases = Arc::clone(&phases);
                    async move {
                        phases.lock().unwrap().push((id, "rebuild"));
                        barrier.wait().await;
                        (id, Ok::<_, ()>(()))
                    }
                }
            }),
        )
        .await
        .expect("independent rebuilds should overlap")
        .unwrap();

        assert_eq!(result, [1, 2]);
        let phases = phases.lock().unwrap();
        for id in [1, 2] {
            let init = phases
                .iter()
                .position(|phase| *phase == (id, "init"))
                .unwrap();
            let rebuild = phases
                .iter()
                .position(|phase| *phase == (id, "rebuild"))
                .unwrap();
            assert!(init < rebuild);
        }
    }

    #[tokio::test]
    async fn independent_work_finishes_all_items_before_returning_an_error() {
        let barrier = Arc::new(Barrier::new(2));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let result = run_independent(vec![1_u8, 2], {
            let barrier = Arc::clone(&barrier);
            let completed = Arc::clone(&completed);
            move |id| {
                let barrier = Arc::clone(&barrier);
                let completed = Arc::clone(&completed);
                async move {
                    barrier.wait().await;
                    completed.lock().unwrap().push(id);
                    let result = if id == 1 { Err("init failed") } else { Ok(()) };
                    (id, result)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap_err(), "init failed");
        let mut completed = completed.lock().unwrap().clone();
        completed.sort_unstable();
        assert_eq!(completed, [1, 2]);
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
