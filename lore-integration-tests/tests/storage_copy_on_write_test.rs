// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the write path duplicating an association instead of transferring a
//! payload.
//!
//! Content the peer already holds under another context or another partition needs an association,
//! not an upload. The write path establishes that from the resolution it already runs per fragment
//! and issues `Copy` where it would have issued `Put`. These tests spin up a real gRPC server whose
//! store counts what it was asked to do, so "no payload was transferred" is observed at the peer
//! rather than inferred from the client.

#[cfg(all(test, feature = "integration_tests"))]
mod storage_copy_on_write_tests {
    use std::error::Error;
    use std::sync::Arc;

    use lore::storage::handle::LoreStore;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_storage::ImmutableStore;
    use lore_storage::StoreMatch;
    use lore_storage::immutable_store::query_one;

    use crate::common::net_common::CountingStore;
    use crate::common::net_common::Partitions;
    use crate::common::net_common::Traffic;
    use crate::common::net_common::close_handle;
    use crate::common::net_common::make_backends;
    use crate::common::net_common::open_handle;
    use crate::common::net_common::put_content;
    use crate::common::net_common::start_grpc_server;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    struct TestServer {
        url: String,
        counted: Arc<CountingStore>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn start_server() -> TestServer {
        start_server_with(CountingStore::new).await
    }

    async fn start_refusing_server() -> TestServer {
        start_server_with(CountingStore::refusing_copy).await
    }

    async fn start_server_with(
        wrap: impl FnOnce(Arc<dyn ImmutableStore>) -> Arc<CountingStore>,
    ) -> TestServer {
        // Isolated, as Lore Server's store is: isolation decides what the client is told about
        // content it did not store itself.
        let (backend, mutable) = make_backends(Partitions::Isolated).await;
        let counted = wrap(backend);
        let (url, shutdown, _stopped) = start_grpc_server(counted.clone(), mutable);

        TestServer {
            url,
            counted,
            _shutdown: shutdown,
        }
    }

    async fn put_remote(
        handle: LoreStore,
        partition: Partition,
        context: Context,
        bytes: &[u8],
    ) -> Address {
        put_content(handle, partition, context, bytes, 1, 0).await
    }

    async fn put_local_only(
        handle: LoreStore,
        partition: Partition,
        context: Context,
        bytes: &[u8],
    ) -> Address {
        put_content(handle, partition, context, bytes, 0, 0).await
    }

    async fn assert_peer_holds(
        server: &TestServer,
        partition: Partition,
        address: Address,
        payload: &[u8],
    ) {
        let store: Arc<dyn ImmutableStore> = server.counted.clone();
        let resolved = query_one(&store, partition, address)
            .await
            .expect("query the peer");
        assert_eq!(
            resolved.match_made,
            StoreMatch::MatchFull,
            "the peer must hold the association the write registered"
        );

        let (_fragment, bytes) = lore_storage::read::read(
            store,
            partition,
            address,
            None,
            lore_storage::options::ReadOptions::default(),
            None,
        )
        .await
        .expect("read the address back from the peer");
        assert_eq!(
            bytes.as_ref(),
            payload,
            "the copied association must serve the original bytes"
        );
    }

    /// The flagship case: the same bytes written again under a different context in the same
    /// partition. The peer holds the payload already, so it is asked for an association.
    #[tokio::test]
    async fn a_second_context_in_the_same_partition_copies_instead_of_uploading() -> TestResult {
        let execution = setup_execution("copy-on-write-same-partition".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x11u8; 16]);
                let first_context = Context::from([0x12u8; 16]);
                let second_context = Context::from([0x13u8; 16]);
                let payload = b"one payload, two files that happen to be identical".to_vec();

                let first = put_remote(handle, partition, first_context, &payload).await;
                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 0
                    },
                    "content the peer has never seen has to be uploaded"
                );

                server.counted.reset();
                let second = put_remote(handle, partition, second_context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 0,
                        empty_puts: 0,
                        copies: 1
                    },
                    "the peer already holds these bytes, so it must be asked for an association"
                );
                assert_eq!(first.hash, second.hash);
                assert_eq!(second.context, second_context);

                assert_eq!(
                    server.counted.copy_sources(),
                    vec![(
                        partition,
                        Address {
                            hash: first.hash,
                            context: first_context
                        }
                    )],
                    "the source named is the association the client knows the peer holds"
                );

                assert_peer_holds(&server, partition, second, &payload).await;
                assert_peer_holds(&server, partition, first, &payload).await;

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// The same bytes written into a second partition. The source is one the client has a claim to,
    /// so the association can be duplicated across the boundary rather than uploaded again.
    #[tokio::test]
    async fn a_second_partition_copies_from_the_first() -> TestResult {
        let execution = setup_execution("copy-on-write-cross-partition".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let source_partition = Partition::from([0x21u8; 16]);
                let target_partition = Partition::from([0x22u8; 16]);
                let context = Context::from([0x23u8; 16]);
                let payload = b"shared store, two repositories, one payload".to_vec();

                let first = put_remote(handle, source_partition, context, &payload).await;
                server.counted.reset();

                let second = put_remote(handle, target_partition, context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 0,
                        empty_puts: 0,
                        copies: 1
                    },
                    "a partition the caller can reach is a source, not a reason to re-upload"
                );
                assert_eq!(
                    server.counted.copy_sources(),
                    vec![(
                        source_partition,
                        Address {
                            hash: first.hash,
                            context
                        }
                    )],
                    "the copy must name the partition the client found the content in"
                );

                assert_peer_holds(&server, target_partition, second, &payload).await;
                assert_peer_holds(&server, source_partition, first, &payload).await;

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// A local association the peer never received names no source it could copy from. Skipping the
    /// upload on the strength of it would leave the address registered nowhere.
    #[tokio::test]
    async fn a_source_the_peer_never_received_is_uploaded() -> TestResult {
        let execution = setup_execution("copy-on-write-not-durable".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x31u8; 16]);
                let payload = b"stored locally, never pushed".to_vec();

                put_local_only(handle, partition, Context::from([0x32u8; 16]), &payload).await;
                assert_eq!(
                    server.counted.traffic(),
                    Traffic::default(),
                    "a local-only write must not touch the peer at all"
                );

                let second =
                    put_remote(handle, partition, Context::from([0x33u8; 16]), &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 0
                    },
                    "the peer holds nothing to copy from, so the payload has to be transferred"
                );
                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// A copy is an attempt, not a commitment. A peer that refuses one leaves the write to upload
    /// exactly as it would have without ever trying.
    #[tokio::test]
    async fn a_refused_copy_falls_back_to_uploading() -> TestResult {
        let execution = setup_execution("copy-on-write-refused".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_refusing_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x41u8; 16]);
                let payload = b"the peer will not answer a copy for this".to_vec();

                put_remote(handle, partition, Context::from([0x42u8; 16]), &payload).await;
                server.counted.reset();

                let second =
                    put_remote(handle, partition, Context::from([0x43u8; 16]), &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic {
                        payload_puts: 1,
                        empty_puts: 0,
                        copies: 1
                    },
                    "the refused copy must be followed by the upload it was standing in for"
                );
                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// An address the peer already holds outright is a full match, which needs neither verb.
    #[tokio::test]
    async fn re_writing_the_same_address_asks_the_peer_for_nothing() -> TestResult {
        let execution = setup_execution("copy-on-write-full-match".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x51u8; 16]);
                let context = Context::from([0x52u8; 16]);
                let payload = b"written once, written again identically".to_vec();

                put_remote(handle, partition, context, &payload).await;
                server.counted.reset();

                put_remote(handle, partition, context, &payload).await;

                assert_eq!(
                    server.counted.traffic(),
                    Traffic::default(),
                    "the association already exists, so there is nothing to send or duplicate"
                );

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// Content cut into a fragment tree makes the choice per fragment. Every leaf and every list
    /// block is content-addressed, so a second context copies the whole tree.
    #[tokio::test]
    async fn every_fragment_of_a_tree_copies_rather_than_uploads() -> TestResult {
        let execution = setup_execution("copy-on-write-fragmented".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x61u8; 16]);
                let chunk = 64 * 1024u64;
                // Deterministic and non-repeating, so the fragments differ from one another.
                let payload: Vec<u8> = (0..(chunk as usize * 6 + 977))
                    .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
                    .collect();

                let first = put_content(
                    handle,
                    partition,
                    Context::from([0x62u8; 16]),
                    &payload,
                    1,
                    chunk,
                )
                .await;
                let uploaded = server.counted.traffic();
                assert!(
                    uploaded.payload_puts > 6,
                    "the test wants a real tree, got {uploaded:?}"
                );

                server.counted.reset();
                let second = put_content(
                    handle,
                    partition,
                    Context::from([0x63u8; 16]),
                    &payload,
                    1,
                    chunk,
                )
                .await;
                let duplicated = server.counted.traffic();

                assert_eq!(
                    duplicated.payload_puts, 0,
                    "no fragment of an identical tree needs its payload transferred again"
                );
                assert_eq!(
                    duplicated.copies, uploaded.payload_puts,
                    "every fragment the first write uploaded is one the second duplicates"
                );
                assert_eq!(first.hash, second.hash);

                assert_peer_holds(&server, partition, second, &payload).await;

                close_handle(handle).await;
                Ok(())
            })
            .await
    }

    /// The source a copy names is the exact association the client's own store recorded, which is
    /// what lets a peer confirm it with a keyed read instead of searching the partition.
    #[tokio::test]
    async fn the_copy_names_the_exact_association_the_client_knows_about() -> TestResult {
        let execution = setup_execution("copy-on-write-exact-source".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let handle = open_handle(Some(&server.url)).await;

                let partition = Partition::from([0x71u8; 16]);
                let held = Context::from([0x72u8; 16]);
                let payload = b"the source is named, not searched for".to_vec();

                let first = put_remote(handle, partition, held, &payload).await;
                server.counted.reset();

                put_remote(handle, partition, Context::from([0x73u8; 16]), &payload).await;

                let sources = server.counted.copy_sources();
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0].0, partition);
                assert_eq!(sources[0].1.hash, first.hash);
                assert_eq!(
                    sources[0].1.context, held,
                    "an unnamed context would make the peer search the partition instead"
                );

                close_handle(handle).await;
                Ok(())
            })
            .await
    }
}
