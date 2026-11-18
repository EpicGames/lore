// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
use lore_base::types::Fragment;
use lore_base::types::Partition;
use lore_server::quic::replication_store_service::ReplicationServiceErrorCode;
use lore_server::quic::replication_store_service::client::*;
use lore_storage::StoreError;
use lore_transport::quic::QuicClientError;

mod integration {
    use std::net::SocketAddr;
    use std::net::UdpSocket;
    use std::time::Duration;
    use std::time::Instant;

    use futures::FutureExt;
    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_revision::fragment::generate_random;
    use lore_revision::store::composite::ReplicaType;
    use lore_server::protocol::client_identify::UserAgentValue;
    use lore_server::protocol::replication_store::get::Get;
    use lore_server::protocol::replication_store::header::ReplicationHeader;
    use lore_server::quic::quinn::QuinnConfigBuilder;
    use lore_server::quic::quinn::QuinnServer;
    use lore_server::quic::replication_store_service::client::CommandBehavior;
    use lore_server::quic::replication_store_service::client::ReplicationStoreClient;
    use lore_server::quic::replication_store_service::client_container::ClientFactory;
    use lore_server::quic::tests::ObservedContexts;
    use lore_server::quic::tests::TestHandlerFactory;
    use lore_server::quic::tests::server_certs;
    use lore_server::server::replicated_client_factory;
    use lore_server::settings::ReplicatedStoreSettings;
    use lore_server::store::replica_factory::READ_PEER_COMMAND_PERMIT_WAIT;
    use lore_server::store::replica_factory::peer_command_permit_wait;
    use lore_transport::quic::client::CertificateSettings;
    use lore_transport::quic::client::CongestionAlgorithm;
    use lore_transport::quic::client::DEFAULT_EXPECTED_RTT_MS;
    use lore_transport::quic::client::ServiceClient;
    use lore_transport::quic::client::TransportConfig;
    use rand::random;

    use super::*;
    use crate::store::test_support::test_store_create;

    fn start_test_server(
        immutable_store: std::sync::Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: std::sync::Arc<dyn lore_storage::MutableStore>,
    ) -> (SocketAddr, QuinnServer, ObservedContexts) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = socket.local_addr().unwrap();
        drop(socket);

        let factory = TestHandlerFactory::new(immutable_store, mutable_store);
        let contexts = factory.observed_contexts();

        let (cert_path, key_path, _ca) = server_certs().expect("Bad server cert paths");
        let server = QuinnServer::start(
            QuinnConfigBuilder::new()
                .address(server_addr)
                .cert_file(cert_path)
                .pkey_file(key_path)
                .stream_handler_factory(Box::new(factory))
                .build()
                .unwrap(),
        )
        .expect("Failed to start test QUIC server");

        (server_addr, server, contexts)
    }

    /// Polls the first connection the server accepted until it carries a user agent, and
    /// returns it. Panics if none arrives within `ANNOUNCE_TIMEOUT`.
    async fn await_announced_user_agent(contexts: &ObservedContexts) -> String {
        const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
        const POLL_INTERVAL: Duration = Duration::from_millis(10);

        let poll = async {
            loop {
                let user_agent = contexts
                    .lock()
                    .first()
                    .and_then(|context| context.get::<UserAgentValue>())
                    .map(|value| value.0.to_string());

                if let Some(user_agent) = user_agent {
                    return user_agent;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        };

        tokio::time::timeout(ANNOUNCE_TIMEOUT, poll)
            .await
            .expect("user agent must be announced")
    }

    fn no_tls_transport() -> TransportConfig {
        TransportConfig {
            max_bytes_bandwidth_per_second: 1_000_000,
            expected_rtt_ms: DEFAULT_EXPECTED_RTT_MS,
            congestion_algorithm: CongestionAlgorithm::Bbr,
            initial_cwnd: None,
        }
    }

    fn permissive_command_behavior() -> CommandBehavior {
        CommandBehavior {
            message_limit: 10,
            permit_wait: CommandPermitWait::Refuse,
        }
    }

    /// Connects a client that sends one command at a time and waits for its command permit as a
    /// peer client of `replica_type` does.
    async fn connect_peer_client_with_one_permit(
        server_addr: SocketAddr,
        replica_type: ReplicaType,
    ) -> ReplicationStoreClient {
        ReplicationStoreClient::connect(
            &format!("quic://{server_addr}"),
            CertificateSettings {
                custom_ca: None,
                client: None,
            },
            None,
            no_tls_transport(),
            CommandBehavior {
                message_limit: 1,
                permit_wait: peer_command_permit_wait(replica_type),
            },
            None,
            None,
        )
        .await
        .expect("connect must succeed")
    }

    fn get_of_a_missing_address() -> Get {
        let (_, address, _) = generate_random();
        Get {
            header: ReplicationHeader {
                correlation_id: Default::default(),
                repository: random(),
            },
            address,
        }
    }

    /// A read peer client whose only permit is held waits for it, and sends the request once the
    /// permit is released. The server's answer shows the request was sent.
    #[tokio::test]
    async fn read_peer_client_sends_once_a_permit_is_released_within_its_wait() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, _contexts) =
                start_test_server(immutable_store, mutable_store);
            let client = connect_peer_client_with_one_permit(server_addr, ReplicaType::Read).await;
            let held = client
                .acquire_command_permit()
                .await
                .expect("the only permit is free");

            let mut get = client.get(get_of_a_missing_address());
            assert!(
                futures::poll!(&mut get).is_pending(),
                "the get must wait for the held permit"
            );
            drop(held);

            let error = get.await.err();
            assert!(
                matches!(
                    error,
                    Some(ReplicationStoreClientError::ServiceError(
                        ReplicationServiceErrorCode::AddressNotFound
                    ))
                ),
                "unexpected result: {error:?}"
            );
        }))
        .await
        .expect("Test task failed");
    }

    /// A read peer client whose only permit stays held refuses the request once its wait ends.
    #[tokio::test]
    async fn read_peer_client_refuses_when_no_permit_is_released_within_its_wait() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, _contexts) =
                start_test_server(immutable_store, mutable_store);
            let client = connect_peer_client_with_one_permit(server_addr, ReplicaType::Read).await;
            let _held = client
                .acquire_command_permit()
                .await
                .expect("the only permit is free");

            let start = Instant::now();
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                client.get(get_of_a_missing_address()),
            )
            .await
            .expect("the get should end once its permit wait has passed")
            .err();

            assert!(
                matches!(
                    error,
                    Some(ReplicationStoreClientError::ClientSideThrottling)
                ),
                "unexpected result: {error:?}"
            );
            assert!(
                start.elapsed() >= READ_PEER_COMMAND_PERMIT_WAIT,
                "refused after {:?}, before the wait of {READ_PEER_COMMAND_PERMIT_WAIT:?}",
                start.elapsed()
            );
        }))
        .await
        .expect("Test task failed");
    }

    /// A write peer client whose only permit is held refuses the request without waiting.
    #[tokio::test]
    async fn write_peer_client_refuses_at_once_when_no_permit_is_free() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, _contexts) =
                start_test_server(immutable_store, mutable_store);
            let client = connect_peer_client_with_one_permit(server_addr, ReplicaType::Write).await;
            let _held = client
                .acquire_command_permit()
                .await
                .expect("the only permit is free");

            let error = client
                .get(get_of_a_missing_address())
                .now_or_never()
                .expect("the get must not wait for the held permit")
                .err();

            assert!(
                matches!(
                    error,
                    Some(ReplicationStoreClientError::ClientSideThrottling)
                ),
                "unexpected result: {error:?}"
            );
        }))
        .await
        .expect("Test task failed");
    }

    /// With no user agent supplied, the lore-transport default is what reaches the server.
    #[tokio::test]
    async fn connect_with_no_user_agent_announces_the_default() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, contexts) =
                start_test_server(immutable_store, mutable_store);

            let _client = ReplicationStoreClient::connect(
                &format!("quic://{server_addr}"),
                CertificateSettings {
                    custom_ca: None,
                    client: None,
                },
                None,
                no_tls_transport(),
                permissive_command_behavior(),
                None,
                None,
            )
            .await
            .expect("connect with no user agent must succeed");

            assert_eq!(
                await_announced_user_agent(&contexts).await,
                lore_transport::user_agent()
            );
        }))
        .await
        .expect("Test task failed");
    }

    /// A user agent supplied by the caller reaches the server's per-connection context.
    #[tokio::test]
    async fn connect_with_user_agent_announces_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, contexts) =
                start_test_server(immutable_store, mutable_store);

            let _client = ReplicationStoreClient::connect(
                &format!("quic://{server_addr}"),
                CertificateSettings {
                    custom_ca: None,
                    client: None,
                },
                None,
                no_tls_transport(),
                permissive_command_behavior(),
                None,
                Some("lore-test/1.0".to_string()),
            )
            .await
            .expect("connect with user agent must succeed");

            assert_eq!(await_announced_user_agent(&contexts).await, "lore-test/1.0");
        }))
        .await
        .expect("Test task failed");
    }

    /// A replicated store's client connection runs the congestion controller its settings name,
    /// read back from the QUIC connection. BBR, since the factory's own default is Cubic.
    #[tokio::test]
    async fn a_replicated_stores_client_runs_the_congestion_algorithm_its_settings_name() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create store");

        lore_spawn!(LORE_CONTEXT.scope(execution.clone(), async move {
            let (server_addr, _server, _contexts) =
                start_test_server(immutable_store, mutable_store);
            let settings: ReplicatedStoreSettings = toml::from_str(&format!(
                r#"
                    remote_url = "quic://{server_addr}"
                    periodic_client_refresh_secs = 300
                    congestion_algorithm = "bbr"

                    [regenerate_retry]
                    initial_backoff_ms = 250
                    max_backoff_ms = 10_000
                    max_attempts = 5
                "#
            ))
            .expect("valid settings");

            let client = replicated_client_factory(&settings)
                .make_client(None)
                .await
                .expect("a client of the replicated store");

            assert_eq!(
                client.quic().running_congestion_algorithm().await,
                Some(CongestionAlgorithm::Bbr)
            );
        }))
        .await
        .expect("Test task failed");
    }
}

#[test]
fn throttling_errors_map_to_slow_down() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error =
        map_client_error_to_store_error(ReplicationStoreClientError::ClientSideThrottling, &meta);
    assert!(matches!(error, StoreError::SlowDown(_)));

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ServerSideMessageThrottling,
        &meta,
    );
    assert!(matches!(error, StoreError::SlowDown(_)));
}

#[test]
fn service_error_address_not_found_maps_correctly() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::AddressNotFound),
        &meta,
    );
    assert!(matches!(error, StoreError::AddressNotFound(_)));
}

#[test]
fn service_error_slow_down_maps_correctly() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::SlowDown),
        &meta,
    );
    assert!(matches!(error, StoreError::SlowDown(_)));
}

#[test]
fn service_error_payload_not_found_maps_correctly() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::PayloadNotFound),
        &meta,
    );
    assert!(matches!(error, StoreError::PayloadNotFound(_)));
}

#[test]
fn service_error_internal_maps_correctly() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ServiceError(ReplicationServiceErrorCode::Internal),
        &meta,
    );
    assert!(matches!(error, StoreError::Internal(_)));
}

#[test]
fn connection_failed_maps_to_internal() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error =
        map_client_error_to_store_error(ReplicationStoreClientError::ConnectionFailed, &meta);
    assert!(matches!(error, StoreError::Internal(_)));
}

#[test]
fn make_put_message_rejects_oversized_payload() {
    let payload = Bytes::from(vec![0u8; FRAGMENT_SIZE_THRESHOLD + 1]);
    let result = make_put_message(
        Partition::default(),
        Address::default(),
        Fragment::default(),
        Some(payload),
        false,
    );
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ReplicationStoreClientError::UnexpectedClientError(QuicClientError::ClientMessageTooBig)
    ));
}

#[test]
fn response_error_maps_to_internal() {
    let meta = ServiceRequestMeta {
        client_epoch: 0,
        address: None,
    };

    let error = map_client_error_to_store_error(
        ReplicationStoreClientError::ResponseError("bad response"),
        &meta,
    );
    assert!(matches!(error, StoreError::Internal(_)));
}
