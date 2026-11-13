// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

mod collect_stream;
mod config;
mod decimal;
mod encoding;
mod fan_out;
mod fs;
mod path;
mod request_tracker;

/// What `f` returns and the allocations the calling thread made running it.
///
/// Counting stays on, since switching it off would undercount a test measuring on another thread.
pub(crate) fn allocations_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    lore_base::allocator::set_allocation_counting(true);
    let before = lore_base::allocator::thread_allocation_count();
    let value = std::hint::black_box(f());
    (
        value,
        lore_base::allocator::thread_allocation_count() - before,
    )
}
