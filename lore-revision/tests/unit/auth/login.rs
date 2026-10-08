// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The interactive login's polling loop, on a paused clock: the session sets
//! the cadence, `SlowDown` widens it, denial ends it, and `expires_in` bounds
//! it. Time only passes while the loop sleeps, so every assertion on when a
//! poll happened is exact.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lore_base::error::NotAuthenticated;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::RepositoryId;
use lore_revision::auth::LoreAuthPendingEventData;
use lore_revision::auth::login::InteractiveLoginError;
use lore_revision::auth::login::poll_interactive_session;
use lore_revision::event::LoreEvent;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreEventCallback;
use lore_revision::interface::LoreGlobalArgs;
use lore_revision::relay::EventDispatcher;
use lore_transport::AuthSession;
use lore_transport::AuthSessionPoll;
use lore_transport::Authentication;
use lore_transport::AuthenticationToken;
use lore_transport::AuthorizationToken;
use lore_transport::ProtocolError;
use tokio::time::Instant;

type PollAnswer = Result<AuthSessionPoll, ProtocolError>;

/// Answers each poll from a script, then `Pending` for as long as the loop
/// keeps asking, or never answers again when built with `hanging_after`,
/// and records when each poll arrived on the paused clock.
struct ScriptedAuth {
    answers: Mutex<VecDeque<PollAnswer>>,
    polls: Mutex<Vec<Instant>>,
    hang_when_exhausted: bool,
}

impl ScriptedAuth {
    fn new(answers: impl IntoIterator<Item = PollAnswer>) -> Self {
        ScriptedAuth {
            answers: Mutex::new(answers.into_iter().collect()),
            polls: Mutex::new(Vec::new()),
            hang_when_exhausted: false,
        }
    }

    /// Answers from the script, then leaves every later poll unanswered.
    fn hanging_after(answers: impl IntoIterator<Item = PollAnswer>) -> Self {
        ScriptedAuth {
            hang_when_exhausted: true,
            ..Self::new(answers)
        }
    }

    /// When each poll arrived, in whole seconds after `started`.
    fn poll_seconds(&self, started: Instant) -> Vec<u64> {
        self.polls
            .lock()
            .unwrap()
            .iter()
            .map(|at| at.duration_since(started).as_secs())
            .collect()
    }
}

fn complete() -> PollAnswer {
    Ok(AuthSessionPoll::Complete(AuthenticationToken {
        token: "jwt".into(),
        user_id: "alice".into(),
        user_name: "Alice".into(),
        expires_ms: 0,
        acceptable_root_domains: Vec::new(),
        refresh_token: None,
        scope: None,
    }))
}

fn pending() -> PollAnswer {
    Ok(AuthSessionPoll::Pending)
}

fn slow_down() -> PollAnswer {
    Ok(AuthSessionPoll::SlowDown)
}

fn session(interval_secs: u64, expires_in_secs: u64) -> AuthSession {
    AuthSession {
        session_code: "device-code".into(),
        login_url: "https://idp.test.invalid/device".into(),
        user_code: "ABCD-EFGH".into(),
        interval: Duration::from_secs(interval_secs),
        expires_in: Duration::from_secs(expires_in_secs),
    }
}

#[async_trait]
impl Authentication for ScriptedAuth {
    async fn start_auth_session(
        &self,
        _auth_url: &str,
        _client_state: &str,
        _correlation_id: &str,
    ) -> Result<AuthSession, ProtocolError> {
        Err(ProtocolError::internal("the stub only serves polls"))
    }

    async fn poll_auth_session(
        &self,
        _auth_url: &str,
        _client_state: &str,
        _session_code: &str,
        _correlation_id: &str,
    ) -> PollAnswer {
        self.polls.lock().unwrap().push(Instant::now());
        let answer = self.answers.lock().unwrap().pop_front();
        match answer {
            Some(answer) => answer,
            None if self.hang_when_exhausted => std::future::pending().await,
            None => pending(),
        }
    }

    async fn exchange_external_token(
        &self,
        _auth_url: &str,
        _token: &str,
        _token_type: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        Err(ProtocolError::internal("the stub only serves polls"))
    }

    async fn refresh_authentication(
        &self,
        _auth_url: &str,
        _refresh_token: &str,
        _correlation_id: &str,
    ) -> Result<AuthenticationToken, ProtocolError> {
        Err(ProtocolError::internal("the stub only serves polls"))
    }

    async fn exchange_for_repository(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _repository: RepositoryId,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        Err(ProtocolError::internal("the stub only serves polls"))
    }

    async fn exchange_for_custom_resource(
        &self,
        _auth_url: &str,
        _authn_token: &str,
        _resource_id: &str,
        _correlation_id: &str,
    ) -> Result<AuthorizationToken, ProtocolError> {
        Err(ProtocolError::internal("the stub only serves polls"))
    }
}

/// Runs the loop from a session created at `started`, under an execution
/// context whose callback collects the `AuthPending` events, and hands back
/// the outcome with those events once every one of them has been delivered.
async fn run(
    auth: &ScriptedAuth,
    session: &AuthSession,
    started: Instant,
) -> (
    Result<AuthenticationToken, InteractiveLoginError>,
    Vec<LoreAuthPendingEventData>,
) {
    let pending: Arc<Mutex<Vec<LoreAuthPendingEventData>>> = Arc::default();
    let sink = pending.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
        if let LoreEvent::AuthPending(data) = event {
            sink.lock().unwrap().push(*data);
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
    ));
    let outcome = LORE_CONTEXT
        .scope(execution.clone(), async {
            let outcome = poll_interactive_session(
                auth,
                "https://idp.test.invalid",
                "client-state",
                session,
                started,
                "corr",
            )
            .await;
            execution.dispatcher.drain().await;
            outcome
        })
        .await;
    let events = std::mem::take(&mut *pending.lock().unwrap());
    (outcome, events)
}

/// The UCS Auth session advertises the cadence the client always polled
/// at, so a session nobody approves is polled exactly as before: 30 times,
/// 5 seconds apart, and then given up on.
#[tokio::test(start_paused = true)]
async fn legacy_cadence_is_thirty_polls_five_seconds_apart() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([]);

    let (outcome, events) = run(&auth, &session(5, 150), started).await;

    assert!(
        outcome.as_ref().is_err_and(|e| e.is_internal()),
        "an unapproved session is given up on"
    );
    let polls = auth.poll_seconds(started);
    assert_eq!(polls.len(), 30, "polls at {polls:?}");
    assert_eq!(polls, (0..30).map(|k| k * 5).collect::<Vec<_>>());
    assert_eq!(
        events.len(),
        29,
        "one wait is reported between each pair of polls"
    );
    assert_eq!(
        events[0],
        LoreAuthPendingEventData {
            elapsed_secs: 0,
            interval_secs: 5,
            remaining_secs: 150,
        }
    );
    assert_eq!(
        events[28],
        LoreAuthPendingEventData {
            elapsed_secs: 140,
            interval_secs: 5,
            remaining_secs: 10,
        }
    );
}

/// A provider that sets `interval: 15` is polled every 15 seconds, and each
/// wait is reported so the CLI can show the login is still in progress.
#[tokio::test(start_paused = true)]
async fn a_providers_interval_sets_the_cadence() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([pending(), pending(), complete()]);

    let (outcome, events) = run(&auth, &session(15, 600), started).await;

    assert!(outcome.is_ok());
    assert_eq!(auth.poll_seconds(started), [0, 15, 30]);
    assert_eq!(
        events,
        [
            LoreAuthPendingEventData {
                elapsed_secs: 0,
                interval_secs: 15,
                remaining_secs: 600,
            },
            LoreAuthPendingEventData {
                elapsed_secs: 15,
                interval_secs: 15,
                remaining_secs: 585,
            },
        ]
    );
}

/// `slow_down` adds 5 seconds to the interval, per RFC 8628 §3.5, and the
/// widened interval stays for the rest of the session.
#[tokio::test(start_paused = true)]
async fn slow_down_widens_the_interval_by_five_seconds() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([slow_down(), pending(), slow_down(), complete()]);

    let (outcome, events) = run(&auth, &session(5, 600), started).await;

    assert!(outcome.is_ok());
    assert_eq!(auth.poll_seconds(started), [0, 10, 20, 35]);
    let intervals: Vec<u64> = events.iter().map(|e| e.interval_secs).collect();
    assert_eq!(intervals, [10, 10, 15]);
}

/// A declined login, which the backend answers as an error, ends the loop
/// on the poll that reports it rather than polling on to the session's
/// expiry, and the error reaches the caller as the backend gave it.
#[tokio::test(start_paused = true)]
async fn an_error_from_the_backend_stops_the_loop_at_once() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([pending(), Err(NotAuthenticated.into())]);

    let (outcome, events) = run(&auth, &session(5, 600), started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_not_authenticated()));
    assert_eq!(auth.poll_seconds(started), [0, 5]);
    assert_eq!(events.len(), 1);
}

/// A poll that would land at or after the session's expiry is not made: the
/// loop ends on the client's clock even when the backend keeps answering
/// `Pending`.
#[tokio::test(start_paused = true)]
async fn a_poll_at_or_past_expiry_is_not_made() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([]);

    let (outcome, events) = run(&auth, &session(60, 100), started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert_eq!(auth.poll_seconds(started), [0, 60]);
    assert_eq!(events.len(), 1);
}

/// A login approved on its first poll never waits, and so reports no wait.
#[tokio::test(start_paused = true)]
async fn approval_on_the_first_poll_reports_no_wait() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([complete()]);

    let (outcome, events) = run(&auth, &session(5, 600), started).await;

    assert!(outcome.is_ok());
    assert_eq!(auth.poll_seconds(started), [0]);
    assert!(events.is_empty());
}

/// A backend answering `interval: 0` is polled no faster than once a
/// second, rather than in a tight loop.
#[tokio::test(start_paused = true)]
async fn a_zero_interval_is_not_a_tight_loop() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([pending(), pending(), complete()]);

    let (outcome, _) = run(&auth, &session(0, 600), started).await;

    assert!(outcome.is_ok());
    assert_eq!(auth.poll_seconds(started), [0, 1, 2]);
}

/// The lifetime runs from when the session was requested, not from the
/// first poll: a session whose lifetime ran out while it was being created
/// or the browser was being opened is not polled at all.
#[tokio::test(start_paused = true)]
async fn a_session_whose_lifetime_ran_out_before_the_first_poll_is_not_polled() {
    tokio::time::advance(Duration::from_secs(150)).await;
    let started = Instant::now() - Duration::from_secs(150);
    let auth = ScriptedAuth::new([complete()]);

    let (outcome, events) = run(&auth, &session(5, 150), started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert!(auth.poll_seconds(started).is_empty());
    assert!(events.is_empty());
}

/// Time spent before the first poll counts against the lifetime: only the
/// remainder is polled.
#[tokio::test(start_paused = true)]
async fn time_before_the_first_poll_counts_against_the_lifetime() {
    tokio::time::advance(Duration::from_secs(140)).await;
    let started = Instant::now() - Duration::from_secs(140);
    let auth = ScriptedAuth::new([]);

    let (outcome, events) = run(&auth, &session(5, 150), started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert_eq!(auth.poll_seconds(started), [140, 145]);
    assert_eq!(
        events,
        [LoreAuthPendingEventData {
            elapsed_secs: 140,
            interval_secs: 5,
            remaining_secs: 10,
        }]
    );
}

/// A poll the backend never answers is abandoned at the session's expiry,
/// rather than keeping the login alive for as long as the request hangs.
#[tokio::test(start_paused = true)]
async fn a_poll_in_flight_at_expiry_is_abandoned() {
    let started = Instant::now();
    let auth = ScriptedAuth::hanging_after([pending()]);

    let (outcome, events) = run(&auth, &session(5, 150), started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert_eq!(auth.poll_seconds(started), [0, 5]);
    assert_eq!(events.len(), 1);
    assert_eq!(
        Instant::now().duration_since(started),
        Duration::from_secs(150),
        "the hanging poll is given up on at the session's expiry"
    );
}

/// A provider's interval is unbounded. One already at the maximum does not
/// overflow when `SlowDown` widens it; the session is given up on, since
/// no next poll can land before its expiry.
#[tokio::test(start_paused = true)]
async fn slow_down_on_a_maximal_interval_does_not_overflow() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([slow_down()]);
    let session = AuthSession {
        interval: Duration::MAX,
        ..session(5, 600)
    };

    let (outcome, events) = run(&auth, &session, started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert_eq!(auth.poll_seconds(started), [0]);
    assert!(events.is_empty());
}

/// A provider's lifetime is unbounded. One the clock cannot represent is
/// refused before the first poll, rather than waited on without a bound.
#[tokio::test(start_paused = true)]
async fn an_unrepresentable_lifetime_is_refused_before_polling() {
    let started = Instant::now();
    let auth = ScriptedAuth::new([complete()]);
    let session = AuthSession {
        expires_in: Duration::MAX,
        ..session(5, 600)
    };

    let (outcome, events) = run(&auth, &session, started).await;

    assert!(outcome.as_ref().is_err_and(|e| e.is_internal()));
    assert!(auth.poll_seconds(started).is_empty());
    assert!(events.is_empty());
}
