// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Integration tests for the replication gRPC service.
//!
//! The service is mTLS-only, so each test stands up its own server: a CA and a matching
//! server/client pair are generated per test, written to a tempdir because the server takes cert
//! *paths*, and the client trusts that CA and no other. Nothing is shared between tests and
//! nothing is left behind, so the suite needs no external setup to run.

#[cfg(all(test, feature = "grpc_integration_tests"))]
mod replication_service_tests {
    use std::collections::HashSet;
    use std::error::Error;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::Partition;
    use lore_proto::PutRequest;
    use lore_proto::ReplicationPutRequest;
    use lore_proto::rpc::replication_service_client::ReplicationServiceClient;
    use lore_revision::fragment::generate_random;
    use lore_revision::util;
    use lore_server::grpc::GrpcInternalServerBuilder;
    use lore_server::store::grpc_replica::ReplicationClient;
    use lore_server::store::grpc_replica::ReplicationClientError;
    use tokio::sync::mpsc;
    use tokio::task::JoinSet;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Certificate;
    use tonic::transport::Channel;
    use tonic::transport::ClientTlsConfig;
    use tonic::transport::Identity;

    use crate::setup_execution;

    /// An mTLS chain: one CA, a server certificate for `localhost`, and a client certificate.
    struct TestCerts {
        _dir: tempfile::TempDir,
        ca_pem: String,
        server_cert_path: PathBuf,
        server_key_path: PathBuf,
        ca_path: PathBuf,
        client_cert_pem: String,
        client_key_pem: String,
    }

    /// Generate a fresh CA and a server/client pair signed by it.
    ///
    /// A separate chain per test keeps one test's certificates from validating another's server,
    /// and means `test_invalid_cert_causes_mtls_failure` can present a certificate from an
    /// unrelated CA simply by generating a second chain.
    fn generate_certs() -> Result<TestCerts, Box<dyn Error>> {
        use rcgen::BasicConstraints;
        use rcgen::CertificateParams;
        use rcgen::IsCa;
        use rcgen::Issuer;
        use rcgen::KeyPair;
        use rcgen::KeyUsagePurpose;

        let ca_key = KeyPair::generate()?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let ca_pem = ca_cert.pem();
        let issuer = Issuer::new(ca_params, ca_key);

        let server_key = KeyPair::generate()?;
        let server_params = CertificateParams::new(vec!["localhost".to_string()])?;
        let server_cert = server_params.signed_by(&server_key, &issuer)?;

        let client_key = KeyPair::generate()?;
        let client_params = CertificateParams::new(vec!["client".to_string()])?;
        let client_cert = client_params.signed_by(&client_key, &issuer)?;

        let dir = tempfile::Builder::new()
            .prefix("lore-replication-certs-")
            .tempdir()?;
        let ca_path = dir.path().join("ca.crt");
        let server_cert_path = dir.path().join("server.crt");
        let server_key_path = dir.path().join("server.key");
        std::fs::write(&ca_path, &ca_pem)?;
        std::fs::write(&server_cert_path, server_cert.pem())?;
        std::fs::write(&server_key_path, server_key.serialize_pem())?;

        Ok(TestCerts {
            _dir: dir,
            ca_pem,
            server_cert_path,
            server_key_path,
            ca_path,
            client_cert_pem: client_cert.pem(),
            client_key_pem: client_key.serialize_pem(),
        })
    }

    /// A running replication server and the material needed to talk to it.
    struct TestServer {
        addr: SocketAddr,
        certs: TestCerts,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// Block until `addr` accepts a connection, panicking rather than letting the test discover a
    /// failed bind as a peer that never answers.
    async fn await_listening(addr: SocketAddr) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("replication server at {addr} never started listening");
    }

    async fn start_server() -> Result<TestServer, Box<dyn Error>> {
        let certs = generate_certs()?;

        // Bound here and handed to the server below, so there is no interval in which the number is
        // spoken for but not held. This service is TCP only, so one socket is the whole port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;

        let immutable = lore_storage::local::immutable_store::create(
            None::<&str>,
            lore_storage::local::immutable_store::ImmutableStoreCreateOptions::none(),
            false,
            lore_storage::local::immutable_store::ImmutableStoreSettings::default(),
        )
        .await?;
        let mutable = lore_storage::local::mutable_store::create(
            None::<&str>,
            lore_storage::MutableStoreSettings::default(),
            immutable.clone(),
        )
        .await?;
        let notification: Arc<dyn lore_revision::notification::NotificationSender> =
            Arc::new(lore_server::notification::local::NotificationSender::default());
        let hooks = Arc::new(lore_server::hooks::HookDispatcher::empty());

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let signal = async {
            shutdown_rx.await.ok();
        };
        let server = GrpcInternalServerBuilder::new()
            .with_components(
                immutable.clone(),
                immutable,
                mutable,
                notification,
                hooks,
                lore_revision::environment::EnvironmentConfig::default(),
            )?
            .with_tls_config(
                Some(certs.server_cert_path.clone()),
                Some(certs.server_key_path.clone()),
                Some(certs.ca_path.clone()),
            )?
            .with_http2_config(None, None, Default::default(), Duration::from_secs(30))?;

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            server
                .serve_with_listener(listener, signal)
                .await
                .expect("replication serve");
        });
        await_listening(addr).await;

        Ok(TestServer {
            addr,
            certs,
            _shutdown: shutdown_tx,
        })
    }

    /// Connect to `server`, presenting `identity_from` as the client certificate.
    ///
    /// Connect using `certs` for both the trust anchor and the client identity.
    ///
    /// Passing a chain unrelated to the server's is how the mTLS rejection case is expressed, and
    /// it is the whole chain that has to be unrelated: the client then neither trusts the
    /// certificate the server presents nor presents one the server trusts, so the handshake fails
    /// on the client's own verification of the server — deterministically, during `connect`, rather
    /// than waiting on the server to reject us.
    async fn connect(
        server: &TestServer,
        certs: &TestCerts,
    ) -> Result<ReplicationServiceClient<Channel>, Box<dyn Error>> {
        let tls = ClientTlsConfig::new()
            .domain_name("localhost")
            .ca_certificate(Certificate::from_pem(certs.ca_pem.clone()))
            .identity(Identity::from_pem(
                certs.client_cert_pem.clone(),
                certs.client_key_pem.clone(),
            ));

        Ok(ReplicationServiceClient::new(
            Channel::from_shared(format!("https://localhost:{}", server.addr.port()))?
                .tls_config(tls)?
                .connect()
                .await?,
        ))
    }

    async fn get_channel(
        server: &TestServer,
    ) -> Result<ReplicationServiceClient<Channel>, Box<dyn Error>> {
        connect(server, &server.certs).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_replication_put() -> Result<(), Box<dyn Error>> {
        let server = start_server().await?;
        let mut channel = get_channel(&server).await?;

        let (tx, rx) = mpsc::channel::<ReplicationPutRequest>(10);

        let stream = ReceiverStream::new(rx);

        let mut addresses: HashSet<Address> = HashSet::new();
        for _ in 0..10 {
            let request = put_request();
            addresses.insert(
                request
                    .put_request
                    .as_ref()
                    .map(|r| r.address.as_ref().unwrap().into())
                    .unwrap(),
            );
            tx.send(request).await?;
        }

        let mut response = channel.put(stream).await?.into_inner();

        // Drop the sender so the client side of the connection closes
        drop(tx);

        let mut seen = HashSet::new();
        while let Some(message) = response.message().await? {
            if let Some(address) = message.address {
                seen.insert(address.into());
            }
        }

        assert_eq!(addresses, seen);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_replication_client() -> Result<(), Box<dyn Error>> {
        let server = start_server().await?;
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let client = ReplicationClient::new(
                    get_channel(&server).await.unwrap(),
                    500, /* buffer */
                    util::time::RetryPolicy::builder()
                        .with_initial_backoff_millis(50)
                        .with_max_backoff_millis(1000)
                        .with_limit(3)
                        .build(),
                );

                let client = Arc::new(client);

                // Establish the shared put stream before fanning out. `do_put` sheds a request
                // with `SlowDown` when it finds stream creation already in progress, so without
                // this the tasks race to create it and the losers are dropped — a real behaviour
                // of the client, but not what this test is about.
                {
                    let (fragment, address, payload) = generate_random();
                    client
                        .put(
                            rand::random::<Partition>(),
                            address,
                            fragment,
                            Some(payload),
                        )
                        .await
                        .expect("warm-up put must succeed");
                }

                let mut join_set = JoinSet::new();
                for _ in 0..100 {
                    let client = client.clone();
                    lore_spawn!(join_set, async move {
                        let repository = rand::random::<Partition>();
                        let (fragment, address, payload) = generate_random();

                        client
                            .put(repository, address, fragment, Some(payload))
                            .await
                    });
                }

                while let Some(result) = join_set.join_next().await {
                    result
                        .expect("task panicked")
                        .expect("every put must succeed");
                }
            })
            .await;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_replication_client_stream_full() -> Result<(), Box<dyn Error>> {
        let server = start_server().await?;
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution, async move {
                let client = ReplicationClient::new(
                    get_channel(&server).await.unwrap(),
                    1, /* buffer, to ensure that we get a slow-down limit to 1 message at a time */
                    util::time::RetryPolicy::builder()
                        .with_initial_backoff_millis(50)
                        .with_max_backoff_millis(1000)
                        .with_limit(3)
                        .build(),
                );

                let client = Arc::new(client);
                let mut join_set = JoinSet::new();
                for _ in 0..2 {
                    let client = client.clone();
                    lore_spawn!(join_set, async move {
                        let repository = rand::random::<Partition>();
                        let (fragment, address, payload) = generate_random();

                        client
                            .put(repository, address, fragment, Some(payload))
                            .await
                    });
                }

                // Assert on the set of outcomes rather than on which task produced which. With a
                // buffer of one, one of the two is shed — but the client picks whichever request
                // reaches `try_send` second, and that is not the second task spawned. Keying the
                // expectation to the loop index is what made this flaky: the run where task 0 lost
                // the race failed, at roughly one run in three here.
                //
                // A shed request is allowed but not required, as it was before: the buffer is free
                // again the moment the stream task drains it, so a sufficiently fast drain lets
                // both through. Observed outcome on this machine is consistently one of each.
                let mut ok = 0;
                while let Some(result) = join_set.join_next().await {
                    match result.expect("task panicked") {
                        Ok(()) => ok += 1,
                        // The point of the buffer of one: a request that cannot be queued is
                        // rejected with `SlowDown` for the caller to retry, not blocked on and not
                        // failed outright.
                        Err(ReplicationClientError::SlowDown) => {}
                        Err(other) => panic!("expected Ok or SlowDown, got {other:?}"),
                    }
                }
                assert!(ok >= 1, "at least one of the two puts must be accepted");
            })
            .await;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_invalid_cert_causes_mtls_failure() -> Result<(), Box<dyn Error>> {
        let server = start_server().await?;
        let untrusted = generate_certs()?;

        let result = connect(&server, &untrusted).await;
        assert!(result.is_err());

        let transport_error: Box<tonic::transport::Error> = result
            .unwrap_err()
            .downcast::<tonic::transport::Error>()
            .expect("expected tonic transport error");

        // Unfortunately there's no good way to verify the error is a specific type of error short
        // of just formatting it. This is, of course, prone to fail if anything in tonic changes how
        // errors are formatted.
        assert_eq!(
            format!("{transport_error:?}"),
            "tonic::transport::Error(Transport, ConnectError(Custom { kind: InvalidData, error: InvalidCertificate(BadSignature) }))"
        );

        Ok(())
    }

    fn put_request() -> ReplicationPutRequest {
        let repository = rand::random::<Context>();
        let (fragment, address, payload) = generate_random();

        ReplicationPutRequest {
            repository_id: repository.into(),
            put_request: Some(PutRequest {
                address: Some(address.into()),
                fragment: Some(fragment.into()),
                payload: Some(payload),
            }),
        }
    }
}
