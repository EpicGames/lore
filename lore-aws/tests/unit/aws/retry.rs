// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_aws::aws::retry::*;
use lore_base::retry::DEFAULT_JITTER_CLAMP_MS;
use lore_base::retry::retry;
use lore_base::runtime::LORE_CONTEXT;
use time::SignedDuration;
use time::UtcDateTime;
use tokio::time::Instant;

use crate::aws::utc;

fn at(hour: u8, minute: u8) -> UtcDateTime {
    utc(2026, 8, 12, hour, minute, 0)
}

#[tokio::test(start_paused = true)]
async fn each_attempt_gets_what_is_left_of_the_budget() {
    let mut budget = RetryBudget::new(Duration::from_millis(1_000));

    let first = budget.attempt_timeout().expect("the budget starts unspent");
    assert!(first <= Duration::from_millis(1_000));

    tokio::time::sleep(Duration::from_millis(600)).await;

    let second = budget.attempt_timeout().expect("400ms remain");
    assert!(second <= Duration::from_millis(400), "{second:?}");
    assert_eq!(budget.attempts(), 2);
}

/// A first attempt that stalls leaves room for a second.
///
/// Nothing underneath bounds a connection that never answers, so an attempt handed the whole
/// allowance could spend all of it and the operation would report a timeout having never retried.
#[tokio::test(start_paused = true)]
async fn one_attempt_cannot_spend_the_whole_budget() {
    let mut budget = RetryBudget::new(Duration::from_millis(1_000));

    let first = budget.attempt_timeout().expect("the budget starts unspent");
    assert_eq!(first, Duration::from_millis(600));

    // The first attempt stalls for everything it was given.
    tokio::time::sleep(first).await;

    let second = budget
        .attempt_timeout()
        .expect("a stalled first attempt must still leave a retry");
    assert_eq!(second, Duration::from_millis(400));
}

#[tokio::test(start_paused = true)]
async fn a_spent_budget_offers_no_attempt_and_no_wait() {
    let mut budget = RetryBudget::new(Duration::from_millis(100));

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(budget.remaining(), None);
    assert_eq!(budget.attempt_timeout(), None);
    assert!(
        !budget.wait().await,
        "waiting past the deadline is pointless"
    );
    assert!(budget.is_spent());
}

/// What remains is the deadline's distance, uncapped by the per-attempt share, so a wait for
/// credentials may use all of it.
#[tokio::test(start_paused = true)]
async fn the_remaining_budget_is_the_whole_distance_to_the_deadline() {
    LORE_CONTEXT
        .scope(crate::store::setup_execution("test".to_string()), async {
            let budget = RetryBudget::new(Duration::from_millis(1_000));

            assert_eq!(budget.remaining(), Some(Duration::from_millis(1_000)));

            tokio::time::sleep(Duration::from_millis(250)).await;

            assert_eq!(budget.remaining(), Some(Duration::from_millis(750)));
        })
        .await;
}

/// A budget too large to add to the clock stands for no deadline rather than overflowing.
#[test]
fn a_budget_past_the_clock_range_is_unbounded_rather_than_a_panic() {
    for total in [Duration::from_millis(u64::MAX), Duration::MAX] {
        let mut budget = RetryBudget::new(total);

        assert!(budget.attempt_timeout().is_some(), "{total:?}");
        assert!(!budget.is_spent(), "{total:?}");
    }
}

/// The budget is the caller's whole allowance, so no wait may run past it. Waiting a full
/// backoff with a few milliseconds left would return a timeout the caller had already paid for.
///
/// Which wait is the one that gets cut short depends on how the draws land, so the chain is run
/// to its end and the deadline is asserted throughout rather than against a single wait.
#[tokio::test(start_paused = true)]
async fn a_wait_never_outlasts_the_budget() {
    const BUDGET: Duration = Duration::from_millis(20);

    let mut budget = RetryBudget::new(BUDGET);
    let started = Instant::now();

    while budget.wait().await {
        assert!(
            started.elapsed() <= BUDGET,
            "a wait overran the budget: {:?}",
            started.elapsed()
        );
    }

    assert!(
        started.elapsed() <= BUDGET,
        "the chain overran the budget by {:?}",
        started.elapsed() - BUDGET
    );
}

/// The count in a log line has to be attempts made, not attempts considered.
#[tokio::test(start_paused = true)]
async fn the_attempt_count_is_attempts_actually_made() {
    let mut budget = RetryBudget::new(Duration::from_millis(100));

    assert!(budget.attempt_timeout().is_some());
    assert_eq!(budget.attempts(), 1);

    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(budget.attempt_timeout(), None);
    assert_eq!(
        budget.attempts(),
        1,
        "an attempt that never ran must not be counted"
    );
}

#[tokio::test(start_paused = true)]
async fn waits_stay_inside_a_growing_but_capped_envelope() {
    let mut budget = RetryBudget::new(Duration::from_secs(60));
    let mut waits = Vec::new();

    for _ in 0..BACKOFF_LIMIT {
        let before = tokio::time::Instant::now();
        assert!(budget.wait().await);
        waits.push(before.elapsed());
    }

    assert!(
        !budget.wait().await,
        "the attempt limit stops the chain even inside the budget"
    );

    // Waits are displaced either side of the backoff, so only the band they fall in is ordered.
    let mut backoff = BACKOFF_START_MILLIS;
    for wait in &waits {
        let deviation = backoff / 2;
        assert!(
            *wait >= Duration::from_millis(backoff - deviation)
                && *wait <= Duration::from_millis(backoff + deviation),
            "a wait must land within {deviation}ms of its {backoff}ms backoff: {wait:?}"
        );
        backoff = (backoff * 2).min(BACKOFF_MAX_MILLIS);
    }

    assert_eq!(backoff, BACKOFF_MAX_MILLIS, "the chain reaches its ceiling");
}

/// Waits spread either side of the backoff, so requests that failed together are not resent
/// together.
#[tokio::test(start_paused = true)]
async fn waits_spread_either_side_of_the_backoff() {
    const DRAWS: usize = 64;
    const BACKOFF_MILLIS: u64 = 400;

    // Start and maximum equal, so every draw displaces the same interval.
    let mut backoff =
        retry(BACKOFF_MILLIS, BACKOFF_MILLIS, DRAWS).with_jitter_clamp(BACKOFF_JITTER_CLAMP_MILLIS);
    let mut waits = Vec::with_capacity(DRAWS);

    for _ in 0..DRAWS {
        let before = tokio::time::Instant::now();
        assert!(backoff.wait().await);
        waits.push(before.elapsed());
    }

    let shortest = waits.iter().min().expect("draws were taken");
    let longest = waits.iter().max().expect("draws were taken");

    // With 64 draws, landing entirely on one side has probability 2^-64.
    assert!(
        *shortest < Duration::from_millis(BACKOFF_MILLIS),
        "some waits must fall short of the backoff: shortest was {shortest:?}"
    );
    assert!(
        *longest > Duration::from_millis(BACKOFF_MILLIS),
        "some waits must overshoot the backoff: longest was {longest:?}"
    );
    assert!(
        *shortest >= Duration::from_millis(BACKOFF_MILLIS / 2)
            && *longest <= Duration::from_millis(BACKOFF_MILLIS + BACKOFF_MILLIS / 2),
        "no wait may stray more than half the backoff: {shortest:?}..{longest:?}"
    );
    assert!(
        *shortest < Duration::from_millis(BACKOFF_MILLIS - DEFAULT_JITTER_CLAMP_MS)
            || *longest > Duration::from_millis(BACKOFF_MILLIS + DEFAULT_JITTER_CLAMP_MS),
        "the raised clamp must be in force: {shortest:?}..{longest:?}"
    );
}

#[test]
fn no_skew_leaves_the_clock_alone() {
    let skew = ClockSkew::new();

    assert_eq!(skew.adjust(at(12, 0)), at(12, 0));
    assert_eq!(skew.offset_seconds(), 0);
}

/// A host running well behind must adopt the service's time, or every signature is rejected.
#[test]
fn a_large_skew_is_learned_and_applied() {
    let skew = ClockSkew::new();
    let local = at(12, 0);
    // The service says it is 12:30 — the host is half an hour slow.
    let server = "Wed, 12 Aug 2026 12:30:00 GMT";

    skew.learn(server, local);

    assert_eq!(skew.offset_seconds(), 30 * 60);
    assert_eq!(skew.adjust(local), at(12, 30));
}

#[test]
fn a_skew_ahead_of_the_service_is_learned_as_negative() {
    let skew = ClockSkew::new();
    let local = at(13, 0);

    skew.learn("Wed, 12 Aug 2026 12:00:00 GMT", local);

    assert_eq!(skew.offset_seconds(), -3600);
    assert_eq!(skew.adjust(local), at(12, 0));
}

/// A signature rejection with the clocks in agreement is about something else — a wrong secret,
/// say. Adopting a one-second offset would hide that.
#[test]
fn a_small_skew_is_not_learned() {
    let skew = ClockSkew::new();

    skew.learn("Wed, 12 Aug 2026 12:00:30 GMT", at(12, 0));

    assert_eq!(skew.offset_seconds(), 0);
}

/// A correction unwinds once the host's clock is put right.
///
/// Once an offset is learned it is applied to every signature. If NTP then corrects the host, the
/// offset is the only thing still wrong — signatures go out at `correct + offset` and AWS rejects
/// all of them. The rejection is measured against the *raw* clock, which now agrees with the
/// service, so the offset is cleared rather than left in force.
#[test]
fn a_correction_unwinds_once_the_host_clock_is_fixed() {
    let skew = ClockSkew::new();

    // The host is half an hour slow, and the client adopts the service's time.
    skew.learn("Wed, 12 Aug 2026 12:30:00 GMT", at(12, 0));
    assert_eq!(skew.offset_seconds(), 30 * 60);

    // NTP fixes the clock. Signatures now go out 30 minutes fast and AWS rejects them, reporting
    // a time that agrees with the raw clock.
    skew.learn("Wed, 12 Aug 2026 12:30:00 GMT", at(12, 30));

    assert_eq!(
        skew.offset_seconds(),
        0,
        "the stale correction must be cleared, not kept"
    );
    assert_eq!(skew.adjust(at(12, 30)), at(12, 30));
}

/// A header that is not a date neither records a correction nor clears one in force.
#[test]
fn an_unreadable_date_header_teaches_nothing() {
    let skew = ClockSkew::new();
    skew.learn("Wed, 12 Aug 2026 12:30:00 GMT", at(12, 0));

    for header in ["", "not a date", "2026-08-12T12:00:00Z"] {
        skew.learn(header, at(12, 0));

        assert_eq!(skew.offset_seconds(), 30 * 60, "{header:?}");
    }
}

/// The time a request is signed at and the offset recorded for it come from one reading, so the
/// offset names the correction that time carries.
#[test]
fn now_with_offset_reports_the_offset_it_applied() {
    let skew = ClockSkew::new();
    skew.learn("Wed, 12 Aug 2026 12:30:00 GMT", at(12, 0));

    let before = UtcDateTime::now();
    let (now, offset) = skew.now_with_offset();
    let after = UtcDateTime::now();

    assert_eq!(offset, 30 * 60);
    assert!(
        now >= before + SignedDuration::minutes(30) && now <= after + SignedDuration::minutes(30),
        "{now} is not this host's clock moved by the offset"
    );
}

#[test]
fn http_dates_parse_in_the_form_aws_sends() {
    assert_eq!(
        parse_http_date("Wed, 12 Aug 2026 12:30:00 GMT"),
        Some(at(12, 30))
    );
    assert_eq!(
        parse_http_date("  Wed, 12 Aug 2026 12:30:00 GMT  "),
        Some(at(12, 30))
    );
}
