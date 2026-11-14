// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_storage::Address;
use lore_storage::Context;
use lore_storage::FRAGMENT_SIZE_EXPECTED;
use lore_storage::FragmentReference;
use lore_storage::Hash;
use lore_storage::ImmutableStore;
use lore_storage::Partition;
use lore_storage::compress::FRAGMENT_SIZE_THRESHOLD;
use lore_storage::fragment_engine::*;
use lore_storage::options::WriteOptions;
use lore_storage::validate_fragment_list;
use lore_storage::validate_fragment_metadata;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

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

/// Content cutting to one chunk is addressed without being stored, the same as content
/// cutting to several: the fast path answers with the address the store would have held it
/// under, and the store holds nothing.
#[tokio::test]
async fn addressing_a_single_chunk_stores_nothing() {
    let dir = lore_base::test_util::TempDir::new("lore-storage-hash-only-");
    let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");
    let partition = Partition::from([5u8; 16]);
    let buffer = mixed_pattern_buffer(1024);
    assert_eq!(
        chunk_boundaries(buffer.clone(), None).expect("chunking succeeds"),
        vec![(0, buffer.len())],
        "the fast path is what this covers"
    );

    let (address, stored_local, stored_durable) = write_fragmented(
        store.clone(),
        partition,
        Context::default(),
        buffer,
        WriteOptions::default().hash_only(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
        None,
        None,
    )
    .await
    .expect("addressing the content");

    assert!(!stored_local, "addressing stored the chunk locally");
    assert!(!stored_durable, "addressing stored the chunk durably");
    let described = store.get_metadata(partition, address).await;
    assert!(
        !described.is_ok_and(|described| {
            described.match_made != lore_storage::store_types::StoreMatch::MatchNone
        }),
        "addressing stored the chunk"
    );
}

/// A fragment list too long for one fragment is split into sublists the local store and the
/// server both accept, each holding at least two references, and reads back as written. A list
/// one reference longer than a full sublist gives its last sublist two references, whatever the
/// size of the last leaf. A longer one keeps its full first sublist.
#[tokio::test]
async fn a_long_fragment_list_splits_into_valid_sublists() {
    let partition = Partition::from([7u8; 16]);
    let context = Context::from([7u8; 16]);
    let reference_size = size_of::<FragmentReference>();
    let full = FRAGMENT_SIZE_THRESHOLD / reference_size;

    for (count, last_leaf, sublists) in [
        (full + 1, 10, [full - 1, 2]),
        (full + 1, FRAGMENT_SIZE_EXPECTED, [full - 1, 2]),
        (full + 2, 10, [full, 2]),
    ] {
        // Lists differing only in their last leaf hold the same bytes, so each has its own store.
        let dir = lore_base::test_util::TempDir::new("lore-storage-sublists-");
        let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
            Some(std::path::PathBuf::from(dir.as_ref())),
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await
        .expect("create test store");
        // The references name a fragment, as a zero hash is a hole, and adjacent holes are refused.
        let references: Vec<FragmentReference> = (0..count)
            .map(|index| FragmentReference {
                hash: Hash::from_u64(1),
                offset_content: (index * FRAGMENT_SIZE_EXPECTED) as u64,
            })
            .collect();
        let content_size = (count - 1) * FRAGMENT_SIZE_EXPECTED + last_leaf;

        let (root, _, _) = write_fragmentlist(
            store.clone(),
            partition,
            context,
            Bytes::copy_from_slice(references.as_bytes()),
            content_size,
            WriteOptions::default(),
            None,
            lore_storage::write_tracker::WriteContext::none(),
            None,
            None,
        )
        .await
        .expect("writing the fragment list");

        let root = store.clone().get(partition, root).await.expect("root list");
        let root_payload = root.payload.expect("root list payload");
        assert_eq!(root.fragment.size_content, content_size as u64);
        validate_fragment_list(&root.fragment, &root_payload).expect("root list");

        let mut lengths = vec![];
        let mut read_back = vec![];
        for reference in root_payload.chunks_exact(reference_size) {
            let reference = FragmentReference::read_from_bytes(reference).expect("reference");
            let address = Address {
                context,
                hash: reference.hash,
            };
            let sublist = store
                .clone()
                .get(partition, address)
                .await
                .expect("sublist");
            let payload = sublist.payload.expect("sublist payload");
            validate_fragment_metadata(&sublist.fragment).expect("sublist metadata");
            validate_fragment_list(&sublist.fragment, &payload).expect("sublist references");
            lengths.push(payload.len() / reference_size);
            read_back.extend_from_slice(&payload);
        }
        assert_eq!(lengths, sublists, "sublists of a list of {count}");
        assert_eq!(read_back, references.as_bytes(), "a list of {count}");
    }
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
    let dir = lore_base::test_util::TempDir::new("lore-storage-truncated-");
    let path = std::path::Path::new(dir.as_ref()).join("truncated");
    std::fs::write(&path, b"").expect("create empty file");
    let (file, _) = lore_storage::content::ContentSource::file(&path)
        .open()
        .await
        .expect("open file");
    let store = lore_storage::local::immutable_store::LocalImmutableStore::new(
        Some(std::path::PathBuf::from(dir.as_ref())),
        lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
    )
    .await
    .expect("create test store");

    let err = write_fragmented_from_file(
        store,
        Partition::from([1u8; 16]),
        Context::from([1u8; 16]),
        file,
        4 * FRAGMENT_SIZE_THRESHOLD,
        WriteOptions::default().hash_only(),
        None,
        lore_storage::write_tracker::WriteContext::none(),
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
