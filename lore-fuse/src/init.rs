// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! INIT negotiation: pure functions over the kernel's `InitIn`/`InitOut` wire structs.
//!
//! No I/O here. `negotiate` either rejects the kernel's offer or produces both the
//! session-local [`Negotiated`] view and the exact `InitOut` to write back.

use crate::abi::InitIn;
use crate::abi::InitOut;
use crate::abi::KERNEL_MAJOR;
use crate::abi::REQUIRED_MINOR;
use crate::abi::init_flags;
use crate::error::FuseError;

/// `max_write` of a read-only mount: a large write buffer would only be memory the kernel never
/// fills. Also the kernel's floor for `max_write`.
pub const READ_ONLY_MAX_WRITE: u32 = 4096;

/// Default `max_write` of a read-write mount.
pub const READ_WRITE_MAX_WRITE: u32 = 1 << 20;

/// Page size the kernel counts `max_pages` in.
const PAGE_BYTES: u32 = 4096;

/// Flags this crate always requires the kernel to offer. Negotiation fails otherwise.
pub const REQUIRED_FLAGS: u64 = init_flags::ASYNC_READ
    | init_flags::DO_READDIRPLUS
    | init_flags::READDIRPLUS_AUTO
    | init_flags::PARALLEL_DIROPS
    | init_flags::MAX_PAGES
    | init_flags::INIT_EXT;

/// Flags this crate takes when the kernel offers them, but does not require.
pub const WANTED_FLAGS: u64 =
    init_flags::EXPLICIT_INVAL_DATA | init_flags::HAS_EXPIRE_ONLY | init_flags::NO_EXPORT_SUPPORT;

/// What a session configures before INIT runs.
pub struct InitConfig {
    pub max_pages: u16,
    pub zero_message_open: bool,
    pub max_background: u16,
    pub congestion_threshold: u16,
    /// Largest `WRITE` payload. Negotiation clamps it to one page at least and to `max_pages`
    /// pages at most; each worker's request buffer holds one full `WRITE`.
    pub max_write: u32,
}

impl InitConfig {
    /// The defaults with [`READ_WRITE_MAX_WRITE`], for a read-write mount.
    pub fn read_write() -> Self {
        Self {
            max_write: READ_WRITE_MAX_WRITE,
            ..Self::default()
        }
    }
}

impl Default for InitConfig {
    fn default() -> Self {
        Self {
            max_pages: 256,
            zero_message_open: false,
            max_background: 64,
            congestion_threshold: 48,
            max_write: READ_ONLY_MAX_WRITE,
        }
    }
}

/// The outcome of INIT negotiation, in the shape the rest of the session cares about.
#[derive(Debug, Clone, Copy)]
pub struct Negotiated {
    pub minor: u32,
    pub flags: u64,
    pub max_readahead: u32,
    pub max_write: u32,
    pub max_pages: u16,
    pub zero_message_open: bool,
}

/// Negotiates the FUSE protocol from the kernel's INIT request.
///
/// Rejects a kernel below the required minor, or one that does not offer every flag in
/// [`REQUIRED_FLAGS`]. Otherwise returns the session's [`Negotiated`] view alongside the exact
/// `InitOut` to reply with.
pub fn negotiate(
    request: &InitIn,
    config: &InitConfig,
) -> Result<(Negotiated, InitOut), FuseError> {
    if request.minor < REQUIRED_MINOR {
        return Err(FuseError::ProtocolTooOld {
            minor: request.minor,
            required: REQUIRED_MINOR,
        });
    }

    let offered = init_flags::combine(request.flags, request.flags2);
    let missing = REQUIRED_FLAGS & !offered;
    if missing != 0 {
        return Err(FuseError::InvalidRequest(format!(
            "kernel did not offer required INIT flags: {missing:#x}"
        )));
    }

    let mut flags = REQUIRED_FLAGS | (offered & WANTED_FLAGS);
    if config.zero_message_open {
        flags |= init_flags::NO_OPEN_SUPPORT;
    }

    let max_readahead = request
        .max_readahead
        .min(config.max_pages as u32 * PAGE_BYTES);
    let max_write = negotiated_max_write(config);
    if max_write > READ_ONLY_MAX_WRITE {
        flags |= offered & init_flags::BIG_WRITES;
    }
    let (flags_lo, flags_hi) = init_flags::split(flags);

    let out = InitOut {
        major: KERNEL_MAJOR,
        minor: REQUIRED_MINOR,
        max_readahead,
        flags: flags_lo,
        max_background: config.max_background,
        congestion_threshold: config.congestion_threshold,
        max_write,
        time_gran: 1,
        max_pages: config.max_pages,
        map_alignment: 0,
        flags2: flags_hi,
        max_stack_depth: 0,
        request_timeout: 0,
        unused: [0; 11],
    };

    let negotiated = Negotiated {
        minor: REQUIRED_MINOR,
        flags,
        max_readahead,
        max_write,
        max_pages: config.max_pages,
        zero_message_open: config.zero_message_open,
    };

    Ok((negotiated, out))
}

/// The `max_write` the kernel uses for `config`: it raises anything below one page to one page,
/// and splits a write into requests of at most `max_pages` pages, at least one.
fn negotiated_max_write(config: &InitConfig) -> u32 {
    let page_limit = u32::from(config.max_pages.max(1)) * PAGE_BYTES;
    config.max_write.min(page_limit).max(READ_ONLY_MAX_WRITE)
}
