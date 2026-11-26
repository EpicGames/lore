// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::test_util::TempDir;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::SharedStoreToUseConfig;
use lore_revision::repository::StoreConfig;
use lore_revision::repository::cache_disabled;
use lore_revision::repository::clone::CloneOptions;
use lore_revision::repository::create_immutable_store_at_path;
use lore_revision::repository::incremental_gc_options;
use lore_revision::shared_store::SharedStoreConfig;
use lore_storage::local::immutable_store::ImmutableStoreCreateOptions;

/// Each limit that is not 0 replaces its default, and the delays stay the defaults.
#[test]
fn client_default_with_limits_takes_each_limit_that_is_not_zero() {
    let default = StoreConfig::client_default();
    for (max_capacity, max_size, expected_capacity, expected_size) in [
        (50_000_000, 1 << 40, Some(50_000_000), Some(1 << 40)),
        (50_000_000, 0, Some(50_000_000), default.max_size),
        (0, 1 << 40, default.max_capacity, Some(1 << 40)),
        (0, 0, default.max_capacity, default.max_size),
    ] {
        let config = StoreConfig::client_default_with_limits(max_capacity, max_size);
        assert_eq!(config.max_capacity, expected_capacity, "{max_capacity}");
        assert_eq!(config.max_size, expected_size, "{max_size}");
        assert_eq!(config.eviction_delay, default.eviction_delay);
        assert_eq!(config.compaction_delay, default.compaction_delay);
    }
}

/// A clone's limits go into the `[store]` config of an instance with a store of its own, and an
/// instance over a shared store gets the defaults.
#[test]
fn clone_store_config_holds_the_limits_only_without_a_shared_store() {
    let limited = |shared_store_options| CloneOptions {
        shared_store_options,
        max_capacity: 50_000_000,
        max_size: 1 << 40,
        ..CloneOptions::default()
    };
    let shared = SharedStoreToUseConfig {
        use_shared_store: Some(true),
        shared_store_path: None,
    };
    let default = StoreConfig::client_default();

    let own = limited(None).store_config(false);
    assert_eq!(own.max_capacity, Some(50_000_000));
    assert_eq!(own.max_size, Some(1 << 40));

    let over_shared = limited(Some(shared)).store_config(false);
    assert_eq!(over_shared.max_capacity, default.max_capacity);
    assert_eq!(over_shared.max_size, default.max_size);

    let unlimited = CloneOptions::default().store_config(false);
    assert_eq!(unlimited.max_capacity, default.max_capacity);
    assert_eq!(unlimited.max_size, default.max_size);
}

/// The new instance's `[store]` config over a shared store warns once where the clone is given
/// limits, and not where it is given none or has a store of its own.
#[tokio::test]
async fn clone_store_config_warns_once_of_limits_a_shared_store_ignores() {
    let over_shared = |max_capacity| CloneOptions {
        shared_store_options: Some(SharedStoreToUseConfig {
            use_shared_store: Some(true),
            shared_store_path: None,
        }),
        max_capacity,
        ..CloneOptions::default()
    };
    let own_store = CloneOptions {
        max_capacity: 50_000_000,
        ..CloneOptions::default()
    };
    for (options, warnings, case) in [
        (
            over_shared(50_000_000),
            1,
            "limits given over a shared store",
        ),
        (over_shared(0), 0, "no limits given over a shared store"),
        (own_store, 0, "limits given with a store of its own"),
    ] {
        let (_, warned) = crate::helper::warned(async { options.store_config(false) }).await;
        assert_eq!(warned.len(), warnings, "{case}");
    }
}

/// A clone that keeps its stores in memory, because it is without tracking or the global
/// `in_memory` is set, gets the default `[store]` config in place of the limits it is given, with or
/// without a shared store, and warns once where it is given either limit, and not where it is given
/// none.
#[tokio::test]
async fn in_memory_clone_store_config_holds_the_defaults_and_warns_once_of_given_limits() {
    let shared = || {
        Some(SharedStoreToUseConfig {
            use_shared_store: Some(true),
            shared_store_path: None,
        })
    };
    let default = StoreConfig::client_default();
    for (no_tracking, global_in_memory, cause) in [
        (true, false, "without tracking"),
        (false, true, "with the global in_memory"),
        (true, true, "without tracking and with the global in_memory"),
    ] {
        let clone = |shared_store_options, max_capacity, max_size| CloneOptions {
            no_tracking,
            shared_store_options,
            max_capacity,
            max_size,
            ..CloneOptions::default()
        };
        for (options, warnings, case) in [
            (clone(None, 50_000_000, 1 << 40), 1, "both limits"),
            (clone(None, 50_000_000, 0), 1, "a capacity"),
            (clone(None, 0, 1 << 40), 1, "a size"),
            (
                clone(shared(), 50_000_000, 1 << 40),
                1,
                "both limits over a shared store",
            ),
            (clone(None, 0, 0), 0, "no limits"),
        ] {
            let (config, warned) =
                crate::helper::warned(async { options.store_config(global_in_memory) }).await;
            assert_eq!(config.max_capacity, default.max_capacity, "{case} {cause}");
            assert_eq!(config.max_size, default.max_size, "{case} {cause}");
            assert_eq!(warned.len(), warnings, "{case} {cause}");
        }
    }
}

/// The default GC caps that back the automatic incremental GC on writes when a
/// repository has no `[store]` config must be populated and non-zero; otherwise the
/// evictor and compactor would be spawned with zero caps (`Some(0)`) and never run.
#[test]
fn client_default_yields_nonzero_gc_caps() {
    let options = StoreConfig::client_default().to_options();
    assert!(options.max_capacity.is_some_and(|c| c > 0));
    assert!(options.max_size.is_some_and(|s| s > 0));
}

#[test]
fn write_op_spawns_incremental_gc_by_default() {
    // Write op, `--no-gc` not set, no `[store]` config: incremental GC on with
    // the non-zero `client_default` caps.
    let options = incremental_gc_options(false, false, None);
    assert!(options.max_capacity.is_some_and(|c| c > 0));
    assert!(options.max_size.is_some_and(|s| s > 0));
}

#[test]
fn no_gc_suppresses_incremental_gc() {
    let options = incremental_gc_options(false, true, None);
    assert!(options.max_capacity.is_none());
    assert!(options.max_size.is_none());
}

#[test]
fn read_only_op_never_spawns_incremental_gc() {
    let options = incremental_gc_options(true, false, None);
    assert!(options.max_capacity.is_none());
    assert!(options.max_size.is_none());
}

/// Whether a plain instance with `[store]` text `store` leaves fetched content out of the store,
/// for a command that asks to cache or not.
fn cache_disabled_for(store: &str, cache: bool) -> bool {
    let config: RepositoryConfig = toml::from_str(store).expect("the config must parse");
    let globals = LoreGlobalArgs {
        cache: cache.into(),
        ..LoreGlobalArgs::default()
    };
    cache_disabled(&globals, &config)
}

/// `[store] cache_payloads = true` keeps fetched content; unset or false leaves it out, and
/// `--cache` keeps it either way.
#[test]
fn cache_payloads_sets_whether_fetched_content_is_kept() {
    for (store, disabled) in [
        ("[store]\ncache_payloads = true\n", false),
        ("[store]\ncache_payloads = false\n", true),
        ("[store]\n", true),
        ("", true),
    ] {
        assert_eq!(cache_disabled_for(store, false), disabled, "{store:?}");
        assert!(!cache_disabled_for(store, true), "--cache with {store:?}");
    }
}

/// A shared store's own limits replace those of an open that collects, and leave an open that
/// doesn't collect, or a store without limits of its own, as the open asks.
#[test]
fn shared_store_limits_replace_those_of_an_open_that_collects() {
    let limited = SharedStoreConfig {
        remote_url: None,
        store_config: Some(StoreConfig {
            max_size: Some(4 << 40),
            max_capacity: Some(50_000_000),
            ..StoreConfig::client_default()
        }),
    };
    let collecting = StoreConfig::client_default().to_options();

    let options = limited.gc_options(collecting);
    assert_eq!(options.max_size, Some(4 << 40));
    assert_eq!(options.max_capacity, Some(50_000_000));

    assert!(
        !limited
            .gc_options(ImmutableStoreCreateOptions::none())
            .collects()
    );

    let unlimited = SharedStoreConfig::default().gc_options(collecting);
    assert_eq!(unlimited.max_size, collecting.max_size);
    assert_eq!(unlimited.max_capacity, collecting.max_capacity);
}

/// A limit or delay a shared store's config leaves out is the default, not 0.
#[test]
fn shared_store_limits_left_out_are_the_defaults() {
    let partial = SharedStoreConfig {
        remote_url: None,
        store_config: Some(StoreConfig {
            max_size: Some(4 << 40),
            ..StoreConfig::default()
        }),
    };
    let defaults = StoreConfig::client_default().to_options();

    let options = partial.gc_options(defaults);

    assert_eq!(options.max_size, Some(4 << 40));
    assert_eq!(options.max_capacity, defaults.max_capacity);
    assert_eq!(options.eviction_delay, defaults.eviction_delay);
    assert_eq!(options.compaction_delay, defaults.compaction_delay);
}

/// A store already open takes the limits of a later open that collects, and keeps them through
/// one that doesn't.
#[tokio::test]
async fn a_cached_store_takes_the_limits_of_a_later_open_that_collects() {
    let dir = TempDir::new("lore-cached-store-limits-");
    let limited = StoreConfig {
        max_size: Some(4 << 40),
        max_capacity: Some(3_000_000),
        ..StoreConfig::client_default()
    }
    .to_options();

    let (store, _opening) = create_immutable_store_at_path(
        dir.to_path_buf(),
        ImmutableStoreCreateOptions::none(),
        false,
    )
    .await
    .expect("opens");
    let (_again, _) = create_immutable_store_at_path(dir.to_path_buf(), limited, false)
        .await
        .expect("reuses");
    let counters = store.gc_counters().expect("a local store has GC counters");
    assert_eq!(counters.caps(), (4 << 40, 3_000_000));

    let (_again, _) = create_immutable_store_at_path(
        dir.to_path_buf(),
        ImmutableStoreCreateOptions::none(),
        false,
    )
    .await
    .expect("reuses");
    assert_eq!(counters.caps(), (4 << 40, 3_000_000));
}
