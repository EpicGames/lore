// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A mount's notification thread. A kernel invalidation blocks its sender until the kernel holds
//! what it invalidates: an entry invalidation takes its directory's lock, which a LOOKUP there
//! keeps until the daemon answers it. Sent from a runtime worker, it could block the very task
//! that answer needs; sent from a shared blocking pool, it could hold a thread other work waits
//! on. Swaps and instance operations hand their invalidations to this thread instead.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use smallvec::SmallVec;
use tokio::sync::Notify;
use tokio::sync::oneshot;

use crate::fs::fuse::inode::Name;
use crate::fs::fuse::swap::NotificationSink;
use crate::lore_warn;

/// How long [`NotificationQueue::stop`] waits for the thread. A send still waiting in the kernel
/// on a request the stopped session never answers ends when the connection does.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// The thread's stack: it only loops over writes.
const STACK_BYTES: usize = 128 << 10;

/// Invalidations sent together, entries before inodes.
#[derive(Default)]
pub struct Invalidations {
    pub entries: Vec<(u64, Name)>,
    pub inodes: Vec<u64>,
}

impl Invalidations {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.inodes.is_empty()
    }
}

enum Message {
    Send(Invalidations, oneshot::Sender<Invalidations>),
    Post(Invalidations),
    Entries(u64, SmallVec<[Name; 2]>),
    Stop,
}

/// Set as the thread ends, once it has dropped its sink.
#[derive(Default)]
struct Ended {
    ended: AtomicBool,
    notify: Notify,
}

/// A thread sending batches of invalidations through a sink, in the order they were queued. A
/// batch handed over goes out whether or not its sender still waits for it. The thread is
/// detached: [`Self::stop`] waits for it to end rather than joining it.
pub struct NotificationQueue {
    sender: mpsc::Sender<Message>,
    batches: Arc<AtomicU64>,
    ended: Arc<Ended>,
}

impl NotificationQueue {
    /// Starts the thread, sending through `sink`.
    pub fn start(sink: Arc<dyn NotificationSink>) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let batches = Arc::new(AtomicU64::new(0));
        let ended = Arc::new(Ended::default());
        let (counted, ending) = (batches.clone(), ended.clone());
        std::thread::Builder::new()
            .name("lore-fuse-notify".to_string())
            .stack_size(STACK_BYTES)
            .spawn(move || {
                send_all(&*sink, &receiver, &counted);
                drop((sink, receiver));
                ending.ended.store(true, Ordering::Release);
                ending.notify.notify_waiters();
            })?;
        Ok(Self {
            sender,
            batches,
            ended,
        })
    }

    /// Sends `batch` after everything queued before it, and answers its buffers, emptied, once
    /// it is out. Answers it unsent once the thread has ended, and empty if it ends first.
    pub async fn send(&self, batch: Invalidations) -> Invalidations {
        if batch.is_empty() {
            return batch;
        }
        let (sent, out) = oneshot::channel();
        if let Err(mpsc::SendError(message)) = self.sender.send(Message::Send(batch, sent)) {
            return match message {
                Message::Send(batch, _) | Message::Post(batch) => batch,
                Message::Entries(..) | Message::Stop => Invalidations::default(),
            };
        }
        out.await.unwrap_or_default()
    }

    /// Queues `batch` after everything queued before it, without waiting for it.
    pub fn post(&self, batch: Invalidations) {
        if !batch.is_empty() {
            let _ = self.sender.send(Message::Post(batch));
        }
    }

    /// Queues the entries `names` in directory `parent`, without waiting for them.
    pub fn post_entries(&self, parent: u64, names: SmallVec<[Name; 2]>) {
        if !names.is_empty() {
            let _ = self.sender.send(Message::Entries(parent, names));
        }
    }

    /// How many batches the thread has sent.
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }

    /// Sends what was queued before it, then waits for the thread to end, its sink dropped.
    pub async fn stop(&self) {
        let _ = self.sender.send(Message::Stop);
        let ended = async {
            loop {
                let notified = self.ended.notify.notified();
                if self.ended.ended.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(STOP_WAIT, ended).await.is_err() {
            lore_warn!("FUSE notification thread still sending after the session stopped");
        }
    }
}

/// The thread's loop: each message in turn, until [`Message::Stop`] or every sender is gone.
fn send_all(sink: &dyn NotificationSink, receiver: &mpsc::Receiver<Message>, counted: &AtomicU64) {
    let send = |batch: &Invalidations| {
        for (parent, name) in &batch.entries {
            sink.inval_entry(*parent, name);
        }
        for &ino in &batch.inodes {
            sink.inval_inode(ino);
        }
        counted.fetch_add(1, Ordering::Relaxed);
    };
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Send(mut batch, sent) => {
                send(&batch);
                batch.entries.clear();
                batch.inodes.clear();
                let _ = sent.send(batch);
            }
            Message::Post(batch) => send(&batch),
            Message::Entries(parent, names) => {
                for name in &names {
                    sink.inval_entry(parent, name);
                }
            }
            Message::Stop => return,
        }
    }
}

/// Invalidations collected for one send. Dropped unsent, as a cancelled future drops them, they
/// are queued without waiting, so what was changed is still invalidated.
pub struct Pending<'a> {
    queue: Option<&'a NotificationQueue>,
    pub batch: Invalidations,
}

impl<'a> Pending<'a> {
    pub fn new(queue: Option<&'a NotificationQueue>, batch: Invalidations) -> Self {
        Self { queue, batch }
    }

    /// Sends the batch and waits for it, answering its buffers, emptied.
    pub async fn send(mut self) -> Invalidations {
        let mut batch = std::mem::take(&mut self.batch);
        dedup_inodes(&mut batch);
        match self.queue {
            Some(queue) => queue.send(batch).await,
            None => batch,
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(queue) = self.queue
            && !self.batch.is_empty()
        {
            let mut batch = std::mem::take(&mut self.batch);
            dedup_inodes(&mut batch);
            queue.post(batch);
        }
    }
}

/// Each inode once: a directory is invalidated once however many of its entries changed.
fn dedup_inodes(batch: &mut Invalidations) {
    batch.inodes.sort_unstable();
    batch.inodes.dedup();
}
