// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore::storage::open::*;

#[test]
fn both_zero_targets_yield_no_evictor_or_compactor() {
    let options = build_create_options(0, 0);
    assert!(options.max_capacity.is_none());
    assert!(options.max_size.is_none());
}

#[test]
fn explicit_targets_pass_through() {
    let options = build_create_options(512, 16);
    assert_eq!(options.max_size, Some(512));
    assert_eq!(options.max_capacity, Some(16));
}

#[test]
fn one_zero_field_only_defaults_that_field() {
    let bytes_only = build_create_options(4096, 0);
    assert_eq!(bytes_only.max_size, Some(4096));
    assert_eq!(
        bytes_only.max_capacity,
        Some(DEFAULT_CACHE_TARGET_FRAGMENTS),
    );
    let frags_only = build_create_options(0, 32);
    assert_eq!(frags_only.max_size, Some(DEFAULT_CACHE_TARGET_BYTES));
    assert_eq!(frags_only.max_capacity, Some(32));
}

/// Sub-floor `cache_target_fragments` must surface a warn-level log so the operator can
/// see the misconfiguration. This is the smallest behavioral observable proving the
/// target reaches the evictor wiring; deterministic eviction would require driving the
/// evictor's internal floor (`1 << 20` fragments), which is infeasible in a unit test.
/// The test installs a `fn`-pointer log callback that toggles a static flag when the
/// expected message lands.
#[test]
fn below_floor_emits_warn() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    static SAW_WARN: AtomicBool = AtomicBool::new(false);

    fn capture(level: lore_base::log::LoreLogLevel, _location: &str, message: &str) {
        if level == lore_base::log::LoreLogLevel::Warn
            && message.contains("below the evictor's internal floor")
        {
            SAW_WARN.store(true, Ordering::Release);
        }
    }

    let prev_level = lore_base::log::log_level();
    lore_base::log::set_log_level(lore_base::log::LoreLogLevel::Warn);
    lore_base::log::set_log_callback(Some(capture));
    SAW_WARN.store(false, Ordering::Release);

    let options = build_create_options(0, 4);

    // Restore the previous logger state regardless of the assert outcome.
    lore_base::log::set_log_callback(None);
    lore_base::log::set_log_level(prev_level);

    assert_eq!(options.max_capacity, Some(4));
    assert!(
        SAW_WARN.load(Ordering::Acquire),
        "sub-floor cache_target_fragments must emit a warn log",
    );
}

/// A handle trusts its remote where the open's `globals.trust_remote` asks or its `skip_verify`,
/// the alias, does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_trusts_its_remote_where_the_globals_or_skip_verify_ask() {
    use lore::storage::handle::LoreStore;
    use lore_revision::interface::LoreGlobalArgs;

    use crate::call::test_support::opened_handle_id;
    use crate::call::test_support::recording_callback;

    for (trust_remote, skip_verify, trusted) in [(0, 0, false), (1, 0, true), (0, 1, true)] {
        let (events, callback) = recording_callback();
        let globals = LoreGlobalArgs {
            trust_remote,
            ..LoreGlobalArgs::default()
        };
        let args = LoreStorageOpenArgs {
            in_memory: 1,
            skip_verify,
            ..Default::default()
        };
        assert_eq!(open(globals, args, callback).await, 0, "the open succeeds");
        let handle = LoreStore {
            handle_id: opened_handle_id(&events.lock().unwrap())
                .expect("the open reports a handle"),
        };

        let store = lore::storage::handle::unregister(handle).expect("the handle is registered");
        assert_eq!(
            store.trust_remote, trusted,
            "trust_remote {trust_remote}, skip_verify {skip_verify}"
        );
    }
}
