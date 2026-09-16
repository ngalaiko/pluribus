//! One source task owns Wasm; internal deliveries rendezvous at cooperative waits.
use super::{
    Arc, AtomicBool, CancellationHandle, CommitPhase, CommittedEvent, CursorKey, DeliveryCommit,
    DeliveryStore, Duration, HostState, InstanceCore, InstanceRecipe, Instant, MAX_OUTCOME_EVENTS,
    Ordering, Outcome, RuntimeError, RuntimeLimits, StateNamespace, StreamId, convert_mutations,
    execution, host_error, http_request_visible, plugin_failure, prepare_call, stream_denied,
    system_time_ms, types, wit_event,
};
use std::sync::atomic::AtomicU64;
use tokio::sync::{mpsc, oneshot};

type Reply = oneshot::Sender<Result<Outcome, RuntimeError>>;
pub(super) enum Command {
    Deliver(Vec<CommittedEvent>, Reply),
    Stop(i64),
}

#[derive(Default)]
pub(super) struct HostRunner {
    pub startup: Option<oneshot::Sender<super::guest::Outcome>>,
    pub activation: Option<oneshot::Receiver<()>>,
    pub active: bool,
    pub(super) stopping: bool,
    inbox: Option<Arc<tokio::sync::Mutex<mpsc::Receiver<Command>>>>,
    output: Option<mpsc::UnboundedSender<Result<Outcome, RuntimeError>>>,
    store: Option<Arc<dyn DeliveryStore>>,
    cursor: Option<CursorKey>,
    checkpoint: Arc<AtomicU64>,
    pending: Option<(Vec<CommittedEvent>, Reply)>,
    pub(super) timeout: Duration,
    commit_gate: Arc<tokio::sync::Mutex<()>>,
}

impl HostRunner {
    pub(super) const fn has_pending_delivery(&self) -> bool {
        self.pending.is_some()
    }
}

pub(super) struct RunTask {
    pub lifecycle: super::guest::Guest,
    pub context: super::guest::Context,
    pub config: Vec<u8>,
    pub completed: oneshot::Sender<Result<(), RuntimeError>>,
}
impl wasmtime::component::AccessorTask<HostState> for RunTask {
    async fn run(self, accessor: &Accessor<HostState>) -> wasmtime::Result<()> {
        let result = self
            .lifecycle
            .call_run(accessor, self.context, self.config)
            .await
            .map_err(|e| RuntimeError::trap(format!("run trapped: {e:#}")))
            .and_then(|r| r.map_err(|e| plugin_failure(&e)));
        let _ = self.completed.send(result);
        Ok(())
    }
}

struct Running {
    commands: mpsc::Sender<Command>,
    output: mpsc::UnboundedReceiver<Result<Outcome, RuntimeError>>,
    buffered: Option<Result<Outcome, RuntimeError>>,
    checkpoint: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<InstanceCore>,
    commit_gate: Arc<tokio::sync::Mutex<()>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An instance is either idle or owned by its source task.
pub struct PluginInstance {
    pub(super) core: Option<InstanceCore>,
    running: Option<Running>,
    recipe: Arc<InstanceRecipe>,
    config: Vec<u8>,
    limits: RuntimeLimits,
    cancellation: CancellationHandle,
    instance_id: String,
    checkpoint: Arc<AtomicU64>,
}
impl PluginInstance {
    pub(super) fn new(core: InstanceCore) -> Self {
        Self {
            recipe: core.recipe.clone(),
            config: core.config.clone(),
            limits: core.limits.clone(),
            cancellation: core.cancellation.clone(),
            instance_id: core.instance_id.clone(),
            checkpoint: Arc::new(AtomicU64::new(core.checkpoint)),
            core: Some(core),
            running: None,
        }
    }
    /// # Errors
    /// Returns instantiation or storage failures.
    pub fn restart(&self) -> impl Future<Output = Result<Self, RuntimeError>> + Send + use<> {
        let recipe = self.recipe.clone();
        let config = self.config.clone();
        let limits = self.limits.clone();
        async move {
            recipe
                .runtime
                .instantiate_recipe(recipe.clone(), config, limits)
                .await
        }
    }
    #[must_use]
    pub fn requires_reinstantiation(&self) -> bool {
        self.core.as_ref().is_some_and(|c| c.interrupted)
            || self.running.as_ref().is_some_and(|r| r.task.is_finished())
    }
    #[must_use]
    pub fn cancellation(&self) -> CancellationHandle {
        self.cancellation.clone()
    }
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    #[must_use]
    pub fn pinned_session(&self) -> bool {
        self.recipe.pinned_session
    }
    #[must_use]
    pub fn checkpoint(&self) -> u64 {
        self.core
            .as_ref()
            .map_or_else(|| self.checkpoint.load(Ordering::Acquire), |c| c.checkpoint)
    }
    /// # Errors
    /// Returns initialization, activation-state, or commit failures.
    pub async fn init(&mut self) -> Result<Outcome, RuntimeError> {
        self.core
            .as_mut()
            .ok_or_else(|| RuntimeError::new("source already running"))?
            .init()
            .await
    }
    /// # Errors
    /// Returns replay or activation-state failures.
    pub async fn rebuild(&mut self, types: &[String]) -> Result<(), RuntimeError> {
        self.core
            .as_mut()
            .ok_or_else(|| RuntimeError::new("source already running"))?
            .rebuild(types)
            .await
    }

    /// Starts after initialization and replay, never while a candidate is staged.
    pub fn start(&mut self) {
        if self.running.is_some() || self.core.as_ref().is_none_or(|c| c.run_result.is_none()) {
            return;
        }
        let Some(mut core) = self.core.take() else {
            return;
        };
        let (Some(completed), Some(activation)) = (core.run_result.take(), core.activation.take())
        else {
            self.core = Some(core);
            return;
        };
        let (commands, inbox) = mpsc::channel(1);
        let (output, receiver) = mpsc::unbounded_channel();
        let checkpoint = self.checkpoint.clone();
        checkpoint.store(core.checkpoint, Ordering::Release);
        let progress = core.store.data().progress.clone();
        let commit_gate = Arc::new(tokio::sync::Mutex::new(()));
        core.store.data_mut().runner = HostRunner {
            active: true,
            stopping: false,
            startup: None,
            activation: None,
            inbox: Some(Arc::new(tokio::sync::Mutex::new(inbox))),
            output: Some(output.clone()),
            store: Some(core.delivery_store.clone()),
            cursor: Some(core.cursor.clone()),
            checkpoint: checkpoint.clone(),
            pending: None,
            timeout: core.limits.call_timeout,
            commit_gate: commit_gate.clone(),
        };
        let task = tokio::spawn(async move {
            prepare_call(&mut core.store, &core.limits);
            core.store.data_mut().emit_call = true;
            let _ = activation.send(());
            let result = core
                .store
                .run_concurrent(async |_| completed.await)
                .await
                .map_err(|e| RuntimeError::trap(format!("run trapped: {e:#}")))
                .and_then(|r| r.map_err(|_| RuntimeError::trap("run task disappeared")))
                .and_then(|r| r);
            let result = if result.is_ok() && !core.store.data().runner.stopping {
                Err(RuntimeError::trap("source loop exited before shutdown"))
            } else {
                result
            };
            core.interrupted = result.is_err();
            if let Err(error) = result {
                let _ = output.send(Err(error));
            }
            core.checkpoint = core.store.data().runner.checkpoint.load(Ordering::Acquire);
            core.store.data_mut().runner.active = false;
            core.store.data_mut().runner.pending.take();
            core.store.data_mut().release_handles();
            progress.notify_one();
            core
        });
        self.running = Some(Running {
            commands,
            output: receiver,
            buffered: None,
            checkpoint,
            task,
            commit_gate,
        });
    }
    pub fn has_background_output(&mut self) -> bool {
        let Some(r) = self.running.as_mut() else {
            return false;
        };
        if r.buffered.is_none() {
            r.buffered = r.output.try_recv().ok();
            if r.buffered.is_none() && r.task.is_finished() {
                r.buffered = Some(Err(RuntimeError::trap("source task stopped")));
            }
        }
        r.buffered.is_some()
    }
    /// # Errors
    /// Returns source failures or an empty-output error.
    pub fn background_output(&mut self) -> Result<Outcome, RuntimeError> {
        self.running
            .as_mut()
            .and_then(|r| r.buffered.take())
            .ok_or_else(|| RuntimeError::new("no source output"))?
    }
    /// # Errors
    /// Returns delivery, commit, or stopped-instance failures.
    pub async fn handle(&mut self, events: &[CommittedEvent]) -> Result<Outcome, RuntimeError> {
        if let Some(c) = self.core.as_mut() {
            return c.handle(events).await;
        }
        let (reply, result) = oneshot::channel();
        self.running
            .as_ref()
            .ok_or_else(|| RuntimeError::new("instance stopped"))?
            .commands
            .send(Command::Deliver(events.to_vec(), reply))
            .await
            .map_err(|_| RuntimeError::trap("source task stopped"))?;
        result
            .await
            .map_err(|_| RuntimeError::trap("source task stopped during delivery"))?
    }
    /// # Errors
    /// Returns commit or stopped-instance failures.
    pub async fn skip(&mut self, checkpoint: u64) -> Result<(), RuntimeError> {
        if let Some(c) = self.core.as_mut() {
            return c.skip(checkpoint).await;
        }
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| RuntimeError::new("instance stopped"))?;
        let _guard = running.commit_gate.lock().await;
        let receipt = self
            .recipe
            .runtime
            .delivery_store
            .commit(DeliveryCommit {
                cursor: CursorKey {
                    stream_id: StreamId::new(self.recipe.delivery.agent.id.clone()),
                    namespace: StateNamespace::new(self.instance_id.clone()),
                },
                expected_checkpoint: running.checkpoint.load(Ordering::Acquire),
                checkpoint: Some(checkpoint),
                mutations: vec![],
                events: vec![],
            })
            .await
            .map_err(|e| RuntimeError::new(e.to_string()))?;
        running
            .checkpoint
            .store(receipt.checkpoint, Ordering::Release);
        Ok(())
    }
    /// # Errors
    /// Returns shutdown, deadline, or commit failures.
    pub async fn stop(&mut self, deadline: i64) -> Result<Outcome, RuntimeError> {
        self.cancellation.shutdown();
        if let Some(mut running) = self.running.take() {
            let budget = Duration::from_millis(
                deadline
                    .saturating_sub(system_time_ms())
                    .max(1)
                    .cast_unsigned(),
            );
            let core = tokio::time::timeout(budget, async {
                let _ = running.commands.send(Command::Stop(deadline)).await;
                (&mut running.task).await
            })
            .await
            .map_err(|_| RuntimeError::trap("source shutdown deadline exceeded"))?
            .map_err(|_| RuntimeError::trap("source task failed"))?;
            self.core = Some(core);
            while let Ok(outcome) = running.output.try_recv() {
                outcome?;
            }
            if self.core.as_ref().is_some_and(|core| core.interrupted) {
                return Err(RuntimeError::trap("source interrupted during shutdown"));
            }
        }
        self.core
            .as_mut()
            .ok_or_else(|| RuntimeError::new("instance stopped"))?
            .stop(deadline)
            .await
    }
}

impl execution::Host for HostState {
    async fn commit(
        &mut self,
        events: Vec<types::Proposal>,
        mutations: Vec<types::Mutation>,
        checkpoint: Option<u64>,
    ) -> Result<(), types::Error> {
        if !self.runner.active || self.replaying {
            return Err(stream_denied());
        }
        let outcome = self
            .run_commit(&events, &mutations, checkpoint)
            .await
            .map_err(|e| host_error(types::ErrorCode::Internal, &e.to_string()))?;
        if let Some((_, reply)) = self.runner.pending.take() {
            let _ = reply.send(Ok(outcome));
        } else if !outcome.events.is_empty() {
            let _ = self.runner.output.as_ref().unwrap().send(Ok(outcome));
        }
        self.emit_call = true;
        self.progress.notify_one();
        Ok(())
    }
    async fn reject(&mut self, error: types::Error) -> Result<(), types::Error> {
        let Some((_, reply)) = self.runner.pending.take() else {
            return Err(host_error(
                types::ErrorCode::Conflict,
                "no internal delivery",
            ));
        };
        let _ = reply.send(Err(plugin_failure(&error)));
        self.emit_call = true;
        self.progress.notify_one();
        Ok(())
    }
}
impl HostState {
    async fn run_commit(
        &mut self,
        events: &[types::Proposal],
        mutations: &[types::Mutation],
        checkpoint: Option<u64>,
    ) -> Result<Outcome, RuntimeError> {
        if events.len() > MAX_OUTCOME_EVENTS {
            return Err(RuntimeError::new("outcome proposes too many events"));
        }
        let pending = self.runner.pending.as_ref().map(|p| p.0.as_slice());
        let cause = if let Some(batch) = pending {
            let low = batch.first().map_or(0, |e| e.sequence);
            let high = batch.last().map_or(0, |e| e.sequence);
            if !checkpoint.is_some_and(|c| (low..=high).contains(&c)) {
                return Err(RuntimeError::new(
                    "checkpoint must identify a delivered event",
                ));
            }
            (batch.len() == 1).then(|| batch[0].event_id.clone())
        } else {
            if checkpoint.is_some() {
                return Err(RuntimeError::new(
                    "external commit cannot advance delivery checkpoint",
                ));
            }
            if events
                .iter()
                .any(|e| e.idempotency_key.as_ref().is_none_or(String::is_empty))
            {
                return Err(RuntimeError::new(
                    "external events require idempotency keys",
                ));
            }
            None
        };
        let gate = self.runner.commit_gate.clone();
        let _guard = gate.lock().await;
        let current = self.runner.checkpoint.load(Ordering::Acquire);
        let requests = events
            .iter()
            .enumerate()
            .map(|(i, e)| {
                self.append_request(
                    e,
                    cause.clone(),
                    Some(CommitPhase::Handle.derived_key(&self.delivery.instance_id, current, i)),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let receipt = self
            .runner
            .store
            .as_ref()
            .unwrap()
            .commit(DeliveryCommit {
                cursor: self.runner.cursor.clone().unwrap(),
                expected_checkpoint: current,
                checkpoint,
                mutations: convert_mutations(mutations)?,
                events: requests,
            })
            .await
            .map_err(|e| RuntimeError::new(format!("cannot commit source output: {e}")))?;
        self.runner
            .checkpoint
            .store(receipt.checkpoint, Ordering::Release);
        Ok(Outcome {
            events: receipt.events,
            checkpoint: receipt.checkpoint,
        })
    }
}

use wasmtime::component::Accessor;
pub(super) struct HostData;
impl wasmtime::component::HasData for HostData {
    type Data<'a> = &'a mut HostState;
}

pub(super) fn resume<T>(accessor: &Accessor<T, HostData>) {
    accessor.with(|mut access| {
        let host = access.get();
        host.call_deadline = Some(Instant::now() + host.runner.timeout);
        host.stream_deadline = host
            .stream_grant
            .as_ref()
            .map(|g| Instant::now() + Duration::from_millis(u64::from(g.max_timeout_ms)));
    });
}

pub(super) fn cancellation<T>(
    accessor: &Accessor<T, HostData>,
) -> Result<(Arc<AtomicBool>, Arc<tokio::sync::Notify>), types::Error> {
    accessor.with(|mut access| {
        let host = access.get();
        if !host.runner.active || host.runner.stopping || host.replaying || host.interrupted() {
            return Err(stream_denied());
        }
        Ok((
            Arc::clone(host.interrupt_flag()),
            host.cancel_notify.clone(),
        ))
    })
}

impl<T> execution::HostWithStore<T> for HostData {
    async fn ready(
        accessor: &Accessor<T, Self>,
        events: Vec<types::Proposal>,
        mutations: Vec<types::Mutation>,
    ) -> Result<(), types::Error> {
        let (startup, activated) = accessor.with(|mut access| {
            let host = access.get();
            if host.replaying || host.runner.active || host.runner.startup.is_none() {
                return Err(host_error(
                    types::ErrorCode::Conflict,
                    "ready is only valid once during startup",
                ));
            }
            Ok((
                host.runner.startup.take().unwrap(),
                host.runner.activation.take().unwrap(),
            ))
        })?;
        startup
            .send(super::guest::Outcome {
                events,
                mutations,
                checkpoint: None,
            })
            .map_err(|_| host_error(types::ErrorCode::Cancelled, "startup cancelled"))?;
        activated
            .await
            .map_err(|_| host_error(types::ErrorCode::Cancelled, "activation cancelled"))?;
        resume(accessor);
        Ok(())
    }
    async fn next(accessor: &Accessor<T, Self>) -> Result<execution::Wake, types::Error> {
        if accessor.with(|mut access| access.get().shutdown.load(Ordering::Acquire)) {
            accessor.with(|mut access| access.get().runner.stopping = true);
            return Ok(execution::Wake::Stop(system_time_ms()));
        }
        let (shutdown, halted) = cancellation(accessor)?;
        let inbox = accessor.with(|mut access| {
            let host = access.get();
            if host.runner.pending.is_some() {
                return Err(host_error(
                    types::ErrorCode::Conflict,
                    "commit or reject delivery before waiting",
                ));
            }
            Ok(host.runner.inbox.as_ref().unwrap().clone())
        })?;
        let command = loop {
            tokio::select! {
                command = async { inbox.lock().await.recv().await } => break command,
                () = halted.notified() => {
                    if shutdown.load(Ordering::Acquire) {
                        break Some(Command::Stop(system_time_ms()));
                    }
                }
            }
        };
        resume(accessor);
        accessor.with(|mut access| {
            let host = access.get();
            match command {
                Some(Command::Deliver(events, reply)) => {
                    let batch = events
                        .iter()
                        .map(|e| {
                            let mut value = wit_event(e);
                            if !http_request_visible(e, &host.delivery.instance_id) {
                                value.payload = types::Payload::Json(b"{}".to_vec());
                            }
                            value
                        })
                        .collect();
                    host.runner.pending = Some((events, reply));
                    host.emit_call = false;
                    Ok(execution::Wake::Events(batch))
                }
                Some(Command::Stop(deadline)) => {
                    host.runner.stopping = true;
                    host.cancel_notify.notify_waiters();
                    Ok(execution::Wake::Stop(deadline))
                }
                None => Err(host_error(types::ErrorCode::Cancelled, "source stopped")),
            }
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::{host, proposal, store as test_store};
    use super::super::{Component, Config, Engine, Linker, Store};
    use super::*;

    pub(crate) async fn source() -> (
        Store<HostState>,
        wasmtime::component::Instance,
        mpsc::Sender<Command>,
    ) {
        let mut host = host(vec!["observation.received".into()]).await;
        let storage = test_store().await;
        host.state_store = storage.clone();
        host.event_store = storage.clone();
        let (tx, rx) = mpsc::channel(1);
        let (output, _) = mpsc::unbounded_channel();
        host.runner = HostRunner {
            active: true,
            inbox: Some(Arc::new(tokio::sync::Mutex::new(rx))),
            store: Some(storage),
            cursor: Some(CursorKey {
                stream_id: StreamId::new("personal"),
                namespace: StateNamespace::new("shell-1"),
            }),
            output: Some(output),
            timeout: Duration::from_secs(1),
            ..Default::default()
        };
        host.emit_call = true;
        let mut config = Config::new();
        config
            .wasm_component_model_async(true)
            .concurrency_support(true);
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(&engine, b"\0asm\x0d\0\x01\0").unwrap();
        let mut store = Store::new(&engine, host);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await
            .unwrap();
        (store, instance, tx)
    }

    #[tokio::test]
    async fn ready_submits_once_and_waits_for_activation() {
        let (mut store, _, _) = source().await;
        let (startup, received) = oneshot::channel();
        let (activate, activation) = oneshot::channel();
        store.data_mut().runner.active = false;
        store.data_mut().runner.startup = Some(startup);
        store.data_mut().runner.activation = Some(activation);
        store
            .run_concurrent(async |accessor| {
                let accessor = accessor.with_getter::<HostData>(|h| h);
                let ready = <HostData as execution::HostWithStore<HostState>>::ready(
                    &accessor,
                    vec![],
                    vec![types::Mutation::Set(types::StateEntry {
                        key: "startup".into(),
                        value: b"configured".to_vec(),
                    })],
                );
                tokio::pin!(ready);
                let outcome = tokio::select! {
                    outcome = received => outcome.unwrap(),
                    _ = &mut ready => panic!("ready returned before activation"),
                };
                assert_eq!(outcome.mutations.len(), 1);
                assert!(outcome.checkpoint.is_none());
                assert!(futures_util::poll!(ready.as_mut()).is_pending());
                activate.send(()).unwrap();
                ready.await.unwrap();
                let duplicate = <HostData as execution::HostWithStore<HostState>>::ready(
                    &accessor,
                    vec![],
                    vec![],
                )
                .await
                .unwrap_err();
                assert!(matches!(duplicate.code, types::ErrorCode::Conflict));
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn timer_source_commits_without_network_and_invalid_commit_is_atomic() {
        let (mut store, _instance, _commands) = source().await;
        store
            .run_concurrent(async |accessor| {
                crate::bindings::wasi::clocks::monotonic_clock::HostWithStore::wait_for(
                    &accessor.with_getter::<HostData>(|h| h),
                    1_000_000,
                )
                .await;
            })
            .await
            .unwrap();
        let host = store.data_mut();
        let mutation = types::Mutation::Set(types::StateEntry {
            key: "count".into(),
            value: b"1".to_vec(),
        });
        let mut event = proposal("observation.received");
        assert!(
            execution::Host::commit(host, vec![event.clone()], vec![mutation.clone()], None)
                .await
                .is_err()
        );
        assert!(
            host.state_store
                .get(&host.state_namespace, "count")
                .await
                .unwrap()
                .value
                .is_none()
        );
        event.idempotency_key = Some("timer:1".into());
        execution::Host::commit(host, vec![event.clone()], vec![mutation], None)
            .await
            .unwrap();
        execution::Host::commit(host, vec![event], vec![], None)
            .await
            .unwrap();
        assert_eq!(
            host.event_store
                .read(&StreamId::new("personal"), 0, 100)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            host.state_store
                .get(&host.state_namespace, "count")
                .await
                .unwrap()
                .value,
            Some(b"1".to_vec())
        );
        assert_eq!(host.runner.checkpoint.load(Ordering::Acquire), 0);
        assert!(
            execution::Host::commit(host, vec![], vec![], Some(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn idle_source_stays_suspended_and_shutdown_wakes_it() {
        let (mut store, _instance, commands) = source().await;
        store
            .run_concurrent(async |accessor| {
                let accessor = accessor.with_getter::<HostData>(|h| h);
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(20),
                        execution::HostWithStore::next(&accessor)
                    )
                    .await
                    .is_err()
                );
                commands.send(Command::Stop(42)).await.unwrap();
                assert!(matches!(
                    execution::HostWithStore::next(&accessor).await.unwrap(),
                    execution::Wake::Stop(42)
                ));
            })
            .await
            .unwrap();
        assert!(
            store
                .data()
                .event_store
                .read(&StreamId::new("personal"), 0, 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn internal_delivery_requires_commit_or_reject() {
        let (mut store, _instance, commands) = source().await;
        let mut event = proposal("observation.received");
        event.idempotency_key = Some("input:1".into());
        execution::Host::commit(store.data_mut(), vec![event], vec![], None)
            .await
            .unwrap();
        let batch = store
            .data()
            .event_store
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap();
        let sequence = batch[0].sequence;
        let (reply, result) = oneshot::channel();
        commands
            .send(Command::Deliver(batch.clone(), reply))
            .await
            .unwrap();
        store
            .run_concurrent(async |accessor| {
                let accessor = accessor.with_getter::<HostData>(|h| h);
                assert!(matches!(
                    execution::HostWithStore::next(&accessor).await.unwrap(),
                    execution::Wake::Events(_)
                ));
                assert!(execution::HostWithStore::next(&accessor).await.is_err());
            })
            .await
            .unwrap();
        assert!(
            execution::Host::commit(store.data_mut(), vec![], vec![], Some(sequence + 1))
                .await
                .is_err()
        );
        execution::Host::reject(
            store.data_mut(),
            host_error(types::ErrorCode::Unavailable, "retry"),
        )
        .await
        .unwrap();
        assert!(result.await.unwrap().is_err());
        assert_eq!(store.data().runner.checkpoint.load(Ordering::Acquire), 0);
        let (reply, result) = oneshot::channel();
        commands.send(Command::Deliver(batch, reply)).await.unwrap();
        store
            .run_concurrent(async |accessor| {
                execution::HostWithStore::next(&accessor.with_getter::<HostData>(|h| h))
                    .await
                    .unwrap();
            })
            .await
            .unwrap();
        execution::Host::commit(store.data_mut(), vec![], vec![], Some(sequence))
            .await
            .unwrap();
        assert_eq!(result.await.unwrap().unwrap().checkpoint, sequence);
    }

    #[tokio::test]
    async fn cancelling_an_activity_leaves_the_idle_loop_waiting() {
        let (mut store, _instance, commands) = source().await;
        let cancel = CancellationHandle::of(store.data());
        cancel.cancel();
        let mut event = proposal("observation.received");
        event.idempotency_key = Some("input:1".into());
        execution::Host::commit(store.data_mut(), vec![event], vec![], None)
            .await
            .unwrap();
        let batch = store
            .data()
            .event_store
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap();
        let (reply, _result) = oneshot::channel();
        commands.send(Command::Deliver(batch, reply)).await.unwrap();
        store
            .run_concurrent(async |accessor| {
                let wake = execution::HostWithStore::next(&accessor.with_getter::<HostData>(|h| h))
                    .await
                    .unwrap();
                assert!(matches!(wake, execution::Wake::Events(_)));
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_wakes_all_suspended_operations() {
        let (mut store, _instance, _commands) = source().await;
        let cancel = CancellationHandle::of(store.data());
        store
            .run_concurrent(async |accessor| {
                let accessor = accessor.with_getter::<HostData>(|h| h);
                let (inbox, (), ()) = tokio::join!(
                    execution::HostWithStore::next(&accessor),
                    crate::bindings::wasi::clocks::monotonic_clock::HostWithStore::wait_for(
                        &accessor,
                        60_000_000_000
                    ),
                    async {
                        tokio::task::yield_now().await;
                        cancel.shutdown();
                    }
                );
                assert!(matches!(inbox.unwrap(), execution::Wake::Stop(_)));
            })
            .await
            .unwrap();
    }
}
