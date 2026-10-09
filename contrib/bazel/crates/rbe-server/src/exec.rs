// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Execution: the `Execution` service bazel drives, and the queue workers lease from.
//!
//! The scheduler is deliberately thin. It owns a FIFO of queued actions and one
//! `google.longrunning.Operation` per action; workers do all the real work (fetch the action,
//! build the input root, run it, upload the outputs) and hand back an `ActionResult`. The
//! scheduler's own jobs are: check the Action Cache before queueing, deduplicate identical
//! actions that are already in flight, publish operation state to everyone waiting, and write
//! the Action Cache entry when a worker reports success.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use dashmap::DashMap;
use futures::Stream;
use prost::Message as _;
use rbe_lore::LoreBlobStore;
use rbe_lore::digest;
use rbe_proto::longrunning::Operation;
use rbe_proto::longrunning::operation::Result as OpResult;
use rbe_proto::reapi::ActionResult;
use rbe_proto::reapi::Digest;
use rbe_proto::reapi::ExecuteOperationMetadata;
use rbe_proto::reapi::ExecuteRequest;
use rbe_proto::reapi::ExecuteResponse;
use rbe_proto::reapi::WaitExecutionRequest;
use rbe_proto::reapi::execution_server::Execution;
use rbe_proto::reapi::execution_stage;
use rbe_proto::worker::CompleteLeaseRequest;
use rbe_proto::worker::CompleteLeaseResponse;
use rbe_proto::worker::HeartbeatRequest;
use rbe_proto::worker::HeartbeatResponse;
use rbe_proto::worker::TakeLeaseRequest;
use rbe_proto::worker::TakeLeaseResponse;
use rbe_proto::worker::worker_queue_server::WorkerQueue;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tonic::Request;
use tonic::Response;
use tonic::Status;

use crate::ac;

/// How long a completed operation stays addressable by name, so a `WaitExecution` that arrives
/// just after completion still finds its answer.
const COMPLETED_RETENTION: Duration = Duration::from_secs(120);

/// A lease not renewed within this window is assumed dead and its action re-queued. A multiple
/// of the worker's heartbeat interval, not of the action timeout: a long-running action still
/// renews, so this bounds how long a *dead* worker holds an action, not how long a live one may
/// take.
const LEASE_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the sweeper looks for leases past [`LEASE_TIMEOUT`].
const LEASE_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Attempts an action gets before it is failed rather than re-queued, so a worker that dies on
/// one particular action cannot cycle it forever.
const MAX_ATTEMPTS: u32 = 3;

/// One queued or running action, and the operation everybody watching it sees.
struct Op {
    name: String,
    action_digest: Digest,
    tx: watch::Sender<Operation>,
    /// Key into `Scheduler::inflight`, so completion can withdraw the dedup entry.
    dedup_key: (String, String, i64),
    /// Times this action has been leased, counting re-queues after a lease went dead.
    attempts: AtomicU32,
}

impl Op {
    fn publish(&self, stage: execution_stage::Value, result: Option<ExecuteResponse>) {
        let metadata = ExecuteOperationMetadata {
            stage: stage as i32,
            action_digest: Some(self.action_digest.clone()),
            stdout_stream_name: String::new(),
            stderr_stream_name: String::new(),
            partial_execution_metadata: None,
            digest_function: rbe_proto::reapi::digest_function::Value::Sha256 as i32,
        };
        let done = result.is_some();
        let op = Operation {
            name: self.name.clone(),
            metadata: Some(prost_types::Any {
                type_url: rbe_proto::type_url::EXECUTE_OPERATION_METADATA.to_string(),
                value: metadata.encode_to_vec(),
            }),
            done,
            result: result.map(|r| {
                OpResult::Response(prost_types::Any {
                    type_url: rbe_proto::type_url::EXECUTE_RESPONSE.to_string(),
                    value: r.encode_to_vec(),
                })
            }),
        };
        // A send error only means nobody is listening any more, which is fine: the value is
        // still retained by the channel for a later subscriber.
        let _ = self.tx.send(op);
    }
}

struct Lease {
    op: Arc<Op>,
    worker_id: String,
    expires_at: Instant,
}

pub struct Scheduler {
    store: Arc<LoreBlobStore>,
    verify_ac: bool,
    queue: Mutex<VecDeque<Arc<Op>>>,
    /// Signalled on every enqueue. `notify_one` stores a permit, so a worker that checks the
    /// queue and then waits cannot miss an item enqueued in between.
    work_available: Notify,
    by_name: DashMap<String, Arc<Op>>,
    leases: DashMap<String, Lease>,
    /// (instance, action hash, action size) -> the operation already running it.
    inflight: Mutex<HashMap<(String, String, i64), Arc<Op>>>,
}

impl Scheduler {
    pub fn new(store: Arc<LoreBlobStore>, verify_ac: bool) -> Arc<Self> {
        let sched = Arc::new(Self {
            store,
            verify_ac,
            queue: Mutex::new(VecDeque::new()),
            work_available: Notify::new(),
            by_name: DashMap::new(),
            leases: DashMap::new(),
            inflight: Mutex::new(HashMap::new()),
        });
        sched.clone().spawn_lease_sweeper();
        sched
    }

    /// Reclaim leases whose worker stopped renewing, so a machine dying costs one action's
    /// re-execution rather than stalling the build behind it.
    fn spawn_lease_sweeper(self: Arc<Self>) {
        lore_base::lore_spawn!(async move {
            let mut tick = tokio::time::interval(LEASE_SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                let now = Instant::now();
                let expired: Vec<String> = self
                    .leases
                    .iter()
                    .filter(|entry| entry.value().expires_at <= now)
                    .map(|entry| entry.key().clone())
                    .collect();
                for lease_id in expired {
                    let Some((_, lease)) = self.leases.remove(&lease_id) else {
                        continue;
                    };
                    let attempts = lease.op.attempts.load(Ordering::Relaxed);
                    if attempts >= MAX_ATTEMPTS {
                        tracing::warn!(
                            lease = %lease_id,
                            worker = %lease.worker_id,
                            action = %digest::fmt(&lease.op.action_digest),
                            attempts,
                            "lease went dead and the action is out of attempts; failing it"
                        );
                        self.finish(
                            &lease.op,
                            ExecuteResponse {
                                result: None,
                                cached_result: false,
                                status: Some(rbe_proto::rpc::Status {
                                    code: tonic::Code::Aborted as i32,
                                    message: format!(
                                        "no worker completed this action in {attempts} attempts"
                                    ),
                                    details: Vec::new(),
                                }),
                                server_logs: Default::default(),
                                message: String::new(),
                            },
                        )
                        .await;
                        continue;
                    }
                    tracing::warn!(
                        lease = %lease_id,
                        worker = %lease.worker_id,
                        action = %digest::fmt(&lease.op.action_digest),
                        attempts,
                        "lease went dead; re-queueing the action"
                    );
                    self.requeue(lease.op).await;
                }
            }
        });
    }

    /// Return an action to the queue for another worker.
    async fn requeue(&self, op: Arc<Op>) {
        op.publish(execution_stage::Value::Queued, None);
        self.queue.lock().await.push_back(op);
        self.work_available.notify_one();
    }

    /// Enqueue `action_digest`, or join the operation already running it.
    async fn submit(&self, instance: &str, action_digest: Digest) -> watch::Receiver<Operation> {
        let dedup_key = (
            instance.to_string(),
            action_digest.hash.clone(),
            action_digest.size_bytes,
        );

        let mut inflight = self.inflight.lock().await;
        if let Some(existing) = inflight.get(&dedup_key) {
            return existing.tx.subscribe();
        }

        let name = format!("operations/{}", uuid::Uuid::new_v4());
        let (tx, rx) = watch::channel(Operation {
            name: name.clone(),
            ..Default::default()
        });
        let op = Arc::new(Op {
            name: name.clone(),
            action_digest,
            tx,
            dedup_key: dedup_key.clone(),
            attempts: AtomicU32::new(0),
        });
        op.publish(execution_stage::Value::Queued, None);

        inflight.insert(dedup_key, op.clone());
        drop(inflight);

        self.by_name.insert(name, op.clone());
        self.queue.lock().await.push_back(op);
        self.work_available.notify_one();
        rx
    }

    async fn next_queued(&self) -> Option<Arc<Op>> {
        self.queue.lock().await.pop_front()
    }

    /// Publish the terminal state and retire the operation.
    async fn finish(&self, op: &Arc<Op>, response: ExecuteResponse) {
        self.inflight.lock().await.remove(&op.dedup_key);
        op.publish(execution_stage::Value::Completed, Some(response));

        let name = op.name.clone();
        let by_name = self.by_name.clone();
        lore_base::lore_spawn!(async move {
            tokio::time::sleep(COMPLETED_RETENTION).await;
            by_name.remove(&name);
        });
    }
}

// -------------------------------------------------------------------------------------------
// bazel-facing Execution service
// -------------------------------------------------------------------------------------------

pub struct ExecutionService {
    sched: Arc<Scheduler>,
}

impl ExecutionService {
    pub fn new(sched: Arc<Scheduler>) -> Self {
        Self { sched }
    }
}

/// Drive a `watch` channel as a gRPC server stream: emit the current operation immediately,
/// then every update, and stop after the one marked done.
enum StreamState {
    First(watch::Receiver<Operation>),
    Next(watch::Receiver<Operation>),
    Done,
}

fn operation_stream(
    rx: watch::Receiver<Operation>,
) -> Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>> {
    Box::pin(futures::stream::unfold(
        StreamState::First(rx),
        |state| async move {
            let mut rx = match state {
                StreamState::Done => return None,
                StreamState::First(rx) => rx,
                StreamState::Next(mut rx) => {
                    // All senders dropped: the operation can never complete, so end the stream
                    // rather than hang.
                    if rx.changed().await.is_err() {
                        return None;
                    }
                    rx
                }
            };
            let op = rx.borrow_and_update().clone();
            let next = if op.done {
                StreamState::Done
            } else {
                StreamState::Next(rx)
            };
            Some((Ok(op), next))
        },
    ))
}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>>;
    type WaitExecutionStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>>;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("missing action_digest"))?;

        // Bazel normally checks the Action Cache itself before calling Execute, so this mostly
        // catches an action that another client finished in between. It is cheap and it is what
        // makes `--remote_executor` alone (no separate `--remote_cache`) still get cache hits.
        if !req.skip_cache_lookup
            && let Some(result) =
                ac::lookup(&self.sched.store, &action_digest, self.sched.verify_ac).await
        {
            self.sched
                .store
                .stats
                .actions_ac_hit
                .fetch_add(1, Ordering::Relaxed);
            let op = Operation {
                name: format!("operations/{}", uuid::Uuid::new_v4()),
                metadata: Some(prost_types::Any {
                    type_url: rbe_proto::type_url::EXECUTE_OPERATION_METADATA.to_string(),
                    value: ExecuteOperationMetadata {
                        stage: execution_stage::Value::Completed as i32,
                        action_digest: Some(action_digest.clone()),
                        stdout_stream_name: String::new(),
                        stderr_stream_name: String::new(),
                        partial_execution_metadata: None,
                        digest_function: rbe_proto::reapi::digest_function::Value::Sha256 as i32,
                    }
                    .encode_to_vec(),
                }),
                done: true,
                result: Some(OpResult::Response(prost_types::Any {
                    type_url: rbe_proto::type_url::EXECUTE_RESPONSE.to_string(),
                    value: ExecuteResponse {
                        result: Some(result),
                        cached_result: true,
                        status: Some(ok_rpc_status()),
                        server_logs: Default::default(),
                        message: String::new(),
                    }
                    .encode_to_vec(),
                })),
            };
            return Ok(Response::new(Box::pin(futures::stream::once(async move {
                Ok(op)
            }))));
        }

        let rx = self.sched.submit(&req.instance_name, action_digest).await;
        Ok(Response::new(operation_stream(rx)))
    }

    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        let name = request.into_inner().name;
        let op = self
            .sched
            .by_name
            .get(&name)
            .ok_or_else(|| Status::not_found(format!("no such operation: {name}")))?;
        let rx = op.tx.subscribe();
        Ok(Response::new(operation_stream(rx)))
    }
}

fn ok_rpc_status() -> rbe_proto::rpc::Status {
    rbe_proto::rpc::Status {
        code: tonic::Code::Ok as i32,
        message: String::new(),
        details: Vec::new(),
    }
}

// -------------------------------------------------------------------------------------------
// Worker-facing queue service
// -------------------------------------------------------------------------------------------

pub struct WorkerQueueService {
    sched: Arc<Scheduler>,
}

impl WorkerQueueService {
    pub fn new(sched: Arc<Scheduler>) -> Self {
        Self { sched }
    }
}

#[tonic::async_trait]
impl WorkerQueue for WorkerQueueService {
    async fn take_lease(
        &self,
        request: Request<TakeLeaseRequest>,
    ) -> Result<Response<TakeLeaseResponse>, Status> {
        let req = request.into_inner();
        let wait = Duration::from_secs(req.wait_seconds.clamp(1, 60) as u64);
        let deadline = tokio::time::Instant::now() + wait;

        loop {
            if let Some(op) = self.sched.next_queued().await {
                let lease_id = uuid::Uuid::new_v4().to_string();
                op.attempts.fetch_add(1, Ordering::Relaxed);
                op.publish(execution_stage::Value::Executing, None);
                let action_digest = op.action_digest.clone();
                self.sched.leases.insert(
                    lease_id.clone(),
                    Lease {
                        op,
                        worker_id: req.worker_id.clone(),
                        expires_at: Instant::now() + LEASE_TIMEOUT,
                    },
                );
                return Ok(Response::new(TakeLeaseResponse {
                    have_work: true,
                    lease_id,
                    instance_name: String::new(),
                    action_digest: Some(action_digest),
                    digest_function: rbe_proto::reapi::digest_function::Value::Sha256 as i32,
                    timeout_seconds: 0,
                }));
            }

            tokio::select! {
                _ = self.sched.work_available.notified() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(Response::new(TakeLeaseResponse {
                        have_work: false,
                        ..Default::default()
                    }));
                }
            }
        }
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let renewed_until = Instant::now() + LEASE_TIMEOUT;
        let mut unknown_lease_ids = Vec::new();
        for lease_id in req.lease_ids {
            match self.sched.leases.get_mut(&lease_id) {
                Some(mut lease) => lease.expires_at = renewed_until,
                None => unknown_lease_ids.push(lease_id),
            }
        }
        Ok(Response::new(HeartbeatResponse { unknown_lease_ids }))
    }

    async fn complete_lease(
        &self,
        request: Request<CompleteLeaseRequest>,
    ) -> Result<Response<CompleteLeaseResponse>, Status> {
        let req = request.into_inner();
        let Some((_, lease)) = self.sched.leases.remove(&req.lease_id) else {
            // The sweeper already gave up on it, or it was reported twice.
            return Ok(Response::new(CompleteLeaseResponse {}));
        };
        let op = lease.op;
        let mut to_cache: Option<ActionResult> = None;

        let response = if !req.failure.is_empty() {
            ExecuteResponse {
                result: None,
                cached_result: false,
                status: Some(rbe_proto::rpc::Status {
                    code: if req.timed_out {
                        tonic::Code::DeadlineExceeded as i32
                    } else {
                        tonic::Code::Internal as i32
                    },
                    message: req.failure.clone(),
                    details: Vec::new(),
                }),
                server_logs: Default::default(),
                message: req.failure,
            }
        } else {
            let result: ActionResult = req.result.unwrap_or_default();

            // Only a clean run is cacheable. A failing action is a legitimate result to return
            // -- bazel wants the exit code and the compiler's stderr -- but caching it would
            // pin the failure for every other client until the inputs change.
            if !req.timed_out && result.exit_code == 0 && !req.do_not_cache {
                to_cache = Some(result.clone());
            }
            self.sched
                .store
                .stats
                .actions_executed
                .fetch_add(1, Ordering::Relaxed);

            ExecuteResponse {
                result: Some(result),
                cached_result: false,
                status: Some(if req.timed_out {
                    rbe_proto::rpc::Status {
                        code: tonic::Code::DeadlineExceeded as i32,
                        message: "the action exceeded its timeout".into(),
                        details: Vec::new(),
                    }
                } else {
                    ok_rpc_status()
                }),
                server_logs: Default::default(),
                message: String::new(),
            }
        };

        self.sched.finish(&op, response).await;

        // Written after bazel has its answer rather than before: the entry is for the next
        // build, and publishing it upstream is a round trip this one need not wait for. Losing
        // it to a crash in between costs a re-execution, never a wrong result.
        if let Some(result) = to_cache {
            let store = self.sched.store.clone();
            let action_digest = op.action_digest.clone();
            lore_base::lore_spawn!(async move {
                if let Err(status) = ac::store_result(&store, &action_digest, &result).await {
                    tracing::warn!(
                        "could not cache the result for action {}: {status}",
                        digest::fmt(&action_digest)
                    );
                }
            });
        }
        Ok(Response::new(CompleteLeaseResponse {}))
    }
}
