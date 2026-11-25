// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! How long an operation gets, and what happens between its attempts.
//!
//! One budget covers the whole operation, retries included. The wait between resends of a request
//! that got no answer is [`lore_base::retry::Retry`]. A throttle is not retried here, so the
//! storage layer's ladder is the only one it escalates through.
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lore_base::retry::Retry;
use lore_base::retry::retry;
use time::OffsetDateTime;
use time::SignedDuration;
use time::UtcDateTime;
use time::format_description::well_known::Rfc2822;
// The runtime's clock rather than the system's, so a test can assert the schedule against paused
// time instead of spending it.
use tokio::time::Instant;

/// First wait before a retry, doubling per attempt.
#[lore_macro::test_pub]
const BACKOFF_START_MILLIS: u64 = 25;
/// Ceiling on a single wait, so a long chain does not stretch past the budget in one step.
#[lore_macro::test_pub]
const BACKOFF_MAX_MILLIS: u64 = 500;
/// How many waits an operation gets. The budget usually binds first; this bounds the case where a
/// connection fails instantly, over and over, well inside the deadline.
#[lore_macro::test_pub]
const BACKOFF_LIMIT: usize = 8;
/// How far jitter may move a single wait.
///
/// Above half of any interval this ladder reaches, so every wait keeps the full deviation either
/// side of its backoff. The shared 100ms default would flatten the 400ms and 500ms steps, which are
/// where requests that lost their connections together bunch up.
#[lore_macro::test_pub]
const BACKOFF_JITTER_CLAMP_MILLIS: u64 = 1_000;

/// A deadline far enough out to stand for "none", for a budget too large to add to the clock.
const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// The largest measured skew treated as this host being on time.
///
/// Inside AWS's own five-minute signature window, by a margin, so a correction is only ever
/// recorded for a clock far enough out that leaving it alone would start costing signatures.
const MAX_ACCEPTABLE_SKEW: Duration = Duration::from_secs(4 * 60);

/// The largest share of the whole budget one attempt may take.
///
/// Without a ceiling an attempt gets whatever is left, so a first attempt that stalls spends the
/// entire allowance and the operation reports a timeout having never retried — and there is no
/// connect timeout underneath to bound it, by design. At 60% a stalled first attempt still leaves
/// 40% for a second, which is what lets a hung connection be retried at all.
const MAX_ATTEMPT_SHARE: f64 = 0.6;

/// What is left of an operation's allowance.
pub struct RetryBudget {
    deadline: Instant,
    /// The most any single attempt may take, so one of them cannot consume the budget.
    attempt_ceiling: Duration,
    backoff: Retry,
    /// How many attempts have run, for the caller's telemetry.
    attempts: u32,
}

/// When a budget of `total`, measured from now, runs out.
pub fn deadline_after(total: Duration) -> Instant {
    let now = Instant::now();

    now.checked_add(total).unwrap_or_else(|| now + FAR_FUTURE)
}

impl RetryBudget {
    /// A budget of `total`, measured from now.
    pub fn new(total: Duration) -> Self {
        Self {
            deadline: deadline_after(total),
            attempt_ceiling: total.mul_f64(MAX_ATTEMPT_SHARE),
            backoff: retry(BACKOFF_START_MILLIS, BACKOFF_MAX_MILLIS, BACKOFF_LIMIT)
                .with_jitter_clamp(BACKOFF_JITTER_CLAMP_MILLIS),
            attempts: 0,
        }
    }

    /// What is left of the budget. `None` when it is spent.
    pub fn remaining(&self) -> Option<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());

        (!remaining.is_zero()).then_some(remaining)
    }

    /// The time this attempt may take: what is left, capped so a retry stays possible. `None` when
    /// the budget is spent.
    pub fn attempt_timeout(&mut self) -> Option<Duration> {
        let remaining = self.remaining()?;

        self.attempts += 1;

        Some(remaining.min(self.attempt_ceiling))
    }

    /// Wait before the next attempt, answering whether there is any point.
    ///
    /// `false` when the attempt limit is spent, or when the wait would outlast the budget —
    /// retrying into a deadline that has already passed just turns one failure into two.
    ///
    /// The wait is bounded by what remains rather than merely started while some remains: the
    /// backoff ceiling is half a second, so an unbounded wait would let an operation overrun its
    /// deadline by that much and hand the caller a timeout it had already paid for.
    pub async fn wait(&mut self) -> bool {
        let Some(remaining) = self.remaining() else {
            return false;
        };

        tokio::time::timeout(remaining, self.backoff.wait())
            .await
            .unwrap_or(false)
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// When the budget runs out.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether the budget rather than the attempt limit is what ran out, so the caller can report a
    /// timeout instead of the last failure.
    pub fn is_spent(&self) -> bool {
        self.remaining().is_none()
    }
}

/// The offset between this host's clock and the service's.
///
/// `SigV4` rejects a signature timestamped more than five minutes from the service's clock, so a
/// host with a drifting clock fails *every* request rather than the first. The service tells us its
/// time in the `Date` header of the rejection, and signing against that recovers.
///
/// Shared by every operation on a client, because the drift is the host's rather than the
/// request's.
#[derive(Debug, Default)]
pub struct ClockSkew(AtomicI64);

impl ClockSkew {
    pub fn new() -> Self {
        Self::default()
    }

    /// Now, as the service sees it.
    pub fn now(&self) -> UtcDateTime {
        self.adjust(UtcDateTime::now())
    }

    /// [`Self::now`], with the offset in seconds it applied.
    ///
    /// One read of the offset serves both, so a signature's timestamp and the offset recorded for
    /// it always agree.
    pub fn now_with_offset(&self) -> (UtcDateTime, i64) {
        let offset = self.0.load(Ordering::Relaxed);

        (shifted(UtcDateTime::now(), offset), offset)
    }

    #[lore_macro::test_pub]
    fn adjust(&self, local: UtcDateTime) -> UtcDateTime {
        shifted(local, self.0.load(Ordering::Relaxed))
    }

    /// Learn the offset from a rejected response's `Date` header.
    ///
    /// `local` is this host's raw clock rather than the adjusted one: the measurement is of the
    /// host against the service, independent of any correction in force. A skew inside the limit
    /// AWS accepts records no correction and clears any that is in force, so a host whose clock has
    /// been put right stops applying an offset learned while it was wrong. A header that is not a
    /// date changes nothing.
    pub fn learn(&self, date_header: &str, local: UtcDateTime) {
        let Some(server_time) = parse_http_date(date_header) else {
            return;
        };

        let offset = server_time - local;
        let on_time = offset.unsigned_abs() < MAX_ACCEPTABLE_SKEW;

        self.0.store(
            if on_time { 0 } else { offset.whole_seconds() },
            Ordering::Relaxed,
        );
    }

    /// The recorded offset in seconds.
    pub fn offset_seconds(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// `local` moved by `offset` seconds.
fn shifted(local: UtcDateTime, offset: i64) -> UtcDateTime {
    if offset == 0 {
        return local;
    }

    local + SignedDuration::seconds(offset)
}

/// Parse an HTTP `Date` header (RFC 7231 preferred form, as AWS sends it).
#[lore_macro::test_pub]
fn parse_http_date(value: &str) -> Option<UtcDateTime> {
    OffsetDateTime::parse(value.trim(), &Rfc2822)
        .ok()
        .map(OffsetDateTime::to_utc)
}
