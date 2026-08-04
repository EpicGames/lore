// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Bounded-memory ingestion of raw content streams into Lore's immutable
//! fragment graph.

use std::mem::size_of;
use std::sync::Arc;

use bytes::Bytes;
use bytes::BytesMut;
use lore_transport::StorageSession;
use tokio::io::AsyncRead;
use tokio_stream::StreamExt;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

use crate::compress::FRAGMENT_SIZE_THRESHOLD;
use crate::concurrency::FRAGMENT_SIZE_EXPECTED;
use crate::concurrency::FRAGMENT_SIZE_MINIMUM;
use crate::error::StorageError;
use crate::fragment_flags::FragmentFlags;
use crate::hash;
use crate::immutable_store::ImmutableStore;
use crate::options::WriteOptions;
use crate::types::Address;
use crate::types::Context;
use crate::types::Fragment;
use crate::types::FragmentReference;
use crate::types::Hash;
use crate::types::Partition;
use crate::write::StoreResult;
use crate::write::store_fragment;
use crate::write_tracker::WriteTracker;

const MAX_FRAGMENT_TREE_DEPTH: usize = 8;

/// Errors specific to raw stream framing/integrity, plus failures from the
/// underlying immutable fragment store.
#[derive(Debug)]
pub enum ContentStreamError {
    InvalidArguments(String),
    Storage(StorageError),
}

impl std::fmt::Display for ContentStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArguments(reason) => formatter.write_str(reason),
            Self::Storage(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ContentStreamError {}

impl From<StorageError> for ContentStreamError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Stream raw content into Lore's immutable store using the same FastCDC and
/// fragment graph encoding as [`crate::write_content`]. Memory is bounded by
/// one CDC chunk plus at most one fragment-reference page at each tree level.
///
/// `expected_size` is an integrity assertion, not a memory allocation hint.
/// A mismatch is reported after EOF; fragments written before the mismatch are
/// harmless unreachable content-addressed objects.
#[allow(clippy::too_many_arguments)]
pub async fn write_content_stream<R>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    reader: R,
    expected_size: Option<u64>,
    max_size: Option<u64>,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<WriteTracker>>,
) -> Result<(Address, Fragment, u64), ContentStreamError>
where
    R: AsyncRead + Unpin,
{
    write_content_stream_impl(
        store,
        partition,
        context,
        reader,
        expected_size,
        max_size,
        flags,
        remote_session,
        tracker,
        FRAGMENT_SIZE_THRESHOLD / size_of::<FragmentReference>(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn write_content_stream_impl<R>(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    reader: R,
    expected_size: Option<u64>,
    max_size: Option<u64>,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<WriteTracker>>,
    references_per_page: usize,
) -> Result<(Address, Fragment, u64), ContentStreamError>
where
    R: AsyncRead + Unpin,
{
    if references_per_page < 2 {
        return Err(StorageError::internal(
            "fragment reference pages must hold at least two entries",
        )
        .into());
    }

    let mut chunker = fastcdc::v2020::AsyncStreamCDC::with_level(
        reader,
        FRAGMENT_SIZE_MINIMUM as u32,
        FRAGMENT_SIZE_EXPECTED as u32,
        FRAGMENT_SIZE_THRESHOLD as u32,
        fastcdc::v2020::Normalization::Level1,
    );
    let chunks = chunker.as_stream();
    tokio::pin!(chunks);

    let mut first: Option<(StoreResult, u64)> = None;
    let mut builder = FragmentListBuilder::new(
        store.clone(),
        partition,
        context,
        flags,
        remote_session.clone(),
        tracker.clone(),
        references_per_page,
    );
    let mut actual_size = 0u64;

    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| match error {
            fastcdc::v2020::Error::IoError(error)
                if error.kind() == std::io::ErrorKind::InvalidData =>
            {
                ContentStreamError::InvalidArguments(error.to_string())
            }
            error => ContentStreamError::from(StorageError::internal_with_context(
                error,
                "stream chunking failed",
            )),
        })?;
        let offset = chunk.offset;
        let chunk_size = u64::try_from(chunk.length)
            .map_err(|error| StorageError::internal_with_context(error, "chunk size overflow"))?;
        actual_size = offset
            .checked_add(chunk_size)
            .ok_or_else(|| StorageError::internal("stream size exceeds u64"))?;
        if let Some(max_size) = max_size
            && actual_size > max_size
        {
            return Err(StorageError::from(crate::errors::Oversized {
                context: format!(
                    "streamed content size exceeds configured upload limit {max_size}"
                ),
            })
            .into());
        }

        let stored = store_leaf(
            store.clone(),
            partition,
            context,
            Bytes::from(chunk.data),
            flags,
            remote_session.clone(),
            tracker.clone(),
        )
        .await?;

        if let Some((first_stored, first_offset)) = first.take() {
            builder
                .push(FragmentReference {
                    hash: first_stored.address.hash,
                    offset_content: first_offset,
                })
                .await?;
            builder
                .push(FragmentReference {
                    hash: stored.address.hash,
                    offset_content: offset,
                })
                .await?;
        } else if builder.is_empty() {
            first = Some((stored, offset));
        } else {
            builder
                .push(FragmentReference {
                    hash: stored.address.hash,
                    offset_content: offset,
                })
                .await?;
        }
    }

    if let Some(expected_size) = expected_size
        && expected_size != actual_size
    {
        return Err(ContentStreamError::InvalidArguments(format!(
            "expected content size {expected_size} does not match streamed size {actual_size}"
        )));
    }

    if let Some((stored, _)) = first {
        return Ok((stored.address, stored.fragment, actual_size));
    }
    if builder.is_empty() {
        return Ok((
            Address {
                context,
                hash: Hash::new_zeroed(),
            },
            Fragment::new_zeroed(),
            0,
        ));
    }

    let stored = builder.finish(actual_size).await?;
    Ok((stored.address, stored.fragment, actual_size))
}

#[allow(clippy::too_many_arguments)]
async fn store_leaf(
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    buffer: Bytes,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<WriteTracker>>,
) -> Result<StoreResult, StorageError> {
    let address = Address {
        context,
        hash: hash::hash_slice(buffer.as_ref()),
    };
    let fragment = Fragment {
        flags: flags.into(),
        size_payload: buffer.len() as u32,
        size_content: buffer.len() as u64,
    };
    let permit = crate::concurrency::acquire_fragment_memory_permit(buffer.len()).await;
    store_fragment(
        store,
        partition,
        address,
        fragment,
        buffer,
        flags.local_cache_priority,
        remote_session,
        tracker,
        permit,
    )
    .await
}

struct FragmentListBuilder {
    store: Arc<dyn ImmutableStore>,
    partition: Partition,
    context: Context,
    flags: WriteOptions,
    remote_session: Option<Arc<StorageSession>>,
    tracker: Option<Arc<WriteTracker>>,
    references_per_page: usize,
    levels: Vec<Vec<FragmentReference>>,
}

impl FragmentListBuilder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<dyn ImmutableStore>,
        partition: Partition,
        context: Context,
        flags: WriteOptions,
        remote_session: Option<Arc<StorageSession>>,
        tracker: Option<Arc<WriteTracker>>,
        references_per_page: usize,
    ) -> Self {
        Self {
            store,
            partition,
            context,
            flags,
            remote_session,
            tracker,
            references_per_page,
            levels: vec![Vec::with_capacity(references_per_page + 1)],
        }
    }

    fn is_empty(&self) -> bool {
        self.levels.iter().all(Vec::is_empty)
    }

    async fn push(&mut self, reference: FragmentReference) -> Result<(), StorageError> {
        let mut level = 0usize;
        let mut reference = reference;
        loop {
            if level >= MAX_FRAGMENT_TREE_DEPTH {
                return Err(StorageError::internal(format!(
                    "fragment tree depth exceeds {MAX_FRAGMENT_TREE_DEPTH}"
                )));
            }
            if self.levels.len() <= level {
                self.levels
                    .push(Vec::with_capacity(self.references_per_page + 1));
            }
            let entries = &mut self.levels[level];
            entries.push(reference);
            if entries.len() <= self.references_per_page {
                return Ok(());
            }

            let end_offset = entries
                .last()
                .expect("overflow page has a lookahead entry")
                .offset_content;
            let page = entries
                .drain(..self.references_per_page)
                .collect::<Vec<_>>();
            let start_offset = page[0].offset_content;
            let stored = self
                .store_page(&page, start_offset, end_offset, false)
                .await?;
            reference = FragmentReference {
                hash: stored.address.hash,
                offset_content: start_offset,
            };
            level += 1;
        }
    }

    async fn finish(mut self, content_size: u64) -> Result<StoreResult, StorageError> {
        let mut level = 0usize;
        loop {
            let has_higher = self
                .levels
                .iter()
                .skip(level + 1)
                .any(|entries| !entries.is_empty());
            let page = std::mem::take(
                self.levels
                    .get_mut(level)
                    .ok_or_else(|| StorageError::internal("missing fragment tree level"))?,
            );
            if page.is_empty() {
                return Err(StorageError::internal("empty fragment reference page"));
            }
            let start_offset = page[0].offset_content;
            let stored = self
                .store_page(&page, start_offset, content_size, !has_higher)
                .await?;
            if !has_higher {
                return Ok(stored);
            }

            self.push_at_level(
                level + 1,
                FragmentReference {
                    hash: stored.address.hash,
                    offset_content: start_offset,
                },
            )
            .await?;
            level += 1;
        }
    }

    async fn push_at_level(
        &mut self,
        mut level: usize,
        mut reference: FragmentReference,
    ) -> Result<(), StorageError> {
        loop {
            if level >= MAX_FRAGMENT_TREE_DEPTH {
                return Err(StorageError::internal(format!(
                    "fragment tree depth exceeds {MAX_FRAGMENT_TREE_DEPTH}"
                )));
            }
            if self.levels.len() <= level {
                self.levels
                    .push(Vec::with_capacity(self.references_per_page + 1));
            }
            let entries = &mut self.levels[level];
            entries.push(reference);
            if entries.len() <= self.references_per_page {
                return Ok(());
            }
            let end_offset = entries
                .last()
                .expect("overflow page has a lookahead entry")
                .offset_content;
            let page = entries
                .drain(..self.references_per_page)
                .collect::<Vec<_>>();
            let start_offset = page[0].offset_content;
            let stored = self
                .store_page(&page, start_offset, end_offset, false)
                .await?;
            reference = FragmentReference {
                hash: stored.address.hash,
                offset_content: start_offset,
            };
            level += 1;
        }
    }

    async fn store_page(
        &self,
        references: &[FragmentReference],
        start_offset: u64,
        end_offset: u64,
        root: bool,
    ) -> Result<StoreResult, StorageError> {
        let size_content = end_offset
            .checked_sub(start_offset)
            .ok_or_else(|| StorageError::internal("fragment page offsets are not monotonic"))?;
        let payload_size = references
            .len()
            .checked_mul(size_of::<FragmentReference>())
            .ok_or_else(|| StorageError::internal("fragment page payload size overflow"))?;
        if payload_size > FRAGMENT_SIZE_THRESHOLD {
            return Err(StorageError::internal(
                "fragment reference page exceeds fragment threshold",
            ));
        }
        let mut payload = BytesMut::with_capacity(payload_size);
        for reference in references {
            payload.extend_from_slice(reference.as_bytes());
        }
        let payload = payload.freeze();
        let address = Address {
            context: self.context,
            hash: hash::hash_slice(payload.as_ref()),
        };
        let fragment = Fragment {
            flags: self.flags.as_u32() | FragmentFlags::PayloadFragmented.bits(),
            size_payload: payload.len() as u32,
            size_content,
        };
        let permit = crate::concurrency::acquire_fragment_memory_permit(payload.len()).await;
        store_fragment(
            self.store.clone(),
            self.partition,
            address,
            fragment,
            payload,
            root || self.flags.local_cache_priority,
            self.remote_session.clone(),
            self.tracker.clone(),
            permit,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::local::immutable_store::ImmutableStoreSettings;
    use crate::local::immutable_store::LocalImmutableStore;
    use crate::options::ReadOptions;
    use crate::test_util::TempDir;

    async fn make_test_store(label: &str) -> (TempDir, Arc<dyn ImmutableStore>) {
        let dir = TempDir::new(label);
        let store = LocalImmutableStore::new(
            Some(PathBuf::from(dir.as_ref())),
            ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");
        (dir, store)
    }

    fn content(size: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9u32;
        (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn stream_writer_matches_buffer_writer_fragment_graph() {
        let (_dir, store) = make_test_store("lore-stream-parity-").await;
        let partition = Partition::from([0x31; 16]);
        let context = Context::from([0x41; 16]);
        let payload = content(FRAGMENT_SIZE_THRESHOLD * 4 + 17);

        let (stream_address, stream_fragment, size) = write_content_stream(
            store.clone(),
            partition,
            context,
            Cursor::new(payload.clone()),
            Some(payload.len() as u64),
            None,
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .expect("stream write");
        let (buffer_address, buffer_fragment) = crate::write_content(
            store,
            partition,
            context,
            Bytes::from(payload),
            WriteOptions::default(),
            None,
            None,
            None,
        )
        .await
        .expect("buffer write");

        assert_eq!(size, buffer_fragment.size_content);
        assert_eq!(stream_address, buffer_address);
        assert_eq!(stream_fragment.size_content, buffer_fragment.size_content);
        assert_eq!(stream_fragment.size_payload, buffer_fragment.size_payload);
    }

    #[tokio::test]
    async fn stream_writer_handles_fragment_threshold_boundaries() {
        let (_dir, store) = make_test_store("lore-stream-boundaries-").await;
        let partition = Partition::from([0x35; 16]);

        for (index, size) in [
            FRAGMENT_SIZE_THRESHOLD - 1,
            FRAGMENT_SIZE_THRESHOLD,
            FRAGMENT_SIZE_THRESHOLD + 1,
        ]
        .into_iter()
        .enumerate()
        {
            let payload = content(size);
            let context = Context::from([0x50 + index as u8; 16]);
            let (address, fragment, actual) = write_content_stream(
                store.clone(),
                partition,
                context,
                Cursor::new(payload.clone()),
                Some(size as u64),
                None,
                WriteOptions::default(),
                None,
                None,
            )
            .await
            .expect("boundary stream write");
            assert_eq!(actual, size as u64);
            assert_eq!(fragment.size_content, size as u64);
            let restored = crate::read(
                store.clone(),
                partition,
                address,
                None,
                ReadOptions::default(),
                None,
            )
            .await
            .expect("boundary content read");
            assert_eq!(restored.as_ref(), payload.as_slice());
        }
    }

    #[tokio::test]
    async fn stream_writer_reassembles_incremental_multilevel_lists() {
        let (_dir, store) = make_test_store("lore-stream-multilevel-").await;
        let partition = Partition::from([0x32; 16]);
        let context = Context::from([0x42; 16]);
        let payload = content(FRAGMENT_SIZE_THRESHOLD * 6 + 29);

        let (address, fragment, size) = write_content_stream_impl(
            store.clone(),
            partition,
            context,
            Cursor::new(payload.clone()),
            None,
            None,
            WriteOptions::default(),
            None,
            None,
            2,
        )
        .await
        .expect("stream write with tiny list pages");
        assert_ne!(fragment.flags & FragmentFlags::PayloadFragmented.bits(), 0);
        assert_eq!(size, payload.len() as u64);

        let restored = crate::read(
            store,
            partition,
            address,
            None,
            ReadOptions::default(),
            None,
        )
        .await
        .expect("reassemble streamed fragment tree");
        assert_eq!(restored.as_ref(), payload.as_slice());
    }

    #[tokio::test]
    async fn stream_writer_rejects_expected_size_mismatch() {
        let (_dir, store) = make_test_store("lore-stream-size-").await;
        let payload = content(4096);
        let error = write_content_stream(
            store,
            Partition::from([0x33; 16]),
            Context::from([0x43; 16]),
            Cursor::new(payload.clone()),
            Some(payload.len() as u64 + 1),
            None,
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .expect_err("size mismatch must fail");
        assert!(matches!(error, ContentStreamError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn stream_writer_supports_empty_content_without_storing_a_fragment() {
        let (_dir, store) = make_test_store("lore-stream-empty-").await;
        let context = Context::from([0x44; 16]);
        let (address, fragment, size) = write_content_stream(
            store,
            Partition::from([0x34; 16]),
            context,
            Cursor::new(Vec::<u8>::new()),
            Some(0),
            None,
            WriteOptions::default(),
            None,
            None,
        )
        .await
        .expect("empty stream");
        assert_eq!(address.context, context);
        assert!(address.hash.is_zero());
        assert_eq!(fragment, Fragment::new_zeroed());
        assert_eq!(size, 0);
    }
}
