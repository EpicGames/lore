// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A close racing an enter on one handle, for the handles whose ops enter by raising an in-flight
//! count and then reading an invalid flag, and whose close sets the flag and then reads the count.

use std::hint::spin_loop;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Rounds a race runs at most. A round lands the two sides within the few cycles the race needs
/// only now and then, so a race takes many.
const ROUNDS: usize = 1_000_000;

/// How long a race runs at most, for a machine whose two threads share a processor.
const BUDGET: Duration = Duration::from_secs(2);

/// How long past [`BUDGET`] a thread waits at a meeting point before it takes the other for stuck.
const STUCK_AFTER: Duration = Duration::from_secs(30);

/// Spins at a meeting point before a thread yields its processor.
const SPINS_BEFORE_YIELD: u32 = 64;

/// Two threads meeting at numbered points, spinning rather than parking, so they leave each
/// point close together, until `deadline`.
struct Meet {
    arrived: AtomicUsize,
    deadline: Instant,
}

impl Meet {
    /// Waits for the other thread at point `point`, counted from zero; panics where the other
    /// has not come by the deadline, so a stuck side fails the race rather than hanging it.
    fn at(&self, point: usize) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        let target = 2 * (point + 1);
        let mut spins = 0u32;
        while self.arrived.load(Ordering::SeqCst) < target {
            spins += 1;
            if spins == SPINS_BEFORE_YIELD {
                spins = 0;
                assert!(
                    Instant::now() < self.deadline,
                    "the other side of the race never reached point {point}"
                );
                thread::yield_now();
            } else {
                spin_loop();
            }
        }
    }
}

/// A spin whose length varies by round and side, its widths cycling through 0, 1, 3, 7, 15 and 31
/// spins, so the racing steps of the two sides meet at offsets spread around zero, most of them
/// close.
fn stagger(round: usize, salt: usize) {
    let width = (1usize << (round % 6)) - 1;
    let spins = (round.wrapping_mul(0x9e37_79b9).wrapping_add(salt) >> 7) & width;
    for _ in 0..spins {
        spin_loop();
    }
}

/// Runs `enter` on one thread against `close` on another, started together, for [`ROUNDS`] rounds
/// or until [`BUDGET`] has passed, and answers how many rounds ended with both sides missing each
/// other: `close` answering it returned without waiting while the guard `enter` answered was
/// held. Between rounds the guard is dropped and `reopen` clears the handle's invalid flag.
pub(crate) fn rounds_missing_each_other<G>(
    enter: impl Fn() -> Option<G> + Sync,
    close: impl Fn() -> bool + Sync,
    reopen: impl Fn() + Sync,
) -> usize {
    let deadline = Instant::now() + BUDGET;
    let meet = Meet {
        arrived: AtomicUsize::new(0),
        deadline: deadline + STUCK_AFTER,
    };
    let entered = AtomicBool::new(false);
    let last = AtomicBool::new(false);
    let mut missed = 0;
    thread::scope(|scope| {
        scope.spawn(|| {
            for round in 0.. {
                meet.at(3 * round);
                let ending = last.load(Ordering::SeqCst);
                stagger(round, 0);
                let guard = enter();
                entered.store(guard.is_some(), Ordering::SeqCst);
                meet.at(3 * round + 1);
                drop(guard);
                meet.at(3 * round + 2);
                if ending {
                    break;
                }
            }
        });
        for round in 0..ROUNDS {
            let ending = round + 1 == ROUNDS || Instant::now() >= deadline;
            last.store(ending, Ordering::SeqCst);
            meet.at(3 * round);
            stagger(round, 0x5bd1);
            let returned = close();
            meet.at(3 * round + 1);
            if returned && entered.load(Ordering::SeqCst) {
                missed += 1;
            }
            meet.at(3 * round + 2);
            reopen();
            if ending {
                break;
            }
        }
    });
    missed
}
