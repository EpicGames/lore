// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::IoSlice;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Instant;

use zerocopy::IntoBytes;

use super::TRACKING_ALLOCATIONS;

thread_local! {
    /// Prevent tracking internal bookkeeping allocations
    pub(crate) static IN_ALLOCATOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How many frames of the allocating callstack a record carries.
///
/// A debug build inlines none of the tracking plumbing, which spends six of them before the
/// code that asked for the memory, so charging an allocation to its source needs this raised.
const MAX_CALLSTACK_FRAMES: usize = 8;

type Callstack = [u64; MAX_CALLSTACK_FRAMES];

fn callstack() -> Callstack {
    let mut callstack = Callstack::default();
    let mut count = 0;
    let mut skip = 2;
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if skip > 0 {
                skip -= 1;
                return true;
            }
            if frame.ip().is_null() {
                return false;
            }
            callstack[count] = frame.ip() as u64;
            count += 1;
            count < MAX_CALLSTACK_FRAMES
        });
    }
    callstack
}

fn timestamp() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

static FREE_TAG: u64 = u64::MAX;
static ALLOCATION_DUMP: OnceLock<std::sync::mpsc::Sender<(u64, u64, u64, Callstack)>> =
    OnceLock::new();

#[cfg(not(target_os = "linux"))]
fn base_address() -> Option<usize> {
    None
}

#[cfg(target_os = "linux")]
fn base_address() -> Option<usize> {
    let path = "/proc/self/maps";
    let contents = std::fs::read_to_string(path).ok()?;
    let first_line = contents.lines().next()?;
    let base_str = first_line.split('-').next()?;
    usize::from_str_radix(base_str, 16).ok()
}

fn allocation_dump() -> std::sync::mpsc::Sender<(u64, u64, u64, Callstack)> {
    IN_ALLOCATOR.with(|internal| {
        internal.replace(true);
        let sender = ALLOCATION_DUMP.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_allocation_file_dump(rx);
            tx
        });
        sender.clone()
    })
}

pub fn spawn_allocation_file_dump(receiver: std::sync::mpsc::Receiver<(u64, u64, u64, Callstack)>) {
    if !TRACKING_ALLOCATIONS.load(Ordering::Relaxed) {
        return;
    }

    IN_ALLOCATOR.with(|internal| {
        internal.set(true);

        thread::spawn(move || {
            IN_ALLOCATOR.with(|internal| {
                internal.set(true);

                let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .read(false)
                    .truncate(true)
                    .create(true)
                    .open("allocations.dmp")
                else {
                    return;
                };

                let base_address = base_address().unwrap_or_default() as u64;

                while let Ok((timestamp, ptr, size, mut callstack)) = receiver.recv() {
                    let timestamp = timestamp.to_ne_bytes();
                    let ptr = ptr.to_ne_bytes();
                    let size = size.to_ne_bytes();
                    let count = (callstack.len() as u64).to_ne_bytes();
                    for addr in callstack.iter_mut() {
                        if *addr >= base_address {
                            *addr -= base_address;
                        }
                    }

                    let _ = file.write_vectored(&[
                        IoSlice::new(&timestamp),
                        IoSlice::new(&ptr),
                        IoSlice::new(&size),
                        IoSlice::new(&count),
                        IoSlice::new(callstack.as_bytes()),
                    ]);
                }
            });
        });

        internal.set(false);
    });
}

pub(crate) fn track_alloc(ptr: *mut u8, size: usize) {
    IN_ALLOCATOR.with(|internal| {
        if !internal.replace(true) {
            {
                let sender = allocation_dump();
                let _ = sender.send((timestamp(), ptr as u64, size as u64, callstack()));
            }
            internal.set(false);
        }
    });
}

pub(crate) fn track_dealloc(ptr: *mut u8) {
    IN_ALLOCATOR.with(|internal| {
        if !internal.replace(true) {
            {
                let sender = allocation_dump();
                let _ = sender.send((timestamp(), ptr as u64, FREE_TAG, callstack()));
            }
            internal.set(false);
        }
    });
}

pub(crate) fn track_realloc(old_ptr: *mut u8, new_ptr: *mut u8, old_size: usize) {
    IN_ALLOCATOR.with(|internal| {
        if !internal.replace(true) {
            {
                let callstack = callstack();
                let sender = allocation_dump();
                if !old_ptr.is_null() {
                    let _ = sender.send((timestamp(), old_ptr as u64, FREE_TAG, callstack));
                }
                if !new_ptr.is_null() {
                    let _ = sender.send((timestamp(), new_ptr as u64, old_size as u64, callstack));
                }
            }
            internal.set(false);
        }
    });
}
