// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::types::Address;
use lore_base::types::Partition;
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
use lore_server::store::replicated_store::*;
use lore_storage::ImmutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatch;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::immutable_store::CopyBehavior;
use lore_transport::ProtocolError;
use lore_transport::quic::client::ConnectionStats;
use parking_lot::Mutex;
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

        async fn get(
            &self,
            request: Get,
        ) -> Result<StoreGetData, ReplicationStoreClientError>;

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

async fn make_store() -> Arc<ReplicatedStore<MockClient>> {
    let (tx, rx) = mpsc::channel(1);
    let factory = ChannelFactory { rx: rx.into() };

    // allow 1 creation for initialization
    tx.send(Ok(make_mock_client())).await.unwrap();
    ReplicatedStore::new(
        Arc::new(factory),
        make_client_container_config(),
        NonZeroUsize::MIN,
        Duration::from_secs(60),
        Duration::from_secs(10),
    )
    .await
    .expect("Creation should work")
}

const TWO_CLIENTS: NonZeroUsize = NonZeroUsize::new(2).expect("2 is not zero");

/// Makes a store with `num_clients` slots, returning the sender its factory takes replacement
/// clients from.
async fn make_store_with_slots(
    num_clients: NonZeroUsize,
) -> (
    Arc<ReplicatedStore<MockClient>>,
    mpsc::Sender<Result<MockClient, ProtocolError>>,
) {
    let (tx, rx) = mpsc::channel(num_clients.get() + 1);
    for _ in 0..num_clients.get() {
        tx.send(Ok(make_mock_client())).await.unwrap();
    }
    let store = ReplicatedStore::new(
        Arc::new(ChannelFactory { rx: rx.into() }),
        make_client_container_config(),
        num_clients,
        Duration::from_secs(60),
        Duration::from_secs(10),
    )
    .await
    .expect("Creation should work");
    (store, tx)
}

/// The contract, against the replicated store. Writes go through a different path than the
/// battery drives, so what applies here are the absence cases — where an answer about content
/// nobody stored has to name no partition as its source.
#[tokio::test]
async fn satisfies_the_immutable_store_contract() {
    let execution = lore_server::util::setup_execution(
        "test",
        uuid::Uuid::new_v4().as_hyphenated().to_string(),
        String::default(),
    );
    lore_base::runtime::LORE_CONTEXT
        .scope(execution, async move {
            let (tx, rx) = mpsc::channel(1);
            let factory = ChannelFactory { rx: rx.into() };

            let mut client = make_mock_client();
            client.expect_query().returning(|request| {
                Ok(QueryResponse {
                    results: vec![StoreMatchResult::default(); request.addresses.len()],
                })
            });
            client
                .expect_get_metadata()
                .returning(|_| Ok(StoreGetData::default()));
            client.expect_get().returning(|_| {
                Err(ReplicationStoreClientError::ServiceError(
                    ReplicationServiceErrorCode::AddressNotFound,
                ))
            });

            tx.send(Ok(client)).await.unwrap();
            let store = ReplicatedStore::new(
                Arc::new(factory),
                make_client_container_config(),
                NonZeroUsize::MIN,
                Duration::from_secs(60),
                Duration::from_secs(10),
            )
            .await
            .expect("Creation should work");

            lore_storage::conformance::verify_immutable_store(
                store,
                lore_storage::conformance::Capabilities::new("ReplicatedStore")
                    .no_put()
                    .no_obliterate(),
            )
            .await;
        })
        .await;
}

mod regenerate_client {
    use lore_base::runtime::LORE_CONTEXT;
    use tokio::join;
    use tokio::select;
    use tokio::sync::mpsc;

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

                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");
                let original_epoch = store.client_containers[0].epoch();
                assert_eq!(original_epoch, 0);

                let regen_1 = store.client_containers[0]
                    .regenerate_client(original_epoch, GenerateClientReason::PeriodicRefresh);
                let regen_2 = store.client_containers[0]
                    .regenerate_client(original_epoch, GenerateClientReason::PeriodicRefresh);

                let (output_1, output_2) = join!(regen_1, regen_2);
                // 1 regenerated and the other didn't
                assert_ne!(
                    output_1.expect("future 1 failed"),
                    output_2.expect("future 2 failed")
                );
                assert_eq!(store.client_containers[0].epoch(), original_epoch + 1);
            })
            .await;
    }

    /// A store operation runs on the current client while a regeneration waits for its
    /// replacement to connect.
    #[tokio::test]
    async fn query_completes_while_regeneration_waits_for_a_new_client() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let partition: Partition = rand::random();
                let (_, address, _) = lore_revision::fragment::generate_random();
                let expected = StoreMatchResult {
                    match_made: StoreMatch::MatchFull,
                    partition,
                    context: lore_base::types::Context::default(),
                    stored_local: true,
                    stored_durable: true,
                };
                let mut client = make_mock_client();
                client.expect_query().return_once(move |_| {
                    Ok(QueryResponse {
                        results: vec![expected],
                    })
                });

                // allow 1 creation for initialization
                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let regen = store.client_containers[0].regenerate_client(
                    store.client_containers[0].epoch(),
                    GenerateClientReason::PeriodicRefresh,
                );

                tokio::pin!(regen);
                assert!(
                    futures::poll!(&mut regen).is_pending(),
                    "regen should wait for the factory"
                );

                assert!(store.client_containers[0].is_healthy());
                let mut matches = vec![StoreMatchResult::default()];
                tokio::time::timeout(
                    Duration::from_secs(5),
                    store.clone().query(partition, &[address], &mut matches),
                )
                .await
                .expect("the query should not wait for the regeneration")
                .expect("the query should succeed");
                assert_eq!(matches, vec![expected]);

                // unblock the regen future
                tx.send(Ok(make_mock_client())).await.unwrap();
                let did_regen = regen.await.expect("regen should work");
                assert!(did_regen);
                assert!(store.client_containers[0].is_healthy());
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn regenerate_unhealthy_client_eventually_succeeds() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(15);
                let factory = ChannelFactory { rx: rx.into() };

                // allow 1 creation for initialization
                tx.send(Ok(make_mock_client())).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");
                assert!(store.client_containers[0].is_healthy());

                for _n in 1..10 {
                    tx.send(Err(ProtocolError::internal("test-error")))
                        .await
                        .expect("send error");
                }

                let regen = store.client_containers[0].regenerate_client(
                    store.client_containers[0].epoch(),
                    // ConnectionFailed should mark the store's client as unhealthy
                    GenerateClientReason::ConnectionFailed,
                );

                tokio::pin!(regen);
                select! {
                    _ = &mut regen => {panic!("regen should not finish at this point");},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
                // client should be still marked as unhealthy as regen is not finished
                assert!(!store.client_containers[0].is_healthy());

                tx.send(Ok(make_mock_client()))
                    .await
                    .expect("send success error");

                let did_regen = regen.await.expect("regen should work");
                assert!(did_regen);
                assert!(store.client_containers[0].is_healthy());
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_refresh_regenerates_every_slot() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let num_clients_to_mock = 30;
                let (tx, rx) = mpsc::channel(num_clients_to_mock);
                let factory = ChannelFactory { rx: rx.into() };

                // one per slot for initialization + plenty for periodic refreshes
                for _ in 0..num_clients_to_mock {
                    tx.send(Ok(make_mock_client())).await.unwrap();
                }

                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    TWO_CLIENTS,
                    Duration::from_millis(100),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                tokio::time::sleep(Duration::from_secs(1)).await;

                for (slot, container) in store.client_containers.iter().enumerate() {
                    let epoch = container.epoch();
                    assert!(
                        // The last slot's first refresh comes at most 125 ms in, then one every
                        // 100 ms, so every slot refreshes at least nine times in the second.
                        epoch >= 9,
                        "expected several refreshes of slot {slot}, got {epoch}"
                    );
                    assert!(container.is_healthy());
                }
            })
            .await;
    }
}

mod handle_service_response {
    use lore_base::runtime::LORE_CONTEXT;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn connection_failed_regenerates_client() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                // allow 1 creation for initialization
                tx.send(Ok(make_mock_client())).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let (meta, _) = store.client_containers[0].request_client(None);
                let start_epoch = meta.client_epoch;
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    0,
                    meta.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));

                // we should be hanging in regenerate
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!store.client_containers[0].is_healthy());

                // unblock and observe new client regenerated
                tx.send(Ok(make_mock_client())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[0].is_healthy());
                assert_eq!(store.client_containers[0].epoch(), start_epoch + 1);

                // Reporting the replaced client's failure again leaves its successor in place. A
                // regeneration would mark the client unhealthy and wait on the empty factory.
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    0,
                    meta,
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[0].is_healthy());
                assert_eq!(store.client_containers[0].epoch(), start_epoch + 1);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn connection_failed_regenerates_only_the_failed_slot() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (store, tx) = make_store_with_slots(TWO_CLIENTS).await;

                let (meta, _) = store.client_containers[1].request_client(None);
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    1,
                    meta,
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));

                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[0].is_healthy());
                assert!(!store.client_containers[1].is_healthy());

                tx.send(Ok(make_mock_client())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[1].is_healthy());
                assert_eq!(store.client_containers[0].epoch(), 0);
                assert_eq!(store.client_containers[1].epoch(), 1);
            })
            .await;
    }

    /// Requests skip an unhealthy slot, so a slot whose reconnect gives up keeps trying on its
    /// own until it is back.
    #[tokio::test(start_paused = true)]
    async fn a_failed_slot_keeps_reconnecting_until_it_succeeds() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(4);
                tx.send(Ok(make_mock_client())).await.unwrap();
                tx.send(Ok(make_mock_client())).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(ChannelFactory { rx: rx.into() }),
                    ClientContainerConfig {
                        regenerate_retry_policy: RetryPolicy::builder()
                            .with_initial_backoff(Duration::ZERO)
                            .with_max_backoff(Duration::ZERO)
                            .with_limit(0)
                            .build(),
                        connection_lost_sleep: Duration::from_millis(1),
                    },
                    TWO_CLIENTS,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                // Two failures, so a stored wake-up cannot stand in for the retry.
                for _ in 0..2 {
                    tx.send(Err(ProtocolError::internal("test-error")))
                        .await
                        .unwrap();
                }
                let (meta, _) = store.client_containers[1].request_client(None);
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    1,
                    meta,
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));

                // The first two reconnects fail and give up, and the next waits on the empty factory.
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!store.client_containers[1].is_healthy());
                assert_eq!(store.client_containers[1].epoch(), 0);

                tx.send(Ok(make_mock_client())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[1].is_healthy());
                assert_eq!(store.client_containers[1].epoch(), 1);
            })
            .await;
    }

    /// A failure reported while a periodic refresh holds the slot marks the slot unhealthy
    /// straight away, so requests stop going to its dead client before the refresh finishes.
    #[tokio::test]
    async fn connection_failed_during_a_refresh_marks_the_slot_unhealthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (store, tx) = make_store_with_slots(TWO_CLIENTS).await;

                let refresh = store.client_containers[0]
                    .regenerate_client(0, GenerateClientReason::PeriodicRefresh);
                tokio::pin!(refresh);
                assert!(futures::poll!(&mut refresh).is_pending());
                assert!(store.client_containers[0].is_healthy());

                let (meta, _) = store.client_containers[0].request_client(None);
                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    0,
                    meta,
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::Internal(_)));
                assert!(!store.client_containers[0].is_healthy());

                tx.send(Ok(make_mock_client())).await.unwrap();
                let did_regen = refresh.await.expect("refresh should work");
                assert!(did_regen);
                assert!(store.client_containers[0].is_healthy());
                assert_eq!(store.client_containers[0].epoch(), 1);
            })
            .await;
    }

    /// Reporting failures starts no task: the slot's existing supervisor does the reconnecting,
    /// however many requests report the failure.
    #[tokio::test(start_paused = true)]
    async fn repeated_connection_failures_start_no_tasks() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
                let metrics = tokio::runtime::Handle::current().metrics();
                let tasks_before = metrics.num_alive_tasks();

                let (meta, _) = store.client_containers[1].request_client(None);
                for _ in 0..50 {
                    let error = handle_service_response::<(), MockClient>(
                        Err(ReplicationStoreClientError::ConnectionFailed),
                        &store,
                        1,
                        meta.clone(),
                    )
                    .unwrap_err();
                    assert!(matches!(error, StoreError::Internal(_)));
                }

                // The supervisor wakes and waits on the empty factory for a new client.
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!store.client_containers[1].is_healthy());
                assert_eq!(metrics.num_alive_tasks(), tasks_before);
            })
            .await;
    }

    /// A refresh that fell due while the slot was reconnecting does not replace the client the
    /// reconnect just made.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_restarts_the_refresh_period() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(3);
                tx.send(Ok(make_mock_client())).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(ChannelFactory { rx: rx.into() }),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_millis(200),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let (meta, _) = store.client_containers[0].request_client(None);
                let _ = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    0,
                    meta,
                );
                // The first refresh is due within 300 ms, while the reconnect waits on the
                // empty factory.
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert!(!store.client_containers[0].is_healthy());

                tx.send(Ok(make_mock_client())).await.unwrap();
                tx.send(Ok(make_mock_client())).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(store.client_containers[0].is_healthy());
                assert_eq!(store.client_containers[0].epoch(), 1);
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
                let store = make_store().await;

                let context = ServiceRequestMeta {
                    client_epoch: 0,
                    address: None,
                };

                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound,
                    )),
                    &store,
                    0,
                    context.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::AddressNotFound(_)));

                let error = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    )),
                    &store,
                    0,
                    context.clone(),
                )
                .unwrap_err();
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }
}

mod put {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::FragmentFlags;
    use lore_revision::fragment;
    use mockall::predicate::eq;
    use rand::random;

    use super::*;

    #[tokio::test]
    async fn successful_put_request_transformation_works() {
        let correlation_id = uuid::Uuid::new_v4();
        let partition: Partition = random();
        let (fragment, address, payload) = fragment::generate_random();

        // The put method sets PayloadDoNotReplicate on the fragment before
        // sending to the remote peer, so the expected fragment must include it.
        let mut expected_fragment = fragment;
        expected_fragment.flags |= FragmentFlags::PayloadDoNotReplicate;

        let (tx, rx) = mpsc::channel(1);
        let factory = ChannelFactory { rx: rx.into() };

        let mut client = make_mock_client();
        client
            .expect_put()
            .with(eq(Put {
                header: ReplicationHeader {
                    correlation_id,
                    repository: partition.into(),
                },
                address,
                fragment: expected_fragment,
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
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                store
                    .put(
                        partition,
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
                let partition: Partition = random();
                let (fragment, address, payload) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_put().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let error = store
                    .put(
                        partition,
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
}

mod query {
    use std::collections::HashMap;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment;
    use lore_server::protocol::replication_store::query::MAX_ADDRESSES;
    use mockall::predicate::eq;
    use rand::random;

    use super::*;

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
                let partition: Partition = random();

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
                        context: lore_base::types::Context::default(),
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
                        .expect_query()
                        .with(eq(Query {
                            header: ReplicationHeader {
                                correlation_id,
                                repository: partition.into(),
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
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let mut store_matches = vec![StoreMatchResult::default(); addresses.len()];
                store
                    .query(partition, &addresses, &mut store_matches)
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
                let partition: Partition = random();

                let mut addresses = Vec::new();
                for _ in 0..MAX_ADDRESSES * 2 {
                    let (_, address, _) = fragment::generate_random();
                    addresses.push(address);
                }

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                // 1 of the batched calls is ok, the other isn't
                client.expect_query().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });
                client.expect_query().returning(|request| {
                    Ok(QueryResponse {
                        results: request
                            .addresses
                            .iter()
                            .map(|_| StoreMatchResult::default())
                            .collect(),
                    })
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let mut results = vec![StoreMatchResult::default(); addresses.len()];
                let error = store
                    .query(partition, &addresses, &mut results)
                    .await
                    .expect_err("resolve should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }

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
                let partition: Partition = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_query()
                    .with(eq(Query {
                        header: ReplicationHeader {
                            correlation_id,
                            repository: partition.into(),
                        },
                        addresses: vec![address],
                    }))
                    .returning(move |_| {
                        Ok(QueryResponse {
                            results: vec![StoreMatchResult {
                                match_made: lore_storage::StoreMatch::MatchPartition,
                                partition,
                                context: lore_base::types::Context::default(),
                                stored_local: false,
                                stored_durable: false,
                            }],
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let exist_output = lore_storage::immutable_store::query_one(
                    &(store as Arc<dyn ImmutableStore>),
                    partition,
                    address,
                )
                .await
                .expect("resolve should work");

                assert_eq!(
                    exist_output.match_made,
                    lore_storage::StoreMatch::MatchPartition
                );
            })
            .await;
    }
}

mod obliterate {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment;
    use mockall::predicate::eq;
    use rand::random;

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
                let partition: Partition = random();
                let (_, address, _) = fragment::generate_random();
                // don't generate too large a random value to overflow when
                // we add pre-existing stats to the obliterate() call below
                let expected_num_fragments = random::<u32>() as usize;
                let expected_num_payloads = random::<u32>() as usize;

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                {
                    let num_fragments = expected_num_fragments as u64;
                    let num_payloads = expected_num_payloads as u64;
                    client
                        .expect_obliterate()
                        .with(eq(Obliterate {
                            header: ReplicationHeader {
                                correlation_id,
                                repository: partition.into(),
                            },
                            address,
                        }))
                        .returning(move |_| {
                            Ok(ObliterateResponse {
                                num_fragments,
                                num_payloads,
                            })
                        });
                }

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let base_num_fragments = 0usize;
                let base_num_payloads = random::<u8>() as usize;
                let stats = Arc::new(StoreObliterateStats {
                    num_fragments: base_num_fragments.into(),
                    num_payloads: base_num_payloads.into(),
                });
                store
                    .obliterate(partition, address, stats.clone())
                    .await
                    .expect("obliterate should work");
                assert_eq!(
                    stats.num_fragments.load(Ordering::Relaxed),
                    base_num_fragments + expected_num_fragments
                );
                assert_eq!(
                    stats.num_payloads.load(Ordering::Relaxed),
                    base_num_payloads + expected_num_payloads
                );
            })
            .await;
    }

    #[tokio::test]
    async fn service_error_transformation_works() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let partition: Partition = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_obliterate().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let stats = Arc::new(StoreObliterateStats {
                    num_fragments: 0.into(),
                    num_payloads: 0.into(),
                });
                let error = store
                    .obliterate(partition, address, stats.clone())
                    .await
                    .expect_err("obliterate should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
                assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
                assert_eq!(stats.num_payloads.load(Ordering::Relaxed), 0);
            })
            .await;
    }
}

mod get {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment;
    use mockall::predicate::eq;
    use rand::random;

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
                let partition: Partition = random();
                let (fragment, address, payload) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                {
                    let payload = payload.clone();
                    client
                        .expect_get()
                        .with(eq(Get {
                            header: ReplicationHeader {
                                correlation_id,
                                repository: partition.into(),
                            },
                            address,
                        }))
                        .returning(move |_| {
                            Ok(StoreGetData {
                                fragment,
                                match_made: lore_storage::StoreMatch::MatchFull,
                                partition,
                                payload: Some(payload.clone()),
                                content: None,
                            })
                        });
                }

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let get_output = store
                    .get(partition, address)
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
                let partition: Partition = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_get().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let error = store
                    .get(partition, address)
                    .await
                    .expect_err("get should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }
}

mod copy {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_revision::fragment;
    use mockall::predicate::eq;
    use rand::random;

    use super::*;

    #[tokio::test]
    async fn copy_sends_quic_copy_message_to_remote() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let source_partition: Partition = random();
                let (_, source_address, _) = fragment::generate_random();
                let destination_partition: Partition = random();
                let destination_context: Context = random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_copy()
                    .with(eq(ImmutableCopy {
                        header: ReplicationHeader {
                            correlation_id,
                            repository: destination_partition.into(),
                        },
                        source_partition,
                        source_address,
                        destination_context,
                        durable: false,
                        do_not_replicate: true,
                    }))
                    .returning(|_| Ok(()));

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                store
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        CopyBehavior {
                            durable: false,
                            do_not_replicate: false,
                        },
                    )
                    .await
                    .expect("copy should succeed");
            })
            .await;
    }

    #[tokio::test]
    async fn copy_forwards_durable_flag() {
        let correlation_id = uuid::Uuid::new_v4();
        let execution = lore_server::util::setup_execution(
            "test",
            correlation_id.as_hyphenated().to_string(),
            String::default(),
        );
        LORE_CONTEXT
            .scope(execution, async move {
                let source_partition: Partition = random();
                let (_, source_address, _) = fragment::generate_random();
                let destination_partition: Partition = random();
                let destination_context: Context = random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_copy()
                    .withf(|req| req.durable)
                    .returning(|_| Ok(()));

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                store
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        CopyBehavior {
                            durable: true,
                            do_not_replicate: false,
                        },
                    )
                    .await
                    .expect("copy should succeed");
            })
            .await;
    }

    #[tokio::test]
    async fn copy_propagates_service_error() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        LORE_CONTEXT
            .scope(execution, async move {
                let source_partition: Partition = random();
                let (_, source_address, _) = fragment::generate_random();
                let destination_partition: Partition = random();
                let destination_context: Context = random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_copy().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let error = store
                    .copy(
                        source_partition,
                        source_address,
                        destination_partition,
                        destination_context,
                        CopyBehavior {
                            durable: false,
                            do_not_replicate: false,
                        },
                    )
                    .await
                    .expect_err("copy should fail on service error");
                assert!(matches!(error, StoreError::AddressNotFound(_)));
            })
            .await;
    }
}

mod get_metadata {
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment;
    use mockall::predicate::eq;
    use rand::random;

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
                let partition: Partition = random();
                let (fragment, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_get_metadata()
                    .with(eq(GetMetadata {
                        header: ReplicationHeader {
                            correlation_id,
                            repository: partition.into(),
                        },
                        address,
                    }))
                    .returning(move |_| {
                        Ok(StoreGetData {
                            fragment,
                            match_made: lore_storage::StoreMatch::MatchFull,
                            partition,
                            payload: None,
                            content: None,
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let output = store
                    .get_metadata(partition, address)
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
                let partition: Partition = random();
                let (fragment, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client
                    .expect_get_metadata()
                    .with(eq(GetMetadata {
                        header: ReplicationHeader {
                            correlation_id,
                            repository: partition.into(),
                        },
                        address,
                    }))
                    .returning(move |_| {
                        Ok(StoreGetData {
                            fragment,
                            match_made: lore_storage::StoreMatch::MatchPartition,
                            partition,
                            payload: None,
                            content: None,
                        })
                    });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let output = store
                    .get_metadata(partition, address)
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
                let partition: Partition = random();
                let (_, address, _) = fragment::generate_random();

                let (tx, rx) = mpsc::channel(1);
                let factory = ChannelFactory { rx: rx.into() };

                let mut client = make_mock_client();
                client.expect_get_metadata().returning(|_| {
                    Err(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::SlowDown,
                    ))
                });

                tx.send(Ok(client)).await.unwrap();
                let store = ReplicatedStore::new(
                    Arc::new(factory),
                    make_client_container_config(),
                    NonZeroUsize::MIN,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");

                let error = store
                    .get_metadata(partition, address)
                    .await
                    .expect_err("get_metadata should fail");
                assert!(matches!(error, StoreError::SlowDown(_)));
            })
            .await;
    }
}

mod new {
    use super::*;

    #[tokio::test]
    async fn connects_a_client_for_every_slot() {
        let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;

        assert_eq!(store.client_containers.len(), 2);
    }

    #[tokio::test]
    async fn fails_when_any_client_cannot_connect() {
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(make_mock_client())).await.unwrap();
        tx.send(Err(ProtocolError::internal("test-error")))
            .await
            .unwrap();

        let error = ReplicatedStore::new(
            Arc::new(ChannelFactory { rx: rx.into() }),
            make_client_container_config(),
            TWO_CLIENTS,
            Duration::from_secs(60),
            Duration::from_secs(10),
        )
        .await
        .err()
        .expect("creation should fail");

        assert!(
            error.to_string().contains("test-error"),
            "unexpected error: {error}"
        );
    }
}

mod select_slot {
    use lore_server::quic::replication_store_service::client_container::RequestKind;

    use super::*;

    #[tokio::test]
    async fn slots_with_equal_counts_take_turns() {
        let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;

        let chosen: Vec<usize> = (0..4)
            .map(|_| store.select_slot(RequestKind::Get))
            .collect();

        assert_eq!(chosen, vec![0, 1, 0, 1]);
    }

    #[tokio::test]
    async fn the_slot_with_fewest_requests_of_the_kind_in_flight_is_chosen() {
        let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
        let (_, _, _held) = store.client_containers[0].lease(RequestKind::Get, None);

        for _ in 0..4 {
            assert_eq!(store.select_slot(RequestKind::Get), 1);
        }
    }

    #[tokio::test]
    async fn slots_level_on_the_kind_go_by_their_total() {
        let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
        let (_, _, _held) = store.client_containers[0].lease(RequestKind::Get, None);

        for _ in 0..4 {
            assert_eq!(store.select_slot(RequestKind::Put), 1);
        }
    }

    #[tokio::test]
    async fn the_count_of_the_kind_outweighs_the_total() {
        let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
        let (_, _, _put_1) = store.client_containers[0].lease(RequestKind::Put, None);
        let (_, _, _put_2) = store.client_containers[0].lease(RequestKind::Put, None);
        let (_, _, _get) = store.client_containers[1].lease(RequestKind::Get, None);

        for _ in 0..4 {
            assert_eq!(store.select_slot(RequestKind::Get), 0);
        }
    }

    /// An unhealthy slot is passed over even when it has fewer requests in flight, since its
    /// requests fail fast and its count stays low.
    #[tokio::test]
    async fn unhealthy_slots_are_skipped() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
                let (_, _, _held) = store.client_containers[1].lease(RequestKind::Get, None);

                let regen = store.client_containers[0]
                    .regenerate_client(0, GenerateClientReason::ConnectionFailed);
                tokio::pin!(regen);
                assert!(futures::poll!(&mut regen).is_pending());
                assert!(!store.client_containers[0].is_healthy());

                for _ in 0..4 {
                    assert_eq!(store.select_slot(RequestKind::Get), 1);
                }
            })
            .await;
    }

    /// A store request goes to the chosen slot, and a connection failure on it is reported
    /// against that slot alone.
    #[tokio::test(start_paused = true)]
    async fn a_store_request_runs_on_the_less_loaded_slot() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let (tx, rx) = mpsc::channel(2);
                for _ in 0..2 {
                    let mut client = make_mock_client();
                    client
                        .expect_get()
                        .returning(|_| Err(ReplicationStoreClientError::ConnectionFailed));
                    tx.send(Ok(client)).await.unwrap();
                }
                let store = ReplicatedStore::new(
                    Arc::new(ChannelFactory { rx: rx.into() }),
                    make_client_container_config(),
                    TWO_CLIENTS,
                    Duration::from_secs(60),
                    Duration::from_secs(10),
                )
                .await
                .expect("Creation should work");
                let (_, _, _held) = store.client_containers[0].lease(RequestKind::Get, None);
                let partition: Partition = rand::random();
                let (_, address, _) = lore_revision::fragment::generate_random();

                let error = store
                    .clone()
                    .get(partition, address)
                    .await
                    .expect_err("the client fails the request");
                assert!(matches!(error, StoreError::Internal(_)));

                // The factory is empty, so the failed slot stays unhealthy while waiting for a
                // replacement client.
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(store.client_containers[0].is_healthy());
                assert!(!store.client_containers[1].is_healthy());
            })
            .await;
    }
}

mod drop {
    use super::*;

    /// The store's background tasks hold its clients, not the store, so dropping the store
    /// stops them and releases the clients even while a slot is reconnecting.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_store_releases_its_clients_while_a_slot_reconnects() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
                let (meta, _) = store.client_containers[1].request_client(None);
                let _ = handle_service_response::<(), MockClient>(
                    Err(ReplicationStoreClientError::ConnectionFailed),
                    &store,
                    1,
                    meta,
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!store.client_containers[1].is_healthy());

                let containers: Vec<_> =
                    store.client_containers.iter().map(Arc::downgrade).collect();
                drop(store);
                tokio::time::sleep(Duration::from_millis(100)).await;

                for container in containers {
                    assert!(container.upgrade().is_none());
                }
            })
            .await;
    }
}

mod is_available {
    use super::*;

    #[tokio::test]
    async fn available_while_any_slot_is_healthy() {
        let execution =
            lore_server::util::setup_execution("test", String::default(), String::default());
        lore_base::runtime::LORE_CONTEXT
            .scope(execution, async move {
                let (store, _tx) = make_store_with_slots(TWO_CLIENTS).await;
                assert!(store.clone().is_available(Duration::from_secs(1)).await);

                let regen_0 = store.client_containers[0]
                    .regenerate_client(0, GenerateClientReason::ConnectionFailed);
                tokio::pin!(regen_0);
                assert!(futures::poll!(&mut regen_0).is_pending());
                assert!(store.clone().is_available(Duration::from_secs(1)).await);

                let regen_1 = store.client_containers[1]
                    .regenerate_client(0, GenerateClientReason::ConnectionFailed);
                tokio::pin!(regen_1);
                assert!(futures::poll!(&mut regen_1).is_pending());
                assert!(!store.clone().is_available(Duration::from_secs(1)).await);
            })
            .await;
    }
}

mod first_refresh_delay {
    use super::*;

    #[test]
    fn slots_are_spaced_evenly_across_one_period() {
        let period = Duration::from_secs(60);

        let delays: Vec<Duration> = (0..3)
            .map(|slot| first_refresh_delay(slot, 3, period, 0.0))
            .collect();

        assert_eq!(
            delays,
            vec![
                Duration::from_secs(20),
                Duration::from_secs(40),
                Duration::from_secs(60)
            ]
        );
    }

    /// The largest jitter delays a slot by half the spacing, so it still refreshes before the
    /// next slot could.
    #[test]
    fn jitter_adds_up_to_half_the_spacing() {
        let period = Duration::from_secs(60);

        assert_eq!(
            first_refresh_delay(0, 3, period, 1.0),
            Duration::from_secs(30)
        );
    }
}
