// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Keeps a native SWFS handle from being closed while anything is still using it.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use parking_lot::Condvar;
use parking_lot::Mutex;

/// Set in [`HandleGate::state`] once the gate is closed. Every other bit counts the guards held.
const CLOSED: usize = 1 << (usize::BITS - 1);

/// Admits users of a native handle until it is closed, and closes it only once none remain.
///
/// Entering and leaving are one atomic add each and never block: the gate is entered for every
/// SWFS callback and every call into SWFS. Closing is rare, and takes as long as the users it
/// waits for. Once closing has begun, every later attempt to enter fails, so nothing reaches the
/// handle after the close, including a callback SWFS was already dispatching.
#[derive(Debug, Default)]
pub struct HandleGate {
    /// [`CLOSED`], and the number of guards held.
    state: AtomicUsize,
    /// Whether the close has finished. Waited on by a second caller of [`Self::close`] until the
    /// first has closed the handle.
    closed: Mutex<bool>,
    /// Signalled when the last guard is dropped after the gate closed, and when the close has
    /// finished.
    changed: Condvar,
}

/// A use of the handle, which keeps the gate from closing until it is dropped.
#[derive(Debug)]
pub struct HandleGateGuard<'a> {
    gate: &'a HandleGate,
}

impl HandleGate {
    /// A guard for one use of the handle, or `None` once the gate has begun to close.
    ///
    /// A thread holding a guard can enter again, as a callback does that SWFS makes from within a
    /// call this thread made, but must not call [`Self::close`], which would wait for itself.
    pub fn enter(&self) -> Option<HandleGateGuard<'_>> {
        // Adding first, then looking at the flag, orders this entry against a close: either the
        // close sees the count and waits for this guard, or this sees the flag and leaves.
        if self.state.fetch_add(1, Ordering::Acquire) & CLOSED != 0 {
            self.leave();
            return None;
        }
        Some(HandleGateGuard { gate: self })
    }

    fn leave(&self) {
        // The last guard out of a closing gate lets the close proceed.
        if self.state.fetch_sub(1, Ordering::Release) == CLOSED | 1 {
            let _closed = self.closed.lock();
            self.changed.notify_all();
        }
    }

    /// Whether the gate has begun to close.
    pub fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Stop admitting users, wait for the guards held to be dropped, and run `close_handle`.
    ///
    /// `close_handle` runs once, whoever calls this and however many times. Every caller returns
    /// only after it has run, so the handle is closed when any call returns.
    pub fn close(&self, close_handle: impl FnOnce()) {
        let first = self.state.fetch_or(CLOSED, Ordering::AcqRel) & CLOSED == 0;
        let mut closed = self.closed.lock();
        if first {
            // Checked under the lock the last guard takes to signal, so its signal cannot fall
            // between the check and the wait.
            while self.state.load(Ordering::Acquire) != CLOSED {
                self.changed.wait(&mut closed);
            }
            close_handle();
            *closed = true;
            self.changed.notify_all();
        } else {
            while !*closed {
                self.changed.wait(&mut closed);
            }
        }
    }
}

impl Drop for HandleGateGuard<'_> {
    fn drop(&mut self) {
        self.gate.leave();
    }
}
