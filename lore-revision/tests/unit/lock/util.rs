// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_revision::lock::util::*;

#[test]
fn fold_batch_results_collects_every_batch() {
    let (items, num_success, first_error) =
        fold_batch_results::<u32, String>(vec![Ok(vec![1, 2]), Ok(vec![]), Ok(vec![3])], 3);

    assert_eq!(items, vec![1, 2, 3]);
    assert_eq!(num_success, 3);
    assert!(first_error.is_none());
}

#[test]
fn fold_batch_results_reserves_the_requested_capacity() {
    let (items, _, _) = fold_batch_results::<u32, String>(vec![Ok(vec![1])], 64);

    assert!(
        items.capacity() >= 64,
        "capacity {} was not reserved up front",
        items.capacity()
    );
}

#[test]
fn fold_batch_results_keeps_one_error_and_the_successful_items() {
    let (items, num_success, first_error) = fold_batch_results::<u32, String>(
        vec![
            Ok(vec![1]),
            Err("first".to_string()),
            Err("second".to_string()),
            Ok(vec![2]),
        ],
        2,
    );

    assert_eq!(items, vec![1, 2]);
    assert_eq!(num_success, 2);
    assert_eq!(first_error, Some("first".to_string()));
}

#[test]
fn fold_batch_results_reports_no_error_without_batches() {
    let (items, num_success, first_error) = fold_batch_results::<u32, String>(vec![], 0);

    assert!(items.is_empty());
    assert_eq!(num_success, 0);
    assert!(first_error.is_none());
}

fn environment_with_lock_batch(max_lock_batch: Option<usize>) -> lore_transport::EnvironmentConfig {
    lore_transport::EnvironmentConfig {
        config: Some(lore_transport::types::EnvironmentServerConfig {
            max_lock_batch,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn lock_batch_size_is_the_default_without_an_advertised_limit() {
    assert_eq!(
        lock_batch_size(&lore_transport::EnvironmentConfig::default()),
        LOCK_BATCH_SIZE
    );
    assert_eq!(
        lock_batch_size(&environment_with_lock_batch(None)),
        LOCK_BATCH_SIZE
    );
}

#[test]
fn lock_batch_size_reads_zero_as_no_advertised_limit() {
    assert_eq!(
        lock_batch_size(&environment_with_lock_batch(Some(0))),
        LOCK_BATCH_SIZE
    );
}

#[test]
fn lock_batch_size_is_the_advertised_limit() {
    assert_eq!(lock_batch_size(&environment_with_lock_batch(Some(25))), 25);
    assert_eq!(
        lock_batch_size(&environment_with_lock_batch(Some(LOCK_BATCH_SIZE * 2))),
        LOCK_BATCH_SIZE * 2
    );
}
