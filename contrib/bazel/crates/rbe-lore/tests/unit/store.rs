// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::Ordering;

use lore_base::types::Hash;
use lore_base::types::Partition;
use rbe_lore::*;

/// The two partitions must differ, and neither may be the default: mutable ops reject the
/// zero partition, and a collision would silently merge the build cache with the toolchains.
#[test]
fn partitions_are_distinct_and_non_zero() {
    let build = rbe_partition(Part::BuildCache);
    let toolchain = rbe_partition(Part::Toolchain);
    assert_ne!(build, Partition::default());
    assert_ne!(toolchain, Partition::default());
    assert_ne!(build, toolchain);
}

#[test]
fn partitions_are_stable() {
    assert_eq!(
        rbe_partition(Part::BuildCache),
        rbe_partition(Part::BuildCache)
    );
}

/// Only CAS content is ever seeded, so only CAS reads may fall through. An `ActionResult`
/// reaching for a partition shared across projects would be reading another build's outputs.
#[test]
fn only_cas_reads_fall_through_to_the_toolchain_partition() {
    assert_eq!(
        LoreBlobStore::read_order(Ns::Cas),
        &[Part::BuildCache, Part::Toolchain]
    );
    assert_eq!(LoreBlobStore::read_order(Ns::Ac), &[Part::BuildCache]);
}

/// The build cache is tried first in both cases, so a digest present there is never served
/// from the toolchain partition by accident.
#[test]
fn the_build_cache_is_always_tried_first() {
    for ns in [Ns::Cas, Ns::Ac] {
        assert_eq!(LoreBlobStore::read_order(ns)[0], Part::BuildCache);
    }
}

/// An `Action` message is itself a CAS blob, so the same digest can appear in both
/// namespaces. The prefix is what stops one answering for the other.
#[test]
fn namespaces_do_not_collide_on_one_digest() {
    assert_ne!(key(Ns::Cas, "abc", 7), key(Ns::Ac, "abc", 7));
}

/// Size is part of the key, so two blobs whose hashes match but whose declared sizes differ
/// cannot be confused for each other.
#[test]
fn size_is_part_of_the_key() {
    assert_ne!(key(Ns::Cas, "abc", 1), key(Ns::Cas, "abc", 2));
}

/// The key is how every stored entry is found again, so its format is persistent: a change
/// here does not fail anything, it silently orphans the whole cache and every seeded
/// toolchain.
#[test]
fn the_key_format_is_stable() {
    assert_eq!(key(Ns::Cas, "abc", 7), Hash::hash_buffer(b"cas:abc:7"));
    assert_eq!(key(Ns::Ac, "abc", 7), Hash::hash_buffer(b"ac:abc:7"));
}

/// The empty blob is a CAS rule. An empty *action* digest names no stored result, and
/// answering it with empty content would serve a successful `ActionResult` nobody produced.
#[test]
fn only_a_cas_digest_can_be_the_empty_blob() {
    assert!(is_empty_content(Ns::Cas, digest::EMPTY_SHA256, 0));
    assert!(!is_empty_content(Ns::Ac, digest::EMPTY_SHA256, 0));
    assert!(!is_empty_content(Ns::Cas, digest::EMPTY_SHA256, 1));
}

#[test]
fn render_omits_input_fetch_for_a_process_that_never_materialises() {
    let stats = Stats::default();
    assert!(!stats.render().contains("input fetch"));
    stats.input_fetch_actions.store(3, Ordering::Relaxed);
    stats.input_fetch_ms.store(1500, Ordering::Relaxed);
    let rendered = stats.render();
    assert!(
        rendered.contains("input fetch: 1.5 s over 3 actions"),
        "{rendered}"
    );
}
