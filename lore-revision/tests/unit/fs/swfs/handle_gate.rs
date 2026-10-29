// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_revision::fs::swfs::handle_gate::HandleGate;

/// Waits for `condition`, failing the test if it does not hold within a few seconds.
fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::yield_now();
    }
}

#[test]
fn the_gate_admits_users_until_it_is_closed() {
    let gate = HandleGate::default();
    let closes = AtomicUsize::new(0);

    drop(gate.enter().expect("open"));
    gate.close(|| {
        closes.fetch_add(1, Ordering::Relaxed);
    });

    assert!(gate.enter().is_none());
    assert!(gate.is_closed());
    gate.close(|| {
        closes.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(
        closes.load(Ordering::Relaxed),
        1,
        "the handle is closed once"
    );
}

#[test]
fn a_thread_holding_a_guard_can_enter_again() {
    let gate = HandleGate::default();
    let outer = gate.enter().expect("open");
    let inner = gate.enter().expect("still open");
    drop(inner);
    drop(outer);
    gate.close(|| {});
}

#[test]
fn a_close_waits_for_the_guards_held_and_admits_no_one_meanwhile() {
    let gate = Arc::new(HandleGate::default());
    let held = Arc::new(AtomicBool::new(true));
    let guard = gate.enter().expect("open");

    let closer = std::thread::spawn({
        let gate = gate.clone();
        let held = held.clone();
        // Whether a guard was still held when the handle was closed.
        move || {
            let mut closed_while_held = None;
            gate.close(|| closed_while_held = Some(held.load(Ordering::Acquire)));
            closed_while_held
        }
    });
    wait_until(|| gate.is_closed());
    assert!(gate.enter().is_none(), "no one enters a closing gate");

    // The close must not finish while the guard is held. A close that did not wait would finish
    // in microseconds; this gives it far longer before letting go.
    let deadline = Instant::now() + Duration::from_millis(100);
    while !closer.is_finished() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    held.store(false, Ordering::Release);
    drop(guard);

    assert_eq!(
        closer.join().expect("close"),
        Some(false),
        "the handle is closed, and only after the guard is dropped"
    );
}

#[test]
fn a_second_close_returns_only_once_the_handle_is_closed() {
    let gate = Arc::new(HandleGate::default());
    let closes = Arc::new(AtomicUsize::new(0));
    let guard = gate.enter().expect("open");

    let first = std::thread::spawn({
        let gate = gate.clone();
        let closes = closes.clone();
        move || {
            gate.close(|| {
                closes.fetch_add(1, Ordering::AcqRel);
            });
        }
    });
    wait_until(|| gate.is_closed());
    let second = std::thread::spawn({
        let gate = gate.clone();
        let closes = closes.clone();
        move || {
            gate.close(|| {
                closes.fetch_add(1, Ordering::AcqRel);
            });
            closes.load(Ordering::Acquire)
        }
    });

    drop(guard);
    first.join().expect("first close");
    assert_eq!(
        second.join().expect("second close"),
        1,
        "the second close returned after the handle was closed, and did not close it again"
    );
}

#[test]
fn no_user_sees_the_handle_closed_while_it_holds_a_guard() {
    let gate = Arc::new(HandleGate::default());
    let closed = Arc::new(AtomicBool::new(false));
    let uses = Arc::new(AtomicUsize::new(0));
    // Users between entering and leaving, as they would be while calling into SWFS.
    let inside = Arc::new(AtomicUsize::new(0));

    let users: Vec<_> = (0..8)
        .map(|_| {
            let gate = gate.clone();
            let closed = closed.clone();
            let uses = uses.clone();
            let inside = inside.clone();
            std::thread::spawn(move || {
                while let Some(_guard) = gate.enter() {
                    inside.fetch_add(1, Ordering::AcqRel);
                    assert!(!closed.load(Ordering::Acquire), "used after close");
                    uses.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                    assert!(!closed.load(Ordering::Acquire), "closed while in use");
                    inside.fetch_sub(1, Ordering::AcqRel);
                }
            })
        })
        .collect();
    wait_until(|| uses.load(Ordering::Relaxed) > 1_000);

    let mut inside_at_close = None;
    gate.close(|| {
        inside_at_close = Some(inside.load(Ordering::Acquire));
        closed.store(true, Ordering::Release);
    });
    assert_eq!(inside_at_close, Some(0), "closed with users inside");
    for user in users {
        user.join().expect("every user stops once the gate closes");
    }
}
