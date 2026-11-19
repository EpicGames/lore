// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use lore_revision::progress::DiscoveryStats;
use lore_revision::progress::ProgressTicker;

/// A progress event read before discovery completes arrives before one a second thread reads and
/// sends after it completes, though the first thread holds its event until the second has sent
/// its own or a wait runs out.
#[test]
fn a_progress_event_read_first_arrives_first() {
    // With the lock the first thread's wait always runs out, which bounds the test; without it
    // the second thread sends first and the order is reversed.
    let stats = DiscoveryStats::default();
    let sent = Mutex::new(Vec::new());
    let (read_tx, read_rx) = mpsc::channel();
    let (tried_tx, tried_rx) = mpsc::channel();
    let (stats, sent) = (&stats, &sent);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            stats.send_progress(|| {
                let complete = stats.complete.load(Ordering::Relaxed);
                read_tx.send(()).expect("the second thread waits");
                let _ = tried_rx.recv_timeout(Duration::from_millis(200));
                sent.lock().expect("the sent events").push(complete);
            });
        });
        scope.spawn(move || {
            read_rx.recv().expect("the first thread read");
            stats.complete.store(true, Ordering::Relaxed);
            stats.send_progress(|| {
                sent.lock()
                    .expect("the sent events")
                    .push(stats.complete.load(Ordering::Relaxed));
            });
            let _ = tried_tx.send(());
        });
    });

    assert_eq!(*sent.lock().expect("the sent events"), vec![false, true]);
}

/// A tick in flight as the ticker is dropped is sent before the drop returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tick_is_sent_after_the_ticker_is_dropped() {
    let stats = Arc::new(DiscoveryStats::default());
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (sending_tx, sending_rx) = mpsc::channel();
    let ticks = sent.clone();
    let ticker = ProgressTicker::start(stats, Duration::from_millis(1), move |_| {
        let _ = sending_tx.send(());
        // Holds the tick in flight while the test drops the ticker.
        std::thread::sleep(Duration::from_millis(200));
        ticks.lock().expect("the sent events").push("tick");
    });
    let recorded = sent.clone();
    let dropped = std::thread::spawn(move || {
        sending_rx.recv().expect("a tick is sending");
        drop(ticker);
        recorded.lock().expect("the sent events").clone()
    })
    .join()
    .expect("the test thread");

    assert_eq!(dropped, vec!["tick"]);
}
