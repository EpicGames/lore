// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Parking a C API call at a scheduling point of the State's node operations, so a scenario runs
//! another call while it waits there.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Waker;

use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use tokio::sync::Notify;

/// Parks a task the `nth` time it reaches `point` after [`Self::arm`], walks' own points included,
/// until [`Self::release`], and nowhere else.
pub(crate) struct Parks {
    point: Point,
    countdown: AtomicUsize,
    parked: AtomicBool,
    release: Arc<Notify>,
}

impl Parks {
    pub(crate) fn at(point: Point) -> Arc<Self> {
        Arc::new(Self {
            point,
            countdown: AtomicUsize::new(0),
            parked: AtomicBool::new(false),
            release: Arc::new(Notify::new()),
        })
    }

    pub(crate) fn arm(&self, nth: usize) {
        self.parked.store(false, Ordering::Relaxed);
        self.countdown.store(nth, Ordering::Relaxed);
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

impl hooks::Scheduler for Parks {
    fn reached(&self, point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        let left = self.countdown.load(Ordering::Relaxed);
        if point != self.point || left == 0 {
            return None;
        }
        self.countdown.store(left - 1, Ordering::Relaxed);
        if left > 1 {
            return None;
        }
        self.parked.store(true, Ordering::Relaxed);
        let release = self.release.clone();
        Some(Box::pin(async move { release.notified().await }))
    }

    fn touched(&self, _: hooks::Resource, _: bool) {}

    fn reader_points(&self) -> bool {
        true
    }
}

/// A call in flight, polled by the test.
pub(crate) type Call<'a> = Pin<Box<dyn Future<Output = i32> + 'a>>;

/// Polls `call` until it parks as `parks` is armed to; it must not finish first.
pub(crate) async fn until_parked(call: &mut Call<'_>, parks: &Parks) {
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..10_000 {
        if parks.parked.load(Ordering::Relaxed) {
            return;
        }
        assert!(
            call.as_mut().poll(&mut context).is_pending(),
            "the call finished before it parked at {:?}",
            parks.point
        );
        tokio::task::yield_now().await;
    }
    panic!("the call never parked at {:?}", parks.point);
}
