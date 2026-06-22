// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Compression pool dispatch: submits compress/decompress work to the shared
//! compute thread pool, with an inline fast path for small payloads and a
//! typed work enum on the worker hot path.
//!
//! The pool itself lives in [`lore_base::runtime::compute_pool`]; this module
//! is the producer side.

use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;

use super::CompressionMode;
use super::FragmentError;
use super::compress_bound;
use super::compress_into;
use super::decompress_into;
use crate::Fragment;

/// Payload size below which compression runs inline on the caller's thread
/// instead of being dispatched to the pool. For fragments this small the pool
/// submit/await round trip costs more than the compression work itself.
const INLINE_COMPRESS_SIZE_THRESHOLD: usize = 4 * 1024;

/// Decompressed size below which decompression runs inline on the caller's
/// thread. Higher than the compress threshold because decompression is cheaper
/// per byte, so a larger fragment still beats the pool round-trip overhead.
const INLINE_DECOMPRESS_SIZE_THRESHOLD: usize = 32 * 1024;

type CompressResult = Result<(Fragment, Bytes), FragmentError>;
type DecompressResult = Result<(Fragment, BytesMut), FragmentError>;

/// A typed unit of work for the compression pool. Using an enum instead of
/// `Box<dyn FnOnce>` removes the virtual-dispatch overhead on the worker hot
/// path and keeps the closure shape monomorphic per variant.
enum CompressionWork {
    Compress {
        fragment: Fragment,
        payload: Bytes,
        mode: CompressionMode,
        output_buffer: BytesMut,
        tx: tokio::sync::oneshot::Sender<CompressResult>,
    },
    Decompress {
        fragment: Fragment,
        compressed: Bytes,
        output_buffer: BytesMut,
        tx: tokio::sync::oneshot::Sender<DecompressResult>,
    },
}

impl CompressionWork {
    // catch_unwind below is only effective under panic=unwind. The release
    // profile for this workspace uses panic=abort, so in production a panic
    // in compress/decompress aborts the process immediately and neither the
    // warn log nor the typed-error send below runs. The catch still covers
    // dev/test builds and any future profile change to unwind.
    fn execute(self) {
        match self {
            Self::Compress {
                fragment,
                payload,
                mode,
                output_buffer,
                tx,
            } => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    compress_into(
                        fragment,
                        &payload.as_ref()[..fragment.size_payload as usize],
                        mode,
                        output_buffer,
                    )
                }))
                .unwrap_or_else(|panic| {
                    lore_base::lore_warn!(
                        "compression worker panicked while compressing {} bytes: {}",
                        fragment.size_payload,
                        panic_message(&panic),
                    );
                    Err(FragmentError::internal("compression worker panicked"))
                });
                let _ = tx.send(result);
            }
            Self::Decompress {
                fragment,
                compressed,
                output_buffer,
                tx,
            } => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decompress_into(fragment, compressed.as_ref(), output_buffer)
                }))
                .unwrap_or_else(|panic| {
                    lore_base::lore_warn!(
                        "compression worker panicked while decompressing {} bytes: {}",
                        fragment.size_payload,
                        panic_message(&panic),
                    );
                    Err(FragmentError::internal("compression worker panicked"))
                });
                let _ = tx.send(result);
            }
        }
    }
}

/// Extract a best-effort human-readable message from a panic payload.
/// Panics with `panic!(&str)` or `panic!("fmt")` carry a `&str` or
/// `String`; anything else yields a placeholder.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "non-string panic payload"
    }
}

/// Dispatch a [`CompressionWork`] item to the shared compute pool. The
/// pool is a rayon `ThreadPool` owned by `lore_base::runtime`; it uses a
/// work-stealing deque per worker plus a lock-free injector for external
/// submissions.
fn dispatch(work: CompressionWork) {
    lore_base::runtime::compute_pool().spawn(move || work.execute());
}

pub async fn decompress_async(
    fragment: Fragment,
    compressed: Bytes,
) -> Result<(Fragment, BytesMut), FragmentError> {
    // The output buffer is allocated here on the caller's thread (typically
    // a tokio worker) so that large buffers never originate on a compute
    // worker's thread-local heap.
    let output_buffer = BytesMut::with_capacity(fragment.size_content as usize);

    // Run inline for fragments whose payload work is smaller than the pool
    // round-trip overhead. size_content is the decompressed size, which is
    // what governs actual decompression cost.
    if (fragment.size_content as usize) < INLINE_DECOMPRESS_SIZE_THRESHOLD {
        return decompress_into(fragment, compressed.as_ref(), output_buffer);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    dispatch(CompressionWork::Decompress {
        fragment,
        compressed,
        output_buffer,
        tx,
    });
    rx.await.internal("compression pool failure")?
}

pub async fn compress_async(
    fragment: Fragment,
    payload: Bytes,
    mode: CompressionMode,
) -> Result<(Fragment, Bytes), FragmentError> {
    // Pre-allocate the output buffer here on the caller's thread so large
    // allocations stay off the compute worker's heap.
    let output_buffer =
        BytesMut::with_capacity(compress_bound(fragment.size_payload as usize, mode));

    // Run inline for small payloads; see [`decompress_async`] for the
    // rationale. For compression size_payload == size_content (uncompressed
    // input is required), so either is a valid cutoff metric.
    if (fragment.size_payload as usize) < INLINE_COMPRESS_SIZE_THRESHOLD {
        return compress_into(
            fragment,
            &payload.as_ref()[..fragment.size_payload as usize],
            mode,
            output_buffer,
        );
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    dispatch(CompressionWork::Compress {
        fragment,
        payload,
        mode,
        output_buffer,
        tx,
    });
    rx.await.internal("compression pool failure")?
}
