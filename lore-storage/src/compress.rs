// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

#[cfg(feature = "oodle")]
use std::alloc::Layout;
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;
use serde::Deserialize;

use crate::Fragment;
use crate::FragmentFlags;
use crate::errors::InefficientCompression;
use crate::errors::NotSupported;

#[error_set]
pub enum CompressFragmentError {
    NotSupported,
    InefficientCompression,
}

#[error_set]
pub enum FragmentError {
    NotSupported,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(bound(deserialize = "'de: 'static"))]
pub enum CompressionMode {
    NotSpecified = 0,
    NoCompression = 1,
    Lz4 = 2,
    Oodle = 3,
    Zstd = 4,
}

impl CompressionMode {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => CompressionMode::NoCompression,
            2 => CompressionMode::Lz4,
            3 => CompressionMode::Oodle,
            4 => CompressionMode::Zstd,
            _ => CompressionMode::NotSpecified,
        }
    }
}

pub static COMPRESSION_MODE: AtomicU32 = AtomicU32::new(0);

pub use lore_base::types::FRAGMENT_SIZE_THRESHOLD;

pub const FRAGMENT_COMPRESS_SIZE_LIMIT: usize = 32;

#[cfg(feature = "oodle")]
#[repr(C)]
struct OodleBlockHeader {
    size: usize,
    align: u32,
    padding: u32,
}

#[cfg(feature = "oodle")]
unsafe extern "C" fn oodle_alloc(size: isize, align: i32) -> *mut core::ffi::c_void {
    let size = size as usize;
    let requested_align = align as usize;
    let final_align = std::cmp::max(align_of::<OodleBlockHeader>(), requested_align);
    let padding = std::cmp::max(size_of::<OodleBlockHeader>(), final_align);
    let total = size + padding;
    let layout = Layout::from_size_align(total, final_align).unwrap();

    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }

    let header = raw.cast::<OodleBlockHeader>();
    let buffer = unsafe {
        (*header).size = total;
        (*header).align = final_align as u32;

        let buffer = raw.add(padding);
        let padding_value = buffer.cast::<u32>().sub(1);
        *padding_value = padding as u32;

        buffer
    };

    buffer.cast()
}

#[cfg(feature = "oodle")]
unsafe extern "C" fn oodle_free(ptr: *mut std::ffi::c_void) {
    unsafe {
        let padding_value = ptr.cast::<u32>().sub(1);
        let padding = *padding_value as usize;

        let header = ptr.cast::<u8>().sub(padding).cast::<OodleBlockHeader>();

        std::alloc::dealloc(
            header.cast(),
            Layout::from_size_align_unchecked((*header).size, (*header).align as usize),
        );
    }
}

// OODEFFUNC typedef void * (OODLE_CALLBACK t_fp_OodleCore_Plugin_MallocAligned)( OO_SINTa bytes, OO_S32 alignment);
#[cfg(feature = "oodle")]
pub type OodleAllocFn =
    unsafe extern "C" fn(bytes: isize, alignment: i32) -> *mut core::ffi::c_void;

// OODEFFUNC typedef void (OODLE_CALLBACK t_fp_OodleCore_Plugin_Free)( void * ptr );
#[cfg(feature = "oodle")]
pub type OodleFreeFn = unsafe extern "C" fn(ptr: *mut core::ffi::c_void);

#[cfg(feature = "oodle")]
unsafe extern "C" {
    fn OodleCore_Plugins_SetAllocators(alloc: OodleAllocFn, free: OodleFreeFn);
}

#[cfg(feature = "oodle")]
static OODLE_INITIALIZER: Once = Once::new();

#[cfg(feature = "oodle")]
fn oodle_initialize() {
    OODLE_INITIALIZER.call_once(|| unsafe {
        OodleCore_Plugins_SetAllocators(oodle_alloc, oodle_free);
    });
}

#[cfg(all(feature = "oodle", target_family = "windows"))]
unsafe extern "system" {
    fn OodleLZ_Decompress(
        compBuf: *const std::ffi::c_void,
        compBufSize: isize,
        rawBuf: *mut std::ffi::c_void,
        rawLen: isize,
        fuzzSafe: i32,
        checkCRC: i32,
        verbosity: i32,
        decBufBase: *mut std::ffi::c_void,
        decBufSize: isize,
        fpCallback: *const std::ffi::c_void,
        callbackUserData: *const std::ffi::c_void,
        decoderMemory: *mut std::ffi::c_void,
        decoderMemorySize: isize,
        threadPhase: i32,
    ) -> isize;

    fn OodleLZ_Compress(
        compressor: u32,
        rawBuf: *const std::ffi::c_void,
        rawLen: isize,
        compBuf: *mut std::ffi::c_void,
        level: u32,
        pOptions: *const std::ffi::c_void, /* *const OodleLZ_CompressOptions */
        dictionaryBase: *const std::ffi::c_void,
        lrm: *const std::ffi::c_void,
        scratchMem: *mut std::ffi::c_void,
        scratchSize: isize,
    ) -> isize;

    fn OodleLZ_GetCompressedBufferSizeNeeded(compressor: u32, rawSize: isize) -> isize;

    /*
    fn OodleLZ_GetCompressScratchMemBound(
        compressor: u32,
        level: u32,
        rawLen: isize,
        pOptions: *const std::ffi::c_void, /* *const OodleLZ_CompressOptions */
    ) -> isize;
    */
}

#[cfg(all(feature = "oodle", target_family = "unix"))]
unsafe extern "C" {
    fn OodleLZ_Decompress(
        compBuf: *const std::ffi::c_void,
        compBufSize: isize,
        rawBuf: *mut std::ffi::c_void,
        rawLen: isize,
        fuzzSafe: i32,
        checkCRC: i32,
        verbosity: i32,
        decBufBase: *mut std::ffi::c_void,
        decBufSize: isize,
        fpCallback: *const std::ffi::c_void,
        callbackUserData: *const std::ffi::c_void,
        decoderMemory: *mut std::ffi::c_void,
        decoderMemorySize: isize,
        threadPhase: i32,
    ) -> isize;

    fn OodleLZ_Compress(
        compressor: u32,
        rawBuf: *const std::ffi::c_void,
        rawLen: isize,
        compBuf: *mut std::ffi::c_void,
        level: u32,
        pOptions: *const std::ffi::c_void, /* *const OodleLZ_CompressOptions */
        dictionaryBase: *const std::ffi::c_void,
        lrm: *const std::ffi::c_void,
        scratchMem: *mut std::ffi::c_void,
        scratchSize: isize,
    ) -> isize;

    fn OodleLZ_GetCompressedBufferSizeNeeded(compressor: u32, rawSize: isize) -> isize;

    /*
    fn OodleLZ_GetCompressScratchMemBound(
        compressor: u32,
        level: u32,
        rawLen: isize,
        pOptions: *const std::ffi::c_void, /* *const OodleLZ_CompressOptions */
    ) -> isize;
    */
}

#[cfg(feature = "oodle")]
const DECOMPRESS_SCRATCH_BUFFER_SIZE: usize = 1024 * 1024;
#[cfg(feature = "oodle")]
const COMPRESS_SCRATCH_BUFFER_SIZE: usize = 8 * 1024 * 1024;

#[cfg(feature = "oodle")]
static COMPRESS_SCRATCH_BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "oodle")]
static DECOMPRESS_SCRATCH_BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "oodle")]
static SCRATCH_BUFFER_LIMIT: OnceLock<usize> = OnceLock::new();
#[cfg(feature = "oodle")]
static SCRATCH_BUFFER_HARD_LIMIT: usize = 256;

#[cfg(feature = "oodle")]
static COMPRESS_SCRATCH_BUFFER_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<BytesMut>> =
    OnceLock::new();
#[cfg(feature = "oodle")]
static DECOMPRESS_SCRATCH_BUFFER_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<BytesMut>> =
    OnceLock::new();

#[cfg(feature = "oodle")]
fn compress_scratch_buffer_queue() -> &'static crossbeam::queue::ArrayQueue<BytesMut> {
    COMPRESS_SCRATCH_BUFFER_QUEUE
        .get_or_init(|| crossbeam::queue::ArrayQueue::new(SCRATCH_BUFFER_HARD_LIMIT))
}

#[cfg(feature = "oodle")]
fn compress_scratch_buffer() -> BytesMut {
    let queue = compress_scratch_buffer_queue();
    if let Some(buffer) = queue.pop() {
        return buffer;
    }

    let limit = *SCRATCH_BUFFER_LIMIT.get_or_init(lore_base::runtime::default_worker_threads);
    let current = COMPRESS_SCRATCH_BUFFER_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    if current < limit {
        let current =
            COMPRESS_SCRATCH_BUFFER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if current < limit {
            return BytesMut::with_capacity(COMPRESS_SCRATCH_BUFFER_SIZE);
        }
        COMPRESS_SCRATCH_BUFFER_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    BytesMut::default()
}

#[cfg(feature = "oodle")]
fn compress_scratch_buffer_done(buffer: BytesMut) {
    if buffer.capacity() > 0 {
        let queue = compress_scratch_buffer_queue();
        let _ = queue.push(buffer);
    }
}

#[cfg(feature = "oodle")]
fn decompress_scratch_buffer_queue() -> &'static crossbeam::queue::ArrayQueue<BytesMut> {
    DECOMPRESS_SCRATCH_BUFFER_QUEUE
        .get_or_init(|| crossbeam::queue::ArrayQueue::new(SCRATCH_BUFFER_HARD_LIMIT))
}

#[cfg(feature = "oodle")]
fn decompress_scratch_buffer() -> BytesMut {
    let queue = decompress_scratch_buffer_queue();
    if let Some(buffer) = queue.pop() {
        return buffer;
    }

    let limit = *SCRATCH_BUFFER_LIMIT.get_or_init(lore_base::runtime::default_worker_threads);
    let current = DECOMPRESS_SCRATCH_BUFFER_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    if current < limit {
        let current =
            DECOMPRESS_SCRATCH_BUFFER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if current < limit {
            return BytesMut::with_capacity(DECOMPRESS_SCRATCH_BUFFER_SIZE);
        }
        DECOMPRESS_SCRATCH_BUFFER_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    BytesMut::default()
}

#[cfg(feature = "oodle")]
fn decompress_scratch_buffer_done(buffer: BytesMut) {
    if buffer.capacity() > 0 {
        let queue = decompress_scratch_buffer_queue();
        let _ = queue.push(buffer);
    }
}

// Zstd context pool, the counterpart to the Oodle scratch buffer pool above: Oodle
// takes a scratch pointer on every call and allocates nothing, and zstd does the
// same when the context is built inside a buffer the caller owns. The pooled item
// is therefore the workspace, and the context lives inside it.
//
// A context built in a fixed workspace cannot resize; zstd fails the compression
// rather than allocating. The workspace is sized for the worst case at the
// configured compression level, and an oversized workspace costs a static context
// nothing.

/// The largest input in each table of compression parameters a fragment can select.
///
/// zstd picks the table by how many of these an input falls under, so the largest
/// input in a table is also the costliest, and a fragment never exceeds the last of
/// them.
const ZSTD_PARAMETER_TABLE_SIZES: [usize; 3] = [16 * 1024, 128 * 1024, FRAGMENT_SIZE_THRESHOLD];

/// Bytes a compression context needs at `level` for any fragment.
///
/// The largest requirement across the tables above, because a smaller input can
/// select a costlier table than a larger one. Bounded by what a
/// [`FRAGMENT_SIZE_THRESHOLD`] window needs, where sizing for the level alone would
/// provision for an unbounded input and cost two orders of magnitude more at the
/// higher levels.
fn zstd_compress_workspace_size_for(level: std::ffi::c_int) -> usize {
    ZSTD_PARAMETER_TABLE_SIZES
        .into_iter()
        .map(|size| zstd_compress_workspace_size_at(level, size))
        .max()
        .unwrap_or_default()
}

/// Bytes a compression context needs at `level` for an input of `size`.
fn zstd_compress_workspace_size_at(level: std::ffi::c_int, size: usize) -> usize {
    // Safety: pure calculations over a level in the range zstd accepts.
    unsafe {
        let parameters = zstd_sys::ZSTD_getCParams(level, size as u64, 0);
        zstd_sys::ZSTD_estimateCCtxSize_usingCParams(parameters)
    }
}

/// Bytes a compression context needs at the configured compression level.
fn zstd_compress_workspace_size() -> usize {
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| zstd_compress_workspace_size_for(zstd_compression_level()))
}

/// A compression context and the workspace it was built in.
///
/// `u64` because [`zstd_sys::ZSTD_initStaticCCtx`] requires eight-byte alignment.
/// Dropping the workspace frees the context: a static context owns nothing outside
/// the buffer it lives in. `context` survives a move of this struct because the
/// buffer is heap-allocated and keeps its address.
struct ZstdCCtx {
    context: *mut zstd_sys::ZSTD_CCtx,
    _workspace: Vec<u64>,
    pooled: bool,
}
// Safety: ZSTD_CCtx is not accessed concurrently — the pool hands it to one thread at a time.
unsafe impl Send for ZstdCCtx {}

impl ZstdCCtx {
    /// The absence of a context, for when its workspace cannot be allocated. Call
    /// sites already test for a null context, so no further path is needed.
    fn none() -> Self {
        Self {
            context: std::ptr::null_mut(),
            _workspace: Vec::new(),
            pooled: false,
        }
    }
}

/// A decompression context and the workspace it was built in. See [`ZstdCCtx`].
struct ZstdDCtx {
    context: *mut zstd_sys::ZSTD_DCtx,
    _workspace: Vec<u64>,
    pooled: bool,
}
// Safety: ZSTD_DCtx is not accessed concurrently — the pool hands it to one thread at a time.
unsafe impl Send for ZstdDCtx {}

impl ZstdDCtx {
    /// The absence of a context. See [`ZstdCCtx::none`].
    fn none() -> Self {
        Self {
            context: std::ptr::null_mut(),
            _workspace: Vec::new(),
            pooled: false,
        }
    }
}

/// A buffer of at least `bytes`, aligned for a static zstd context, or `None` when
/// it cannot be allocated.
///
/// Left unwritten, and so the capacity carries the size while the length stays zero:
/// zstd initialises what it uses, and zeroing the buffer would cost a pass over
/// several megabytes for nothing. No slice is ever formed over it, so nothing in Rust
/// reads the uninitialised memory. A memory checker watching the C side will report
/// it as uninitialised all the same.
///
/// Fallible because a context is optional: a caller without one stores the fragment
/// uncompressed, which is preferable to the process abort an infallible allocation
/// raises on failure.
fn zstd_workspace(bytes: usize) -> Option<Vec<u64>> {
    let mut workspace = Vec::new();
    workspace
        .try_reserve_exact(bytes.div_ceil(size_of::<u64>()))
        .ok()?;
    Some(workspace)
}

/// Upper bound on the contexts kept for reuse.
///
/// The pool fills on demand: a context is built only when a caller finds the queue
/// empty, so a process that never compresses more than eight fragments at once holds
/// eight workspaces rather than this many. Concurrency beyond the bound builds a
/// context per call and frees it on release, which keeps resident workspace
/// independent of the core count at the cost of an allocation per call past it.
const ZSTD_CTX_POOL_LIMIT: usize = 32;

static ZSTD_COMPRESS_CTX_COUNT: AtomicUsize = AtomicUsize::new(0);
static ZSTD_DECOMPRESS_CTX_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Claims one of the [`ZSTD_CTX_POOL_LIMIT`] pooled slots, reporting whether the
/// caller's context is to be kept for reuse.
///
/// A claim is never released: pooled contexts live for the process, so `count` only
/// rises, and it reaching the limit is what makes further contexts transient.
///
/// The pairing is by convention rather than by construction. A claimed context that
/// is dropped without reaching its release costs its slot for the life of the
/// process, leaving the pool one context smaller; every path between a claim and its
/// release therefore has to reach that release.
fn zstd_claim_pooled_slot(count: &AtomicUsize) -> bool {
    let mut current = count.load(std::sync::atomic::Ordering::Relaxed);
    while current < ZSTD_CTX_POOL_LIMIT {
        match count.compare_exchange_weak(
            current,
            current + 1,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
    false
}

static ZSTD_COMPRESS_CTX_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<ZstdCCtx>> = OnceLock::new();
static ZSTD_DECOMPRESS_CTX_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<ZstdDCtx>> =
    OnceLock::new();

fn zstd_compress_ctx_queue() -> &'static crossbeam::queue::ArrayQueue<ZstdCCtx> {
    ZSTD_COMPRESS_CTX_QUEUE.get_or_init(|| crossbeam::queue::ArrayQueue::new(ZSTD_CTX_POOL_LIMIT))
}

fn zstd_compress_ctx() -> ZstdCCtx {
    let queue = zstd_compress_ctx_queue();
    if let Some(ctx) = queue.pop() {
        return ctx;
    }

    let Some(mut workspace) = zstd_workspace(zstd_compress_workspace_size()) else {
        return ZstdCCtx::none();
    };
    // Safety: the buffer is at least the estimated size and `u64`-aligned as
    // ZSTD_initStaticCCtx requires, and it outlives the context because the two are
    // dropped together. A null return is checked at the call sites.
    let context = unsafe {
        zstd_sys::ZSTD_initStaticCCtx(
            workspace.as_mut_ptr().cast::<std::ffi::c_void>(),
            workspace.capacity() * size_of::<u64>(),
        )
    };
    ZstdCCtx {
        context,
        _workspace: workspace,
        pooled: !context.is_null() && zstd_claim_pooled_slot(&ZSTD_COMPRESS_CTX_COUNT),
    }
}

/// Returns a context to the pool, or frees it when it holds no pooled slot.
///
/// The queue has one slot per claim, so pushing a pooled context cannot fail.
fn zstd_compress_ctx_done(ctx: ZstdCCtx) {
    if ctx.pooled {
        let _ = zstd_compress_ctx_queue().push(ctx);
    }
}

fn zstd_decompress_ctx_queue() -> &'static crossbeam::queue::ArrayQueue<ZstdDCtx> {
    ZSTD_DECOMPRESS_CTX_QUEUE.get_or_init(|| crossbeam::queue::ArrayQueue::new(ZSTD_CTX_POOL_LIMIT))
}

fn zstd_decompress_ctx() -> ZstdDCtx {
    let queue = zstd_decompress_ctx_queue();
    if let Some(ctx) = queue.pop() {
        return ctx;
    }

    // Safety: as `zstd_compress_ctx`, with the size zstd states for a
    // decompression context.
    let Some(mut workspace) = zstd_workspace(unsafe { zstd_sys::ZSTD_estimateDCtxSize() }) else {
        return ZstdDCtx::none();
    };
    let context = unsafe {
        zstd_sys::ZSTD_initStaticDCtx(
            workspace.as_mut_ptr().cast::<std::ffi::c_void>(),
            workspace.capacity() * size_of::<u64>(),
        )
    };
    ZstdDCtx {
        context,
        _workspace: workspace,
        pooled: !context.is_null() && zstd_claim_pooled_slot(&ZSTD_DECOMPRESS_CTX_COUNT),
    }
}

/// Returns a context to the pool, or frees it when it holds no pooled slot. See
/// [`zstd_compress_ctx_done`].
fn zstd_decompress_ctx_done(ctx: ZstdDCtx) {
    if ctx.pooled {
        let _ = zstd_decompress_ctx_queue().push(ctx);
    }
}

pub fn decompress(
    fragment: Fragment,
    compressed: &[u8],
) -> Result<(Fragment, BytesMut), FragmentError> {
    let output_buffer = BytesMut::with_capacity(fragment.size_content as usize);
    decompress_into(fragment, compressed, output_buffer)
}

/// Decompress a fragment into a caller-provided output buffer. The
/// buffer's capacity must be at least `fragment.size_content` bytes;
/// callers that do not want to size the buffer themselves should use
/// [`decompress`] which allocates it.
fn decompress_into(
    fragment: Fragment,
    compressed: &[u8],
    mut decompressed: BytesMut,
) -> Result<(Fragment, BytesMut), FragmentError> {
    if fragment.size_content as usize > FRAGMENT_SIZE_THRESHOLD
        || compressed.len() < fragment.size_payload as usize
        || decompressed.capacity() < fragment.size_content as usize
    {
        return Err(FragmentError::internal("fragment has invalid sizes"));
    }
    if (fragment.flags & FragmentFlags::PayloadCompressedLZ4) != 0 {
        lore_base::lore_trace!(
            "Decompress {} bytes to {} bytes with LZ4",
            fragment.size_payload,
            fragment.size_content,
        );

        // Safety: Buffer sizes are validated
        let decompressed_size = unsafe {
            lz4_sys::LZ4_decompress_safe(
                compressed.as_ptr().cast::<std::ffi::c_char>(),
                decompressed.as_mut_ptr().cast::<std::ffi::c_char>(),
                fragment.size_payload as std::ffi::c_int,
                decompressed.capacity() as std::ffi::c_int,
            )
        };
        if decompressed_size != fragment.size_content as i32 {
            lore_base::lore_debug!("LZ4 decompress failed: {}", decompressed_size);
            return Err(FragmentError::internal("invalid compressed data"));
        }
    } else if (fragment.flags & FragmentFlags::PayloadCompressedOodle2) != 0 {
        #[cfg(feature = "oodle")]
        {
            oodle_initialize();
            lore_base::lore_trace!(
                "Decompress {} bytes to {} bytes with Oodle",
                fragment.size_payload,
                fragment.size_content,
            );

            let mut scratch_buffer = decompress_scratch_buffer();

            let decompressed_size = unsafe {
                OodleLZ_Decompress(
                    compressed.as_ptr().cast::<std::ffi::c_void>(),
                    fragment.size_payload as isize,
                    decompressed.as_mut_ptr().cast::<std::ffi::c_void>(),
                    fragment.size_content as isize,
                    1, /* OodleLZ_FuzzSafe_Yes */
                    0, /* OodleLZ_CheckCRC_No */
                    0, /* OodleLZ_Verbosity_None */
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    if scratch_buffer.capacity() > 0 {
                        scratch_buffer.as_mut_ptr().cast::<std::ffi::c_void>()
                    } else {
                        std::ptr::null_mut()
                    },
                    scratch_buffer.capacity() as isize,
                    3, /* OodleLZ_Decode_Unthreaded */
                )
            };

            decompress_scratch_buffer_done(scratch_buffer);

            if decompressed_size != fragment.size_content as isize {
                lore_base::lore_debug!("Oodle decompress failed: {}", decompressed_size);
                return Err(FragmentError::internal("invalid compressed data"));
            }
        }
        #[cfg(not(feature = "oodle"))]
        {
            return Err(FragmentError::from(NotSupported {
                operation: "encountered an Oodle compressed fragment but this client was built without Oodle support".to_string(),
            }));
        }
    } else if (fragment.flags & FragmentFlags::PayloadCompressedZstd) != 0 {
        lore_base::lore_trace!(
            "Decompress {} bytes to {} bytes with Zstd",
            fragment.size_payload,
            fragment.size_content,
        );

        let ctx = zstd_decompress_ctx();
        if ctx.context.is_null() {
            return Err(FragmentError::internal("failed to allocate zstd context"));
        }
        // Safety: ctx.context is a valid non-null ZSTD_DCtx. Output buffer has capacity >= size_content.
        // Input slice length is validated against size_payload at function entry.
        let decompressed_size = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                ctx.context,
                decompressed.as_mut_ptr().cast::<std::ffi::c_void>(),
                fragment.size_content as usize,
                compressed.as_ptr().cast::<std::ffi::c_void>(),
                fragment.size_payload as usize,
            )
        };
        zstd_decompress_ctx_done(ctx);

        // Safety: Pure query on the return value, no pointer dereference.
        if unsafe { zstd_sys::ZSTD_isError(decompressed_size) } != 0
            || decompressed_size != fragment.size_content as usize
        {
            lore_base::lore_debug!("Zstd decompress failed: {}", decompressed_size);
            return Err(FragmentError::internal("invalid compressed data"));
        }
    } else {
        return Err(FragmentError::from(NotSupported {
            operation:
                "encountered a fragment with an unknown compression algorithm; update the client"
                    .to_string(),
        }));
    }

    // Safety: Decompression succeeded and wrote exactly size_content bytes.
    unsafe { decompressed.set_len(fragment.size_content as usize) };

    Ok((
        Fragment {
            flags: fragment.flags & !FragmentFlags::PayloadCompressed,
            size_payload: fragment.size_content as u32,
            size_content: fragment.size_content,
        },
        decompressed,
    ))
}

pub fn decompress_into_slice(
    fragment: Fragment,
    compressed: &[u8],
    decompressed: &mut [u8],
) -> Result<Fragment, FragmentError> {
    if fragment.size_content as usize > FRAGMENT_SIZE_THRESHOLD
        || decompressed.len() < fragment.size_content as usize
        || compressed.len() < fragment.size_payload as usize
    {
        return Err(FragmentError::internal("fragment has invalid sizes"));
    }
    if (fragment.flags & FragmentFlags::PayloadCompressedLZ4) != 0 {
        lore_base::lore_trace!(
            "Decompress {} bytes to {} bytes with LZ4",
            fragment.size_payload,
            fragment.size_content,
        );
        // Safety: Buffer sizes are validated
        let decompressed_size = unsafe {
            lz4_sys::LZ4_decompress_safe(
                compressed.as_ptr().cast::<std::ffi::c_char>(),
                decompressed.as_mut_ptr().cast::<std::ffi::c_char>(),
                fragment.size_payload as std::ffi::c_int,
                decompressed.len() as std::ffi::c_int,
            )
        };
        if decompressed_size != fragment.size_content as i32 {
            lore_base::lore_debug!("LZ4 decompress failed: {}", decompressed_size);
            return Err(FragmentError::internal("invalid compressed data"));
        }
    } else if (fragment.flags & FragmentFlags::PayloadCompressedOodle2) != 0 {
        #[cfg(feature = "oodle")]
        {
            oodle_initialize();
            lore_base::lore_trace!(
                "Decompress {} bytes to {} bytes with Oodle",
                fragment.size_payload,
                fragment.size_content,
            );

            let mut scratch_buffer = decompress_scratch_buffer();

            let decompressed_size = unsafe {
                OodleLZ_Decompress(
                    compressed.as_ptr().cast::<std::ffi::c_void>(),
                    fragment.size_payload as isize,
                    decompressed.as_mut_ptr().cast::<std::ffi::c_void>(),
                    fragment.size_content as isize,
                    1, /* OodleLZ_FuzzSafe_Yes */
                    0, /* OodleLZ_CheckCRC_No */
                    0, /* OodleLZ_Verbosity_None */
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    if scratch_buffer.capacity() > 0 {
                        scratch_buffer.as_mut_ptr().cast::<std::ffi::c_void>()
                    } else {
                        std::ptr::null_mut()
                    },
                    scratch_buffer.capacity() as isize,
                    3, /* OodleLZ_Decode_Unthreaded */
                )
            };

            decompress_scratch_buffer_done(scratch_buffer);

            if decompressed_size != fragment.size_content as isize {
                lore_base::lore_debug!("Oodle decompress failed: {}", decompressed_size);
                return Err(FragmentError::internal("invalid compressed data"));
            }
        }
        #[cfg(not(feature = "oodle"))]
        {
            return Err(FragmentError::from(NotSupported {
                operation: "encountered an Oodle compressed fragment but this client was built without Oodle support".to_string(),
            }));
        }
    } else if (fragment.flags & FragmentFlags::PayloadCompressedZstd) != 0 {
        lore_base::lore_trace!(
            "Decompress {} bytes to {} bytes with Zstd",
            fragment.size_payload,
            fragment.size_content,
        );

        let ctx = zstd_decompress_ctx();
        if ctx.context.is_null() {
            return Err(FragmentError::internal("failed to allocate zstd context"));
        }
        // Safety: ctx.context is a valid non-null ZSTD_DCtx. Output slice length is validated >= size_content.
        // Input slice length is validated against size_payload at function entry.
        let decompressed_size = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                ctx.context,
                decompressed.as_mut_ptr().cast::<std::ffi::c_void>(),
                fragment.size_content as usize,
                compressed.as_ptr().cast::<std::ffi::c_void>(),
                fragment.size_payload as usize,
            )
        };
        zstd_decompress_ctx_done(ctx);

        // Safety: Pure query on the return value, no pointer dereference.
        if unsafe { zstd_sys::ZSTD_isError(decompressed_size) } != 0
            || decompressed_size != fragment.size_content as usize
        {
            lore_base::lore_debug!("Zstd decompress failed: {}", decompressed_size);
            return Err(FragmentError::internal("invalid compressed data"));
        }
    } else {
        return Err(FragmentError::from(NotSupported {
            operation:
                "encountered a fragment with an unknown compression algorithm; update the client"
                    .to_string(),
        }));
    }

    Ok(Fragment {
        flags: fragment.flags & !FragmentFlags::PayloadCompressed,
        size_payload: fragment.size_content as u32,
        size_content: fragment.size_content,
    })
}

#[cfg(feature = "oodle")]
static COMPRESSION_LEVEL: OnceLock<u32> = OnceLock::new();

#[cfg(feature = "oodle")]
fn compression_level() -> u32 {
    *COMPRESSION_LEVEL.get_or_init(|| {
        if let Ok(level) = std::env::var("LORE_COMPRESSION_LEVEL")
            && let Ok(level) = level.parse::<u32>()
            && level < 10
        {
            level
        } else {
            3 /* OodleLZ_CompressionLevel_Fast */
        }
    })
}

/// Returns the maximum compressed size for the given payload length and
/// compression mode. Used to pre-allocate the output buffer for [`compress`].
///
/// Returns 0 for modes that will refuse to compress (`NoCompression`;
/// `Oodle` without the oodle feature). In those cases the compress call
/// will return an error and the zero-capacity buffer is harmless.
fn compress_bound(size_payload: usize, mode: CompressionMode) -> usize {
    match mode {
        CompressionMode::Lz4 => {
            // Safety: pure query returning worst-case size for the input length.
            unsafe { lz4_sys::LZ4_compressBound(size_payload as std::ffi::c_int) as usize }
        }
        CompressionMode::Zstd | CompressionMode::NotSpecified => {
            // Safety: pure query returning worst-case size for the input length.
            unsafe { zstd_sys::ZSTD_compressBound(size_payload) }
        }
        #[cfg(feature = "oodle")]
        CompressionMode::Oodle => {
            oodle_initialize();
            // Safety: pure query returning worst-case size for the input length.
            unsafe {
                OodleLZ_GetCompressedBufferSizeNeeded(8 /* Kraken */, size_payload as isize)
                    as usize
            }
        }
        #[cfg(not(feature = "oodle"))]
        CompressionMode::Oodle => 0,
        CompressionMode::NoCompression => 0,
    }
}

pub fn compress(
    fragment: Fragment,
    payload: &[u8],
    mode: CompressionMode,
) -> Result<(Fragment, Bytes), CompressFragmentError> {
    let output_buffer =
        BytesMut::with_capacity(compress_bound(fragment.size_payload as usize, mode));
    compress_into(fragment, payload, mode, output_buffer)
}

/// Compress a fragment into a caller-provided output buffer. The buffer's
/// capacity must be at least `compress_bound(fragment.size_payload, mode)`;
/// callers that do not know the correct bound should use [`compress`] which
/// sizes the buffer itself.
fn compress_into(
    fragment: Fragment,
    payload: &[u8],
    mode: CompressionMode,
    output_buffer: BytesMut,
) -> Result<(Fragment, Bytes), CompressFragmentError> {
    if fragment.size_content as usize > FRAGMENT_SIZE_THRESHOLD {
        return Err(CompressFragmentError::internal(
            "fragment has invalid sizes",
        ));
    }
    // Only try to compress previously uncompressed raw data buffers of more than 32 bytes
    // Fragment lists and below 32 byte buffers are always raw uncompressed
    if (fragment.flags & FragmentFlags::PayloadCompressed) != 0
        || (fragment.flags & FragmentFlags::PayloadFragmented) != 0
        || (fragment.size_payload as u64) != fragment.size_content
    {
        return Err(CompressFragmentError::internal(
            "fragment incompatible with compression",
        ));
    }

    if payload.len() < fragment.size_payload as usize {
        return Err(CompressFragmentError::internal(
            "fragment has invalid sizes",
        ));
    }

    match mode {
        CompressionMode::Lz4 => compress_lz4_impl(fragment, payload, output_buffer),
        CompressionMode::Zstd | CompressionMode::NotSpecified => {
            compress_zstd_impl(fragment, payload, output_buffer)
        }
        #[cfg(feature = "oodle")]
        CompressionMode::Oodle => compress_oodle_impl(fragment, payload, output_buffer),
        #[cfg(not(feature = "oodle"))]
        CompressionMode::Oodle => Err(CompressFragmentError::from(NotSupported {
            operation:
                "Oodle compression requested but this client was built without Oodle support"
                    .to_string(),
        })),
        CompressionMode::NoCompression => Err(CompressFragmentError::internal(
            "fragment compression disabled",
        )),
    }
}

#[cfg(feature = "oodle")]
fn compress_oodle_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), CompressFragmentError> {
    oodle_initialize();

    // Save at least 5% to be worth compressing
    let compressed_size_threshold = ((fragment.size_payload as usize) * 95) / 100;
    let compressor = 8 /* OodleLZ_Compressor_Kraken */;
    let level = compression_level();

    let mut scratch_buffer = compress_scratch_buffer();

    let compressed_size = unsafe {
        OodleLZ_Compress(
            compressor,
            payload.as_ptr().cast::<std::ffi::c_void>(),
            fragment.size_payload as isize,
            compressed_buffer.as_mut_ptr().cast::<std::ffi::c_void>(),
            level,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            if scratch_buffer.capacity() > 0 {
                scratch_buffer.as_mut_ptr().cast::<std::ffi::c_void>()
            } else {
                std::ptr::null_mut()
            },
            scratch_buffer.capacity() as isize,
        )
    };

    compress_scratch_buffer_done(scratch_buffer);

    if compressed_size > 0 && compressed_size < compressed_size_threshold as isize {
        unsafe {
            compressed_buffer.set_len(compressed_size as usize);
        }
        Ok((
            Fragment {
                flags: fragment.flags | FragmentFlags::PayloadCompressedOodle2,
                size_payload: compressed_size as u32,
                size_content: fragment.size_content,
            },
            compressed_buffer.freeze(),
        ))
    } else {
        Err(InefficientCompression.into())
    }
}

fn compress_lz4_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), CompressFragmentError> {
    // Save at least 5% to be worth compressing
    let compressed_size_threshold = ((fragment.size_payload as usize) * 95) / 100;

    // Safety: Buffer capacity was sized by the caller via compress_bound().
    let compressed_size = unsafe {
        lz4_sys::LZ4_compress_default(
            payload.as_ptr().cast::<std::ffi::c_char>(),
            compressed_buffer.as_mut_ptr().cast::<std::ffi::c_char>(),
            fragment.size_payload as std::ffi::c_int,
            compressed_buffer.capacity() as std::ffi::c_int,
        )
    };

    if compressed_size > 0 && (compressed_size as usize) < compressed_size_threshold {
        // Safety: Buffer size is validated
        unsafe {
            compressed_buffer.set_len(compressed_size as usize);
        }
        Ok((
            Fragment {
                flags: fragment.flags | FragmentFlags::PayloadCompressedLZ4,
                size_payload: compressed_size as u32,
                size_content: fragment.size_content,
            },
            compressed_buffer.freeze(),
        ))
    } else {
        Err(InefficientCompression.into())
    }
}

static ZSTD_COMPRESSION_LEVEL: OnceLock<std::ffi::c_int> = OnceLock::new();

fn zstd_compression_level() -> std::ffi::c_int {
    *ZSTD_COMPRESSION_LEVEL.get_or_init(|| {
        if let Ok(level) = std::env::var("LORE_COMPRESSION_LEVEL")
            && let Ok(level) = level.parse::<std::ffi::c_int>()
            && (1..=22).contains(&level)
        {
            level
        } else {
            6
        }
    })
}

/// The name zstd gives a return code.
fn zstd_error_name(code: usize) -> std::borrow::Cow<'static, str> {
    // Safety: ZSTD_getErrorName returns a static nul-terminated string for any code.
    unsafe { std::ffi::CStr::from_ptr(zstd_sys::ZSTD_getErrorName(code)) }.to_string_lossy()
}

/// What a failed zstd compression reports. The cause goes to the log, where it is
/// stated once, rather than into the error, which every caller discards.
const ZSTD_COMPRESS_FAILED: &str = "zstd compression failed";

/// The error a failed zstd compression maps to, reported the first time one occurs.
///
/// Distinct from [`InefficientCompression`], which states that the content would not
/// shrink: a context sized by [`zstd_compress_workspace_size`] cannot run out of
/// workspace, so a failure here is a broken invariant and not a property of the
/// content. The caller stores the fragment uncompressed either way, and discards
/// this error, so the report is the only trace such a failure leaves. Reported once
/// because its cause persists for every fragment that follows.
fn zstd_compress_failure(reason: &str) -> CompressFragmentError {
    static REPORTED: Once = Once::new();
    REPORTED.call_once(|| lore_base::lore_warn!("zstd compression failed: {reason}"));
    CompressFragmentError::internal(ZSTD_COMPRESS_FAILED)
}

fn compress_zstd_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), CompressFragmentError> {
    // Save at least 5% to be worth compressing
    let compressed_size_threshold = ((fragment.size_payload as usize) * 95) / 100;

    let ctx = zstd_compress_ctx();
    if ctx.context.is_null() {
        return Err(zstd_compress_failure("no context"));
    }
    // Safety: ctx.context is a valid non-null ZSTD_CCtx. Buffer capacity was sized
    // by the caller via compress_bound(). Input payload length is validated
    // against size_payload at function entry.
    let compressed_size = unsafe {
        zstd_sys::ZSTD_compressCCtx(
            ctx.context,
            compressed_buffer.as_mut_ptr().cast::<std::ffi::c_void>(),
            compressed_buffer.capacity(),
            payload.as_ptr().cast::<std::ffi::c_void>(),
            fragment.size_payload as usize,
            zstd_compression_level(),
        )
    };
    zstd_compress_ctx_done(ctx);

    // Safety: Pure query on the return value, no pointer dereference.
    if unsafe { zstd_sys::ZSTD_isError(compressed_size) } != 0 {
        return Err(zstd_compress_failure(&zstd_error_name(compressed_size)));
    }

    if compressed_size < compressed_size_threshold {
        // Safety: ZSTD_compressCCtx succeeded, compressed_size bytes were written.
        unsafe {
            compressed_buffer.set_len(compressed_size);
        }
        Ok((
            Fragment {
                flags: fragment.flags | FragmentFlags::PayloadCompressedZstd,
                size_payload: compressed_size as u32,
                size_content: fragment.size_content,
            },
            compressed_buffer.freeze(),
        ))
    } else {
        Err(InefficientCompression.into())
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::FRAGMENT_SIZE_EXPECTED;

    use super::*;

    /// Compressible bytes, with enough structure that zstd beats the 5% threshold
    /// at every size below and a shape that does not depend on the length.
    fn payload(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| {
                let word = index / 37;
                ((word * 31 + index % 37) % 251) as u8
            })
            .collect()
    }

    fn raw_fragment(length: usize) -> Fragment {
        Fragment {
            flags: 0,
            size_payload: length as u32,
            size_content: length as u64,
        }
    }

    /// Bytes with no repetition for a window to find and a flat distribution for the
    /// entropy coder, so there is nothing for any encoder to save. splitmix64 over a
    /// counter.
    fn incompressible_payload(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| {
                let mut value = (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                (value ^ (value >> 31)) as u8
            })
            .collect()
    }

    /// Whether a payload of `length` bytes is too small for any encoder to save 5%
    /// on, in which case refusing it is correct and there is nothing to round trip.
    fn below_compressible_size(length: usize) -> bool {
        length < 1024
    }

    /// Fragment lengths spanning the ranges zstd keys its compression parameters on,
    /// both sides of every boundary in [`ZSTD_PARAMETER_TABLE_SIZES`] and both ends
    /// of the permitted range.
    const FRAGMENT_LENGTHS: &[usize] = &[
        1,
        17,
        64,
        FRAGMENT_COMPRESS_SIZE_LIMIT + 1,
        1024,
        4 * 1024,
        16 * 1024 - 1,
        16 * 1024,
        16 * 1024 + 1,
        32 * 1024,
        FRAGMENT_SIZE_EXPECTED,
        100 * 1024,
        128 * 1024 - 1,
        128 * 1024,
        128 * 1024 + 1,
        FRAGMENT_SIZE_THRESHOLD - 1,
        FRAGMENT_SIZE_THRESHOLD,
    ];

    /// Every fragment size has to survive the round trip through the pooled context
    /// that [`compress`] uses.
    #[test]
    fn zstd_round_trips_every_fragment_size() {
        for &length in FRAGMENT_LENGTHS {
            let source = payload(length);
            let Ok((compressed_fragment, compressed)) = compress_zstd_impl(
                raw_fragment(length),
                source.as_slice(),
                BytesMut::with_capacity(compress_bound(length, CompressionMode::Zstd)),
            ) else {
                assert!(
                    below_compressible_size(length),
                    "{length} bytes should have compressed"
                );
                continue;
            };

            assert_eq!(compressed_fragment.size_content, length as u64);
            assert!(
                (compressed_fragment.flags & FragmentFlags::PayloadCompressedZstd) != 0,
                "{length} bytes was not marked as zstd"
            );
            assert_eq!(compressed.len(), compressed_fragment.size_payload as usize);

            let (_, decompressed) = decompress(compressed_fragment, compressed.as_ref())
                .unwrap_or_else(|err| panic!("{length} bytes failed to decompress: {err:?}"));
            assert_eq!(
                decompressed.as_ref(),
                source.as_slice(),
                "{length} bytes did not round trip"
            );
        }
    }

    /// Decompression is handed `Fragment::size_content` and an output buffer, and
    /// must fill it from a context that cannot allocate either.
    #[test]
    fn zstd_decompresses_from_the_fragment_header_alone() {
        let length = FRAGMENT_SIZE_EXPECTED;
        let source = payload(length);
        let (compressed_fragment, compressed) = compress_zstd_impl(
            raw_fragment(length),
            source.as_slice(),
            BytesMut::with_capacity(compress_bound(length, CompressionMode::Zstd)),
        )
        .expect("compresses");

        let mut into = vec![0u8; length];
        decompress_into_slice(
            compressed_fragment,
            compressed.as_ref(),
            into.as_mut_slice(),
        )
        .expect("decompresses");
        assert_eq!(into, source);
    }

    /// Fragment sizes fine enough to find every size at which zstd changes its
    /// compression parameters: a stride below the smallest interval between changes,
    /// and each power of two with its neighbours, which is where they fall.
    fn fragment_size_scan() -> Vec<usize> {
        let mut sizes: Vec<usize> = (0..=18)
            .flat_map(|power: u32| {
                let size = 1usize << power;
                [size - 1, size, size + 1]
            })
            .filter(|size| (1..=FRAGMENT_SIZE_THRESHOLD).contains(size))
            .collect();

        let mut size = 1;
        while size <= FRAGMENT_SIZE_THRESHOLD {
            sizes.push(size);
            size += 64;
        }
        sizes.push(FRAGMENT_SIZE_THRESHOLD);
        sizes
    }

    /// The bound has to be the largest requirement across every fragment size, not
    /// only across [`ZSTD_PARAMETER_TABLE_SIZES`]. Those are the boundaries of zstd's
    /// internal parameter tables, which no header states, so a release that moves them
    /// has to fail here rather than in a silent loss of compression.
    #[test]
    fn the_workspace_bound_is_the_largest_over_every_fragment_size() {
        let scan = fragment_size_scan();
        for level in 1..=22 {
            let (worst, at) = scan
                .iter()
                .map(|&size| (zstd_compress_workspace_size_at(level, size), size))
                .max()
                .expect("the scan is not empty");

            assert_eq!(
                zstd_compress_workspace_size_for(level),
                worst,
                "level {level}: {at} bytes needs {worst}, which no size in \
                 ZSTD_PARAMETER_TABLE_SIZES accounts for"
            );
        }
    }

    /// The workspace has to hold a context and compress every fragment size at every
    /// level the configuration accepts, not only the default: zstd selects its
    /// parameters from one of several tables keyed on the size of the input, so a
    /// smaller fragment can demand more workspace than a larger one, and an
    /// undersized workspace fails the compression rather than growing.
    #[test]
    fn the_workspace_estimate_holds_at_every_level() {
        for level in 1..=22 {
            let bytes = zstd_compress_workspace_size_for(level);
            let mut workspace = zstd_workspace(bytes).expect("allocates a workspace");
            // Safety: the buffer is the estimated size and `u64`-aligned.
            let context = unsafe {
                zstd_sys::ZSTD_initStaticCCtx(
                    workspace.as_mut_ptr().cast::<std::ffi::c_void>(),
                    workspace.capacity() * size_of::<u64>(),
                )
            };
            assert!(
                !context.is_null(),
                "level {level}: {bytes} bytes did not hold a context"
            );

            for &length in FRAGMENT_LENGTHS {
                let source = payload(length);
                let mut destination = vec![0u8; compress_bound(length, CompressionMode::Zstd)];
                // Safety: the context is non-null, the destination holds the bound
                // for `length` bytes, and the source is `length` bytes long.
                let compressed_size = unsafe {
                    zstd_sys::ZSTD_compressCCtx(
                        context,
                        destination.as_mut_ptr().cast::<std::ffi::c_void>(),
                        destination.len(),
                        source.as_ptr().cast::<std::ffi::c_void>(),
                        length,
                        level,
                    )
                };
                // Safety: Pure query on the return value, no pointer dereference.
                assert!(
                    unsafe { zstd_sys::ZSTD_isError(compressed_size) } == 0,
                    "level {level}, {length} bytes: {} in {bytes} bytes of workspace",
                    zstd_error_name(compressed_size)
                );
            }
        }
    }

    /// The decompression estimate has to hold a context. Unlike compression its size
    /// depends on neither the input nor the level.
    #[test]
    fn the_decompress_workspace_estimate_builds_a_context() {
        // Safety: a pure calculation with no arguments.
        let bytes = unsafe { zstd_sys::ZSTD_estimateDCtxSize() };
        let mut workspace = zstd_workspace(bytes).expect("allocates a workspace");
        // Safety: the buffer is the estimated size and `u64`-aligned.
        let context = unsafe {
            zstd_sys::ZSTD_initStaticDCtx(
                workspace.as_mut_ptr().cast::<std::ffi::c_void>(),
                workspace.capacity() * size_of::<u64>(),
            )
        };
        assert!(
            !context.is_null(),
            "{bytes} bytes did not hold a decompression context"
        );
    }

    /// The pool stops claiming slots at its limit, which is what keeps resident
    /// workspace bounded independently of the core count.
    #[test]
    fn the_pool_claims_at_most_its_limit() {
        let count = AtomicUsize::new(0);
        let claimed = (0..ZSTD_CTX_POOL_LIMIT + 8)
            .filter(|_| zstd_claim_pooled_slot(&count))
            .count();

        assert_eq!(claimed, ZSTD_CTX_POOL_LIMIT);
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            ZSTD_CTX_POOL_LIMIT
        );
    }

    /// A context built past the pool limit is a working context; only its fate on
    /// release differs, and releasing it must not disturb the pool.
    #[test]
    fn contexts_past_the_pool_limit_are_usable() {
        let contexts: Vec<ZstdDCtx> = (0..ZSTD_CTX_POOL_LIMIT + 4)
            .map(|_| zstd_decompress_ctx())
            .collect();

        assert!(contexts.iter().all(|ctx| !ctx.context.is_null()));
        assert!(
            contexts.iter().filter(|ctx| !ctx.pooled).count() >= 4,
            "the pool kept more contexts than its limit"
        );

        for ctx in contexts {
            zstd_decompress_ctx_done(ctx);
        }

        let ctx = zstd_decompress_ctx();
        assert!(
            !ctx.context.is_null(),
            "the pool did not survive releasing transient contexts"
        );
        zstd_decompress_ctx_done(ctx);
    }

    /// Content that would not shrink is refused as [`InefficientCompression`], not as
    /// a zstd failure: the two are distinct so that a context too small for its input
    /// cannot pass for content that does not compress.
    #[test]
    fn zstd_refuses_content_it_cannot_shrink() {
        let length = 16 * 1024;
        let source = incompressible_payload(length);

        let result = compress_zstd_impl(
            raw_fragment(length),
            source.as_slice(),
            BytesMut::with_capacity(compress_bound(length, CompressionMode::Zstd)),
        );

        assert!(
            matches!(
                result,
                Err(CompressFragmentError::InefficientCompression(_))
            ),
            "incompressible content was not refused as inefficient"
        );
    }

    /// A zstd failure is an internal error rather than [`InefficientCompression`], so
    /// that a context too small for its input cannot pass for content that does not
    /// compress and be stored uncompressed in silence.
    #[test]
    fn a_zstd_failure_is_not_inefficient_compression() {
        let source = payload(4 * 1024);
        let mut destination = [0u8; 8];
        let ctx = zstd_compress_ctx();
        assert!(!ctx.context.is_null());

        // Safety: the context is non-null and both buffers are valid for the lengths
        // given. The destination is deliberately too small to hold a frame.
        let code = unsafe {
            zstd_sys::ZSTD_compressCCtx(
                ctx.context,
                destination.as_mut_ptr().cast::<std::ffi::c_void>(),
                destination.len(),
                source.as_ptr().cast::<std::ffi::c_void>(),
                source.len(),
                zstd_compression_level(),
            )
        };
        zstd_compress_ctx_done(ctx);

        // Safety: Pure query on the return value, no pointer dereference.
        assert!(unsafe { zstd_sys::ZSTD_isError(code) } != 0);
        assert!(zstd_compress_failure(&zstd_error_name(code)).is_internal());
    }
}
