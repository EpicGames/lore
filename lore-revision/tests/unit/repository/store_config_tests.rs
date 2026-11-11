// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::repository::RepositoryConfig;
use lore_revision::repository::StoreConfig;
use lore_revision::repository::cache_disabled;
use lore_revision::repository::incremental_gc_options;

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
