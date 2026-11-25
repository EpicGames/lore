// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use lore_aws::aws::creds::*;
use lore_base::runtime::LORE_CONTEXT;
use time::UtcDateTime;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

use crate::aws::utc;

fn at(hour: u8, minute: u8) -> UtcDateTime {
    at_s(hour, minute, 0)
}

fn at_s(hour: u8, minute: u8, second: u8) -> UtcDateTime {
    utc(2026, 8, 12, hour, minute, second)
}

fn expiring(expiry: UtcDateTime) -> Credentials {
    Credentials::new("AKID", "secret", Some("token".into()), Some(expiry))
}

#[test]
fn debug_never_prints_the_secret_or_the_token() {
    let credentials = Credentials::new(
        "AKID",
        "SECRET-VALUE-a1b2c3",
        Some("TOKEN-VALUE-d4e5f6".into()),
        Some(at(12, 0)),
    );

    let rendered = format!("{credentials:?}");

    assert!(!rendered.contains("SECRET-VALUE"), "{rendered}");
    assert!(!rendered.contains("TOKEN-VALUE"), "{rendered}");
    assert!(rendered.contains("AKID"), "the key id is not a secret");
    assert_eq!(
        rendered.matches("<redacted>").count(),
        2,
        "both the secret and the token are hidden: {rendered}"
    );
}

#[test]
fn static_keys_never_expire_and_never_refresh() {
    let static_keys = Credentials::new("AKID", "secret", None, None);

    assert!(!static_keys.is_expired(at(23, 59)));
    assert!(!static_keys.needs_refresh(at(23, 59)));
}

#[test]
fn refresh_starts_five_minutes_before_expiry() {
    let credentials = expiring(at(12, 0));

    assert!(!credentials.needs_refresh(at(11, 54)));
    assert!(credentials.needs_refresh(at(11, 55)));
    assert!(!credentials.is_expired(at(11, 59)));
    assert!(credentials.is_expired(at(12, 0)));
}

#[test]
fn a_profile_yields_its_static_keys() {
    let file = "\
[default]
aws_access_key_id = AKIDDEFAULT
aws_secret_access_key = secret-default

[other]
aws_access_key_id = AKIDOTHER
aws_secret_access_key = secret-other
aws_session_token = token-other
";

    let default = parse_profile(file, "default").unwrap();
    assert_eq!(&*default.access_key_id, "AKIDDEFAULT");
    assert_eq!(default.session_token, None);

    let other = parse_profile(file, "other").unwrap();
    assert_eq!(&*other.access_key_id, "AKIDOTHER");
    assert_eq!(other.session_token.as_deref(), Some("token-other"));
}

#[test]
fn a_prefixed_section_and_comments_are_read() {
    let file = "\
# a comment
; another
[profile dev]
aws_access_key_id = AKIDDEV
aws_secret_access_key = secret-dev
";

    assert_eq!(
        &*parse_profile(file, "dev").unwrap().access_key_id,
        "AKIDDEV"
    );
}

/// A profile that is right for the AWS CLI but unsupported here is refused as unsupported rather
/// than skipped. Skipping it silently reads as "no credentials found".
#[test]
fn an_unsupported_profile_is_refused_rather_than_skipped() {
    for key in [
        "role_arn = arn:aws:iam::1:role/R",
        "credential_process = /usr/bin/get-creds",
        "sso_session = my-sso",
        "credential_source = Ec2InstanceMetadata",
    ] {
        let file = format!("[default]\n{key}\n");

        let error = parse_profile(&file, "default").expect_err("must not be silently skipped");

        assert!(
            matches!(
                error,
                CredentialsError::Unsupported {
                    provider: "profile",
                    ..
                }
            ),
            "{key} must be refused as unsupported, got: {error:?}"
        );
        assert!(
            !error.is_not_provided(),
            "{key} must stop the chain's silence"
        );
    }
}

#[test]
fn a_missing_profile_declines_rather_than_failing() {
    let file = "[other]\naws_access_key_id = A\naws_secret_access_key = B\n";

    let error = parse_profile(file, "default").expect_err("no such profile");

    assert!(error.is_not_provided());
}

#[test]
fn a_profile_with_an_empty_key_declines() {
    let file = "[default]\naws_access_key_id = \naws_secret_access_key = \n";

    assert!(
        parse_profile(file, "default")
            .unwrap_err()
            .is_not_provided()
    );
}

#[derive(Debug)]
struct Counting {
    calls: AtomicUsize,
    expiry: Option<UtcDateTime>,
}

#[async_trait]
impl ProvideCredentials for Counting {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Credentials::new("AKID", "secret", None, self.expiry))
    }

    fn name(&self) -> &'static str {
        "counting"
    }
}

#[tokio::test]
async fn credentials_are_fetched_once_and_then_reused() {
    let provider = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        expiry: Some(at(12, 0)),
    });
    let cache = CachedCredentials::new(provider.clone());

    for _ in 0..10 {
        cache.get_at(at(10, 0)).await.unwrap();
    }

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn crossing_the_refresh_margin_fetches_again() {
    let provider = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        expiry: Some(at(12, 0)),
    });
    let cache = CachedCredentials::new(provider.clone());

    cache.get_at(at(11, 0)).await.unwrap();
    cache.get_at(at(11, 56)).await.unwrap();

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

/// A provider that holds each call open until the test opens its gate, so callers arriving
/// meanwhile meet a refresh in flight.
#[derive(Debug)]
struct Gated {
    calls: AtomicUsize,
    expiry: UtcDateTime,
    /// Signalled when a call has started.
    entered: Notify,
    /// Holds no permit until the test releases the calls; each call returns the one it takes.
    gate: Semaphore,
}

impl Gated {
    fn expiring(expiry: UtcDateTime) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            expiry,
            entered: Notify::new(),
            gate: Semaphore::new(0),
        }
    }

    /// Let every call through, now and later.
    fn release(&self) {
        self.gate.add_permits(1);
    }
}

#[async_trait]
impl ProvideCredentials for Gated {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let _passage = self.gate.acquire().await.expect("the gate is never closed");

        Ok(Credentials::new(
            "AKIDFRESH",
            "secret",
            None,
            Some(self.expiry),
        ))
    }

    fn name(&self) -> &'static str {
        "gated"
    }
}

/// A hundred requests arriving at expiry must make one call to the provider, not a hundred.
///
/// The callers run on several worker threads and queue on the refresh while the provider holds it
/// open. The provider hands back credentials that are still inside the refresh margin, so every
/// caller that rechecks still wants a refresh; the attempt floor is what holds it to one call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_expiry_makes_one_refresh() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            const CALLERS: usize = 100;

            let provider = Arc::new(Gated::expiring(at(10, 0)));
            let cache = Arc::new(CachedCredentials::new(provider.clone()));
            let started = Arc::new(AtomicUsize::new(0));

            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..CALLERS {
                let cache = cache.clone();
                let started = started.clone();
                lore_base::lore_spawn!(tasks, async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    cache.get_at(at(9, 59)).await.map(|_| ())
                });
            }

            provider.entered.notified().await;
            while started.load(Ordering::SeqCst) < CALLERS {
                tokio::task::yield_now().await;
            }
            provider.release();

            while let Some(result) = tasks.join_next().await {
                result
                    .expect("no caller panicked")
                    .expect("credentials are still valid, so every caller is served");
            }

            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                1,
                "a hundred callers at expiry must make one call to the credential endpoint"
            );
        })
        .await;
}

/// A caller holding credentials that still sign is served them while another caller's refresh is
/// in flight, rather than waiting for the provider.
#[tokio::test]
async fn held_credentials_are_served_while_a_refresh_is_in_flight() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider = Arc::new(Gated::expiring(at(13, 0)));
            let cache = Arc::new(CachedCredentials::new(provider.clone()));
            cache.state.write().credentials = Some(Arc::new(expiring(at(12, 0))));

            let refreshing = {
                let cache = cache.clone();
                lore_base::lore_spawn!(async move { cache.get_at(at(11, 58)).await })
            };
            provider.entered.notified().await;

            let served = tokio::time::timeout(Duration::from_secs(10), cache.get_at(at(11, 58)))
                .await
                .expect("a caller with valid credentials does not wait for the refresh")
                .expect("the held credentials still sign");
            assert_eq!(&*served.access_key_id, "AKID");

            provider.release();
            let refreshed = refreshing
                .await
                .expect("the refreshing caller did not panic")
                .expect("the refresh succeeds");
            assert_eq!(&*refreshed.access_key_id, "AKIDFRESH");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        })
        .await;
}

/// A caller with nothing usable held waits for the refresh in flight and is served its result,
/// rather than starting a second one.
#[tokio::test]
async fn a_caller_with_nothing_usable_waits_for_the_refresh_in_flight() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider = Arc::new(Gated::expiring(at(13, 0)));
            let cache = Arc::new(CachedCredentials::new(provider.clone()));

            let first = {
                let cache = cache.clone();
                lore_base::lore_spawn!(async move { cache.get_at(at(11, 0)).await })
            };
            provider.entered.notified().await;

            let second = {
                let cache = cache.clone();
                lore_base::lore_spawn!(async move { cache.get_at(at(11, 0)).await })
            };
            tokio::task::yield_now().await;
            assert!(
                !second.is_finished(),
                "nothing is held, so the caller waits"
            );

            provider.release();
            for caller in [first, second] {
                let served = caller
                    .await
                    .expect("no caller panicked")
                    .expect("the refresh succeeds");
                assert_eq!(&*served.access_key_id, "AKIDFRESH");
            }
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        })
        .await;
}

/// The floor bounds attempts without preventing recovery: once it passes, a refresh happens.
#[tokio::test]
async fn the_attempt_floor_lets_a_later_refresh_through() {
    let provider = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        expiry: Some(at(10, 0)),
    });
    let cache = CachedCredentials::new(provider.clone());

    cache.get_at(at_s(9, 59, 0)).await.unwrap();
    cache.get_at(at_s(9, 59, 0)).await.unwrap();
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "inside the floor the held credentials are served without a second attempt"
    );

    cache.get_at(at_s(9, 59, 2)).await.unwrap();
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "past the floor the refresh is attempted again"
    );
}

/// With nothing valid to serve and a refresh just attempted, the caller gets an error rather than
/// a request-rate storm against the credential endpoint. The error is transient: once the floor
/// passes, asking again can succeed.
#[tokio::test]
async fn expired_credentials_inside_the_floor_fail_rather_than_hammer() {
    let provider = Arc::new(Counting {
        calls: AtomicUsize::new(0),
        expiry: Some(at(9, 0)),
    });
    let cache = CachedCredentials::new(provider.clone());

    assert!(
        cache.get_at(at_s(10, 0, 0)).await.is_ok(),
        "the first call refreshes and hands back credentials that are already expired"
    );

    let error = cache
        .get_at(at_s(10, 0, 0))
        .await
        .expect_err("the second is inside the floor with nothing usable held");

    assert!(error.is_transient(), "got: {error:?}");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

/// A rejection discards the credentials it names, and the refresh floor with them, so the next
/// caller refreshes at once.
#[tokio::test]
async fn invalidating_the_held_credentials_refreshes_them() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider = Arc::new(Counting {
                calls: AtomicUsize::new(0),
                expiry: Some(at(12, 0)),
            });
            let cache = CachedCredentials::new(provider.clone());

            let rejected = cache.get_at(at(10, 0)).await.expect("the first fetch");
            cache.invalidate(&rejected);
            let replacement = cache.get_at(at(10, 0)).await.expect("the refresh");

            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                2,
                "the floor must not hold off a refresh the service asked for"
            );
            assert!(!Arc::ptr_eq(&rejected, &replacement));
        })
        .await;
}

/// A rejection that arrives after a refresh names credentials already replaced, and must not
/// discard the replacement.
#[tokio::test]
async fn invalidating_superseded_credentials_keeps_the_current_ones() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let provider = Arc::new(Counting {
                calls: AtomicUsize::new(0),
                expiry: Some(at(12, 0)),
            });
            let cache = CachedCredentials::new(provider.clone());

            let rejected = cache.get_at(at(10, 0)).await.expect("the first fetch");
            cache.invalidate(&rejected);
            let replacement = cache.get_at(at(10, 0)).await.expect("the refresh");

            cache.invalidate(&rejected);
            let served = cache.get_at(at(10, 0)).await.expect("the held credentials");

            assert!(
                Arc::ptr_eq(&replacement, &served),
                "a late rejection of the old credentials must leave the new ones in place"
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        })
        .await;
}

#[derive(Debug)]
struct AlwaysFails;

#[async_trait]
impl ProvideCredentials for AlwaysFails {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        Err(CredentialsError::Failed {
            provider: "always-fails",
            detail: "no".to_owned(),
        })
    }

    fn name(&self) -> &'static str {
        "always-fails"
    }
}

#[tokio::test]
async fn a_failed_refresh_serves_credentials_that_still_work() {
    let cache = CachedCredentials::new(Arc::new(AlwaysFails));
    cache.state.write().credentials = Some(Arc::new(expiring(at(12, 0))));

    let served = cache.get_at(at(11, 58)).await.unwrap();
    assert_eq!(
        &*served.access_key_id, "AKID",
        "inside the refresh margin but not yet expired, the old credentials still sign"
    );

    assert!(
        cache.get_at(at(12, 1)).await.is_err(),
        "past expiry there is nothing to serve, so the failure reaches the caller"
    );
}

#[derive(Debug)]
struct Declines;

#[async_trait]
impl ProvideCredentials for Declines {
    async fn provide(&self) -> Result<Credentials, CredentialsError> {
        Err(CredentialsError::NotProvided("declines"))
    }

    fn name(&self) -> &'static str {
        "declines"
    }
}

#[tokio::test]
async fn the_chain_takes_the_first_provider_that_answers() {
    let chain = CredentialsChain::new(vec![
        Box::new(Declines),
        Box::new(StaticProvider::new(Credentials::new(
            "AKIDSECOND",
            "secret",
            None,
            None,
        ))),
    ]);

    let resolved = chain.provide().await.unwrap();

    assert_eq!(&*resolved.access_key_id, "AKIDSECOND");
}

/// A metadata endpoint that fails must not shadow a working provider behind it.
#[tokio::test]
async fn a_failing_provider_does_not_abort_the_chain() {
    let chain = CredentialsChain::new(vec![
        Box::new(AlwaysFails),
        Box::new(StaticProvider::new(Credentials::new(
            "AKIDBEHIND",
            "secret",
            None,
            None,
        ))),
    ]);

    assert_eq!(&*chain.provide().await.unwrap().access_key_id, "AKIDBEHIND");
}

/// When nothing works, the error names the provider that actually broke rather than reporting
/// that none was configured.
#[tokio::test]
async fn the_chain_reports_the_first_real_failure() {
    let chain = CredentialsChain::new(vec![Box::new(AlwaysFails), Box::new(Declines)]);

    let error = chain.provide().await.expect_err("nothing provides");

    assert!(
        matches!(
            error,
            CredentialsError::Failed {
                provider: "always-fails",
                ..
            }
        ),
        "expected the real failure, got: {error:?}"
    );
}

#[tokio::test]
async fn an_empty_chain_says_so() {
    let chain = CredentialsChain::new(vec![]);

    assert!(matches!(
        chain.provide().await,
        Err(CredentialsError::NoProvider)
    ));
}

#[test]
fn expiries_parse_as_rfc3339() {
    assert_eq!(
        parse_expiry("imds", Some("2026-08-12T10:00:00Z")).unwrap(),
        Some(utc(2026, 8, 12, 10, 0, 0))
    );
    assert_eq!(
        parse_expiry("imds", Some("2026-08-12T12:00:00+02:00")).unwrap(),
        Some(utc(2026, 8, 12, 10, 0, 0))
    );
    assert_eq!(parse_expiry("imds", None).unwrap(), None);
}

/// Credentials that never expire are never refreshed, so an expiry that cannot be read must
/// fail the fetch rather than produce them.
#[test]
fn an_unreadable_expiry_is_a_failure_rather_than_no_expiry() {
    for text in ["not a date", "", "2026-08-12 10:00:00", "0"] {
        assert!(
            matches!(
                parse_expiry("imds", Some(text)),
                Err(CredentialsError::Failed { .. })
            ),
            "{text:?} is not an expiry"
        );
    }
}
