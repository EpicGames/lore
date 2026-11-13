// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_storage::Fragment;
use lore_storage::fragment_flags::FragmentFlags;
use lore_storage::write_stats::*;

fn data_fragment(payload: u32, content: u64) -> Fragment {
    Fragment {
        flags: 0,
        size_payload: payload,
        size_content: content,
    }
}

fn list_fragment(payload: u32, content: u64) -> Fragment {
    Fragment {
        flags: FragmentFlags::PayloadFragmented.bits(),
        size_payload: payload,
        size_content: content,
    }
}

#[test]
fn a_fresh_snapshot_is_all_zero() {
    assert_eq!(
        FragmentWriteStats::default().snapshot(),
        FragmentWriteCounts::default()
    );
}

/// Every processed fragment either has a payload prepared or does not, so the
/// three counts partition it. A fragment reported in none of them is one the
/// report cannot account for.
#[test]
fn payload_outcomes_account_for_every_processed_fragment() {
    let stats = FragmentWriteStats::default();
    let data = data_fragment(100, 1000);
    let list = list_fragment(80, 4096);
    for _ in 0..5 {
        stats.fragment_processed(&data);
    }
    stats.fragment_processed(&list);
    stats.payload_prepared(&data);
    stats.payload_prepared(&data);
    stats.payload_prepared(&list);
    for _ in 0..3 {
        stats.payload_not_prepared(&data);
    }

    let counts = stats.snapshot();
    assert_eq!(
        counts.data_fragments + counts.fragmentlists + counts.no_payload_fragments,
        counts.fragments_processed
    );
    assert_eq!(
        counts.data_content_bytes + counts.no_payload_content_bytes,
        counts.processed_content_bytes
    );
}

/// The split the report leans on: a list fragment must not land in the data
/// buckets, or the compression ratio would be computed against reference
/// bytes that were never file content.
#[test]
fn a_list_fragment_is_counted_apart_from_data() {
    let stats = FragmentWriteStats::default();
    stats.payload_prepared(&data_fragment(100, 400));
    stats.payload_prepared(&list_fragment(64, 4096));

    let counts = stats.snapshot();
    assert_eq!(counts.data_fragments, 1);
    assert_eq!(counts.data_payload_bytes, 100);
    assert_eq!(counts.data_content_bytes, 400);
    assert_eq!(counts.fragmentlists, 1);
    assert_eq!(counts.fragmentlist_payload_bytes, 64);
}

#[test]
fn local_writes_split_into_metadata_only_and_payload_bearing() {
    let stats = FragmentWriteStats::default();
    stats.local_write(Some(300));
    stats.local_write(Some(700));
    stats.local_write(None);

    let counts = stats.snapshot();
    assert_eq!(counts.local_writes, 3);
    assert_eq!(counts.local_payload_writes, 2);
    assert_eq!(counts.local_payload_bytes, 1000);
    assert_eq!(counts.local_metadata_writes, 1);
    assert_eq!(
        counts.local_writes,
        counts.local_metadata_writes + counts.local_payload_writes
    );
}

/// Every processed fragment reaches exactly one of four outcomes against the
/// remote. A fragment counted in none of them is one the report cannot
/// account for.
#[test]
fn the_remote_outcomes_account_for_every_processed_fragment() {
    let stats = FragmentWriteStats::default();
    let fragment = data_fragment(64, 1024);
    for _ in 0..9 {
        stats.fragment_processed(&fragment);
    }
    stats.remote_copy();
    stats.remote_copy();
    stats.remote_put(64);
    stats.remote_put(64);
    stats.remote_put(64);
    stats.remote_already_durable();
    stats.remote_already_durable();
    stats.local_only_write();
    stats.remote_upload_failed();

    let counts = stats.snapshot();
    assert_eq!(
        counts.remote_writes
            + counts.remote_already_durable
            + counts.local_only_writes
            + counts.remote_upload_failed,
        counts.fragments_processed
    );
    assert_eq!(counts.remote_upload_failed, 1);
}

/// A copy is a remote write that sends no bytes. Folding it into the put
/// totals would hide exactly the saving the copy path exists to make.
#[test]
fn a_copy_is_a_remote_write_that_carries_no_bytes() {
    let stats = FragmentWriteStats::default();
    stats.remote_copy();
    stats.remote_put(2048);

    let counts = stats.snapshot();
    assert_eq!(counts.remote_writes, 2);
    assert_eq!(counts.remote_copy_writes, 1);
    assert_eq!(counts.remote_put_writes, 1);
    assert_eq!(counts.remote_put_bytes, 2048);
}

#[test]
fn offered_fragments_split_into_deduplicated_and_processed() {
    let stats = FragmentWriteStats::default();
    let fragment = data_fragment(20, 100);
    for _ in 0..5 {
        stats.fragment_produced(&fragment);
    }
    for _ in 0..2 {
        stats.fragment_deduplicated(&fragment);
    }
    for _ in 0..3 {
        stats.fragment_processed(&fragment);
    }

    let counts = stats.snapshot();
    assert_eq!(counts.fragments_produced, 5);
    assert_eq!(
        counts.fragments_produced,
        counts.fragments_deduplicated + counts.fragments_processed
    );
    assert_eq!(counts.fragment_content_bytes, 500);
    assert_eq!(
        counts.fragment_content_bytes,
        counts.deduplicated_content_bytes + counts.processed_content_bytes
    );
}

/// Distinct values per field, so a difference wired to the wrong field shows
/// up rather than passing on a shared zero. Against no baseline every count
/// stands as it is; against itself every count is spent.
#[test]
fn a_difference_is_taken_field_by_field() {
    let mut counts = FragmentWriteCounts::default();
    for (index, field) in [
        &mut counts.fragments_produced,
        &mut counts.fragment_content_bytes,
        &mut counts.fragments_deduplicated,
        &mut counts.deduplicated_content_bytes,
        &mut counts.fragments_processed,
        &mut counts.processed_content_bytes,
        &mut counts.data_fragments,
        &mut counts.data_payload_bytes,
        &mut counts.data_content_bytes,
        &mut counts.fragmentlists,
        &mut counts.fragmentlist_payload_bytes,
        &mut counts.no_payload_fragments,
        &mut counts.no_payload_content_bytes,
        &mut counts.local_writes,
        &mut counts.local_metadata_writes,
        &mut counts.local_payload_writes,
        &mut counts.local_payload_bytes,
        &mut counts.remote_writes,
        &mut counts.remote_copy_writes,
        &mut counts.remote_put_writes,
        &mut counts.remote_put_bytes,
        &mut counts.remote_already_durable,
        &mut counts.local_only_writes,
        &mut counts.remote_upload_failed,
        &mut counts.remote_queries,
        &mut counts.remote_query_hits,
        &mut counts.remote_query_unsent_bytes,
    ]
    .into_iter()
    .enumerate()
    {
        *field = index as u64 + 1;
    }

    assert_eq!(counts.since(&FragmentWriteCounts::default()), counts);
    assert_eq!(counts.since(&counts), FragmentWriteCounts::default());
}

/// A list's `size_content` is the content of the whole tree beneath it, so
/// counting it alongside its leaves would report the content twice.
#[test]
fn a_fragment_list_contributes_no_content_to_a_total() {
    assert_eq!(size_content_of(&data_fragment(100, 400)), 400);
    assert_eq!(size_content_of(&list_fragment(64, 4096)), 0);
}
