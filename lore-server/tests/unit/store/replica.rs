// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Partition;
use lore_revision::fragment;
use lore_revision::util::time::RetryPolicy;
use lore_server::protocol::replication_store::copy::ImmutableCopy;
use lore_server::protocol::replication_store::get::Get;
use lore_server::protocol::replication_store::get_metadata::GetMetadata;
use lore_server::protocol::replication_store::header::ReplicationHeader;
use lore_server::protocol::replication_store::obliterate::Obliterate;
use lore_server::protocol::replication_store::obliterate::ObliterateResponse;
use lore_server::protocol::replication_store::put::Put;
use lore_server::protocol::replication_store::query::Query;
use lore_server::protocol::replication_store::query::QueryResponse;
use lore_server::quic::replication_store_service::ReplicationServiceErrorCode;
use lore_server::quic::replication_store_service::client::ReplicationStoreClientError;
use lore_server::quic::replication_store_service::client::ServiceRequestMeta;
use lore_server::quic::replication_store_service::client::StoreClient;
use lore_server::quic::replication_store_service::client_container::ClientContainerConfig;
use lore_server::quic::replication_store_service::client_container::ClientFactory;
use lore_server::quic::replication_store_service::client_container::GenerateClientReason;
use lore_server::store::replica::*;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::immutable_store::CopyBehavior;
use lore_telemetry::LabelArray;
use lore_transport::ProtocolError;
use lore_transport::quic::client::ConnectionStats;
use mockall::predicate::eq;
use rand::random;
use tokio::join;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;

mockall::mock! {
    pub Client {}

    #[async_trait]
    impl StoreClient for Client {
        async fn connection_stats(&self) -> Option<ConnectionStats>;

        async fn put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

        async fn obliterate(
            &self,
            request: Obliterate,
        ) -> Result<ObliterateResponse, ReplicationStoreClientError>;

        async fn get(&self, request: Get) -> Result<StoreGetData, ReplicationStoreClientError>;

        async fn get_metadata(
            &self,
            request: GetMetadata,
        ) -> Result<StoreGetData, ReplicationStoreClientError>;

        async fn local_put(&self, request: Put) -> Result<(), ReplicationStoreClientError>;

        async fn local_get(&self, request: Get) -> Result<StoreGetData, ReplicationStoreClientError>;

        async fn local_get_metadata(
            &self,
            request: GetMetadata,
        ) -> Result<StoreGetData, ReplicationStoreClientError>;

        async fn query(
            &self,
            request: Query,
        ) -> Result<QueryResponse, ReplicationStoreClientError>;

        async fn local_query(
            &self,
            request: Query,
        ) -> Result<QueryResponse, ReplicationStoreClientError>;

        async fn copy(
            &self,
            request: ImmutableCopy,
        ) -> Result<(), ReplicationStoreClientError>;
    }
}

fn make_mock_client() -> MockClient {
    let mut client = MockClient::new();
    client.expect_connection_stats().return_once(|| None);
    client
}

struct ChannelFactory {
    rx: tokio::sync::Mutex<Receiver<Result<MockClient, ProtocolError>>>,
}

#[async_trait]
impl ClientFactory for ChannelFactory {
    type Output = MockClient;

    async fn make_client(&self, _initial_cwnd: Option<u64>) -> Result<MockClient, ProtocolError> {
        self.rx.lock().await.recv().await.expect("recv should work")
    }
}

fn make_client_container_config() -> ClientContainerConfig {
    let retry = RetryPolicy::builder()
        .with_initial_backoff_millis(50)
        .with_max_backoff_millis(1_000)
        .with_limit(300)
        .build();
    ClientContainerConfig {
        regenerate_retry_policy: retry,
        connection_lost_sleep: Duration::from_millis(1),
    }
}

async fn make_replica() -> Arc<Replica<MockClient>> {
    let (tx, rx) = mpsc::channel(1);
    let factory = ChannelFactory { rx: rx.into() };

    // allow 1 creation for initialization
    tx.send(Ok(make_mock_client())).await.unwrap();
    let replica = Replica::new(
        Arc::new(factory),
        make_client_container_config(),
        LabelArray::default(),
    )
    .await
    .expect("Creation should work");
    Arc::new(replica)
}

/// The contract, against a read replica.
///
/// It never accepts writes, so only the absence cases apply — which is exactly where a peer's
/// answer about content nobody stored has to come back as nothing at all, naming no partition
/// as its source.
#[tokio::test]
async fn satisfies_the_immutable_store_contract() {
    let execution = lore_server::util::setup_execution(
        "test",
        uuid::Uuid::new_v4().as_hyphenated().to_string(),
        String::default(),
    );
    LORE_CONTEXT
        .scope(execution, async move {
            let (tx, rx) = mpsc::channel(1);
            let factory = ChannelFactory { rx: rx.into() };

            let mut client = make_mock_client();
            client.expect_local_query().returning(|request| {
                Ok(QueryResponse {
                    results: vec![StoreMatchResult::default(); request.addresses.len()],
                })
            });
            client
                .expect_local_get_metadata()
                .returning(|_| Ok(StoreGetData::default()));
            client.expect_local_get().returning(|_| {
                Err(ReplicationStoreClientError::ServiceError(
                    ReplicationServiceErrorCode::AddressNotFound,
                ))
            });

            tx.send(Ok(client)).await.unwrap();
            let replica = Replica::new(
                Arc::new(factory),
                make_client_container_config(),
                LabelArray::default(),
            )
            .await
            .expect("Creation should work");

            lore_storage::conformance::verify_immutable_store(
                Arc::new(replica),
                lore_storage::conformance::Capabilities::new("Replica")
                    .no_put()
                    .no_obliterate(),
            )
            .await;
        })
        .await;
}

#[tokio::test]
async fn obliterate_returns_error() {
    let err = StoreError::internal("write operations not supported on read replica");
    assert!(matches!(err, StoreError::Internal(_)));
    let msg = format!("{err}");
    assert!(msg.contains("write operations not supported on read replica"));
}

#[tokio::test]
async fn copy_returns_error() {
    let execution =
        lore_server::util::setup_execution("test", String::default(), String::default());
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let replica = make_replica().await;
            let partition: lore_base::types::Partition = rand::random();
            let (_, address, _) = lore_revision::fragment::generate_random();

            let error = replica
                .copy(
                    partition,
                    address,
                    partition,
                    lore_base::types::Context::default(),
                    CopyBehavior {
                        durable: false,
                        do_not_replicate: false,
                    },
                )
                .await
                .expect_err("copy should not be supported on read replica");
            assert!(matches!(error, StoreError::Internal(_)));
        })
        .await;
}

mod regenerate_client {
    use super::*;

    #[tokio::test]
    async fn regenerate_client_guarded_by_permit() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(2);
                // once during creation, allow one other for the test reconnect
                tx.send(Ok(make_mock_client())).await.expect("send 1");
                tx.send(Ok(make_mock_client())).await.expect("send 2");
                let factory = ChannelFactory { rx: rx.into() };

                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);
                let original_epoch = replica.client_container.epoch();
                assert_eq!(original_epoch, 0);

                let regen_1 = replica.regenerate_client(original_epoch);
                let regen_2 = replica.regenerate_client(original_epoch);

                let (output_1, output_2) = join!(regen_1, regen_2);
                // 1 regenerated and the other didn't
                assert_ne!(
                    output_1.expect("future 1 failed"),
                    output_2.expect("future 2 failed")
                );
                assert_eq!(replica.client_container.epoch(), original_epoch + 1);
            })
            .await;
    }

    #[tokio::test]
    async fn regenerate_unhealthy_client_eventually_succeeds() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(15);
                let factory = ChannelFactory { rx: rx.into() };

                // allow 1 creation for initialization
                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);
                assert!(replica.client_container.is_healthy());

                for _n in 1..10 {
                    tx.send(Err(ProtocolError::internal("test-error")))
                        .await
                        .expect("send error");
                }

                let regen = replica.regenerate_client(replica.client_container.epoch());

                tokio::pin!(regen);
                select! {
                    _ = &mut regen => {panic!("regen should not finish at this point");},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
                // client should still be marked as unhealthy as regen is not finished
                assert!(!replica.client_container.is_healthy());

                tx.send(Ok(make_mock_client())).await.expect("send success");

                let did_regen = regen.await.expect("regen should work");
                assert!(did_regen);
                assert!(replica.client_container.is_healthy());
            })
            .await;
    }
}

mod handle_service_response {
    use super::*;

    #[tokio::test]
    async fn connection_failed_regenerates_client() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                // allow 1 creation for initialization
                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let (meta, _) = replica.client_container.request_client(None);
                let start_epoch = meta.client_epoch;
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    replica.clone(),
                    meta.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));

                // the spawned loop should be hanging in regenerate
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!replica.client_container.is_healthy());

                // unblock and observe new client regenerated
                tx.send(Ok(make_mock_client())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(replica.client_container.is_healthy());
                assert_eq!(replica.client_container.epoch(), start_epoch + 1);

                // Reporting the replaced client's failure again leaves its successor in place. A
                // regeneration would mark the client unhealthy and wait on the empty factory.
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    replica.clone(),
                    meta,
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(replica.client_container.is_healthy());
                assert_eq!(replica.client_container.epoch(), start_epoch + 1);
            })
            .await;
    }

    #[tokio::test]
    async fn connection_failed_loop_stops_when_replica_dropped() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let start_epoch = replica.client_container.epoch();
                let _error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    replica.clone(),
                    ServiceRequestMeta {
                        client_epoch: start_epoch,
                        address: None,
                    },
                );

                // drop the replica — the weak ref in the loop should break it
                drop(replica);

                // the spawned task should exit without panicking once it tries
                // to upgrade the weak reference
                tokio::time::sleep(Duration::from_millis(200)).await;
                // if we reach here without a panic, the loop exited cleanly
            })
            .await;
    }

    // samples some common errors to ensure they are returned
    #[tokio::test]
    async fn service_errors_are_mapped_to_store_errors() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let replica = make_replica().await;

                let context = ServiceRequestMeta {
                    client_epoch: 0,
                    address: None,
                };

                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound,
                    )),
                    replica.clone(),
                    context.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::AddressNotFound(_)));

                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    )),
                    replica.clone(),
                    context.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }
}

mod put {
    use super::*;

    #[tokio::test]
    async fn successful_put_request_transformation_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let repository: Context = random();
        let (fragment, address, payload) = fragment::generate_random();

        let (tx, rx) = mpsc::channel(1);
        let factory = ChannelFactory { rx: rx.into() };

        let mut client = make_mock_client();
        client
            .expect_local_put()
            .with(eq(Put {
                header: ReplicationHeader {
                    correlation_id,
                    repository,
                },
                address,
                fragment,
                flags: 0,
                payload: Some(payload.clone()),
            }))
            .returning(|_| Ok(()));

        tx.send(Ok(client)).await.unwrap();

        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                replica
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload),
                        false, /* force */
                    )
                    .await
                    .expect("put should work");
            })
            .await;
    }

    #[tokio::test]
    async fn service_error_put_request_transformation_works() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (fragment, address, payload) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_local_put().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let error = replica
                    .put(
                        repository.into(),
                        address,
                        fragment,
                        Some(payload),
                        false, /* force */
                    )
                    .await
                    .expect_err("put should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }

    #[tokio::test]
    async fn put_returns_error_when_unhealthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                // mark unhealthy via ConnectionFailed regenerate
                // drive the regenerate forward enough to mark unhealthy, then drop
                {
                    let regen = replica.client_container.regenerate_client(
                        replica.client_container.epoch(),
                        GenerateClientReason::ConnectionFailed,
                    );
                    tokio::pin!(regen);
                    tokio::select! {
                        _ = &mut regen => { panic!("regen should not finish"); },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                assert!(!replica.client_container.is_healthy());

                let repository: Context = random();
                let (fragment, address, payload) = fragment::generate_random();

                let error = replica
                    .put(repository.into(), address, fragment, Some(payload), false)
                    .await
                    .expect_err("put should fail when unhealthy");
                assert!(matches!(error, StoreError::Internal(_)));
            })
            .await;
    }
}

mod query {
    use std::collections::HashMap;

    use lore_server::protocol::replication_store::query::MAX_ADDRESSES;
    use parking_lot::Mutex;

    use super::*;

    #[tokio::test]
    async fn successful_query_one_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let repository_partition: Partition = repository.into();
                let mut client = make_mock_client();
                client
                    .expect_local_query()
                    .with(eq(Query {
                        header: ReplicationHeader {
                            correlation_id,
                            repository,
                        },
                        addresses: vec![address],
                    }))
                    .returning(move |_| {
                        Ok(QueryResponse {
                            results: vec![StoreMatchResult {
                                match_made: lore_storage::StoreMatch::MatchPartition,
                                partition: repository_partition,
                                context: Context::default(),
                                stored_local: false,
                                stored_durable: false,
                            }],
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let resolved = lore_storage::immutable_store::query_one(
                    &(replica as Arc<dyn ImmutableStore>),
                    repository.into(),
                    address,
                )
                .await
                .expect("resolve should work");

                assert_eq!(resolved.match_made, StoreMatch::MatchPartition);
            })
            .await;
    }

    // Creates a large number of addresses, each with a pre-assigned random StoreMatchResult.
    // The mocks return the pre-assigned result per address, and the test verifies the full
    // result (match level, partition, and storage flags) is forwarded in the original order.
    #[tokio::test]
    async fn successful_transformation_with_order_preserved() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let partition: Partition = repository.into();

                // create random addresses and decide randomly what store match result they will be
                // for the duration of the test
                let mut addresses = Vec::new();
                let address_results: Arc<Mutex<HashMap<Address, StoreMatchResult>>> =
                    Arc::new(HashMap::new().into());
                for _ in 0..MAX_ADDRESSES * 4 {
                    let (_, address, _) = fragment::generate_random();
                    addresses.push(address);

                    let random_match: u8 = random::<u8>() % 4;
                    let result = StoreMatchResult {
                        match_made: random_match.try_into().expect("invalid store match"),
                        partition,
                        context: Context::default(),
                        stored_local: random(),
                        stored_durable: random(),
                    };
                    address_results.lock().insert(address, result);
                }

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                for addresses in addresses.chunks(MAX_ADDRESSES) {
                    let address_results = address_results.clone();
                    client
                        .expect_local_query()
                        .with(eq(Query {
                            header: ReplicationHeader {
                                correlation_id,
                                repository,
                            },
                            addresses: addresses.to_vec(),
                        }))
                        .returning(move |request| {
                            let map = address_results.lock();
                            let results = request.addresses.iter().map(|a| map[a]).collect();
                            Ok(QueryResponse { results })
                        });
                }

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let mut store_matches = vec![StoreMatchResult::default(); addresses.len()];
                replica
                    .query(repository.into(), &addresses, &mut store_matches)
                    .await
                    .expect("resolve should work");

                // sanity check the addresses are what we expect
                let map = address_results.lock();
                for (index, got) in store_matches.iter().enumerate() {
                    let address = addresses[index];
                    assert_eq!(*got, map[&address]);
                }
            })
            .await;
    }

    #[tokio::test]
    async fn service_error_transformation_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();

                let mut addresses = Vec::new();
                for _ in 0..MAX_ADDRESSES * 2 {
                    let (_, address, _) = fragment::generate_random();
                    addresses.push(address);
                }

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                // 1 of the batched calls is ok, the other isn't
                client.expect_local_query().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });
                client.expect_local_query().returning(|request| {
                    Ok(QueryResponse {
                        results: request
                            .addresses
                            .iter()
                            .map(|_| StoreMatchResult::default())
                            .collect(),
                    })
                });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let mut results = vec![StoreMatchResult::default(); addresses.len()];
                let error = replica
                    .query(repository.into(), &addresses, &mut results)
                    .await
                    .expect_err("resolve should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }

    #[tokio::test]
    async fn resolve_returns_error_when_unhealthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                // drive the regenerate forward enough to mark unhealthy, then drop
                {
                    let regen = replica.client_container.regenerate_client(
                        replica.client_container.epoch(),
                        GenerateClientReason::ConnectionFailed,
                    );
                    tokio::pin!(regen);
                    tokio::select! {
                        _ = &mut regen => { panic!("regen should not finish"); },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                assert!(!replica.client_container.is_healthy());

                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let mut results = [StoreMatchResult::default(); 1];
                let error = replica
                    .query(repository.into(), &[address], &mut results)
                    .await
                    .expect_err("resolve should fail when unhealthy");
                assert!(matches!(error, StoreError::Internal(_)));
            })
            .await;
    }
}

mod get {
    use super::*;

    #[tokio::test]
    async fn successful_request_transformation_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (fragment, address, payload) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                {
                    let payload = payload.clone();
                    client
                        .expect_local_get()
                        .with(eq(Get {
                            header: ReplicationHeader {
                                correlation_id,
                                repository,
                            },
                            address,
                        }))
                        .returning(move |_| {
                            Ok(StoreGetData {
                                fragment,
                                match_made: lore_storage::StoreMatch::MatchFull,
                                partition: repository.into(),
                                payload: Some(payload.clone()),
                                content: None,
                            })
                        });
                }

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let get_output = replica
                    .get(repository.into(), address)
                    .await
                    .and_then(lore_storage::StoreGetData::into_payload)
                    .expect("get should work");

                assert_eq!(get_output.0, fragment);
                assert_eq!(get_output.1, payload);
            })
            .await;
    }

    #[tokio::test]
    async fn service_error_transformation_works() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_local_get().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let error = replica
                    .get(repository.into(), address)
                    .await
                    .expect_err("get should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }

    #[tokio::test]
    async fn get_returns_error_when_unhealthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                // drive the regenerate forward enough to mark unhealthy, then drop
                {
                    let regen = replica.client_container.regenerate_client(
                        replica.client_container.epoch(),
                        GenerateClientReason::ConnectionFailed,
                    );
                    tokio::pin!(regen);
                    tokio::select! {
                        _ = &mut regen => { panic!("regen should not finish"); },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                assert!(!replica.client_container.is_healthy());

                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let error = replica
                    .get(repository.into(), address)
                    .await
                    .expect_err("get should fail when unhealthy");
                assert!(matches!(error, StoreError::Internal(_)));
            })
            .await;
    }
}

mod get_metadata {
    use super::*;

    #[tokio::test]
    async fn full_match_transform_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (fragment, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_local_get_metadata()
                    .with(eq(GetMetadata {
                        header: ReplicationHeader {
                            correlation_id,
                            repository,
                        },
                        address,
                    }))
                    .returning(move |_| {
                        Ok(StoreGetData {
                            fragment,
                            match_made: lore_storage::StoreMatch::MatchFull,
                            partition: repository.into(),
                            payload: None,
                            content: None,
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let output = replica
                    .get_metadata(repository.into(), address)
                    .await
                    .expect("get_metadata should work");

                assert_eq!(output.match_made, StoreMatch::MatchFull);
                assert_eq!(output.fragment, fragment);
            })
            .await;
    }

    #[tokio::test]
    async fn partial_match_transform_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (fragment, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_local_get_metadata()
                    .with(eq(GetMetadata {
                        header: ReplicationHeader {
                            correlation_id,
                            repository,
                        },
                        address,
                    }))
                    .returning(move |_| {
                        Ok(StoreGetData {
                            fragment,
                            match_made: lore_storage::StoreMatch::MatchPartition,
                            partition: repository.into(),
                            payload: None,
                            content: None,
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let output = replica
                    .get_metadata(repository.into(), address)
                    .await
                    .expect("get_metadata should work");

                assert_eq!(output.match_made, StoreMatch::MatchPartition);
                assert_eq!(output.fragment, fragment);
            })
            .await;
    }

    #[tokio::test]
    async fn service_error_transformation_works() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_local_get_metadata().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                let error = replica
                    .get_metadata(repository.into(), address)
                    .await
                    .expect_err("get_metadata should fail");

                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }

    #[tokio::test]
    async fn get_metadata_returns_error_when_unhealthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                tx.send(Ok(make_mock_client())).await.unwrap();
                let replica = Replica::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    LabelArray::default(),
                )
                .await
                .expect("Creation should work");
                let replica = Arc::new(replica);

                {
                    let regen = replica.client_container.regenerate_client(
                        replica.client_container.epoch(),
                        GenerateClientReason::ConnectionFailed,
                    );
                    tokio::pin!(regen);
                    tokio::select! {
                        _ = &mut regen => { panic!("regen should not finish"); },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                assert!(!replica.client_container.is_healthy());

                let repository: Context = random();
                let (_, address, _) = fragment::generate_random();

                let error = replica
                    .get_metadata(repository.into(), address)
                    .await
                    .expect_err("get_metadata should fail when unhealthy");
                assert!(matches!(error, StoreError::Internal(_)));
            })
            .await;
    }
}
