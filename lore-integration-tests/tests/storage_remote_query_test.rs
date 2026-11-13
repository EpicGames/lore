// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the put option `remote_query`, which asks the server whether it holds
//! each fragment before the client compresses and uploads it.
//!
//! A real gRPC server whose store counts what it was asked to do, so the metadata reads and the
//! uploads are observed at the server rather than inferred from the client.

#[cfg(all(test, feature = "integration_tests"))]
mod storage_remote_query_tests {
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::storage::handle::LoreStore;
    use lore::storage::put::LoreStoragePutArgs;
    use lore::storage::put::LoreStoragePutItem;
    use lore::storage::put_file::LoreStoragePutFileArgs;
    use lore::storage::put_file::LoreStoragePutFileItem;
    use lore::storage::put_file_resolved::LoreStoragePutFileResolvedArgs;
    use lore::storage::put_file_resolved::LoreStoragePutFileResolvedItem;
    use lore::storage::put_resolved::LoreStoragePutResolvedArgs;
    use lore::storage::put_resolved::LoreStoragePutResolvedItem;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::test_util::TempFile;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_revision::event::LoreBytes;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;

    use crate::common::net_common::CountingStore;
    use crate::common::net_common::Partitions;
    use crate::common::net_common::TestServerStop;
    use crate::common::net_common::close_handle;
    use crate::common::net_common::make_backends;
    use crate::common::net_common::open_handle;
    use crate::common::net_common::start_grpc_server;
    use crate::setup_execution;

    type TestResult = Result<(), Box<dyn Error>>;

    struct TestServer {
        url: String,
        counted: Arc<CountingStore>,
        _stop: TestServerStop,
    }

    async fn start_server() -> TestServer {
        // Isolated, as Lore Server's store is, so a metadata read answers for the exact address.
        let (backend, mutable) = make_backends(Partitions::Isolated).await;
        let counted = CountingStore::new(backend);
        let (url, stop) = start_grpc_server(counted.clone(), mutable);
        TestServer {
            url,
            counted,
            _stop: stop,
        }
    }

    /// The four put calls, which each take `remote_query` on their items.
    #[derive(Clone, Copy, Debug)]
    enum Call {
        Put,
        PutFile,
        PutResolved,
        PutFileResolved,
    }

    const CALLS: [Call; 4] = [
        Call::Put,
        Call::PutFile,
        Call::PutResolved,
        Call::PutFileResolved,
    ];

    /// What one put item asks for.
    #[derive(Clone, Copy)]
    struct Flags {
        remote_write: u8,
        remote_query: u8,
    }

    /// What one put item ended with, from its `PUT_ITEM_COMPLETE`: the address, the error code,
    /// and whether the content reached the remote.
    type Completion = (Address, i32, u8);

    /// Stores `file`'s content in `partition` through `handle` with `call`, as one item with
    /// `flags`, and returns how the item completed. The resolved calls publish `key`.
    async fn put_with(
        call: Call,
        handle: LoreStore,
        partition: Partition,
        file: &TempFile,
        content: &[u8],
        flags: Flags,
    ) -> Completion {
        let completed: Arc<Mutex<Vec<Completion>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = completed.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::StoragePutItemComplete(data) = event {
                sink.lock().unwrap().push((
                    data.address,
                    data.error.error_code,
                    data.stored_remote,
                ));
            }
        }));
        let context = Context::from([0x7C; 16]);
        let key = lore_base::types::Hash::hash_buffer(b"a key for remote_query");
        let data = LoreBytes {
            ptr: content.as_ptr().cast(),
            len: content.len(),
        };
        let path = LoreString::from(file.path().to_str().expect("the path is text"));
        let globals = LoreGlobalArgs::default();
        let status = match call {
            Call::Put => {
                lore::storage::put::put(
                    globals,
                    LoreStoragePutArgs {
                        handle,
                        items: LoreArray::from_vec(vec![LoreStoragePutItem {
                            id: 1,
                            partition,
                            context,
                            data,
                            remote_write: flags.remote_write,
                            local_cache: 0,
                            remote_query: flags.remote_query,
                            fixed_size_chunk: 0,
                        }]),
                    },
                    callback,
                )
                .await
            }
            Call::PutFile => {
                lore::storage::put_file::put_file(
                    globals,
                    LoreStoragePutFileArgs {
                        handle,
                        items: LoreArray::from_vec(vec![LoreStoragePutFileItem {
                            id: 1,
                            partition,
                            context,
                            path,
                            remote_write: flags.remote_write,
                            local_cache: 0,
                            remote_query: flags.remote_query,
                            fixed_size_chunk: 0,
                        }]),
                    },
                    callback,
                )
                .await
            }
            Call::PutResolved => {
                lore::storage::put_resolved::put_resolved(
                    globals,
                    LoreStoragePutResolvedArgs {
                        handle,
                        items: LoreArray::from_vec(vec![LoreStoragePutResolvedItem {
                            id: 1,
                            partition,
                            key,
                            context,
                            data,
                            remote_write: flags.remote_write,
                            local_cache: 0,
                            remote_query: flags.remote_query,
                            fixed_size_chunk: 0,
                        }]),
                    },
                    callback,
                )
                .await
            }
            Call::PutFileResolved => {
                lore::storage::put_file_resolved::put_file_resolved(
                    globals,
                    LoreStoragePutFileResolvedArgs {
                        handle,
                        items: LoreArray::from_vec(vec![LoreStoragePutFileResolvedItem {
                            id: 1,
                            partition,
                            key,
                            context,
                            path,
                            remote_write: flags.remote_write,
                            local_cache: 0,
                            remote_query: flags.remote_query,
                            fixed_size_chunk: 0,
                        }]),
                    },
                    callback,
                )
                .await
            }
        };
        assert_eq!(status, 0, "{call:?}: the put succeeds");
        let completed = completed.lock().unwrap();
        assert_eq!(completed.len(), 1, "{call:?}: the item completes once");
        completed[0]
    }

    /// Content of several fragments, whose bytes vary so content-defined cutting finds
    /// boundaries in it.
    fn content() -> Vec<u8> {
        (0..1024 * 1024usize)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect()
    }

    /// Each put call with `remote_query` sends none of the content the server holds, having asked
    /// it about every fragment. Without `remote_write` it asks nothing, and without the option it
    /// asks nothing and uploads the content again.
    #[tokio::test]
    async fn a_put_asking_the_server_sends_none_of_the_content_it_holds() -> TestResult {
        let execution = setup_execution("remote-query-put".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let server = start_server().await;
                let content = content();
                let file = TempFile::with_contents("remote-query-put-", &content);
                for (index, call) in CALLS.into_iter().enumerate() {
                    let partition = Partition::from([0x70 + index as u8; 16]);
                    let put = async |flags| {
                        let handle = open_handle(Some(&server.url)).await;
                        let completion =
                            put_with(call, handle, partition, &file, &content, flags).await;
                        close_handle(handle).await;
                        completion
                    };

                    let (uploaded, code, remote) = put(Flags {
                        remote_write: 1,
                        remote_query: 0,
                    })
                    .await;
                    assert_eq!((code, remote), (0, 1), "{call:?}: the upload succeeds");
                    let fragments = server.counted.traffic().payload_puts;
                    assert!(fragments > 2, "{call:?}: the content is several fragments");

                    server.counted.reset();
                    let (address, code, remote) = put(Flags {
                        remote_write: 1,
                        remote_query: 1,
                    })
                    .await;
                    assert_eq!((address, code, remote), (uploaded, 0, 1));
                    assert_eq!(
                        server.counted.traffic().payload_puts,
                        0,
                        "{call:?}: the server holds every fragment"
                    );
                    assert_eq!(
                        server.counted.metadata_reads(),
                        fragments,
                        "{call:?}: the server is asked about every fragment"
                    );

                    server.counted.reset();
                    let (_, code, remote) = put(Flags {
                        remote_write: 0,
                        remote_query: 1,
                    })
                    .await;
                    assert_eq!(
                        server.counted.metadata_reads(),
                        0,
                        "{call:?}: a put that writes nothing remotely asks nothing"
                    );
                    assert_eq!((code, remote), (0, 0));

                    server.counted.reset();
                    let (_, code, remote) = put(Flags {
                        remote_write: 1,
                        remote_query: 0,
                    })
                    .await;
                    assert_eq!((code, remote), (0, 1));
                    assert_eq!(server.counted.metadata_reads(), 0, "{call:?}");
                    assert_eq!(
                        server.counted.traffic().payload_puts,
                        fragments,
                        "{call:?}: without the option the content is uploaded again"
                    );
                    server.counted.reset();
                }
                Ok(())
            })
            .await
    }
}
