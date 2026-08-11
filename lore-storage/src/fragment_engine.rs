// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cmp::min;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_transport::StorageSession;
use tokio::sync::Semaphore;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::chunker::FileChunker;
use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::concurrency::FRAGMENT_SIZE_EXPECTED;
use crate::concurrency::FRAGMENT_SIZE_MINIMUM;
use crate::error::StorageError;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::options::WriteOptions;
use crate::typed_bytes::TypedBytes;
use crate::typed_bytes::TypedBytesMut;
use crate::types::Address;
use crate::types::Context;
use crate::types::Fragment;
use crate::types::FragmentReference;
use crate::types::Partition;
use crate::write::store_fragment;

/// Figure out where to cut `buffer` into chunks, all in one go.
///
/// `cut_size` comes from [`WriteOptions::cut_size`], which is where the bound on a fixed chunk
/// size is applied; this cuts at whatever it is given.
fn chunk_boundaries(
    buffer: Bytes,
    cut_size: Option<usize>,
) -> Result<Vec<(usize, usize)>, StorageError> {
    let size = buffer.len();
    if let Some(step) = cut_size {
        Ok((0..size)
            .step_by(step)
            .map(|offset| (offset, (offset + step).min(size)))
            .collect())
    } else {
        let chunker = fastcdc::v2020::FastCDC::with_level(
            buffer.as_ref(),
            FRAGMENT_SIZE_MINIMUM as u32,
            FRAGMENT_SIZE_EXPECTED as u32,
            FRAGMENT_SIZE_THRESHOLD as u32,
            fastcdc::v2020::Normalization::Level1,
        );
        Ok(chunker.map(|c| (c.offset, c.offset + c.length)).collect())
    }
}

/// Cuts `buffer` into chunks and stores each one via [`store_fragment`].
///
/// Uses `FastCDC` (content-defined chunking) when `flags.fixed_size_chunk` is 0,
/// or fixed-size chunking when it is >0. Each chunk is hashed, stored as a
/// content-addressed fragment in the immutable `store`, and assembled into a
/// fragment list. Returns the root address.
///
/// When the entire buffer fits in a single chunk, the single-fragment fast path
/// is used and no fragment list is created. In `hash_only` mode, fragments are
/// not actually stored — only their hashes are computed.
#[allow(clippy::too_many_arguments)]
pub async fn write_fragmented(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    hash_only: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<crate::write_tracker::WriteTracker>>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Address, StorageError> {
    let size = buffer.len();
    let mut read_permit = permit;
    let mut tasks = JoinSet::<Result<StoredChunk, StorageError>>::new();

    lore_base::lore_trace!(
        "Write and fragment buffer to immutable store: {size} bytes representing {size} bytes (flags {flags:?})",
    );
    let chunk_boundaries = chunk_boundaries(buffer.clone(), flags.cut_size())?;

    for (chunk_index, (chunk_offset, chunk_end)) in chunk_boundaries.into_iter().enumerate() {
        let chunk_size = chunk_end - chunk_offset;
        let chunk_buffer = buffer.slice(chunk_offset..chunk_end);

        let fragment = Fragment {
            flags: flags.into(),
            size_payload: chunk_size as u32,
            size_content: chunk_size as u64,
        };

        let chunk_permit = if hash_only {
            None
        } else {
            let needed = crate::concurrency::fragment_permit_count(chunk_size) as usize;
            match read_permit.as_mut().and_then(|permit| permit.split(needed)) {
                Some(permit) => Some(permit),
                None => crate::concurrency::acquire_fragment_memory_permit(chunk_size).await,
            }
        };

        if chunk_offset == 0 && chunk_size == size {
            // Everything was put in a single fragment
            let hash = hash::hash_slice(chunk_buffer.as_ref());
            let result = store_fragment(
                store,
                partition,
                Address { context, hash },
                fragment,
                chunk_buffer,
                flags.local_cache_priority,
                remote_session,
                tracker,
                chunk_permit,
            )
            .await?;
            return Ok(result.address);
        }

        let store = store.clone();
        let session = remote_session.clone();
        let task_tracker = tracker.clone();
        lore_base::lore_spawn!(tasks, async move {
            let hash = hash::hash_slice(chunk_buffer.as_ref());
            let chunk_address = if hash_only {
                Address { context, hash }
            } else {
                let result = store_fragment(
                    store,
                    partition,
                    Address { context, hash },
                    fragment,
                    chunk_buffer,
                    flags.local_cache_priority,
                    session,
                    task_tracker,
                    chunk_permit,
                )
                .await?;
                result.address
            };
            Ok(StoredChunk {
                index: chunk_index,
                content_offset: chunk_offset,
                address: chunk_address,
            })
        });
    }

    drop(read_permit);

    let chunk_count = tasks.len();
    write_chunk_list(
        tasks,
        ChunkResults::default(),
        chunk_count,
        store,
        partition,
        context,
        size,
        flags,
        hash_only,
        remote_session,
        tracker,
    )
    .await
}

/// Cuts a file into chunks with [`FileChunker`] and stores each one via
/// [`store_fragment`], without ever holding the file whole. Boundaries are identical
/// to fragmenting the same bytes in memory; see [`crate::chunker`].
///
/// Each chunk's memory permit is taken before its task is spawned, so the chunker
/// stops reading ahead once the fragment limiter saturates: peak residency is bounded
/// by the limiter rather than by the file size.
///
/// A chunk that cannot get a permit uses the one chunk [`FileChunker`] reserved, held as a
/// single-permit slot. Either way the budget travels into the write with the buffer it
/// covers and is released where that buffer is dropped — which is not where this loop
/// dispatched it: a tracker hands the write to a detached leader task, so a budget kept
/// behind here would stop accounting for bytes still resident and, being free again at
/// once, would let this loop stream the whole file into detached writes uncharged.
///
/// Only when the slot is occupied too does the loop wait, for whichever of a permit or the
/// slot comes back first — the chunk holding the slot needs no budget to finish, so the
/// wait always resolves and the window is never held waiting on budget only this file
/// could release.
///
/// This is only reached for files larger than one fragment, so the single-fragment
/// fast path in [`write_fragmented`] cannot apply — no chunk ever exceeds
/// `FRAGMENT_SIZE_THRESHOLD`, so such a file always yields at least two chunks.
#[allow(clippy::too_many_arguments)]
pub async fn write_fragmented_from_file(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    file: lore_io::IoFile,
    size: usize,
    flags: WriteOptions,
    hash_only: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<crate::write_tracker::WriteTracker>>,
) -> Result<Address, StorageError> {
    let mut tasks = JoinSet::<Result<StoredChunk, StorageError>>::new();

    lore_base::lore_trace!(
        "Write and fragment file to immutable store: {size} bytes (flags {flags:?})",
    );

    let file_size = size as u64;
    let mut chunker = if let Some(step) = flags.cut_size() {
        FileChunker::fixed_size(file, file_size, step).await
    } else {
        FileChunker::content_defined(file, file_size).await
    };

    // The chunk the chunker pre-paid, as a slot so its use is tracked rather than inferred.
    let reserved_chunk = Arc::new(Semaphore::new(1));

    let mut results = ChunkResults::default();
    let mut chunk_index = 0usize;
    while let Some(chunk) = chunker.next_chunk().await? {
        results.reap(&mut tasks);
        if results.failure.is_some() {
            break;
        }

        let chunk_offset = chunk.offset as usize;
        let chunk_buffer = chunk.data;
        let chunk_size = chunk_buffer.len();

        let fragment = Fragment {
            flags: flags.into(),
            size_payload: chunk_size as u32,
            size_content: chunk_size as u64,
        };

        let chunk_budget =
            crate::concurrency::acquire_chunk_budget(chunk_size, &reserved_chunk).await;

        let store = store.clone();
        let session = remote_session.clone();
        let task_tracker = tracker.clone();
        lore_base::lore_spawn!(tasks, async move {
            let hash = hash::hash_slice(chunk_buffer.as_ref());
            let chunk_address = if hash_only {
                Address { context, hash }
            } else {
                let result = store_fragment(
                    store,
                    partition,
                    Address { context, hash },
                    fragment,
                    chunk_buffer,
                    flags.local_cache_priority,
                    session,
                    task_tracker,
                    chunk_budget,
                )
                .await?;
                result.address
            };
            Ok(StoredChunk {
                index: chunk_index,
                content_offset: chunk_offset,
                address: chunk_address,
            })
        });
        chunk_index += 1;
    }

    // Holding a window while the list waits for budget is the hold-and-wait the
    // reservation exists to avoid.
    drop(chunker);

    write_chunk_list(
        tasks,
        results,
        chunk_index,
        store,
        partition,
        context,
        size,
        flags,
        hash_only,
        remote_session,
        tracker,
    )
    .await
}

/// What one stored chunk reports back: where its reference belongs in the fragment list, where
/// its bytes begin in the content, and the address it hashed to.
struct StoredChunk {
    index: usize,
    content_offset: usize,
    address: Address,
}

/// Chunk store results gathered so far, so a file can drain its in-flight tasks part
/// way through without losing what already finished.
#[derive(Default)]
struct ChunkResults {
    entries: Vec<StoredChunk>,
    failure: Option<StorageError>,
}

impl ChunkResults {
    /// Await every task currently in `tasks`, keeping the first error seen.
    async fn drain(&mut self, tasks: &mut JoinSet<Result<StoredChunk, StorageError>>) {
        while let Some(result) = tasks.join_next().await {
            self.record(result);
        }
    }

    /// Collect the tasks that have already finished, without waiting for any.
    fn reap(&mut self, tasks: &mut JoinSet<Result<StoredChunk, StorageError>>) {
        while let Some(result) = tasks.try_join_next() {
            self.record(result);
        }
    }

    fn record(&mut self, result: Result<Result<StoredChunk, StorageError>, JoinError>) {
        match result
            .map_err(|e| StorageError::internal_with_context(e, "task failure"))
            .and_then(|r| r)
        {
            Ok(entry) => self.entries.push(entry),
            Err(err) => self.failure = self.failure.take().or(Some(err)),
        }
    }
}

/// Joins the per-chunk store tasks into a fragment reference list, then Merklizes it.
///
/// The list is reserved before it is allocated and the reservation travels into the write: a
/// list outlives the chunks it names, going on to be compressed, uploaded and stored.
#[allow(clippy::too_many_arguments)]
async fn write_chunk_list(
    mut tasks: JoinSet<Result<StoredChunk, StorageError>>,
    mut results: ChunkResults,
    chunk_count: usize,
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    size: usize,
    flags: WriteOptions,
    hash_only: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<crate::write_tracker::WriteTracker>>,
) -> Result<Address, StorageError> {
    results.drain(&mut tasks).await;

    if let Some(err) = results.failure {
        return Err(err);
    }

    if chunk_count == 0 {
        return Err(StorageError::internal(format!(
            "no chunks were written for {size} bytes of content"
        )));
    }

    let list_bytes = chunk_count * std::mem::size_of::<FragmentReference>();
    let list_permit = crate::concurrency::acquire_fragment_memory_permit(list_bytes).await;

    let mut list_buffer = BytesMut::with_count_capacity::<FragmentReference>(chunk_count);
    let list = list_buffer.as_type_slice_mut::<FragmentReference>();

    for chunk in results.entries {
        list[chunk.index].hash = chunk.address.hash;
        list[chunk.index].offset_content = chunk.content_offset as u64;
    }

    // Safety: one entry per chunk was written above, and the capacity was sized for that many.
    unsafe {
        list_buffer.set_count::<FragmentReference>(chunk_count);
    }

    write_fragmentlist(
        store,
        partition,
        context,
        list_buffer.freeze(),
        size,
        flags,
        hash_only,
        remote_session,
        tracker,
        list_permit,
    )
    .await
}

/// Helper function to write a list of fragment references
///
/// `permit` covers `buffer`, which the caller reserved before allocating it. It is reused for a
/// list that fits one fragment and split per chunk for one that does not, so the bytes are
/// charged once however deep the tree goes.
///
/// The next level is reserved while this level's chunks still hold their splits. That cannot
/// deadlock: nothing holding a chunk permit ever waits on the budget, so every holder drains
/// regardless.
#[allow(clippy::too_many_arguments)]
async fn write_fragmentlist_impl(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    content_size: usize,
    flags: WriteOptions,
    hash_only: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<crate::write_tracker::WriteTracker>>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Address, StorageError> {
    let size = buffer.len();

    if size <= FRAGMENT_SIZE_THRESHOLD {
        let hash = hash::hash_slice(buffer.as_ref());
        let fragment = Fragment {
            flags: flags.as_u32() | FragmentFlags::PayloadFragmented,
            size_payload: size as u32,
            size_content: content_size as u64,
        };
        if hash_only {
            Ok(Address { context, hash })
        } else {
            let permit = match permit {
                Some(permit) => Some(permit),
                None => crate::concurrency::acquire_fragment_memory_permit(buffer.len()).await,
            };
            let result = store_fragment(
                store,
                partition,
                Address { context, hash },
                fragment,
                buffer,
                true, /* Fragment lists have local priority */
                remote_session,
                tracker,
                permit,
            )
            .await?;
            Ok(result.address)
        }
    } else {
        // Fixed size chunking for fragment list
        let mut list_permit = permit;
        let mut tasks = JoinSet::<Result<StoredChunk, StorageError>>::new();
        let mut chunk_index = 0usize;
        let mut chunk_offset = 0;
        let mut chunk_content_offset = 0;

        let max_fragment_ref_count =
            FRAGMENT_SIZE_THRESHOLD / std::mem::size_of::<FragmentReference>();
        let max_chunk_size = std::mem::size_of::<FragmentReference>() * max_fragment_ref_count;

        let buffer = buffer.to_aligned::<FragmentReference>();
        let fragment_references = buffer.as_type_slice::<FragmentReference>();

        while chunk_offset < size {
            let chunk_size = min(size - chunk_offset, max_chunk_size);
            let chunk_buffer = buffer.slice(chunk_offset..(chunk_offset + chunk_size));

            let chunk_fragment_ref_index = chunk_offset / std::mem::size_of::<FragmentReference>();
            let next_fragment_ref_index =
                chunk_fragment_ref_index + (chunk_size / std::mem::size_of::<FragmentReference>());

            let chunk_content_size = if chunk_offset + chunk_size < size {
                fragment_references[next_fragment_ref_index].offset_content as usize
            } else {
                content_size
            } - fragment_references[chunk_fragment_ref_index]
                .offset_content as usize;

            let fragment = Fragment {
                flags: flags.as_u32() | FragmentFlags::PayloadFragmented,
                size_payload: chunk_size as u32,
                size_content: chunk_content_size as u64,
            };

            let chunk_permit = if hash_only {
                None
            } else {
                let needed = crate::concurrency::fragment_permit_count(chunk_size) as usize;
                match list_permit.as_mut().and_then(|permit| permit.split(needed)) {
                    Some(permit) => Some(permit),
                    None => crate::concurrency::acquire_fragment_memory_permit(chunk_size).await,
                }
            };

            let store = store.clone();
            let session = remote_session.clone();
            let task_tracker = tracker.clone();
            lore_base::lore_spawn!(tasks, async move {
                let hash = hash::hash_slice(chunk_buffer.as_ref());
                let chunk_address = if hash_only {
                    Address { context, hash }
                } else {
                    let permit = chunk_permit;
                    let result = store_fragment(
                        store,
                        partition,
                        Address { context, hash },
                        fragment,
                        chunk_buffer,
                        flags.local_cache_priority,
                        session,
                        task_tracker,
                        permit,
                    )
                    .await?;
                    result.address
                };
                Ok(StoredChunk {
                    index: chunk_index,
                    content_offset: chunk_content_offset,
                    address: chunk_address,
                })
            });

            chunk_content_offset += chunk_content_size;
            chunk_offset += chunk_size;
            chunk_index += 1;
        }
        drop(buffer);

        let list_count = tasks.len();
        let next_bytes = list_count * std::mem::size_of::<FragmentReference>();
        let next_permit = crate::concurrency::acquire_fragment_memory_permit(next_bytes).await;
        let mut list_buffer = BytesMut::with_count_capacity::<FragmentReference>(list_count);
        let list = list_buffer.as_type_slice_mut::<FragmentReference>();

        let mut failure = None;
        while let Some(result) = tasks.join_next().await {
            match result
                .map_err(|e| StorageError::internal_with_context(e, "task failure"))
                .and_then(|r| r)
            {
                Ok(chunk) => {
                    list[chunk.index].hash = chunk.address.hash;
                    list[chunk.index].offset_content = chunk.content_offset as u64;
                }
                Err(err) => {
                    failure = failure.or(Some(err));
                }
            }
        }

        if let Some(err) = failure {
            return Err(err);
        }
        drop(list_permit);

        // Safety: one entry per chunk was written above, and the capacity was sized for that many.
        unsafe {
            list_buffer.set_count::<FragmentReference>(list_count);
        }
        let buffer = list_buffer.freeze();

        write_fragmentlist(
            store,
            partition,
            context,
            buffer,
            content_size,
            flags,
            hash_only,
            remote_session,
            tracker,
            next_permit,
        )
        .await
    }
}

/// Helper function to enforce compiler breaking the async recursion chain
/// and pinning the future on heap allocation.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn write_fragmentlist(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    content_size: usize,
    flags: WriteOptions,
    hash_only: bool,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<crate::write_tracker::WriteTracker>>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Address, StorageError>> + Send>> {
    Box::pin(write_fragmentlist_impl(
        store,
        partition,
        context,
        buffer,
        content_size,
        flags,
        hash_only,
        remote_session,
        tracker,
        permit,
    ))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    /// Make a buffer of random bytes so chunking has to actually cut it.
    fn mixed_pattern_buffer(size: usize) -> Bytes {
        use rand::Rng;
        let mut data = vec![0u8; size];
        rand::rng().fill(&mut data[..]);
        Bytes::from(data)
    }

    /// The builder clamps, but `fixed_size_chunk` is `pub`, so a struct literal reaches the
    /// cutting paths without passing through it. Both paths ask `cut_size`, which is why an
    /// oversized request cannot produce a leaf chunk that no store would accept.
    #[tokio::test]
    async fn an_oversized_fixed_chunk_request_is_bounded_on_both_paths() {
        let flags = WriteOptions {
            fixed_size_chunk: 4 * FRAGMENT_SIZE_THRESHOLD,
            ..WriteOptions::default()
        };
        assert_eq!(flags.cut_size(), Some(FRAGMENT_SIZE_THRESHOLD));

        let buffer = mixed_pattern_buffer(3 * FRAGMENT_SIZE_THRESHOLD);
        let actual = chunk_boundaries(buffer.clone(), flags.cut_size()).expect("chunking succeeds");

        assert!(
            actual
                .iter()
                .all(|&(start, end)| end - start <= FRAGMENT_SIZE_THRESHOLD),
            "a leaf chunk above the threshold is unstorable and reads back as a sublist"
        );
        assert_eq!(actual.first().copied(), Some((0, FRAGMENT_SIZE_THRESHOLD)));
        assert_eq!(actual.last().unwrap().1, buffer.len());
    }

    /// Content-defined chunking is what a zero request means, not a zero-length cut.
    #[tokio::test]
    async fn a_zero_fixed_chunk_request_means_content_defined() {
        assert_eq!(WriteOptions::default().cut_size(), None);
    }

    #[tokio::test]
    async fn fastcdc_batch_handles_buffer_smaller_than_min_chunk() {
        // Tiny buffer — should be one chunk covering the whole thing.
        let buffer = mixed_pattern_buffer(1024);
        let actual = chunk_boundaries(buffer.clone(), None).expect("chunking succeeds");
        assert_eq!(actual, vec![(0, buffer.len())]);
    }

    /// A file holding less than the size it was measured at is the truncation race: the size
    /// comes from `metadata()`, taken before the chunker ever opens the file. The chunker reads
    /// exactly the length that size promises, so the read itself rejects the file. Either way it
    /// has to fail rather than describe content that was never written, since zero-length
    /// content is the zero hash and no fragment list stands in for nothing.
    #[tokio::test]
    async fn a_file_holding_less_than_its_measured_size_is_rejected() {
        let dir = crate::test_util::TempDir::new("lore-storage-truncated-");
        let path = std::path::Path::new(dir.as_ref()).join("truncated");
        std::fs::write(&path, b"").expect("create empty file");
        let (file, _) = crate::chunker::open_read(&path).await.expect("open file");
        let store = crate::local::immutable_store::LocalImmutableStore::new(
            Some(std::path::PathBuf::from(dir.as_ref())),
            crate::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");

        let err = write_fragmented_from_file(
            store,
            Partition::from([1u8; 16]),
            Context::from([1u8; 16]),
            file,
            4 * FRAGMENT_SIZE_THRESHOLD,
            WriteOptions::default(),
            true,
            None,
            None,
        )
        .await
        .expect_err("a fragment list with no entries must not be written");

        assert!(
            format!("{err}").contains("file ended before the requested read length"),
            "unexpected failure: {err}"
        );
    }

    #[tokio::test]
    async fn fastcdc_batch_handles_empty_buffer() {
        let buffer = Bytes::new();
        let actual = chunk_boundaries(buffer, None).expect("chunking succeeds");
        assert!(actual.is_empty());
    }

    #[tokio::test]
    async fn fastcdc_batch_handles_pathological_all_zero_buffer() {
        // All-zero input — the rolling hash never matches, but the split should still be clean.
        let buffer = Bytes::from(vec![0u8; 512 * 1024]);
        let actual = chunk_boundaries(buffer.clone(), None).expect("chunking succeeds");
        let reconstructed_size: usize = actual.iter().map(|(s, e)| e - s).sum();
        assert_eq!(reconstructed_size, buffer.len());
        assert!(actual.iter().all(|&(s, e)| s < e));
        assert_eq!(actual.first().copied(), Some((0, actual[0].1)));
        assert_eq!(actual.last().unwrap().1, buffer.len());
    }

    #[tokio::test]
    async fn fixed_size_chunking_covers_whole_buffer() {
        let buffer = mixed_pattern_buffer(10 * 1024 + 17);
        let step = 1024;
        let actual = chunk_boundaries(buffer.clone(), Some(step)).expect("chunking succeeds");

        let expected: Vec<(usize, usize)> = (0..buffer.len())
            .step_by(step)
            .map(|o| (o, (o + step).min(buffer.len())))
            .collect();
        assert_eq!(actual, expected);

        let reconstructed_size: usize = actual.iter().map(|(s, e)| e - s).sum();
        assert_eq!(reconstructed_size, buffer.len());
    }

    #[tokio::test]
    async fn fixed_size_chunking_handles_buffer_smaller_than_step() {
        let buffer = Bytes::from(vec![1u8; 100]);
        let actual = chunk_boundaries(buffer.clone(), Some(1024)).expect("chunking succeeds");
        assert_eq!(actual, vec![(0, buffer.len())]);
    }
}
