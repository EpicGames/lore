// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes that ask the remote whether it holds each fragment before preparing its payload
//! ([`WriteOptions::remote_query`]), against a peer that holds what was uploaded to it.

use lore_storage::MutableStore;
use lore_storage::write_stats::FragmentWriteCounts;
use lore_storage::write_stats::FragmentWriteStats;
use lore_transport::ProtocolError;
use lore_transport::test_util::StubPeer;

use super::*;

/// The context every write here stores under.
fn context() -> Context {
    Context::from([0x5E; 16])
}

/// A write that asks the remote first.
fn querying() -> WriteOptions {
    WriteOptions::default()
        .with_remote_write()
        .with_remote_query()
}

/// What the peer was asked for while the write under test ran, and how that write ended.
struct Written {
    result: StoreResult,
    metadata_requests: usize,
    puts: usize,
    counts: FragmentWriteCounts,
}

/// Writes `content` under [`context`] in `partition` of `store` with `flags`, through a session on
/// `peer`, inline.
async fn write(
    store: Arc<dyn ImmutableStore>,
    peer: &Arc<StubPeer>,
    partition: Partition,
    content: &[u8],
    flags: WriteOptions,
) -> Written {
    let (metadata_requests, puts) = (peer.metadata_requests(), peer.puts());
    let stats = Arc::new(FragmentWriteStats::default());
    let result = write_content(
        store,
        partition,
        context(),
        Bytes::copy_from_slice(content),
        flags,
        Some(Arc::new(peer.session(partition))),
        WriteContext::counted(Some(stats.clone())),
        None,
    )
    .await
    .expect("the write succeeds");
    Written {
        result,
        metadata_requests: peer.metadata_requests() - metadata_requests,
        puts: peer.puts() - puts,
        counts: stats.snapshot(),
    }
}

/// A peer that holds `content`, uploaded to it under [`context`] in `partition` from a store of
/// its own.
async fn peer_holding(partition: Partition, content: &[u8]) -> Arc<StubPeer> {
    let (_dir, store) = make_test_store().await;
    let peer = Arc::new(StubPeer::default());
    let uploaded = write(
        store,
        &peer,
        partition,
        content,
        WriteOptions::default().with_remote_write(),
    )
    .await;
    assert!(uploaded.result.stored_durable, "the peer holds the content");
    peer
}

/// A write beside a copy source, and what it wrote into.
struct BesideCopySource {
    written: Written,
    peer: Arc<StubPeer>,
    store: Arc<dyn ImmutableStore>,
    address: Address,
    _dir: TempDir,
}

/// Writes content under [`context`] in `partition` of a store that holds it durably under another
/// context, as a copy source, through a peer that holds the write's address and answers copy
/// requests as [`StubPeer::with_copy_answers`] does with `slow_downs` and `refusal`.
async fn write_beside_a_copy_source(
    partition: Partition,
    slow_downs: usize,
    refusal: Option<ProtocolError>,
) -> BesideCopySource {
    let content = pseudo_random_content(48 * 1024);
    let (dir, store) = make_test_store().await;
    let uploader = Arc::new(StubPeer::default());
    let source = write_content(
        store.clone(),
        partition,
        Context::from([0x5B; 16]),
        Bytes::copy_from_slice(&content),
        WriteOptions::default().with_remote_write(),
        Some(Arc::new(uploader.session(partition))),
        WriteContext::none(),
        None,
    )
    .await
    .expect("store the association to copy from");
    assert!(source.stored_durable, "the copy source is durable");
    let held = Fragment {
        flags: FragmentFlags::PayloadCompressedLZ4.bits(),
        size_payload: 1000,
        size_content: content.len() as u64,
    };
    let address = Address {
        hash: source.address.hash,
        context: context(),
    };
    let peer = Arc::new(
        StubPeer::default()
            .with_held(vec![(address, held)])
            .with_copy_answers(slow_downs, refusal),
    );
    let written = write(store.clone(), &peer, partition, &content, querying()).await;
    BesideCopySource {
        written,
        peer,
        store,
        address,
        _dir: dir,
    }
}

/// A fragment the remote holds is recorded as the remote's fragment, stored durable and without a
/// payload, and none is loaded, compressed or sent.
#[tokio::test]
async fn a_fragment_the_remote_holds_is_recorded_durable_and_not_sent() {
    let partition = Partition::from([0x51; 16]);
    let content = pseudo_random_content(48 * 1024);
    // A representation of its own, so the entry shows whose fragment it records.
    let held = Fragment {
        flags: FragmentFlags::PayloadCompressedLZ4.bits(),
        size_payload: 1000,
        size_content: content.len() as u64,
    };
    let address = Address {
        hash: lore_storage::hash::hash_slice(&content),
        context: context(),
    };
    let peer = Arc::new(StubPeer::default().with_held(vec![(address, held)]));
    let (_dir, store) = make_test_store().await;

    let written = write(store.clone(), &peer, partition, &content, querying()).await;

    assert_eq!(written.result.address, address);
    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 0, "the remote holds the fragment");
    assert!(written.result.stored_durable);
    assert!(!written.result.stored_local);
    let recorded = store
        .get_metadata(partition, written.result.address)
        .await
        .expect("read the recorded entry");
    assert_eq!(recorded.match_made, StoreMatch::MatchFull);
    assert_ne!(
        recorded.fragment.flags & FragmentFlags::PayloadStoredDurable.bits(),
        0
    );
    assert_eq!(
        recorded.fragment.flags & FragmentFlags::PayloadStoredLocal.bits(),
        0,
        "the entry holds no payload"
    );
    assert_eq!(recorded.fragment.size_payload, held.size_payload);
    assert_eq!(
        recorded.fragment.flags & FragmentFlags::PayloadCompressed.bits(),
        FragmentFlags::PayloadCompressedLZ4.bits(),
        "the entry describes the payload the remote holds"
    );
    assert_eq!(written.counts.remote_queries, 1);
    assert_eq!(written.counts.remote_query_hits, 1);
    assert_eq!(
        written.counts.remote_query_unsent_bytes,
        u64::from(held.size_payload)
    );
    assert_eq!(written.counts.data_fragments, 0, "no payload was prepared");
    assert_eq!(written.counts.no_payload_fragments, 1);
    assert_eq!(written.counts.remote_already_durable, 1);
    assert_eq!(written.counts.remote_put_writes, 0);
    assert_eq!(written.counts.local_metadata_writes, 1);
}

/// A fragment the remote describes as other content than the write's, by its size, or with a header
/// no local store takes, is no hit, and the write uploads.
#[tokio::test]
async fn a_fragment_the_remote_misdescribes_is_uploaded() {
    let content = pseudo_random_content(48 * 1024);
    let size = content.len() as u64;
    let address = Address {
        hash: lore_storage::hash::hash_slice(&content),
        context: context(),
    };
    let misdescribed = [
        Fragment {
            flags: FragmentFlags::PayloadCompressedLZ4.bits(),
            size_payload: 1000,
            size_content: size + 1,
        },
        Fragment {
            flags: 0,
            size_payload: 1000,
            size_content: size,
        },
    ];
    for (index, held) in misdescribed.into_iter().enumerate() {
        let partition = Partition::from([0x60 + index as u8; 16]);
        let peer = Arc::new(StubPeer::default().with_held(vec![(address, held)]));
        let (_dir, store) = make_test_store().await;

        let written = write(store, &peer, partition, &content, querying()).await;

        assert_eq!(written.metadata_requests, 1, "{held:?}");
        assert_eq!(written.puts, 1, "{held:?}");
        assert!(written.result.stored_durable, "{held:?}");
        assert_eq!(written.counts.remote_query_hits, 0, "{held:?}");
    }
}

/// A fragment the remote holds as another kind of payload than the write's, a fragment list for
/// content the write stores whole, is no hit though a local store takes its header, and the write
/// uploads and records its own fragment.
#[tokio::test]
async fn a_fragment_the_remote_holds_as_another_kind_of_payload_is_uploaded() {
    let partition = Partition::from([0x5D; 16]);
    let content = pseudo_random_content(48 * 1024);
    let address = Address {
        hash: lore_storage::hash::hash_slice(&content),
        context: context(),
    };
    let held = Fragment {
        flags: FragmentFlags::PayloadFragmented.bits(),
        size_payload: size_of::<FragmentReference>() as u32,
        size_content: content.len() as u64,
    };
    let durable = Fragment {
        flags: held.flags | FragmentFlags::PayloadStoredDurable.bits(),
        ..held
    };
    assert!(
        lore_storage::immutable_store::put_is_valid(&durable, None),
        "a local store takes the header"
    );
    let peer = Arc::new(StubPeer::default().with_held(vec![(address, held)]));
    let (_dir, store) = make_test_store().await;

    let written = write(store.clone(), &peer, partition, &content, querying()).await;

    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 1, "the write uploads");
    assert!(written.result.stored_durable);
    assert_eq!(written.counts.remote_query_hits, 0);
    let recorded = store
        .get_metadata(partition, address)
        .await
        .expect("read the recorded entry")
        .fragment;
    assert_eq!(
        recorded.flags & FragmentFlags::PayloadFragmented.bits(),
        0,
        "the entry records the write's own fragment"
    );
}

/// Content the remote does not hold is uploaded after the query.
#[tokio::test]
async fn content_the_remote_lacks_is_uploaded() {
    let partition = Partition::from([0x52; 16]);
    let peer = Arc::new(StubPeer::default());
    let (_dir, store) = make_test_store().await;

    let written = write(
        store,
        &peer,
        partition,
        &pseudo_random_content(48 * 1024),
        querying(),
    )
    .await;

    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 1);
    assert!(written.result.stored_durable);
    assert_eq!(written.counts.remote_queries, 1);
    assert_eq!(written.counts.remote_query_hits, 0);
    assert_eq!(written.counts.remote_query_unsent_bytes, 0);
    assert_eq!(written.counts.remote_put_writes, 1);
}

/// Writes content in `partition` through a peer that answers every metadata request with `failure`,
/// and checks that the write asks once and uploads as it would without asking.
async fn assert_a_failed_query_falls_through(partition: Partition, failure: ProtocolError) {
    let peer = Arc::new(StubPeer::default().with_metadata_failure(failure));
    let (_dir, store) = make_test_store().await;

    let written = write(
        store,
        &peer,
        partition,
        &pseudo_random_content(48 * 1024),
        querying(),
    )
    .await;

    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 1);
    assert!(written.result.stored_durable);
    assert_eq!(written.counts.remote_queries, 1);
    assert_eq!(written.counts.remote_query_hits, 0);
    assert_eq!(written.counts.remote_put_writes, 1);
}

/// A query that fails is no answer, and the write uploads as it would without asking.
#[tokio::test]
async fn a_failed_query_falls_through_to_the_upload() {
    assert_a_failed_query_falls_through(
        Partition::from([0x53; 16]),
        ProtocolError::internal("the peer fails every metadata request"),
    )
    .await;
}

/// A query the remote answers with `SlowDown` is not asked again and is no answer, and the write
/// uploads as it would without asking.
#[tokio::test]
async fn a_throttled_query_falls_through_to_the_upload() {
    assert_a_failed_query_falls_through(
        Partition::from([0x63; 16]),
        ProtocolError::from(lore_base::error::SlowDown),
    )
    .await;
}

/// Without the option the remote is asked nothing, and content it holds is uploaded again.
#[tokio::test]
async fn without_the_option_the_remote_is_asked_nothing() {
    let partition = Partition::from([0x54; 16]);
    let content = pseudo_random_content(48 * 1024);
    let peer = peer_holding(partition, &content).await;
    let (_dir, store) = make_test_store().await;

    let written = write(
        store,
        &peer,
        partition,
        &content,
        WriteOptions::default().with_remote_write(),
    )
    .await;

    assert_eq!(written.metadata_requests, 0);
    assert_eq!(written.puts, 1);
    assert!(written.result.stored_durable);
    assert_eq!(written.counts.remote_queries, 0);
    assert_eq!(written.counts.remote_put_writes, 1);
}

/// Where the local store knows an association the remote holds, the write copies it without
/// asking first: the copy sends no payload either, and asking could only add a round trip.
#[tokio::test]
async fn a_known_copy_source_is_copied_without_asking() {
    let partition = Partition::from([0x58; 16]);
    let content = pseudo_random_content(48 * 1024);
    let peer = Arc::new(StubPeer::default());
    let (_dir, store) = make_test_store().await;
    let held = write(
        store.clone(),
        &peer,
        partition,
        &content,
        WriteOptions::default().with_remote_write(),
    )
    .await;
    assert!(held.result.stored_durable, "the copy source is durable");
    let (metadata_requests, puts) = (peer.metadata_requests(), peer.puts());

    let written = write_content(
        store.clone(),
        partition,
        Context::from([0x59; 16]),
        Bytes::from(content),
        querying(),
        Some(Arc::new(peer.session(partition))),
        WriteContext::none(),
        None,
    )
    .await
    .expect("the write succeeds");

    assert_eq!(
        peer.metadata_requests(),
        metadata_requests,
        "nothing was asked"
    );
    assert_eq!(peer.copies(), 1);
    assert_eq!(peer.puts(), puts, "nothing was uploaded");
    assert!(written.stored_durable);
}

/// A write that keeps its payload locally still prepares and keeps it, and sends none the remote
/// holds.
#[tokio::test]
async fn a_write_keeping_its_payload_sends_none_the_remote_holds() {
    let partition = Partition::from([0x55; 16]);
    let content = pseudo_random_content(48 * 1024);
    let peer = peer_holding(partition, &content).await;
    let (_dir, store) = make_test_store().await;

    let written = write(
        store.clone(),
        &peer,
        partition,
        &content,
        querying().with_keep_local(),
    )
    .await;

    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 0);
    assert!(written.result.stored_durable);
    assert!(written.result.stored_local);
    let recorded = query_one(&store, partition, written.result.address)
        .await
        .expect("query the recorded entry");
    assert_eq!(recorded.match_made, StoreMatch::MatchFull);
    assert!(recorded.stored_local);
    assert!(recorded.stored_durable);
    assert_eq!(written.counts.remote_query_hits, 1);
    assert_eq!(
        written.counts.data_fragments, 1,
        "the payload kept is prepared"
    );
}

/// Every fragment of a file the remote holds is asked about, leaves and list alike, and none is
/// sent, from the leaders a tracker runs as from an inline write.
#[tokio::test]
async fn no_fragment_of_a_file_the_remote_holds_is_sent() {
    let partition = Partition::from([0x56; 16]);
    let content = pseudo_random_content(3 * lore_storage::compress::FRAGMENT_SIZE_THRESHOLD + 4096);
    let file = lore_base::test_util::TempFile::with_contents("remote-query-file-", &content);
    let source = ContentSource::file(file.path());
    let peer = Arc::new(StubPeer::default());
    let (_uploading_dir, uploading) = make_test_store().await;
    let uploaded = write_from_file(
        uploading,
        partition,
        &source,
        context(),
        WriteOptions::default().with_remote_write(),
        Some(Arc::new(peer.session(partition))),
        WriteContext::none(),
    )
    .await
    .expect("upload the file");
    let fragments = peer.puts();
    assert!(fragments > 2, "the file is a list of several fragments");

    let (_dir, store) = make_test_store().await;
    let tracker = Arc::new(WriteTracker::new());
    let stats = Arc::new(FragmentWriteStats::default());
    let written = write_from_file(
        store.clone(),
        partition,
        &source,
        context(),
        querying(),
        Some(Arc::new(peer.session(partition))),
        WriteContext::tracked(Some(tracker.clone()), Some(stats.clone())),
    )
    .await
    .expect("write the file");
    tracker.await_all().await.expect("the leaders succeed");

    assert_eq!(written.address, uploaded.address);
    assert_eq!(peer.metadata_requests(), fragments);
    assert_eq!(peer.puts(), fragments, "nothing more was uploaded");
    let counts = stats.snapshot();
    assert_eq!(counts.remote_queries, fragments as u64);
    assert_eq!(counts.remote_query_hits, fragments as u64);
    let root = query_one(&store, partition, written.address)
        .await
        .expect("query the root");
    assert_eq!(root.match_made, StoreMatch::MatchFull);
    assert!(root.stored_durable);
}

/// A resolved write of content the remote holds uploads none of it, and publishes its key with a
/// mapping write of its own.
#[tokio::test]
async fn a_resolved_write_of_content_the_remote_holds_sends_none() {
    let partition = Partition::from([0x57; 16]);
    let content = pseudo_random_content(48 * 1024);
    let peer = peer_holding(partition, &content).await;
    let (_dir, store) = make_test_store().await;
    let mutable = lore_storage::local::mutable_store::create(
        None::<&str>,
        lore_storage::MutableStoreSettings::default(),
        store.clone(),
    )
    .await
    .expect("create the mutable store");
    let key = lore_storage::hash::hash_slice(b"a key the remote does not know");

    let result = write_resolved(
        store,
        mutable.clone(),
        partition,
        key,
        context(),
        Bytes::from(content),
        querying(),
        Some(Arc::new(peer.session(partition))),
        WriteContext::none(),
    )
    .await
    .expect("the resolved write succeeds");

    assert_eq!(peer.metadata_requests(), 1);
    assert_eq!(peer.puts(), 1, "only the first upload reached the peer");
    assert!(result.stored_durable);
    assert!(!result.published, "no upload carried the key");
    assert_eq!(
        mutable
            .load(partition, key, lore_base::types::KeyType::Resolve)
            .await
            .expect("load the local mapping"),
        result.address.hash
    );
}

/// Where the remote refuses the copy from an association the local store knows, the write asks it
/// whether it holds the address, and sends none it holds.
#[tokio::test]
async fn a_refused_copy_is_followed_by_the_query() {
    let beside = write_beside_a_copy_source(
        Partition::from([0x5A; 16]),
        0,
        Some(ProtocolError::from(lore_base::error::NotFound)),
    )
    .await;
    let written = &beside.written;

    assert_eq!(beside.peer.copies(), 1, "the copy was asked for");
    assert_eq!(
        written.metadata_requests, 1,
        "the refused copy is followed by the query"
    );
    assert_eq!(written.puts, 0, "the remote holds the fragment");
    assert!(written.result.stored_durable);
    assert_eq!(written.counts.remote_copy_writes, 0);
    assert_eq!(written.counts.remote_query_hits, 1);
}

/// Where the remote throttles the copy from an association the local store knows through every
/// retry, the write does not ask whether it holds the address, though it does, uploads nothing,
/// and keeps the payload locally, not durable, for a later push.
#[tokio::test(start_paused = true)]
async fn a_throttled_copy_is_followed_by_no_query() {
    let partition = Partition::from([0x5F; 16]);
    let beside = write_beside_a_copy_source(partition, usize::MAX, None).await;
    let written = &beside.written;

    assert_eq!(
        beside.peer.copies(),
        lore_storage::store_retry().limit() + 1
    );
    assert_eq!(
        written.metadata_requests, 0,
        "the throttled copy is followed by no query"
    );
    assert_eq!(written.puts, 0);
    assert!(!written.result.stored_durable);
    assert!(written.result.stored_local);
    assert_eq!(written.counts.remote_queries, 0);
    let recorded = query_one(&beside.store, partition, beside.address)
        .await
        .expect("query the entry");
    assert_eq!(recorded.match_made, StoreMatch::MatchFull);
    assert!(recorded.stored_local);
    assert!(!recorded.stored_durable);
}

/// A fragment the remote holds, over a local entry of the address that holds a payload but is not
/// durable, keeps the entry's own header and payload, and is recorded durable.
#[tokio::test]
async fn a_hit_over_a_local_payload_keeps_it() {
    let partition = Partition::from([0x5C; 16]);
    let content = pseudo_random_content(48 * 1024);
    let (_dir, store) = make_test_store().await;
    let local = write_content(
        store.clone(),
        partition,
        context(),
        Bytes::copy_from_slice(&content),
        WriteOptions::default(),
        None,
        WriteContext::none(),
        None,
    )
    .await
    .expect("write locally");
    assert!(local.stored_local, "the entry holds a payload");
    assert!(!local.stored_durable, "the entry is not durable");
    let kept = store
        .clone()
        .get_metadata(partition, local.address)
        .await
        .expect("read the local entry")
        .fragment;
    // A representation of its own, so the entry shows whose header it keeps.
    let held = Fragment {
        flags: FragmentFlags::PayloadCompressedLZ4.bits(),
        size_payload: 1000,
        size_content: content.len() as u64,
    };
    assert_ne!(kept.size_payload, held.size_payload);
    let peer = Arc::new(StubPeer::default().with_held(vec![(local.address, held)]));

    let written = write(store.clone(), &peer, partition, &content, querying()).await;

    assert_eq!(written.metadata_requests, 1);
    assert_eq!(written.puts, 0, "the remote holds the fragment");
    assert!(written.result.stored_durable);
    assert!(written.result.stored_local);
    let recorded = query_one(&store, partition, local.address)
        .await
        .expect("query the entry");
    assert_eq!(recorded.match_made, StoreMatch::MatchFull);
    assert!(recorded.stored_local);
    assert!(recorded.stored_durable);
    let header = store
        .clone()
        .get_metadata(partition, local.address)
        .await
        .expect("read the entry")
        .fragment;
    assert_eq!(
        header.size_payload, kept.size_payload,
        "the entry keeps its own header"
    );
    assert_eq!(
        header.flags & FragmentFlags::PayloadCompressed.bits(),
        kept.flags & FragmentFlags::PayloadCompressed.bits()
    );
    let (_fragment, read) = lore_storage::read::read(
        store,
        partition,
        local.address,
        None,
        lore_storage::options::ReadOptions::default(),
        None,
    )
    .await
    .expect("read the content back");
    assert_eq!(read.as_ref(), content.as_slice());
}
