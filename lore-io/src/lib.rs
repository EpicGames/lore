// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Completion-based asynchronous file I/O.
//!
//! This crate provides positional file operations whose futures are
//! runtime-independent: they suspend on wakers only and never require a
//! specific async runtime to be driving them. The memory an operation
//! exposes to the kernel belongs to the operation for its whole lifetime —
//! a write takes ownership of the caller's buffer and hands it back, and a
//! read allocates its own and returns it as [`bytes::Bytes`]. That is the
//! contract completion-based backends (`io_uring`, overlapped I/O) require,
//! where the kernel holds a pointer into the buffer while an operation is
//! in flight and an abandoned future must not free it.
//!
//! Backends are selected once and dispatched statically:
//!
//! - `psync` — positional syscalls executed on a dedicated bounded syscall
//!   pool (`min(2 × cores, 16)` threads, idle-reaped). The portable
//!   baseline backend and the semantic reference for all others.
//! - `uring` (Linux 5.6+) — data-plane operations submitted to sharded
//!   `io_uring` instances; synchronously-completing operations resolve on
//!   the submitting thread, pending ones through a single reaper thread.
//!   Control-plane operations run on the syscall pool.
//! - `iocp` (Windows) — positional reads and writes issued as overlapped
//!   I/O against one completion port, on the calling thread; operations the
//!   kernel finishes during the issuing call never reach the port, and the
//!   rest are completed by a single reaper. Control-plane operations run on
//!   the syscall pool.
//!
//! The [`clone`] module is the exception: blocking calls that clone or copy a
//! directory tree, which a caller on an async runtime runs on a blocking thread.

mod buffer;
pub mod clone;
mod dir;
#[cfg(not(feature = "test-util"))]
mod driver;
#[cfg(feature = "test-util")]
pub mod driver;
#[cfg(not(feature = "test-util"))]
mod file;
#[cfg(feature = "test-util")]
pub mod file;
#[cfg(all(target_family = "windows", not(feature = "test-util")))]
mod find;
#[cfg(all(target_family = "windows", feature = "test-util"))]
pub mod find;
#[cfg(all(target_family = "windows", not(feature = "test-util")))]
mod iocp;
#[cfg(all(target_family = "windows", feature = "test-util"))]
pub mod iocp;
#[cfg(target_family = "windows")]
mod overlapped;
#[cfg(not(feature = "test-util"))]
mod pool;
#[cfg(feature = "test-util")]
pub mod pool;
#[cfg(not(feature = "test-util"))]
mod priority;
#[cfg(feature = "test-util")]
pub mod priority;
#[cfg(not(feature = "test-util"))]
mod psync;
#[cfg(feature = "test-util")]
pub mod psync;
pub mod rename;
#[cfg(not(feature = "test-util"))]
mod stat;
#[cfg(feature = "test-util")]
pub mod stat;
#[cfg(target_os = "linux")]
mod uring;
#[cfg(target_family = "windows")]
pub mod win_path;

pub use buffer::StableBuf;
pub use buffer::StableBufList;
pub use buffer::StableBufListMut;
pub use buffer::uninit_buffer;
pub use dir::DirEntry;
pub use dir::DirStream;
pub use driver::BackendKind;
pub use driver::IoDriver;
pub use driver::WHOLE_FILE_LIMIT;
pub use file::IoFile;
pub use file::OpenOptions;
#[cfg(target_family = "windows")]
pub use iocp::IocpStats;
pub use pool::PoolStats;
pub use pool::requested_max_threads;
pub use pool::set_max_threads;
pub use stat::Stat;
pub use stat::StatKind;
#[cfg(target_os = "linux")]
pub use uring::UringStats;

/// A snapshot of the process-wide syscall pool, which every driver and every backend shares.
///
/// Exposed so the thread budget can be checked against a real workload rather than assumed: the
/// high-water marks say whether the cap is ever reached, and a deep queue alongside idle threads
/// says the cap is not what is limiting the work.
pub fn pool_stats() -> PoolStats {
    pool::SyscallPool::global().stats()
}
