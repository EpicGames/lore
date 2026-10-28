// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::time::Duration;

use lore_base::error::*;
use lore_base::types::*;
use lore_error_set::ext::ResultExt;
use lore_transport::auth::AuthMode;
use lore_transport::connection::*;
use lore_transport::error::ProtocolError;
use lore_transport::session::StorageSession;
use lore_transport::types::*;

/// A long-lived process rotates the credentials it supplies. The connection
/// outlives any one of them, so the services it already built have to see
/// the newest ones -- the server checks gRPC authorization on every request
/// and storage authorization at each session start, so what matters is what
/// the client presents at the time, not what it was given when it connected.
/// The fallback for a caller with no identity yet must not cross modes: a
/// call working from the token store would otherwise be handed a connection
/// opened for one that supplied its own credentials, and be authorized by
/// them. Same user either way, but each asked to be authorized a particular
/// way.
#[test]
fn the_no_identity_fallback_does_not_cross_credential_modes() {
    let remote = "lores://mode-isolation.test.invalid";
    let auto = AuthMode::Auto;
    let stored = (
        remote.to_string(),
        "alice".to_string(),
        false,
        String::new(),
        auto,
    );
    let supplied = (
        remote.to_string(),
        "alice".to_string(),
        true,
        String::new(),
        auto,
    );

    assert!(matches_url_mode_and_store(&stored, remote, false, "", auto));
    assert!(matches_url_mode_and_store(
        &supplied, remote, true, "", auto
    ));

    assert!(
        !matches_url_mode_and_store(&supplied, remote, false, "", auto),
        "a store-mode call must not be given a supplied-credential connection"
    );
    assert!(
        !matches_url_mode_and_store(&stored, remote, true, "", auto),
        "a supplied-credential call must not be given a store-mode connection"
    );

    assert!(!matches_url_mode_and_store(
        &stored,
        "lores://elsewhere.invalid",
        false,
        "",
        auto
    ));
}

/// The lookup runs before the environment is fetched, so which path a mode
/// resolves to is unknown at that point. A call in one auth mode must not be
/// handed a connection opened in another: an `oidc` call after an `auto` one
/// to the same remote would otherwise run over the gRPC path the first call
/// selected, and the inverse.
#[test]
fn the_no_identity_fallback_does_not_cross_auth_modes() {
    let remote = "lores://auth-mode-isolation.test.invalid";
    let opened_auto = (
        remote.to_string(),
        "alice".to_string(),
        false,
        String::new(),
        AuthMode::Auto,
    );
    let opened_oidc = (
        remote.to_string(),
        "alice".to_string(),
        false,
        String::new(),
        AuthMode::Oidc,
    );

    assert!(matches_url_mode_and_store(
        &opened_auto,
        remote,
        false,
        "",
        AuthMode::Auto
    ));
    assert!(matches_url_mode_and_store(
        &opened_oidc,
        remote,
        false,
        "",
        AuthMode::Oidc
    ));

    assert!(!matches_url_mode_and_store(
        &opened_auto,
        remote,
        false,
        "",
        AuthMode::Oidc
    ));
    assert!(!matches_url_mode_and_store(
        &opened_auto,
        remote,
        false,
        "",
        AuthMode::Grpc
    ));
    assert!(!matches_url_mode_and_store(
        &opened_oidc,
        remote,
        false,
        "",
        AuthMode::Auto
    ));
}

/// Nor does it cross token stores: a process carrying out calls for other processes reads the
/// store each caller names, and the identity a call resolves is fixed by the URL and its own store,
/// not by whichever store opened the connection cached under the URL.
#[test]
fn the_no_identity_fallback_does_not_cross_token_stores() {
    let remote = "lores://store-isolation.test.invalid";
    let alices = (
        remote.to_string(),
        "alice".to_string(),
        false,
        "/home/alice/tokens".to_string(),
        AuthMode::Auto,
    );

    assert!(matches_url_mode_and_store(
        &alices,
        remote,
        false,
        "/home/alice/tokens",
        AuthMode::Auto
    ));
    assert!(
        !matches_url_mode_and_store(&alices, remote, false, "/home/bob/tokens", AuthMode::Auto),
        "a call reading another store must not be given the connection alice's opened"
    );
}

/// A call that supplied its own credentials must never be matched on URL
/// alone.
///
/// Which connection sits under a URL in that mode depends on whose token
/// opened it. If Alice's supplied-credential connection is cached and a
/// caller then supplies Bob's token, matching on the URL would run Bob's call
/// against Alice's connection and authorize it as Alice -- declining to write
/// Bob's credentials onto it does not help, since it is still the connection
/// the call gets. Such a call resolves its identity from the token it
/// supplied and matches the full key instead.
#[test]
fn a_call_supplying_credentials_is_never_matched_on_url_alone() {
    assert!(
        !may_match_on_url_alone("", true),
        "an identity-less supplied-credential call must resolve its identity, \
             not borrow whichever connection shares the URL"
    );
    assert!(!may_match_on_url_alone("bob", true));

    // The store-mode shortcut stands: the identity such a call resolves is
    // fixed by the URL and the store, so the entry there is its own.
    assert!(may_match_on_url_alone("", false));
    assert!(
        !may_match_on_url_alone("alice", false),
        "a named identity is matched on the full key either way"
    );
}

/// The mode a connection was opened in, which keys it apart from the other.
#[test]
fn credentials_report_whether_a_caller_supplied_them() {
    assert!(!SuppliedCredentials::default().from_supplied_credentials());
    assert!(SuppliedCredentials::new("an-identity-token", "").from_supplied_credentials());
    assert!(SuppliedCredentials::new("", "an-access-token").from_supplied_credentials());
}

#[test]
fn a_later_call_replaces_the_credentials_the_services_read() {
    let credentials = SuppliedCredentials::new("first-identity", "first-access");
    assert_eq!(
        credentials.tokens(),
        ("first-identity".to_string(), "first-access".to_string())
    );

    credentials.update("second-identity", "second-access");

    assert_eq!(
        credentials.tokens(),
        ("second-identity".to_string(), "second-access".to_string()),
        "the newest credentials are the ones handed out"
    );
}

/// A rotation has to reach the refreshers, or the tokens a service client
/// already presents would go on being the replaced ones until the next
/// scheduled refresh.
///
/// It has to reach a refresher whose signal was taken alongside the earlier
/// credentials too: deriving a token from them can take an exchange with the
/// auth service, and the connection is discoverable while that runs, so this
/// is the rotation most in need of reporting.
#[test]
fn replacing_the_credentials_signals_the_refreshers() {
    let credentials = SuppliedCredentials::new("first-identity", "");
    let ((identity_token, _), rotated) = credentials.tokens_and_signal();
    assert_eq!(identity_token, "first-identity");
    assert!(
        !rotated.has_changed().expect("the sender outlives this"),
        "nothing to report before a rotation"
    );

    credentials.update("second-identity", "");

    assert!(
        rotated.has_changed().expect("the sender outlives this"),
        "the refreshers must be told the credentials were replaced"
    );
}

/// A call repeating the credentials already in place is not a rotation, and
/// must not send every refresher off to re-derive tokens that have not
/// changed.
#[test]
fn repeating_the_same_credentials_signals_nothing() {
    let credentials = SuppliedCredentials::new("an-identity-token", "an-access-token");
    let (_, rotated) = credentials.tokens_and_signal();

    credentials.update("an-identity-token", "an-access-token");
    assert!(!rotated.has_changed().expect("the sender outlives this"));

    // Nor does a call that supplies nothing at all.
    credentials.update("", "");
    assert!(!rotated.has_changed().expect("the sender outlives this"));
}

/// A call that supplies nothing is asking for the usual resolution, not for
/// the connection to forget what it was given. Clearing here would strip the
/// credential from every service the connection already built.
#[test]
fn a_call_supplying_nothing_leaves_the_credentials_alone() {
    let credentials = SuppliedCredentials::new("an-identity-token", "");

    credentials.update("", "");

    assert_eq!(
        credentials.tokens(),
        ("an-identity-token".to_string(), String::new())
    );
}

#[test]
fn no_credentials_supplied_reads_as_empty() {
    let credentials = SuppliedCredentials::default();
    assert_eq!(credentials.tokens(), (String::new(), String::new()));
}
use lore_transport::MatchedProtocolError;

/// Every network call funnels through `parse`, so it is the one place that decides what a
/// repository with no remote URL reports. A repository may legitimately have no remote, and
/// that is not the same answer as a remote that is configured but cannot be reached.
mod parse {
    use super::*;

    #[test]
    fn no_remote_url_is_no_remote() {
        // `Arc<dyn Protocol>` is not `Debug`, so unwrap the error side rather than
        // reaching for `expect_err`, which would need to format the success value.
        let err = lore_transport::connection::parse("")
            .err()
            .expect("an empty remote URL cannot be parsed");
        assert!(
            matches!(err, ProtocolError::NoRemote(_)),
            "{err:?} should be NoRemote: there is no remote configured to reach"
        );
    }

    /// The distinction that matters to the caller: nothing configured is `NoRemote`, whereas a
    /// remote that is configured but malformed remains an internal fault rather than silently
    /// reading as an unconfigured repository.
    #[test]
    fn a_malformed_remote_url_is_not_no_remote() {
        let err = lore_transport::connection::parse("nonsense://host")
            .err()
            .expect("an unknown protocol cannot be parsed");
        assert!(
            !matches!(err, ProtocolError::NoRemote(_)),
            "{err:?} names a remote, so it must not report as having none"
        );
    }

    #[test]
    fn a_bare_host_gets_the_default_protocol() {
        let (url, _) =
            lore_transport::connection::parse("host:41337").expect("a bare host should parse");
        assert_eq!(url.scheme(), DEFAULT_PROTOCOL);
    }
}

/// A copy naming a source partition asks whether it may before it tries, and a `false` costs a
/// `session_start`. Latching the answer bounds that at one per partition — but only for the
/// failure that is actually about the claim, or a disconnect would disable a legitimate source
/// for as long as the connection lives.
mod refusal_is_final {
    use super::*;

    #[test]
    fn a_refusal_settles_it() {
        assert!(lore_transport::connection::refusal_is_final(
            &ProtocolError::from(NotAuthorized)
        ));
        assert!(lore_transport::connection::refusal_is_final(
            &ProtocolError::from(NotAuthenticated)
        ));
    }

    #[test]
    fn anything_else_is_worth_asking_again() {
        for err in [
            ProtocolError::from(Disconnected),
            ProtocolError::from(SlowDown),
            ProtocolError::from(Maintenance),
            ProtocolError::from(NotFound),
            ProtocolError::internal("transport blew up"),
        ] {
            assert!(
                !lore_transport::connection::refusal_is_final(&err),
                "{err:?} says the answer was not obtained, not that it is no"
            );
        }
    }
}

/// The server names the endpoint of each service, and each connection takes its own URL's
/// scheme. A remote whose scheme authenticates the server must not hand a service to an endpoint
/// that does not, or the user's choice of scheme protects only the first connection.
mod check_advertised_endpoints {
    use std::sync::Weak;

    use lore_transport::auth::AuthEndpoint;
    use lore_transport::traits::Admin;
    use lore_transport::traits::Environment;
    use lore_transport::traits::Lock;
    use lore_transport::traits::Protocol;
    use lore_transport::traits::Repository;
    use lore_transport::traits::Revision;
    use lore_transport::traits::Storage;
    use url::Url;

    use super::*;

    type Advertise = fn(&mut Endpoint, String);

    /// Every service the client connects to at an advertised URL.
    const SERVICES: [(&str, Advertise); 5] = [
        ("storage", |endpoint, url| {
            endpoint.storage_url = Some(url);
        }),
        ("revision", |endpoint, url| {
            endpoint.revision_url = Some(url);
        }),
        ("lock", |endpoint, url| {
            endpoint.lock_url = Some(url);
        }),
        ("repository", |endpoint, url| {
            endpoint.repository_url = Some(url);
        }),
        ("notification", |endpoint, url| {
            endpoint.notification_url = Some(url);
        }),
    ];

    const SECURE_REMOTES: [&str; 3] = [
        "lores://remote.invalid:41337",
        "urcs://remote.invalid:41337",
        "grpcs://remote.invalid:41337",
    ];

    fn check_endpoint(remote: &str, endpoint: Endpoint) -> Result<(), ProtocolError> {
        let environment = EnvironmentConfig {
            endpoint: Some(endpoint),
            ..Default::default()
        };
        let remote = Url::parse(remote).expect("the test remote is a valid URL");
        lore_transport::connection::check_advertised_endpoints(&remote, &environment)
    }

    fn check(remote: &str, advertise: Advertise, url: &str) -> Result<(), ProtocolError> {
        let mut endpoint = Endpoint::default();
        advertise(&mut endpoint, url.to_string());
        check_endpoint(remote, endpoint)
    }

    #[test]
    fn a_secure_remote_refuses_an_endpoint_that_skips_server_authentication() {
        for (remote, advertised) in [
            (
                "lores://remote.invalid:41337",
                "lore://service.invalid:41337",
            ),
            ("urcs://remote.invalid:41337", "urc://service.invalid:41337"),
            (
                "grpcs://remote.invalid:41337",
                "grpc://service.invalid:41337",
            ),
            (
                "lores://remote.invalid:41337",
                "quic://service.invalid:7000",
            ),
            ("lores://remote.invalid:41337", "not a url"),
        ] {
            for (service, advertise) in SERVICES {
                let err = check(remote, advertise, advertised).expect_err(service);
                assert!(
                    err.is_not_supported(),
                    "{remote} advertising {service} at {advertised}: {err:?}"
                );
            }
        }
    }

    #[test]
    fn a_secure_remote_refuses_an_insecure_endpoint_among_secure_ones() {
        for (insecure, _) in SERVICES {
            let mut endpoint = Endpoint::default();
            for (service, advertise) in SERVICES {
                let scheme = if service == insecure { "lore" } else { "lores" };
                advertise(&mut endpoint, format!("{scheme}://service.invalid:41337"));
            }
            let err = check_endpoint("lores://remote.invalid:41337", endpoint).expect_err(insecure);
            assert!(err.is_not_supported(), "{insecure}: {err:?}");
        }
    }

    #[test]
    fn a_secure_remote_accepts_an_endpoint_that_authenticates_the_server() {
        for remote in SECURE_REMOTES {
            for advertised in [
                "lores://service.invalid:41337",
                "urcs://service.invalid:41337",
                "grpcs://service.invalid:41337",
                "https://service.invalid",
            ] {
                for (service, advertise) in SERVICES {
                    if let Err(err) = check(remote, advertise, advertised) {
                        panic!("{remote} advertising {service} at {advertised}: {err:?}");
                    }
                }
            }
        }
    }

    /// The auth and user directory endpoints follow a scheme policy of their own.
    #[test]
    fn a_secure_remote_accepts_auth_and_user_endpoints_of_any_scheme() {
        check_endpoint(
            "lores://remote.invalid:41337",
            Endpoint {
                auth_url: Some("ucs-auth://auth.invalid".into()),
                user_url: Some("ucs-auth://directory.invalid".into()),
                ..Default::default()
            },
        )
        .expect("auth_url and user_url are not service endpoints");
    }

    /// An empty override falls back to the remote, as no override does.
    #[test]
    fn a_secure_remote_accepts_its_own_endpoints() {
        let remote = Url::parse("lores://remote.invalid:41337").expect("valid URL");
        for environment in [
            EnvironmentConfig::default(),
            EnvironmentConfig {
                endpoint: Some(Endpoint::default()),
                ..Default::default()
            },
        ] {
            lore_transport::connection::check_advertised_endpoints(&remote, &environment)
                .expect("no override leaves every service on the remote");
        }
        for (service, advertise) in SERVICES {
            check("lores://remote.invalid:41337", advertise, "").expect(service);
        }
    }

    /// A remote that skips server authentication has no guarantee to lose.
    #[test]
    fn an_insecure_remote_accepts_any_endpoint() {
        for advertised in [
            "lore://service.invalid:41337",
            "lores://service.invalid:41337",
        ] {
            for (service, advertise) in SERVICES {
                check("lore://remote.invalid:41337", advertise, advertised).expect(service);
            }
        }
    }

    /// A protocol whose server advertises the storage endpoint it holds, and whose services all
    /// fail to connect.
    struct AdvertisingProtocol(&'static str);

    #[async_trait::async_trait]
    impl Environment for AdvertisingProtocol {
        async fn get(&self) -> Result<EnvironmentConfig, ProtocolError> {
            Ok(EnvironmentConfig {
                endpoint: Some(Endpoint {
                    storage_url: Some(self.0.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
    }

    #[async_trait::async_trait]
    impl Protocol for AdvertisingProtocol {
        async fn storage(
            &self,
            _: Weak<Connection>,
            _: &str,
            _: &AuthEndpoint,
            _: &str,
            _: Partition,
            _: usize,
            _: &Arc<SuppliedCredentials>,
        ) -> Result<Arc<dyn Storage>, ProtocolError> {
            Err(ProtocolError::from(Disconnected))
        }

        async fn revision(
            &self,
            _: Weak<Connection>,
            _: &str,
            _: &AuthEndpoint,
            _: &str,
            _: RepositoryId,
            _: &Arc<SuppliedCredentials>,
        ) -> Result<Arc<dyn Revision>, ProtocolError> {
            Err(ProtocolError::from(Disconnected))
        }

        async fn repository(
            &self,
            _: Weak<Connection>,
            _: &str,
            _: &AuthEndpoint,
            _: &str,
            _: &Arc<SuppliedCredentials>,
        ) -> Result<Arc<dyn Repository>, ProtocolError> {
            Err(ProtocolError::from(Disconnected))
        }

        async fn admin(
            &self,
            _: Weak<Connection>,
            _: &str,
            _: &AuthEndpoint,
            _: &str,
            _: RepositoryId,
            _: &Arc<SuppliedCredentials>,
        ) -> Result<Arc<dyn Admin>, ProtocolError> {
            Err(ProtocolError::from(Disconnected))
        }

        async fn lock(
            &self,
            _: Weak<Connection>,
            _: &str,
            _: &AuthEndpoint,
            _: &str,
            _: RepositoryId,
            _: &Arc<SuppliedCredentials>,
        ) -> Result<Arc<dyn Lock>, ProtocolError> {
            Err(ProtocolError::from(Disconnected))
        }

        async fn environment(
            &self,
            _: Weak<Connection>,
            _: &str,
        ) -> Result<Arc<dyn Environment>, ProtocolError> {
            Ok(Arc::new(AdvertisingProtocol(self.0)))
        }
    }

    /// Both schemes end in `s`, so both remotes authenticate the server.
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_refuses_a_remote_advertising_an_endpoint_that_skips_server_authentication() {
        add(
            "refuses",
            Arc::new(AdvertisingProtocol("lore://service.invalid:41337")),
        )
        .expect("the scheme is not registered yet");
        add(
            "accepts",
            Arc::new(AdvertisingProtocol("lores://service.invalid:41337")),
        )
        .expect("the scheme is not registered yet");

        let refused = connect(
            "refuses://remote.invalid:41337",
            "",
            RepositoryId::default(),
            1,
            "",
            "",
            AuthMode::Auto,
        )
        .await
        .err()
        .expect("an endpoint that skips server authentication is refused");
        assert!(refused.is_not_supported(), "{refused:?}");

        connect(
            "accepts://remote.invalid:41337",
            "",
            RepositoryId::default(),
            1,
            "",
            "",
            AuthMode::Auto,
        )
        .await
        .expect("an endpoint that authenticates the server is accepted");
    }
}

/// The refusal is remembered so the round trip is paid once, and retired by the success that
/// proves it no longer holds.
#[test]
fn a_refusal_lasts_until_a_session_start_succeeds() {
    let connector = lore_transport::session::StorageConnector::new(Vec::new());
    let partition = Partition::from([0x7au8; 16]);

    assert!(!connector.is_partition_refused(partition));

    connector.mark_partition_refused(partition);
    assert!(connector.is_partition_refused(partition));
    assert!(!connector.is_partition_authorized(partition));

    connector.mark_partition_authorized(partition);
    assert!(!connector.is_partition_refused(partition));
    assert!(connector.is_partition_authorized(partition));
}

/// A command's session takes turns over every storage connection, so bulk reads through it,
/// such as a cache fill, are not held to the rate of the connection a single pick lands on.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_spreads_requests_over_every_storage_connection() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_address;
    use crate::quic::storage_service::test_server::test_partition;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(4).await);
    let session = connection
        .session(test_partition(), "correlation")
        .await
        .expect("session over the detached storage");

    for index in 0..400 {
        session.get(&test_address(index)).await.expect("fetch");
    }

    assert_eq!(server.gets_per_connection(), vec![100, 100, 100, 100]);
}

/// Calls under one correlation id share the pool the connection pins for a partition, so its
/// sessions start once however many calls take it. A call under another correlation id pins a
/// pool of its own in its place, and the sessions of the one it replaces stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_pool_serves_its_correlation_id_until_another_replaces_it() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_partition;
    use crate::quic::storage_service::test_server::wait_until;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(2).await);
    for _ in 0..3 {
        connection
            .session_pool(test_partition(), "first")
            .await
            .expect("a pool");
    }
    assert_eq!(server.started_sessions_per_connection(), vec![1, 1]);

    connection
        .session_pool(test_partition(), "second")
        .await
        .expect("a pool");

    assert_eq!(server.started_sessions_per_connection(), vec![2, 2]);
    wait_until(
        || server.live_sessions_per_connection() == [1, 1],
        "the replaced pool's server sessions were left open",
    )
    .await;
}

/// A call under another correlation id replaces the pool pinned for its own partition alone: the
/// pool pinned for another partition keeps serving its correlation id, and its sessions stay open.
#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_pool_leaves_another_partitions_pool_pinned() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_partition;
    use crate::quic::storage_service::test_server::wait_until;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(2).await);
    let other = Partition::from([0x7au8; 16]);
    for (partition, correlation_id) in [
        (other, "first"),
        (test_partition(), "first"),
        (test_partition(), "second"),
        (other, "first"),
    ] {
        connection
            .session_pool(partition, correlation_id)
            .await
            .expect("a pool");
    }

    assert_eq!(server.started_sessions_per_connection(), vec![3, 3]);
    wait_until(
        || server.live_sessions_per_connection() == [2, 2],
        "the replaced pool's server sessions were left open",
    )
    .await;
}

/// An unpinned pool's server-side sessions stop once its last holder drops it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unpinned_pool_stops_its_server_sessions_when_dropped() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_partition;
    use crate::quic::storage_service::test_server::wait_until;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(2).await);
    let pool = connection
        .unpinned_session_pool(test_partition(), "correlation")
        .await
        .expect("an unpinned pool");
    assert_eq!(server.live_sessions_per_connection(), vec![1, 1]);

    drop(pool);

    wait_until(
        || server.live_sessions_per_connection() == [0, 0],
        "the unpinned pool's server sessions were left open",
    )
    .await;
}

/// The pool that replaces an invalidated one serves on every connection while the invalidated pool
/// is still held.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalidated_pools_replacement_serves_on_every_connection() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_address;
    use crate::quic::storage_service::test_server::test_partition;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(2).await);
    let stale = connection
        .session(test_partition(), "correlation")
        .await
        .expect("session over the detached storage");
    stale.get(&test_address(0)).await.expect("fetch");

    stale.invalidate().await;
    let fresh = connection
        .session(test_partition(), "correlation")
        .await
        .expect("a fresh session");

    for index in 1..3 {
        fresh
            .get(&test_address(index))
            .await
            .expect("the fresh pool serves");
    }
    let gets = server.gets_per_connection();
    assert!(
        gets.iter().all(|&served| served > 0),
        "a connection served nothing: {gets:?}"
    );
    drop(stale);
}

/// Invalidating a lazy session marks the pool it resolved to stale, so the next resolution starts
/// a new pool even while another holder keeps the old one and its session ids.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalidated_pool_is_not_handed_out_again() {
    use crate::quic::storage_service::test_server::TestStorageServer;
    use crate::quic::storage_service::test_server::test_address;
    use crate::quic::storage_service::test_server::test_partition;

    let server = TestStorageServer::start();
    let connection = Connection::detached(server.connect(2).await);
    let resolving = connection.clone();
    let lazy = StorageSession::pending(move || {
        let connection = resolving.clone();
        async move { connection.session(test_partition(), "correlation").await }
    });
    lazy.get(&test_address(0)).await.expect("fetch");
    let held = connection
        .session_pool(test_partition(), "correlation")
        .await
        .expect("the pool the lazy session resolved to");

    lazy.invalidate().await;
    let after = connection
        .session_pool(test_partition(), "correlation")
        .await
        .expect("a pool after the invalidation");

    assert!(held.is_stale());
    assert!(!Arc::ptr_eq(&held, &after), "a fresh pool");
}

/// A cache get cap above what a semaphore holds is clamped to it, and a cap of zero sets
/// none.
#[test]
fn cache_get_permits_clamp_to_what_a_semaphore_holds() {
    let permits = Connection::detached(Vec::new())
        .cache_get_permits(usize::MAX)
        .expect("a cap");
    assert_eq!(
        permits.available_permits(),
        tokio::sync::Semaphore::MAX_PERMITS
    );
    assert!(
        Connection::detached(Vec::new())
            .cache_get_permits(0)
            .is_none()
    );
}

/// The first call fixes a connection's cache get permits; a later call with another cap
/// answers the same ones.
#[test]
fn cache_get_permits_are_fixed_by_the_first_call() {
    let connection = Connection::detached(Vec::new());
    let first = connection.cache_get_permits(4).expect("a cap");
    let later = connection.cache_get_permits(8).expect("a cap");
    assert!(Arc::ptr_eq(&first, &later));
    assert_eq!(later.available_permits(), 4);
}

#[test]
fn not_supported_to_tonic_status() {
    let err = ProtocolError::from(NotSupported {
        operation: "refresh".into(),
    });
    let status: tonic::Status = err.into();
    assert_eq!(status.code(), tonic::Code::Unimplemented);
}

#[test]
fn tonic_unimplemented_to_not_supported() {
    let status = tonic::Status::new(tonic::Code::Unimplemented, "not implemented");
    let err = ProtocolError::from(status);
    assert!(err.is_not_supported());
}

#[test]
fn not_supported_try_match() {
    let result: Result<(), ProtocolError> = Err(ProtocolError::from(NotSupported {
        operation: "refresh".into(),
    }));
    let matched = result.try_match("testing not supported");
    // try_match returns Result<Result<T, Matched>, Internal>
    // NotSupported is a handleable variant, not Internal, so outer should be Ok
    let inner = matched.expect("should not propagate as Internal");
    assert!(inner.is_err());
    match inner.unwrap_err() {
        MatchedProtocolError::NotSupported(e) => {
            assert_eq!(e.operation, "refresh");
        }
        other => panic!("expected NotSupported, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Protocol-agnostic type tests
// -----------------------------------------------------------------------

#[test]
fn auth_session_fields() {
    let session = AuthSession {
        session_code: "sess-123".into(),
        login_url: "https://auth.example.com/login?code=abc".into(),
        user_code: "ABCD-EFGH".into(),
        interval: Duration::from_secs(5),
        expires_in: Duration::from_secs(600),
    };
    assert_eq!(session.session_code, "sess-123");
    assert_eq!(session.login_url, "https://auth.example.com/login?code=abc");
    assert_eq!(session.user_code, "ABCD-EFGH");
    assert_eq!(session.interval, Duration::from_secs(5));
    assert_eq!(session.expires_in, Duration::from_secs(600));
}

#[test]
fn authentication_token_with_refresh() {
    let token = AuthenticationToken {
        token: "jwt-token".into(),
        user_id: "user-1".into(),
        user_name: "Alice".into(),
        expires_ms: 1700000000000,
        acceptable_root_domains: vec!["example.com".into()],
        refresh_token: Some("refresh-abc".into()),
        scope: Some("openid offline_access".into()),
    };
    assert_eq!(token.token, "jwt-token");
    assert_eq!(token.user_id, "user-1");
    assert_eq!(token.user_name, "Alice");
    assert_eq!(token.expires_ms, 1700000000000);
    assert_eq!(token.acceptable_root_domains, vec!["example.com"]);
    assert_eq!(token.refresh_token.as_deref(), Some("refresh-abc"));
    assert_eq!(token.scope.as_deref(), Some("openid offline_access"));
}

#[test]
fn authentication_token_without_refresh() {
    let token = AuthenticationToken {
        token: "jwt-token".into(),
        user_id: "user-1".into(),
        user_name: "Alice".into(),
        expires_ms: 1700000000000,
        acceptable_root_domains: vec![],
        refresh_token: None,
        scope: None,
    };
    assert!(token.refresh_token.is_none());
}

#[test]
fn authorization_token_fields() {
    let token = AuthorizationToken {
        token: "authz-jwt".into(),
        expires_ms: 1700000060000,
        acceptable_root_domains: vec!["repo.example.com".into(), "cdn.example.com".into()],
    };
    assert_eq!(token.token, "authz-jwt");
    assert_eq!(token.expires_ms, 1700000060000);
    assert_eq!(token.acceptable_root_domains.len(), 2);
}

#[test]
fn resolved_user_fields() {
    let user = ResolvedUser {
        user_id: "uid-42".into(),
        user_name: "Bob".into(),
    };
    assert_eq!(user.user_id, "uid-42");
    assert_eq!(user.user_name, "Bob");
}
