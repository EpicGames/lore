// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_fuse::FuseError;
use lore_fuse::abi::InitIn;
use lore_fuse::abi::InitOut;
use lore_fuse::abi::KERNEL_MAJOR;
use lore_fuse::abi::REQUIRED_MINOR;
use lore_fuse::abi::init_flags;
use lore_fuse::init::*;

fn request_with_flags(flags64: u64) -> InitIn {
    let (flags, flags2) = init_flags::split(flags64);
    InitIn {
        major: KERNEL_MAJOR,
        minor: REQUIRED_MINOR,
        max_readahead: 1 << 20,
        flags,
        flags2,
        unused: [0; 11],
    }
}

fn valid_request() -> InitIn {
    request_with_flags(REQUIRED_FLAGS)
}

#[test]
fn rejects_minor_below_39_with_protocol_too_old() {
    let mut request = valid_request();
    request.minor = REQUIRED_MINOR - 1;

    let error = negotiate(&request, &InitConfig::default()).expect_err("must reject");

    assert!(matches!(error, FuseError::ProtocolTooOld { .. }));
}

#[test]
fn rejects_kernel_missing_required_flags() {
    let missing_parallel_dirops = REQUIRED_FLAGS & !init_flags::PARALLEL_DIROPS;
    let request = request_with_flags(missing_parallel_dirops);

    let error = negotiate(&request, &InitConfig::default()).expect_err("must reject");

    assert!(matches!(error, FuseError::InvalidRequest(_)));
}

#[test]
fn takes_optional_flags_only_when_offered() {
    let without_wanted = valid_request();
    let (_, without_wanted_out) =
        negotiate(&without_wanted, &InitConfig::default()).expect("negotiate");
    let without_wanted_flags =
        init_flags::combine(without_wanted_out.flags, without_wanted_out.flags2);
    assert_eq!(without_wanted_flags & WANTED_FLAGS, 0);

    let with_wanted = request_with_flags(REQUIRED_FLAGS | WANTED_FLAGS);
    let (_, with_wanted_out) = negotiate(&with_wanted, &InitConfig::default()).expect("negotiate");
    let with_wanted_flags = init_flags::combine(with_wanted_out.flags, with_wanted_out.flags2);
    assert_eq!(with_wanted_flags & WANTED_FLAGS, WANTED_FLAGS);
}

#[test]
fn no_open_support_only_when_zero_message_open_configured() {
    let request = valid_request();

    let (_, default_out) = negotiate(&request, &InitConfig::default()).expect("negotiate");
    let default_flags = init_flags::combine(default_out.flags, default_out.flags2);
    assert_eq!(default_flags & init_flags::NO_OPEN_SUPPORT, 0);

    let zero_message = InitConfig {
        zero_message_open: true,
        ..InitConfig::default()
    };
    let (_, out) = negotiate(&request, &zero_message).expect("negotiate");
    let flags = init_flags::combine(out.flags, out.flags2);
    assert_ne!(flags & init_flags::NO_OPEN_SUPPORT, 0);
}

#[test]
fn reply_sets_max_pages_max_write_and_ext_flags() {
    let request = valid_request();
    let config = InitConfig {
        max_pages: 128,
        ..InitConfig::default()
    };

    let (negotiated, out) = negotiate(&request, &config).expect("negotiate");

    assert_eq!(out.max_pages, 128);
    assert_eq!(out.max_write, 4096);
    assert_eq!(negotiated.max_pages, 128);
    assert_eq!(negotiated.max_write, 4096);
    let flags = init_flags::combine(out.flags, out.flags2);
    assert_ne!(flags & init_flags::INIT_EXT, 0);
}

#[test]
fn default_config_keeps_the_read_only_max_write() {
    assert_eq!(InitConfig::default().max_write, READ_ONLY_MAX_WRITE);
    assert_eq!(READ_ONLY_MAX_WRITE, 4096);
}

#[test]
fn read_write_config_negotiates_one_mib_writes() {
    let config = InitConfig::read_write();
    assert_eq!(config.max_write, READ_WRITE_MAX_WRITE);
    assert_eq!(READ_WRITE_MAX_WRITE, 1 << 20);

    let (negotiated, out) = negotiate(&valid_request(), &config).expect("negotiate");

    assert_eq!(out.max_write, 1 << 20);
    assert_eq!(negotiated.max_write, 1 << 20);
}

#[test]
fn max_write_is_clamped_to_what_max_pages_allows() {
    let config = InitConfig {
        max_pages: 16,
        ..InitConfig::read_write()
    };

    let (negotiated, out) = negotiate(&valid_request(), &config).expect("negotiate");

    assert_eq!(out.max_write, 16 * 4096);
    assert_eq!(negotiated.max_write, 16 * 4096);
}

#[test]
fn max_write_is_never_below_one_page() {
    for (max_pages, max_write) in [(256, 100), (0, READ_WRITE_MAX_WRITE)] {
        let config = InitConfig {
            max_pages,
            max_write,
            ..InitConfig::default()
        };

        let (negotiated, out) = negotiate(&valid_request(), &config).expect("negotiate");

        assert_eq!(out.max_write, 4096, "max_pages {max_pages}");
        assert_eq!(negotiated.max_write, 4096, "max_pages {max_pages}");
    }
}

#[test]
fn big_writes_is_taken_only_when_max_write_exceeds_one_page() {
    let offered = request_with_flags(REQUIRED_FLAGS | init_flags::BIG_WRITES);

    let (_, read_write) = negotiate(&offered, &InitConfig::read_write()).expect("negotiate");
    let (_, read_only) = negotiate(&offered, &InitConfig::default()).expect("negotiate");
    let (_, not_offered) =
        negotiate(&valid_request(), &InitConfig::read_write()).expect("negotiate");

    let taken = |out: &InitOut| init_flags::combine(out.flags, out.flags2) & init_flags::BIG_WRITES;
    assert_eq!(init_flags::BIG_WRITES, 1 << 5);
    assert_ne!(taken(&read_write), 0);
    assert_eq!(taken(&read_only), 0);
    assert_eq!(taken(&not_offered), 0);
}

#[test]
fn writeback_cache_is_never_requested() {
    let offered = request_with_flags(REQUIRED_FLAGS | WANTED_FLAGS | init_flags::WRITEBACK_CACHE);

    let (_, out) = negotiate(&offered, &InitConfig::read_write()).expect("negotiate");

    let flags = init_flags::combine(out.flags, out.flags2);
    assert_eq!(flags & init_flags::WRITEBACK_CACHE, 0);
}
