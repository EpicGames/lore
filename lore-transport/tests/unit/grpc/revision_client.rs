// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_proto::lore::model::v1 as model;
use lore_proto::lore::revision::v1 as proto;
use lore_transport::grpc::revision_client::collect_branch_list;
use tokio_stream::StreamExt;

fn branch(index: usize) -> Result<proto::BranchListResponse, tonic::Status> {
    Ok(proto::BranchListResponse {
        branch: Some(model::Branch {
            name: index.to_string(),
            ..Default::default()
        }),
    })
}

#[tokio::test]
async fn exact_limit_preserves_all_branches() {
    let result = collect_branch_list(tokio_stream::iter((0..10_000).map(branch)), Some(10_000))
        .await
        .unwrap()
        .expect("complete listing");
    assert_eq!(result.list.len(), 10_000);
    assert_eq!(result.list.first().unwrap().name, "0");
    assert_eq!(result.list.last().unwrap().name, "9999");
}

#[tokio::test]
async fn over_limit_stops_consuming_the_stream() {
    let consumed = AtomicUsize::new(0);
    let stream = tokio_stream::iter((0..20_000).map(branch)).map(|item| {
        consumed.fetch_add(1, Ordering::Relaxed);
        item
    });
    let result = collect_branch_list(stream, Some(10_000)).await.unwrap();
    assert!(result.is_none(), "must report incomplete enumeration");
    assert_eq!(consumed.load(Ordering::Relaxed), 10_001);
}

#[tokio::test]
async fn empty_and_zero_limit_are_distinguished_from_over_limit() {
    let empty = collect_branch_list(tokio_stream::iter(std::iter::empty()), Some(0))
        .await
        .unwrap()
        .unwrap();
    assert!(empty.list.is_empty());
    assert!(
        collect_branch_list(tokio_stream::iter([branch(0)]), Some(0))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stream_failure_does_not_return_a_partial_listing() {
    let result = collect_branch_list(
        tokio_stream::iter([branch(0), Err(tonic::Status::unavailable("stream failed"))]),
        Some(10_000),
    )
    .await;
    assert_eq!(result.err().unwrap().code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn unbounded_listing_still_returns_every_branch() {
    let result = collect_branch_list(tokio_stream::iter((0..10_001).map(branch)), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.list.len(), 10_001);
}

#[tokio::test]
async fn absent_branch_messages_do_not_spend_branch_budget() {
    let result = collect_branch_list(
        tokio_stream::iter([Ok(proto::BranchListResponse { branch: None }), branch(0)]),
        Some(1),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.list.len(), 1);
    assert_eq!(result.list[0].name, "0");
}
