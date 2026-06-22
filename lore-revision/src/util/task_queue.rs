// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::fmt::Debug;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::Clock;
use governor::clock::Reference;
use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_telemetry::InstrumentProvider;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tokio::sync::OwnedSemaphorePermit;
#[cfg(test)]
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::lore_error;
use crate::lore_trace;
use crate::lore_warn;

// In a world where multiple TaskQueue's exist on a given service, what is this one?
pub const METRICS_TASK_QUEUE_LABEL: &str = "name";
const METRICS_TASK_QUEUE_CONTEXT_KEY: &str = "context";
const METRICS_LATENCY_SECONDS: u64 = 30;

struct Task<T: Debug + Send + Sync + 'static> {
    work: BoxFuture<'static, T>,
    result_sender: tokio::sync::oneshot::Sender<T>,
    permit: OwnedSemaphorePermit,
}

#[error_set]
pub enum TaskQueueError {}

struct Otel {
    labels: Arc<Vec<KeyValue>>,
    num_rate_limited_counter: OnceLock<Counter<u64>>,
    num_task_result_send_failed_counter: OnceLock<Counter<u64>>,
    num_task_work_send_failed: OnceLock<Counter<u64>>,

    // observing changes in usage in real time is too expensive for such a hot part of the codebase.
    // Instead we can observe the system at intervals. These values represent what has changed since
    // the last observation
    latent_num_active_tasks: AtomicU64,
    latent_num_submitted_tasks: AtomicU64,
    latent_num_pending_tasks: AtomicU64,
}

impl InstrumentProvider for Otel {
    fn namespace(&self) -> &'static str {
        "urc.taskqueue"
    }

    fn labels(&self) -> &[KeyValue] {
        &self.labels
    }
}

pub struct TaskQueue<T>
where
    T: Debug + Send + Sync + 'static,
{
    work_sender: UnboundedSender<Task<T>>,
    submission_limit: Arc<Semaphore>,
    otel: Arc<Otel>,
    observability_task: JoinHandle<()>,

    #[cfg(test)]
    pub rate_limited_count: Arc<RwLock<AtomicU64>>,
}

impl<T> TaskQueue<T>
where
    T: Debug + Send + Sync + 'static,
{
    fn start_observability_loop(labels: Vec<KeyValue>) -> (Arc<Otel>, JoinHandle<()>) {
        let otel = Arc::new(Otel {
            labels: Arc::new(labels),
            num_rate_limited_counter: OnceLock::new(),
            num_task_result_send_failed_counter: OnceLock::new(),
            num_task_work_send_failed: OnceLock::new(),
            latent_num_active_tasks: AtomicU64::new(0),
            latent_num_submitted_tasks: AtomicU64::new(0),
            latent_num_pending_tasks: AtomicU64::new(0),
        });

        otel.num_rate_limited_counter
            .set(otel.counter("num_rate_limited"))
            .expect("rate_limited_counter should not have been set yet");
        otel.num_task_result_send_failed_counter
            .set(otel.counter("num_task_result_send_failed"))
            .expect("num_task_result_send_failed_counter should not have been set yet");
        otel.num_task_work_send_failed
            .set(otel.counter("num_task_work_send_failed"))
            .expect("num_task_work_send_failed should not have been set");

        let num_active_tasks_gauge = otel.gauge("active_tasks");
        let num_submitted_tasks_counter = otel.counter("submitted_tasks");
        let num_pending_tasks_gauge = otel.gauge("pending_tasks");

        let otel_clone = otel.clone();

        let task_handle = lore_spawn!(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(METRICS_LATENCY_SECONDS));
            loop {
                interval.tick().await;

                let num_active_tasks = otel_clone.latent_num_active_tasks.load(Ordering::Acquire);
                let submitted_tasks_update = otel_clone.latent_num_submitted_tasks.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |_| Some(0),
                );

                num_active_tasks_gauge.record(num_active_tasks, &otel_clone.labels);
                if let Ok(num_submitted_tasks) = submitted_tasks_update {
                    num_submitted_tasks_counter.add(num_submitted_tasks, &otel_clone.labels);
                }

                num_pending_tasks_gauge.record(
                    otel_clone.latent_num_pending_tasks.load(Ordering::Relaxed),
                    &otel_clone.labels,
                );
            }
        });

        (otel, task_handle)
    }

    pub fn new(
        quota_per_second: u32,
        concurrency_limit: usize,
        submission_limit: usize,
        otel_labels: Vec<KeyValue>,
    ) -> Self {
        let (work_sender, mut work_receiver) = tokio::sync::mpsc::unbounded_channel::<Task<T>>();

        // Here lie shenanigans! We have a test that wants to assert on the number of times we
        // enforced the concurrency limit, but unfortunately there's no way to read directly from an
        // otel counter. So, for test config we create our own counter and increment/expose that as
        // needed.
        #[cfg(test)]
        let (rate_limited_count_clone, rate_limited_count) = {
            let counter = Arc::new(RwLock::new(AtomicU64::new(0)));
            (counter.clone(), counter)
        };

        let (otel, observability_task) = Self::start_observability_loop(otel_labels);

        let task_queue = Self {
            work_sender,
            submission_limit: Arc::new(Semaphore::new(submission_limit)),
            otel: otel.clone(),
            observability_task,

            #[cfg(test)]
            rate_limited_count,
        };

        let worker_otel = otel.clone();
        // task queue worker
        lore_spawn!(async move {
            let limiter = RateLimiter::direct(Quota::per_second(
                NonZeroU32::new(quota_per_second).unwrap(),
            ));

            let task_limit = Arc::new(Semaphore::new(concurrency_limit));

            loop {
                // Check the throughput rate limit.
                match limiter.check() {
                    Ok(_) => {
                        if let Some(task) = work_receiver.recv().await {
                            // If there's a new task available, check the concurrency rate limit to
                            // ensure we have capacity to process it. If not, wait until we acquire
                            // a permit before executing the task.
                            #[cfg(not(test))]
                            let permit_future = Self::permit(
                                task_limit.clone(),
                                "concurrency",
                                worker_otel.clone(),
                            );
                            #[cfg(test)]
                            let permit_future = TaskQueue::<T>::permit(
                                task_limit.clone(),
                                "concurrency",
                                worker_otel.clone(),
                                rate_limited_count_clone.clone(),
                            );

                            let permit = match permit_future.await {
                                Ok(permit) => permit,
                                Err(e) => {
                                    lore_warn!("Failed to acquire permit: {e:?}");
                                    break;
                                }
                            };

                            let worker_otel = worker_otel.clone();
                            lore_spawn!(async move {
                                worker_otel
                                    .latent_num_pending_tasks
                                    .fetch_sub(1, Ordering::Relaxed);
                                worker_otel
                                    .latent_num_active_tasks
                                    .fetch_add(1, Ordering::Relaxed);
                                let result = task.work.await;
                                worker_otel
                                    .latent_num_active_tasks
                                    .fetch_sub(1, Ordering::Relaxed);
                                drop(permit);
                                drop(task.permit);

                                if let Err(result) = task.result_sender.send(result) {
                                    // The only scenario in which this will return an error is
                                    // if the receiver has dropped, which should never happen in
                                    // the normal course of events.
                                    if let Some(counter) =
                                        worker_otel.num_task_result_send_failed_counter.get()
                                    {
                                        counter.add(1, &worker_otel.labels);
                                    } else {
                                        lore_error!("num_task_result_send_failed_counter not set");
                                    }

                                    // It would be nice if we could log the task and/or the
                                    // result here, unfortunately neither is amenable to
                                    // implementing display nor debug.
                                    lore_trace!(
                                        "Failed to send task result, receiver has dropped: {result:?}"
                                    );
                                }
                            });
                        } else {
                            lore_trace!("No messages remaining");
                            break;
                        }
                    }
                    Err(e) => {
                        if let Some(counter) = worker_otel.num_rate_limited_counter.get() {
                            counter.add(
                                1,
                                &[
                                    &[
                                        KeyValue::new("type", "throughput"),
                                        KeyValue::new(METRICS_TASK_QUEUE_CONTEXT_KEY, "new"),
                                    ],
                                    worker_otel.labels.as_slice(),
                                ]
                                .concat(),
                            );
                        } else {
                            lore_error!("num_rate_limited_counter not set");
                        }

                        let delay = Duration::from_nanos(
                            e.earliest_possible()
                                .duration_since(limiter.clock().now())
                                .as_u64(),
                        );
                        lore_trace!(
                            "Rate limit check failed, waiting {delay:?} before trying again"
                        );
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });

        task_queue
    }

    async fn permit(
        semaphore: Arc<Semaphore>,
        limit_type: &'static str,
        otel: Arc<Otel>,
        #[cfg(test)] rate_limited_count: Arc<RwLock<AtomicU64>>,
    ) -> Result<OwnedSemaphorePermit, TaskQueueError> {
        // We use `try_acquire` so that we can track a metric when we're rate limited.
        match semaphore.clone().try_acquire_owned() {
            Ok(permit) => Ok(permit),
            Err(TryAcquireError::NoPermits) => {
                // If there were no permits available, increment the counter and then just wait
                // until we're able to acquire a permit.
                if let Some(counter) = otel.num_rate_limited_counter.get() {
                    counter.add(
                        1,
                        &[
                            &[
                                KeyValue::new("type", limit_type),
                                KeyValue::new(METRICS_TASK_QUEUE_CONTEXT_KEY, "permit"),
                            ],
                            otel.labels.as_slice(),
                        ]
                        .concat(),
                    );
                } else {
                    lore_error!("num_rate_limited_counter not set");
                }

                #[cfg(test)]
                rate_limited_count
                    .write()
                    .await
                    .fetch_add(1, Ordering::Relaxed);

                if let Ok(permit) = semaphore.clone().acquire_owned().await {
                    Ok(permit)
                } else {
                    Err(TaskQueueError::internal("semaphore shut down"))
                }
            }
            Err(_) => {
                lore_warn!("Semaphore was closed while acquiring permit");
                Err(TaskQueueError::internal("semaphore shut down"))
            }
        }
    }

    pub async fn submit(
        &self,
        work: BoxFuture<'static, T>,
    ) -> Result<TaskProgress<T>, TaskQueueError> {
        #[cfg(not(test))]
        let permit_future = Self::permit(
            self.submission_limit.clone(),
            "submission",
            self.otel.clone(),
        );
        #[cfg(test)]
        let permit_future = TaskQueue::<T>::permit(
            self.submission_limit.clone(),
            "lore_submission",
            self.otel.clone(),
            self.rate_limited_count.clone(),
        );

        let permit = permit_future.await?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = Task {
            work: work.boxed(),
            result_sender: tx,
            permit,
        };
        self.otel
            .latent_num_submitted_tasks
            .fetch_add(1, Ordering::Relaxed);
        self.otel
            .latent_num_pending_tasks
            .fetch_add(1, Ordering::Relaxed);

        if let Err(e) = self.work_sender.send(task) {
            // The only scenario in which this will return an error is if the receiver has
            // dropped, which should never happen in the normal course of events.
            self.otel
                .counter("num_task_work_send_failed")
                .add(1, &self.otel.labels);
            lore_warn!("Failed to submit task: {e}");
            Err(TaskQueueError::internal("receiver shut down"))
        } else {
            Ok(TaskProgress {
                receiver: Box::pin(rx),
            })
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn rate_limited_count(&self) -> usize {
        #[cfg(not(test))]
        {
            0
        }

        #[cfg(test)]
        {
            self.rate_limited_count.read().await.load(Ordering::Relaxed) as usize
        }
    }
}

pub struct TaskProgress<T>
where
    T: Debug + Send + Sync + 'static,
{
    receiver: Pin<Box<tokio::sync::oneshot::Receiver<T>>>,
}

impl<T> Future for TaskProgress<T>
where
    T: Debug + Send + Sync + 'static,
{
    type Output = Result<T, TaskQueueError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut().receiver.as_mut().poll(cx) {
            Poll::Ready(Err(e)) => {
                lore_warn!("Error polling receiver for task: {e}");
                Poll::Ready(Err(TaskQueueError::internal("receiver shut down")))
            }
            Poll::Ready(Ok(result)) => Poll::Ready(Ok(result)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for TaskQueue<T>
where
    T: Debug + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.observability_task.abort();
    }
}
