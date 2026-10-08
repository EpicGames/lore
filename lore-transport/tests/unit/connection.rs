// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_base::error::*;
use lore_base::types::*;
use lore_error_set::ext::ResultExt;
use lore_transport::connection::*;
use lore_transport::error::ProtocolError;
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
    let stored = (remote.to_string(), "alice".to_string(), false);
    let supplied = (remote.to_string(), "alice".to_string(), true);

    assert!(matches_url_and_mode(&stored, remote, false));
    assert!(matches_url_and_mode(&supplied, remote, true));

    assert!(
        !matches_url_and_mode(&supplied, remote, false),
        "a store-mode call must not be given a supplied-credential connection"
    );
    assert!(
        !matches_url_and_mode(&stored, remote, true),
        "a supplied-credential call must not be given a store-mode connection"
    );

    assert!(!matches_url_and_mode(
        &stored,
        "lores://elsewhere.invalid",
        false
    ));
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
