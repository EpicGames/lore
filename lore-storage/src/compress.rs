// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

#[cfg(feature = "oodle")]
use std::alloc::Layout;
use std::ops::RangeInclusive;
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// The codec a payload is compressed with before it is stored.
///
/// Every stored fragment records the codec it was written with, so a mode selected here decides
/// what later writes use and never what already stored content is read back as.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(bound(deserialize = "'de: 'static"))]
pub enum CompressionMode {
    /// No mode was selected; the built-in default applies, which is Zstd.
    NotSpecified = 0,
    /// Store payloads verbatim, compressing nothing.
    NoCompression = 1,
    /// LZ4, which costs less to compress than Zstd and stores more bytes.
    Lz4 = 2,
    /// Oodle, deprecated and refused: a write under this mode fails.
    Oodle = 3,
    /// Zstandard, at the configured level.
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

/// The mode `value` names, if it names one a payload can be written under.
///
/// Read on the value rather than through [`CompressionMode::from_u32`], which answers
/// `NotSpecified` for anything it does not recognize: a caller that passed a typo would be told
/// the default had been selected for it. `Oodle` answers `None` with the values no variant
/// carries, [`compress`] refusing that mode as deprecated whether or not the `oodle` feature is
/// compiled in, so taking it would only move the failure to the first write.
pub fn writable_compression_mode(value: u32) -> Option<CompressionMode> {
    const NOT_SPECIFIED: u32 = CompressionMode::NotSpecified as u32;
    const NO_COMPRESSION: u32 = CompressionMode::NoCompression as u32;
    const LZ4: u32 = CompressionMode::Lz4 as u32;
    const ZSTD: u32 = CompressionMode::Zstd as u32;

    match value {
        NOT_SPECIFIED => Some(CompressionMode::NotSpecified),
        NO_COMPRESSION => Some(CompressionMode::NoCompression),
        LZ4 => Some(CompressionMode::Lz4),
        ZSTD => Some(CompressionMode::Zstd),
        _ => None,
    }
}

/// Offers `mode` as the mode payloads are compressed under, reporting whether it was taken.
///
/// This is how a preference a server states is applied. A mode already selected in this process
/// outranks it, whether that selection came from the API or from a server reached earlier, and
/// `NotSpecified` states no preference at all.
pub fn suggest_compression_mode(mode: CompressionMode) -> bool {
    apply_mode_suggestion(&COMPRESSION_MODE, mode)
}

/// Selects `mode` in `selection` unless one has already been made there.
#[lore_macro::test_pub]
fn apply_mode_suggestion(selection: &AtomicU32, mode: CompressionMode) -> bool {
    if matches!(mode, CompressionMode::NotSpecified) {
        return false;
    }
    selection
        .compare_exchange(
            CompressionMode::NotSpecified as u32,
            mode as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
}

/// Selects the level each codec defaults to, no level having been chosen for it.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = -1;

/// The levels zstd accepts.
#[lore_macro::test_pub]
const ZSTD_LEVELS: RangeInclusive<i32> = 1..=22;

/// The level zstd compresses at where nothing configures one.
const ZSTD_DEFAULT_LEVEL: i32 = 6;

/// The levels Oodle accepts.
#[cfg(feature = "oodle")]
const OODLE_LEVELS: RangeInclusive<i32> = 0..=9;

/// The level Oodle compresses at where nothing configures one, `OodleLZ_CompressionLevel_Fast`.
#[cfg(feature = "oodle")]
const OODLE_DEFAULT_LEVEL: i32 = 3;

/// Selects the level the codecs compress at, reporting whether the selection is the one in force.
///
/// Each codec fixes its level once, the workspace every compression after the first is built in
/// being sized for it. The level is installed here rather than left for that first compression to
/// resolve, so a payload racing this call either finds the selection or fixes the level ahead of
/// it, with no reading of a level this call is about to replace. `false` reports that a payload
/// fixed the level first, leaving the selection deciding nothing.
///
/// [`DEFAULT_COMPRESSION_LEVEL`] installs the level each codec defaults to.
pub fn set_compression_level(level: i32) -> bool {
    let selected = selected_level(level);
    #[cfg(feature = "oodle")]
    let _ = OODLE_COMPRESSION_LEVEL.set(oodle_level(selected));
    ZSTD_COMPRESSION_LEVEL.set(zstd_level(selected)).is_ok()
}

/// The level `level` selects, [`DEFAULT_COMPRESSION_LEVEL`] selecting none.
#[lore_macro::test_pub]
fn selected_level(level: i32) -> Option<i32> {
    (level != DEFAULT_COMPRESSION_LEVEL).then_some(level)
}

/// The level zstd compresses at, given what was selected for the codecs.
fn zstd_level(selected: Option<i32>) -> std::ffi::c_int {
    accepted_compression_level(environment_compression_level(), selected, ZSTD_LEVELS)
        .unwrap_or(ZSTD_DEFAULT_LEVEL) as std::ffi::c_int
}

/// The level Oodle compresses at, given what was selected for the codecs.
#[cfg(feature = "oodle")]
fn oodle_level(selected: Option<i32>) -> u32 {
    accepted_compression_level(environment_compression_level(), selected, OODLE_LEVELS)
        .unwrap_or(OODLE_DEFAULT_LEVEL) as u32
}

/// The level `LORE_COMPRESSION_LEVEL` names, whether or not a codec accepts it.
fn environment_compression_level() -> Option<i32> {
    std::env::var("LORE_COMPRESSION_LEVEL").ok()?.parse().ok()
}

/// Resolves the level a codec accepting `range` compresses at from the two sources that name one.
///
/// `environment` outranks `selected`, so an operator can pin the level of a build that selects one
/// of its own, and is passed over where it names a level outside `range`. A selection is clamped
/// into `range` instead: one setting serves every codec and each accepts its own, so a level meant
/// for one is brought within the other's rather than dropped for it.
#[lore_macro::test_pub]
fn accepted_compression_level(
    environment: Option<i32>,
    selected: Option<i32>,
    range: RangeInclusive<i32>,
) -> Option<i32> {
    environment
        .filter(|level| range.contains(level))
        .or_else(|| selected.map(|level| level.clamp(*range.start(), *range.end())))
}

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
static COMPRESS_SCRATCH_BUFFER_QUEUE: OnceLock<crossbeam_queue::ArrayQueue<BytesMut>> =
    OnceLock::new();
#[cfg(feature = "oodle")]
static DECOMPRESS_SCRATCH_BUFFER_QUEUE: OnceLock<crossbeam_queue::ArrayQueue<BytesMut>> =
    OnceLock::new();

#[cfg(feature = "oodle")]
fn compress_scratch_buffer_queue() -> &'static crossbeam_queue::ArrayQueue<BytesMut> {
    COMPRESS_SCRATCH_BUFFER_QUEUE
        .get_or_init(|| crossbeam_queue::ArrayQueue::new(SCRATCH_BUFFER_HARD_LIMIT))
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
fn decompress_scratch_buffer_queue() -> &'static crossbeam_queue::ArrayQueue<BytesMut> {
    DECOMPRESS_SCRATCH_BUFFER_QUEUE
        .get_or_init(|| crossbeam_queue::ArrayQueue::new(SCRATCH_BUFFER_HARD_LIMIT))
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
#[lore_macro::test_pub]
fn zstd_compress_workspace_size_for(level: std::ffi::c_int) -> usize {
    ZSTD_PARAMETER_TABLE_SIZES
        .into_iter()
        .map(|size| zstd_compress_workspace_size_at(level, size))
        .max()
        .unwrap_or_default()
}

/// Bytes a compression context needs at `level` for an input of `size`.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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

static ZSTD_COMPRESS_CTX_QUEUE: OnceLock<crossbeam_queue::ArrayQueue<ZstdCCtx>> = OnceLock::new();
static ZSTD_DECOMPRESS_CTX_QUEUE: OnceLock<crossbeam_queue::ArrayQueue<ZstdDCtx>> = OnceLock::new();

fn zstd_compress_ctx_queue() -> &'static crossbeam_queue::ArrayQueue<ZstdCCtx> {
    ZSTD_COMPRESS_CTX_QUEUE.get_or_init(|| crossbeam_queue::ArrayQueue::new(ZSTD_CTX_POOL_LIMIT))
}

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
fn zstd_compress_ctx_done(ctx: ZstdCCtx) {
    if ctx.pooled {
        let _ = zstd_compress_ctx_queue().push(ctx);
    }
}

fn zstd_decompress_ctx_queue() -> &'static crossbeam_queue::ArrayQueue<ZstdDCtx> {
    ZSTD_DECOMPRESS_CTX_QUEUE.get_or_init(|| crossbeam_queue::ArrayQueue::new(ZSTD_CTX_POOL_LIMIT))
}

#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
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
    #[cfg(feature = "test-util")]
    crate::test_util::count_decode();
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
    #[cfg(feature = "test-util")]
    crate::test_util::count_decode();
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
static OODLE_COMPRESSION_LEVEL: OnceLock<u32> = OnceLock::new();

/// The Oodle level, fixed by whichever of the first payload compressed and
/// [`set_compression_level`] reaches it first.
#[cfg(feature = "oodle")]
fn oodle_compression_level() -> u32 {
    *OODLE_COMPRESSION_LEVEL.get_or_init(|| oodle_level(None))
}

/// Returns the maximum compressed size for the given payload length and
/// compression mode. Used to pre-allocate the output buffer for [`compress`].
///
/// Returns 0 for modes that will refuse to compress (`NoCompression`;
/// `Oodle` without the oodle feature). In those cases the compress call
/// will return an error and the zero-capacity buffer is harmless.
#[lore_macro::test_pub]
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
    // put deprecated compression mode guards here
    if matches!(mode, CompressionMode::Oodle) {
        return Err(CompressFragmentError::from(NotSupported {
            operation: "Oodle compression requested but this mode is being deprecated".to_string(),
        }));
    }
    compress_without_deprecation_checks(fragment, payload, mode)
}

pub fn compress_without_deprecation_checks(
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
    let level = oodle_compression_level();

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

/// The zstd level, fixed by whichever of the first payload compressed and
/// [`set_compression_level`] reaches it first.
#[lore_macro::test_pub]
fn zstd_compression_level() -> std::ffi::c_int {
    *ZSTD_COMPRESSION_LEVEL.get_or_init(|| zstd_level(None))
}

/// The name zstd gives a return code.
#[lore_macro::test_pub]
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
#[lore_macro::test_pub]
fn zstd_compress_failure(reason: &str) -> CompressFragmentError {
    static REPORTED: Once = Once::new();
    REPORTED.call_once(|| lore_base::lore_warn!("zstd compression failed: {reason}"));
    CompressFragmentError::internal(ZSTD_COMPRESS_FAILED)
}

#[lore_macro::test_pub]
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
