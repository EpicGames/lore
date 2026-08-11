// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bytes::Bytes;
use bytes::BytesMut;
use lore_base::lore_spawn;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::CompressionMode;
use lore_storage::compress::COMPRESSION_MODE;
use rand::RngCore;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::lore::Address;
use crate::lore::Context;
use crate::lore::Fragment;
use crate::lore::Hash;
use crate::lore::RepositoryId;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_warn;
use crate::store::ImmutableStore;
use crate::store::StoreError;

async fn store_fragment(
    store: Arc<crate::store::immutable::ImmutableStore>,
    repository: RepositoryId,
    context: Context,
    payload: Bytes,
) -> Result<(), StoreError> {
    let hash = Hash::hash_buffer(&payload);

    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };

    let address = Address { hash, context };

    lore_debug!("Storing payload address: {address}");

    store
        .put(
            repository,
            address,
            fragment,
            Some(payload),
            false, /* force */
        )
        .await
}

pub static SEEDING_IN_PROGRESS: Semaphore = Semaphore::const_new(1);

pub async fn seed_local_store(
    store: Arc<crate::store::immutable::ImmutableStore>,
    max_size: usize,
    margin: usize,
    buffer_size: usize,
) -> Result<(), StoreError> {
    let _permit = SEEDING_IN_PROGRESS.try_acquire().map_err(|e| {
        lore_warn!("Could not acquire seeding permit: {e:?}");
        StoreError::internal("permit")
    })?;

    // Disable compression for the duration of the seeding process
    let previous_compression_mode =
        COMPRESSION_MODE.swap(CompressionMode::NoCompression as u32, Ordering::Release);

    let result = do_seed_local_store(store, max_size, margin, buffer_size).await;

    let _ = COMPRESSION_MODE.compare_exchange(
        CompressionMode::NoCompression as u32,
        previous_compression_mode,
        Ordering::Acquire,
        Ordering::Relaxed,
    );

    result
}

async fn do_seed_local_store(
    store: Arc<crate::store::immutable::ImmutableStore>,
    max_size: usize,
    margin: usize,
    buffer_size: usize,
) -> Result<(), StoreError> {
    let start = Instant::now();

    let repository = rand::random::<RepositoryId>();
    let context = rand::random::<Context>();

    lore_info!(
        "Beginning store seeding, max capacity: {max_size}, margin: {margin}. Using {repository} and context: {context}"
    );

    let size = store.packstore_total_size().await;
    let needed = max_size.saturating_sub(size + margin);

    lore_info!("Current store size is {size}, {needed} bytes needed");

    if needed == 0 {
        lore_info!("Store is already at desired capacity");
        return Ok(());
    }

    let fragment_count = needed / FRAGMENT_SIZE_THRESHOLD;

    let in_flight = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(buffer_size);

    // We don't expect this to ever fail in any realistic scenario where we'd be running seeding. If
    // it does, I'd rather we panic so we can investigate rather than run at lower than expected
    // concurrency.
    let task_count = std::thread::available_parallelism()
        .expect("could not get available parallelism")
        .get();

    let per_task_count = fragment_count / task_count;

    // Generation runs on dedicated threads rather than the runtime's pools: filling a buffer with
    // random bytes has nothing to await, and a capacity tool wants the whole machine rather than a
    // share of the process thread budget. Writes run on the runtime, bounded separately below, and
    // the channel between the two is what tunes disk utilization.

    lore_info!("Spawning {task_count} threads to generate payloads");
    for index in 0..task_count {
        let tx = tx.clone();
        let in_flight = in_flight.clone();

        std::thread::Builder::new()
            .name(format!("lore-seed-{index}"))
            .spawn(move || {
                let mut rng = rand::rng();
                let mut buffer = BytesMut::with_capacity(FRAGMENT_SIZE_THRESHOLD);

                for _ in 0..per_task_count {
                    buffer.resize(FRAGMENT_SIZE_THRESHOLD, 0);
                    rng.fill_bytes(&mut buffer);

                    if let Err(e) = tx.blocking_send(buffer.split().freeze()) {
                        // The receiver must've gone away (this shouldn't happen in a real world
                        // scenario).
                        lore_warn!("Failed to send fragment data: {e:?}");
                        return;
                    }

                    in_flight.fetch_add(1, Ordering::Relaxed);
                }
            })
            .expect("could not spawn payload generator thread");
    }

    // The generators hold the only remaining senders, so the receive loop below ends when the last
    // of them finishes.
    drop(tx);

    lore_info!("Listening for payloads");

    let mut join_set = JoinSet::new();

    // In local testing on a 16 core system we never exceeded 23 concurrent tasks, so this seems
    // like a reasonable limit.
    let semaphore = Arc::new(Semaphore::new(task_count * 2));

    let written = Arc::new(AtomicUsize::new(0));
    let processed = Arc::new(AtomicUsize::new(0));

    while let Some(bytes) = rx.recv().await {
        let store = store.clone();
        let written = written.clone();
        let processed = processed.clone();
        let in_flight = in_flight.clone();

        let permit = semaphore.clone().acquire_owned().await.unwrap();

        lore_spawn!(join_set, async move {
            let len = bytes.len();
            let result = store_fragment(store, repository, context, bytes).await;
            drop(permit);
            let active_count = in_flight.fetch_sub(1, Ordering::Relaxed);

            if result.is_ok() {
                written.fetch_add(len, Ordering::Relaxed);
            }

            if processed
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(100)
            {
                lore_info!("Seeding in flight: {active_count}");
            }

            result
        });
    }

    lore_info!("Waiting for tasks to complete");

    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            lore_warn!("Storing fragment failed: {e:?}");
        }
    }

    lore_info!(
        "Done seeding, wrote {} bytes across {} fragments in {:?}",
        written.load(Ordering::Relaxed),
        processed.load(Ordering::Relaxed),
        start.elapsed()
    );

    Ok(())
}
