// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::time::Duration;

use lore_base::retry::DEFAULT_JITTER_CLAMP_MS;
use lore_storage::STORE_RETRY_ATTEMPTS_DEFAULT;
use lore_storage::STORE_RETRY_ATTEMPTS_SERVER;
use lore_storage::STORE_RETRY_MAX_MS;
use lore_storage::STORE_RETRY_START_MS;
use lore_storage::retry_with_jitter;
use lore_storage::store_retry;

/// The total a store ladder of `attempts` waits, walked on the paused test clock with jitter off.
async fn ladder_total(attempts: usize) -> Duration {
    let mut backoff = retry_with_jitter(STORE_RETRY_START_MS, STORE_RETRY_MAX_MS, attempts, 0.0);
    let start = tokio::time::Instant::now();

    while backoff.wait().await {}

    start.elapsed()
}

/// The server ladder, stretched by the most jitter can add, stays under ten seconds.
#[tokio::test(start_paused = true)]
async fn the_server_ladder_fits_inside_the_handler_budget() {
    let server = ladder_total(STORE_RETRY_ATTEMPTS_SERVER).await;

    assert!(
        server + server / 2 < Duration::from_secs(10),
        "the server ladder must stay inside the handler budget: {server:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_default_ladder_waits_about_a_minute() {
    let client = ladder_total(STORE_RETRY_ATTEMPTS_DEFAULT).await;

    assert!(
        (Duration::from_secs(45)..=Duration::from_secs(75)).contains(&client),
        "the default ladder waits {client:?}, which is not about a minute"
    );
}

/// The default clamp holds a wait at the ceiling within 100ms of it; the store ladder lets it
/// spread across half the ceiling either way.
#[tokio::test(start_paused = true)]
async fn a_wait_at_the_ceiling_spreads_past_the_default_clamp() {
    let mut backoff = store_retry();
    let mut longest = Duration::ZERO;

    loop {
        let before = tokio::time::Instant::now();
        if !backoff.wait().await {
            break;
        }
        longest = longest.max(before.elapsed());
    }

    assert!(
        longest > Duration::from_millis(STORE_RETRY_MAX_MS + DEFAULT_JITTER_CLAMP_MS),
        "no wait went past the default clamp: {longest:?}"
    );
    assert!(
        longest <= Duration::from_millis(STORE_RETRY_MAX_MS + STORE_RETRY_MAX_MS / 2),
        "a wait strayed more than half the ceiling: {longest:?}"
    );
}
