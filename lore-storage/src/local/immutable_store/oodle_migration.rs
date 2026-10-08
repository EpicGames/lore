// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Re-encodes Oodle-compressed payloads held only in a local immutable store.
//!
//! Oodle is no longer a supported codec: nothing writes it, and a build without the `oodle`
//! feature cannot decode it, so a payload still encoded with it is unreadable the moment support
//! is removed. A durably stored payload is not re-encoded on its own account: a read that cannot
//! decode one replaces it with the remote's encoding. It still adopts the re-encoded payload of
//! a local-only entry with the same hash that the pass reaches first.
//!
//! A pass is resumable and idempotent: progress is recorded per group, and a re-encoded entry no
//! longer carries `PayloadCompressedOodle2`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_base::lore_error;
use lore_base::lore_info;
use lore_base::lore_spawn;
use lore_base::types::Fragment;
use lore_base::types::FragmentFlags;
use lore_error_set::ForwardStrict;
use lore_error_set::WrapInternal;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::task::JoinSet;

use crate::CompressionMode;
use crate::LocalImmutableStore;
use crate::LocalImmutableStoreError;
use crate::hash;
use crate::local::immutable_store::ImmutableStoreBucket;
use crate::local::immutable_store::ImmutableStoreEntry;
use crate::local::immutable_store::ImmutableStoreGroup;
use crate::local::immutable_store::flush_locked_group;

/// Entries a bucket walks between progress reports. A pre-fan-out store holds a whole group in one
/// bucket, so a pass can run long enough that silence is indistinguishable from a hang.
const MIGRATION_UPDATE_NUM_ENTRIES: usize = 10_000;

/// Outcome tally for a single bucket.
#[lore_macro::test_pub]
#[derive(Debug, Default)]
struct MigrationCounts {
    migrated: usize,
    invalid: usize,
    durably_left: usize,
}

/// Warn about `entry`, naming the packfile location a reader needs to inspect the payload.
fn warn_entry(message: &str, entry: &ImmutableStoreEntry) {
    lore_base::lore_warn!(
        "{message}: address {} packfile {} offset {} payload size {}",
        entry.address,
        entry.data.pack_file,
        entry.data.pack_offset,
        entry.data.size_payload
    );
}

/// Re-encode local-only Oodle payloads reachable from one bucket, rewriting its entries in place.
///
/// Iteration follows `sorted_index`, so entries sharing a hash are visited consecutively and the
/// payload they share is decoded and re-encoded once for the whole run.
///
/// Marks the bucket dirty if anything changed; persisting it is the caller's job.
#[lore_macro::test_pub]
async fn migrate_bucket(
    bucket_index: usize,
    mut bucket: OwnedRwLockWriteGuard<ImmutableStoreBucket>,
    group: Arc<ImmutableStoreGroup>,
    path: Arc<PathBuf>,
    group_index: usize,
    gc_counters: Arc<crate::maintenance::GcCounters>,
) -> Result<MigrationCounts, LocalImmutableStoreError> {
    if !bucket.deserialized {
        Box::pin(bucket.deserialize(
            &group.dirty[bucket_index],
            path.as_path(),
            group_index,
            bucket_index,
            Some(&gc_counters),
        ))
        .await?;
    }

    let mut counts = MigrationCounts::default();
    let mut num_entries = 0;

    let mut next_entry = 0;
    while next_entry < bucket.sorted_index.len() {
        num_entries += 1;
        if num_entries % MIGRATION_UPDATE_NUM_ENTRIES == 0 {
            lore_info!(
                "Group '{group_index}' bucket '{bucket_index}' migration: {} Oodle migrated, {} Invalid Oodle, {} Left because durably stored",
                counts.migrated,
                counts.invalid,
                counts.durably_left
            );
        }

        let entry_index = bucket.sorted_index[next_entry] as usize;
        let entry = bucket.entry[entry_index];
        next_entry += 1;

        // Nothing is stored locally to re-encode, and the codec the remote holds it under is
        // not knowable from here.
        if entry.data.pack_file == 0 {
            continue;
        }
        if entry.data.flags & FragmentFlags::PayloadCompressedOodle2 == 0 {
            continue;
        }
        // A durable payload is held upstream, where it is re-encoded on ingress. Leaving the
        // entry untouched keeps the bucket clean, so a group holding nothing else costs no
        // rewrite; a read that cannot decode the local copy refetches the upstream encoding and
        // overwrites it. A durable entry later in a run led by a local-only one is not reached
        // here: it adopts the leader's re-encoded payload below.
        if entry.data.flags & FragmentFlags::PayloadStoredDurable != 0 {
            counts.durably_left += 1;
            continue;
        }

        let (oodle_fragment, oodle_payload) = {
            let buffer = group
                .packstore
                .load(
                    entry.data.pack_file,
                    entry.data.pack_offset,
                    entry.data.size_payload,
                )
                .await
                .map_err(|err| {
                    warn_entry("failed to load Oodle data", &entry);
                    LocalImmutableStoreError::internal_with_context(
                        err,
                        "failed to read Oodle data",
                    )
                })?;

            let fragment = Fragment {
                flags: entry.data.flags,
                size_payload: entry.data.size_payload,
                size_content: entry.data.size_content,
            };

            (fragment, buffer)
        };

        let (decompressed_fragment, decompressed_payload) = {
            let (decompressed_fragment, decompressed_payload) =
                crate::decompress(oodle_fragment, &oodle_payload).map_err(|err| {
                    warn_entry("failed to decompress Oodle data", &entry);
                    LocalImmutableStoreError::internal_with_context(
                        err,
                        "failed to decompress Oodle data",
                    )
                })?;

            let hash = hash::hash_slice(&decompressed_payload);
            if hash != entry.address.hash {
                warn_entry(
                    &format!("SKIPPING: Oodle decompressed hash mismatch - calculated {hash}"),
                    &entry,
                );
                // an invalid fragment is one that cannot be loaded or be accepted by a durable
                // store anyway, so ignore it.
                counts.invalid += 1;
                continue;
            }

            (decompressed_fragment, decompressed_payload)
        };

        let (recompressed_fragment, recompressed_payload) = match crate::compress(
            decompressed_fragment,
            &decompressed_payload,
            CompressionMode::Zstd,
        ) {
            Ok(result) => result,
            Err(err) if err.is_inefficient_compression() => {
                (decompressed_fragment, decompressed_payload.freeze())
            }
            Err(err) => {
                warn_entry(
                    &format!(
                        "failed to recompress Oodle data: uncompressed size payload {}.",
                        decompressed_fragment.size_payload
                    ),
                    &entry,
                );
                return Err(LocalImmutableStoreError::internal_with_context(
                    err,
                    "failed to recompress Oodle data",
                ));
            }
        };

        let pack_ref = group
            .packstore
            .store(recompressed_payload.slice(..recompressed_fragment.size_payload as usize))
            .await
            .forward::<LocalImmutableStoreError>(
                "Failed storing recompressed Oodle immutable data, packstore write failed",
            )?;

        let migrated_data = {
            let entry = &mut bucket.entry[entry_index];
            entry.data.size_payload = recompressed_fragment.size_payload;
            entry.data.flags = recompressed_fragment.flags;
            entry.data.pack_offset = pack_ref.offset;
            entry.data.pack_file = pack_ref.id;
            entry.data
        };
        counts.migrated += 1;

        // Durable siblings adopt the payload as well. It holds the same content, checked against
        // the hash above, in a form this build decodes, and the leader has already dirtied the
        // bucket they share, so adopting costs nothing and saves the sibling a refetch.
        while next_entry < bucket.sorted_index.len() {
            let sibling_index = bucket.sorted_index[next_entry] as usize;
            if bucket.entry[sibling_index].address.hash != entry.address.hash {
                break;
            }
            bucket.entry[sibling_index]
                .data
                .assign_deduplicated_payload(migrated_data);
            next_entry += 1;
            num_entries += 1;
            counts.migrated += 1;
        }
    }

    if counts.migrated > 0 {
        group.dirty[bucket_index].store(true, Ordering::Relaxed);
    }

    Ok(counts)
}

/// Re-encode one group and publish the result.
///
/// The group is taken as a whole - flush lock held throughout, every active bucket write-locked
/// before any work starts. A fan-out moves entries between the buckets of a group, so a pass that
/// took them one at a time could leave Oodle payloads behind in a group it had declared done.
///
/// Publishing goes through the group's own flush rather than a per-bucket serialize. A group that
/// opens with its `committed_level` behind its `bucket_count` has to commit its bucket files and
/// level marker as one unit, or the next open reads the group back at the old level and loses the
/// entries above it. Which path a given group takes is the flush's decision, not this one's.
#[lore_macro::test_pub]
async fn migrate_group(
    store: &Arc<LocalImmutableStore>,
    path: &Arc<PathBuf>,
    group_index: usize,
) -> Result<(), LocalImmutableStoreError> {
    let group = &store.group[group_index];

    let _flush_guard = group.flush_lock.clone().lock_owned().await;

    let num_buckets = group.bucket_count.load(Ordering::Relaxed);
    let mut guards: Vec<OwnedRwLockWriteGuard<ImmutableStoreBucket>> =
        Vec::with_capacity(num_buckets);
    for i in 0..num_buckets {
        guards.push(group.bucket(i).clone().write_owned().await);
    }

    let mut migrate_set = JoinSet::default();
    guards
        .drain(..)
        .enumerate()
        .for_each(|(bucket_index, guard)| {
            lore_spawn!(
                migrate_set,
                migrate_bucket(
                    bucket_index,
                    guard,
                    group.clone(),
                    path.clone(),
                    group_index,
                    store.gc_counters.clone()
                )
            );
        });

    let mut counts = MigrationCounts::default();
    let mut result = Ok(());
    while let Some(joined) = migrate_set.join_next().await {
        match joined
            .internal("Oodle migration bucket task failed")
            .map_err(LocalImmutableStoreError::from)
            .flatten()
        {
            Ok(bucket_counts) => {
                counts.migrated += bucket_counts.migrated;
                counts.invalid += bucket_counts.invalid;
                counts.durably_left += bucket_counts.durably_left;
            }
            Err(err) => result = result.and(Err(err)),
        }
    }
    result?;

    flush_locked_group(
        group.clone(),
        group_index,
        path.clone(),
        true, /* sync data */
    )
    .await?;

    lore_info!(
        "Group '{group_index}' migrated: {} Oodle migrated, {} Invalid Oodle, {} Left because durably stored",
        counts.migrated,
        counts.invalid,
        counts.durably_left
    );
    Ok(())
}

/// Re-encode groups from `first_group_to_migrate` down to 0, recording where to resume after each
/// one completes successfully. Reports whether every group completed.
pub async fn migrate_groups(
    store: Arc<LocalImmutableStore>,
    path: &Arc<PathBuf>,
    first_group_to_migrate: i32,
) -> Result<bool, LocalImmutableStoreError> {
    let mut group_to_migrate = first_group_to_migrate;
    let index_path = path.join("index");
    while group_to_migrate >= 0 {
        let group_index = group_to_migrate as usize;
        let group_path = crate::local::fan_out::group_dir_path(&index_path, group_index);

        // The walk covers a range of indices, but a group is materialized by the hash byte that
        // selects it, so on a store holding few fragments most of that range was never written.
        // Migrating one regardless would create its directory and level marker to hold nothing.
        if crate::local::fan_out::group_has_buckets(&group_path).await {
            lore_info!("Oodle local migration: migrating group {group_to_migrate}");

            // there could unknown errors with old Immutable stores. If we have problem
            // migrating a group then abort the migration but don't prevent the store from
            // eventually opening
            if let Err(err) = migrate_group(&store, path, group_index).await {
                lore_error!(
                    "Oodle local migration: failed to migrate group {group_to_migrate}: {err:?}"
                );
                return Ok(false);
            }
        }

        group_to_migrate -= 1;
        // there shouldn't be any reason why we can't write to the store info file, so error
        // hard in this case
        store
            .update_store_info(|info| info.next_group_index_to_migrate_oodle = group_to_migrate)
            .await?;
    }
    lore_info!("Oodle local migration: complete");
    Ok(true)
}
