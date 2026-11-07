// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use lore_revision::fs::fuse::inode::Name;
use lore_revision::fs::fuse::notifications::Invalidations;
use lore_revision::fs::fuse::notifications::NotificationQueue;
use lore_revision::fs::fuse::notifications::Pending;
use lore_revision::fs::fuse::swap::NotificationSink;

#[derive(Debug, PartialEq)]
enum Call {
    Entry(u64, Vec<u8>),
    Inode(u64),
}

/// Records each call; the first waits for `release` when `gate` is set.
struct Recording {
    calls: parking_lot::Mutex<Vec<Call>>,
    entered: mpsc::Sender<()>,
    release: parking_lot::Mutex<Option<mpsc::Receiver<()>>>,
}

impl Recording {
    fn new(gated: bool) -> (Arc<Self>, mpsc::Receiver<()>, Option<mpsc::Sender<()>>) {
        let (entered, entered_now) = mpsc::channel();
        let (release, released) = if gated {
            let (release, released) = mpsc::channel();
            (Some(release), Some(released))
        } else {
            (None, None)
        };
        let sink = Arc::new(Self {
            calls: parking_lot::Mutex::new(Vec::new()),
            entered,
            release: parking_lot::Mutex::new(released),
        });
        (sink, entered_now, release)
    }

    fn call(&self, call: Call) {
        self.calls.lock().push(call);
        let _ = self.entered.send(());
        if let Some(released) = self.release.lock().take() {
            let _ = released.recv_timeout(Duration::from_secs(10));
        }
    }

    fn calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock())
    }
}

impl NotificationSink for Recording {
    fn inval_inode(&self, ino: u64) {
        self.call(Call::Inode(ino));
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        self.call(Call::Entry(parent, name.to_vec()));
    }
}

fn batch(entries: &[(u64, &str)], inodes: &[u64]) -> Invalidations {
    Invalidations {
        entries: entries
            .iter()
            .map(|(parent, name)| (*parent, Name::from_slice(name.as_bytes())))
            .collect(),
        inodes: inodes.to_vec(),
    }
}

#[tokio::test]
async fn batches_go_out_in_order_entries_first_and_hand_back_their_buffers() {
    let (sink, _entered, _release) = Recording::new(false);
    let queue = NotificationQueue::start(sink.clone()).expect("the thread");
    let sent = queue.send(batch(&[(1, "a"), (2, "b")], &[3, 1])).await;
    assert!(sent.is_empty(), "the buffers come back emptied");
    assert!(sent.entries.capacity() >= 2 && sent.inodes.capacity() >= 2);
    queue.send(batch(&[(4, "c")], &[5])).await;
    assert_eq!(
        sink.calls(),
        vec![
            Call::Entry(1, b"a".to_vec()),
            Call::Entry(2, b"b".to_vec()),
            Call::Inode(3),
            Call::Inode(1),
            Call::Entry(4, b"c".to_vec()),
            Call::Inode(5),
        ]
    );
    assert_eq!(queue.batches(), 2);
    queue.stop().await;
}

#[tokio::test]
async fn a_batch_whose_sender_is_dropped_still_goes_out_before_the_next() {
    let (sink, entered, release) = Recording::new(true);
    let queue = NotificationQueue::start(sink.clone()).expect("the thread");
    {
        let first = queue.send(batch(&[(1, "first")], &[]));
        tokio::pin!(first);
        tokio::select! {
            biased;
            _ = &mut first => panic!("the first batch waits on its sink"),
            () = std::future::ready(()) => {}
        }
    }
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the dropped sender's batch reaches the sink");
    release.expect("gated").send(()).expect("release");
    queue.send(batch(&[(2, "second")], &[])).await;
    assert_eq!(
        sink.calls(),
        vec![
            Call::Entry(1, b"first".to_vec()),
            Call::Entry(2, b"second".to_vec())
        ]
    );
    queue.stop().await;
}

#[tokio::test]
async fn posted_batches_and_entries_go_out_in_queue_order() {
    let (sink, _entered, _release) = Recording::new(false);
    let queue = NotificationQueue::start(sink.clone()).expect("the thread");
    queue.post(batch(&[(1, "posted")], &[1]));
    queue.post_entries(
        2,
        smallvec::smallvec![Name::from_slice(b"x"), Name::from_slice(b"y")],
    );
    queue.send(batch(&[(3, "sent")], &[])).await;
    assert_eq!(
        sink.calls(),
        vec![
            Call::Entry(1, b"posted".to_vec()),
            Call::Inode(1),
            Call::Entry(2, b"x".to_vec()),
            Call::Entry(2, b"y".to_vec()),
            Call::Entry(3, b"sent".to_vec()),
        ]
    );
    queue.stop().await;
}

#[tokio::test]
async fn a_pending_batch_dropped_unsent_is_queued() {
    let (sink, _entered, _release) = Recording::new(false);
    let queue = NotificationQueue::start(sink.clone()).expect("the thread");
    drop(Pending::new(
        Some(&queue),
        batch(&[(1, "dropped")], &[4, 4, 1]),
    ));
    {
        let pending = Pending::new(Some(&queue), batch(&[(2, "unpolled")], &[]));
        let send = pending.send();
        drop(send);
    }
    queue.send(batch(&[(3, "after")], &[])).await;
    assert_eq!(
        sink.calls(),
        vec![
            Call::Entry(1, b"dropped".to_vec()),
            Call::Inode(1),
            Call::Inode(4),
            Call::Entry(2, b"unpolled".to_vec()),
            Call::Entry(3, b"after".to_vec()),
        ],
        "queued in order, each batch's inodes sorted once"
    );
    queue.stop().await;
}

#[tokio::test]
async fn stop_sends_what_was_queued_then_ends_the_thread() {
    let (sink, _entered, _release) = Recording::new(false);
    let queue = NotificationQueue::start(sink.clone()).expect("the thread");
    {
        let queued = queue.send(batch(&[(1, "queued")], &[]));
        tokio::pin!(queued);
        tokio::select! {
            biased;
            _ = &mut queued => {}
            () = std::future::ready(()) => {}
        }
    }
    queue.stop().await;
    assert_eq!(sink.calls(), vec![Call::Entry(1, b"queued".to_vec())]);
    let after = queue.send(batch(&[(2, "late")], &[])).await;
    assert_eq!(
        after.entries.len(),
        1,
        "a batch sent after stop comes back unsent"
    );
    assert_eq!(sink.calls(), Vec::new());
}
