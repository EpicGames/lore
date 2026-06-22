// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

use std::alloc::Layout;
#[cfg(feature = "oodle")]
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU32;
#[cfg(feature = "oodle")]
use std::sync::atomic::AtomicUsize;

use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;
use serde::Deserialize;

use crate::Fragment;
use crate::FragmentFlags;
use crate::errors::NotSupported;

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
            1 => Self::NoCompression,
            2 => Self::Lz4,
            3 => Self::Oodle,
            4 => Self::Zstd,
            _ => Self::NotSpecified,
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

    let limit = *SCRATCH_BUFFER_LIMIT.get_or_init(lore_base::runtime::compute_pool_thread_count);
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

    let limit = *SCRATCH_BUFFER_LIMIT.get_or_init(lore_base::runtime::compute_pool_thread_count);
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

// Zstd custom allocator — routes all zstd internal allocations through std::alloc (the Rust
// global allocator) instead of system malloc, matching the Oodle allocator pattern.
// A usize header is stored before each allocation to track the layout for dealloc.
const ZSTD_ALLOC_HEADER: usize = size_of::<usize>().next_multiple_of(align_of::<usize>());

// Safety: Allocates via std::alloc::alloc. The returned pointer is offset past the header
// and is usize-aligned. Returns null on failure.
unsafe extern "C" fn zstd_alloc(
    _opaque: *mut core::ffi::c_void,
    size: usize,
) -> *mut core::ffi::c_void {
    let total = ZSTD_ALLOC_HEADER + size;
    let Ok(layout) = Layout::from_size_align(total, ZSTD_ALLOC_HEADER) else {
        return core::ptr::null_mut();
    };

    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }

    unsafe {
        raw.cast::<usize>().write(total);
        raw.add(ZSTD_ALLOC_HEADER).cast()
    }
}

// Safety: Reads the header to recover the layout, then frees via std::alloc::dealloc.
// Handles null (zstd may call free with null on error paths).
unsafe extern "C" fn zstd_free(_opaque: *mut core::ffi::c_void, address: *mut core::ffi::c_void) {
    if address.is_null() {
        return;
    }
    unsafe {
        let raw = address.cast::<u8>().sub(ZSTD_ALLOC_HEADER);
        let total = raw.cast::<usize>().read();
        let layout = Layout::from_size_align_unchecked(total, ZSTD_ALLOC_HEADER);
        std::alloc::dealloc(raw, layout);
    }
}

const ZSTD_CUSTOM_MEM: zstd_sys::ZSTD_customMem = zstd_sys::ZSTD_customMem {
    customAlloc: Some(zstd_alloc),
    customFree: Some(zstd_free),
    opaque: std::ptr::null_mut(),
};

// Zstd context pool. ZSTD_CCtx/DCtx retain their internal workspace buffers (~1-2 MB for
// compression, ~130 KB for decompression) between calls. Pooling the contexts avoids repeated
// malloc/free of these workspaces on every compress/decompress — ZSTD_compressCCtx reuses the
// existing workspace when parameters are compatible. This is the zstd equivalent of the Oodle
// scratch buffer pool: Oodle takes a raw scratch pointer each call, while zstd manages workspace
// internally inside the context, so the context itself is what we pool.
struct ZstdCCtx(*mut zstd_sys::ZSTD_CCtx);
// Safety: ZSTD_CCtx is not accessed concurrently — the pool hands it to one thread at a time.
unsafe impl Send for ZstdCCtx {}

impl Drop for ZstdCCtx {
    fn drop(&mut self) {
        // Safety: self.0 is a valid ZSTD_CCtx from ZSTD_createCCtx, freed exactly once via Drop.
        unsafe {
            zstd_sys::ZSTD_freeCCtx(self.0);
        }
    }
}

struct ZstdDCtx(*mut zstd_sys::ZSTD_DCtx);
// Safety: ZSTD_DCtx is not accessed concurrently — the pool hands it to one thread at a time.
unsafe impl Send for ZstdDCtx {}

impl Drop for ZstdDCtx {
    fn drop(&mut self) {
        // Safety: self.0 is a valid ZSTD_DCtx from ZSTD_createDCtx, freed exactly once via Drop.
        unsafe {
            zstd_sys::ZSTD_freeDCtx(self.0);
        }
    }
}

static ZSTD_COMPRESS_CTX_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<ZstdCCtx>> = OnceLock::new();
static ZSTD_DECOMPRESS_CTX_QUEUE: OnceLock<crossbeam::queue::ArrayQueue<ZstdDCtx>> =
    OnceLock::new();

fn zstd_compress_ctx_queue() -> &'static crossbeam::queue::ArrayQueue<ZstdCCtx> {
    // Queue capacity = thread count. Push is atomic — overflow contexts from bursts
    // fail to push and drop immediately, so the pool can't grow beyond thread count.
    ZSTD_COMPRESS_CTX_QUEUE.get_or_init(|| {
        crossbeam::queue::ArrayQueue::new(lore_base::runtime::compute_pool_thread_count())
    })
}

fn zstd_compress_ctx() -> ZstdCCtx {
    let queue = zstd_compress_ctx_queue();
    if let Some(ctx) = queue.pop() {
        return ctx;
    }

    // Safety: ZSTD_createCCtx_advanced returns a valid pointer or null.
    // Null is checked at call sites before any dereference; ZSTD_freeCCtx(null) is a no-op.
    // Uses ZSTD_CUSTOM_MEM so all internal allocations go through std::alloc, not system malloc.
    ZstdCCtx(unsafe { zstd_sys::ZSTD_createCCtx_advanced(ZSTD_CUSTOM_MEM) })
}

fn zstd_compress_ctx_done(ctx: ZstdCCtx) {
    let queue = zstd_compress_ctx_queue();
    // Queue capacity bounds the pool. Overflow contexts drop and free themselves.
    let _ = queue.push(ctx);
}

fn zstd_decompress_ctx_queue() -> &'static crossbeam::queue::ArrayQueue<ZstdDCtx> {
    ZSTD_DECOMPRESS_CTX_QUEUE.get_or_init(|| {
        crossbeam::queue::ArrayQueue::new(lore_base::runtime::compute_pool_thread_count())
    })
}

fn zstd_decompress_ctx() -> ZstdDCtx {
    let queue = zstd_decompress_ctx_queue();
    if let Some(ctx) = queue.pop() {
        return ctx;
    }

    // Safety: ZSTD_createDCtx_advanced returns a valid pointer or null.
    // Null is checked at call sites before any dereference; ZSTD_freeDCtx(null) is a no-op.
    ZstdDCtx(unsafe { zstd_sys::ZSTD_createDCtx_advanced(ZSTD_CUSTOM_MEM) })
}

fn zstd_decompress_ctx_done(ctx: ZstdDCtx) {
    let queue = zstd_decompress_ctx_queue();
    let _ = queue.push(ctx);
}

mod pool;

pub use pool::compress_async;
pub use pool::decompress_async;

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
///
/// Separated from `decompress` so async callers can allocate the output
/// buffer on the tokio worker thread before dispatching work to a rayon
/// compute worker, keeping large allocations on the producer's heap.
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
        if ctx.0.is_null() {
            return Err(FragmentError::internal("failed to allocate zstd context"));
        }
        // Safety: ctx.0 is a valid non-null ZSTD_DCtx. Output buffer has capacity >= size_content.
        // Input slice length is validated against size_payload at function entry.
        let decompressed_size = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                ctx.0,
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
        if ctx.0.is_null() {
            return Err(FragmentError::internal("failed to allocate zstd context"));
        }
        // Safety: ctx.0 is a valid non-null ZSTD_DCtx. Output slice length is validated >= size_content.
        // Input slice length is validated against size_payload at function entry.
        let decompressed_size = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                ctx.0,
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
/// compression mode. Used to pre-allocate an output buffer on the caller's
/// thread so large allocations stay off the compute-worker heap.
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
) -> Result<(Fragment, Bytes), FragmentError> {
    let output_buffer =
        BytesMut::with_capacity(compress_bound(fragment.size_payload as usize, mode));
    compress_into(fragment, payload, mode, output_buffer)
}

/// Compress a fragment into a caller-provided output buffer. The buffer's
/// capacity must be at least `compress_bound(fragment.size_payload, mode)`;
/// callers that do not know the correct bound should use [`compress`] which
/// sizes the buffer itself.
///
/// Separated from `compress` so async callers can allocate the output
/// buffer on the tokio worker thread before dispatching work to a rayon
/// compute worker, keeping large allocations on the producer's heap.
fn compress_into(
    fragment: Fragment,
    payload: &[u8],
    mode: CompressionMode,
    output_buffer: BytesMut,
) -> Result<(Fragment, Bytes), FragmentError> {
    if fragment.size_content as usize > FRAGMENT_SIZE_THRESHOLD {
        return Err(FragmentError::internal("fragment has invalid sizes"));
    }
    // Only try to compress previously uncompressed raw data buffers of more than 32 bytes
    // Fragment lists and below 32 byte buffers are always raw uncompressed
    if (fragment.flags & FragmentFlags::PayloadCompressed) != 0
        || (fragment.flags & FragmentFlags::PayloadFragmented) != 0
        || (fragment.size_payload as u64) != fragment.size_content
    {
        return Err(FragmentError::internal(
            "fragment incompatible with compression",
        ));
    }

    if payload.len() < fragment.size_payload as usize {
        return Err(FragmentError::internal("fragment has invalid sizes"));
    }

    match mode {
        CompressionMode::Lz4 => compress_lz4_impl(fragment, payload, output_buffer),
        CompressionMode::Zstd | CompressionMode::NotSpecified => {
            compress_zstd_impl(fragment, payload, output_buffer)
        }
        #[cfg(feature = "oodle")]
        CompressionMode::Oodle => compress_oodle_impl(fragment, payload, output_buffer),
        #[cfg(not(feature = "oodle"))]
        CompressionMode::Oodle => Err(FragmentError::from(NotSupported {
            operation:
                "Oodle compression requested but this client was built without Oodle support"
                    .to_string(),
        })),
        CompressionMode::NoCompression => {
            Err(FragmentError::internal("fragment compression disabled"))
        }
    }
}

#[cfg(feature = "oodle")]
fn compress_oodle_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), FragmentError> {
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
        Err(FragmentError::internal("compression was inefficient"))
    }
}

fn compress_lz4_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), FragmentError> {
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
        Err(FragmentError::internal("compression was inefficient"))
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

fn compress_zstd_impl(
    fragment: Fragment,
    payload: &[u8],
    mut compressed_buffer: BytesMut,
) -> Result<(Fragment, Bytes), FragmentError> {
    // Save at least 5% to be worth compressing
    let compressed_size_threshold = ((fragment.size_payload as usize) * 95) / 100;

    let ctx = zstd_compress_ctx();
    if ctx.0.is_null() {
        return Err(FragmentError::internal("failed to allocate zstd context"));
    }
    // Safety: ctx.0 is a valid non-null ZSTD_CCtx. Buffer capacity was sized
    // by the caller via compress_bound(). Input payload length is validated
    // against size_payload at function entry.
    let compressed_size = unsafe {
        zstd_sys::ZSTD_compressCCtx(
            ctx.0,
            compressed_buffer.as_mut_ptr().cast::<std::ffi::c_void>(),
            compressed_buffer.capacity(),
            payload.as_ptr().cast::<std::ffi::c_void>(),
            fragment.size_payload as usize,
            zstd_compression_level(),
        )
    };
    zstd_compress_ctx_done(ctx);

    // Safety: Pure query on the return value, no pointer dereference.
    if unsafe { zstd_sys::ZSTD_isError(compressed_size) } == 0
        && compressed_size < compressed_size_threshold
    {
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
        Err(FragmentError::internal("compression was inefficient"))
    }
}
