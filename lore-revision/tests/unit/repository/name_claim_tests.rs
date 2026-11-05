// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::ClaimedCreate;
use lore_revision::repository::NameClaim;
use lore_revision::repository::RepositoryContext;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;

async fn stores() -> (Arc<dyn ImmutableStore>, Arc<dyn MutableStore>) {
    repository::create_client_memory_stores()
        .await
        .expect("in-memory stores should be creatable")
}

fn server_context(
    stores: &(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>),
    id: RepositoryId,
) -> Arc<RepositoryContext> {
    Arc::new(RepositoryContext::new_server_context(
        stores.0.clone(),
        stores.1.clone(),
        id,
    ))
}

fn repository_id(n: u8) -> RepositoryId {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    RepositoryId::from(Context::from(bytes))
}

#[tokio::test]
async fn concurrent_claims_of_one_name_have_one_winner() {
    let stores = stores().await;
    let ids: Vec<RepositoryId> = (1..=8).map(repository_id).collect();
    let claims = ids.iter().map(|id| {
        let context = server_context(&stores, *id);
        async move { repository::claim_name(context, "goalables/app", *id).await }
    });
    let results = futures::future::join_all(claims).await;

    let winners: Vec<RepositoryId> = ids
        .iter()
        .zip(&results)
        .filter(|(_, result)| matches!(result, Ok(NameClaim::Claimed)))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(winners.len(), 1, "exactly one create may claim the name");
    for result in &results {
        match result {
            Ok(NameClaim::Claimed) => {}
            Ok(NameClaim::HeldBy(holder)) => {
                assert_eq!(*holder, winners[0], "losers are told who holds it");
            }
            Ok(NameClaim::AlreadyHeld) => panic!("no claimant held the name before"),
            Err(err) => panic!("claim failed: {err}"),
        }
    }
    let resolved = repository::id_from_name(server_context(&stores, ids[0]), "goalables/app")
        .await
        .expect("the claimed name resolves");
    assert_eq!(resolved, winners[0]);
}

#[tokio::test]
async fn a_repository_can_claim_its_own_name_again() {
    let stores = stores().await;
    let id = repository_id(1);
    let context = server_context(&stores, id);
    assert_eq!(
        repository::claim_name(context.clone(), "a", id)
            .await
            .unwrap(),
        NameClaim::Claimed
    );
    assert_eq!(
        repository::claim_name(context, "a", id).await.unwrap(),
        NameClaim::AlreadyHeld
    );
}

#[tokio::test]
async fn a_released_name_can_be_claimed_by_another_repository() {
    let stores = stores().await;
    let (first, second) = (repository_id(1), repository_id(2));
    assert_eq!(
        repository::claim_name(server_context(&stores, first), "a", first)
            .await
            .unwrap(),
        NameClaim::Claimed
    );
    repository::release_name(server_context(&stores, first), "a", first)
        .await
        .unwrap();
    assert_eq!(
        repository::claim_name(server_context(&stores, second), "a", second)
            .await
            .unwrap(),
        NameClaim::Claimed
    );
}

#[tokio::test]
async fn releasing_does_not_take_a_name_another_repository_holds() {
    let stores = stores().await;
    let (holder, other) = (repository_id(1), repository_id(2));
    repository::claim_name(server_context(&stores, holder), "a", holder)
        .await
        .unwrap();
    repository::release_name(server_context(&stores, other), "a", other)
        .await
        .unwrap();
    assert_eq!(
        repository::claim_name(server_context(&stores, other), "a", other)
            .await
            .unwrap(),
        NameClaim::HeldBy(holder)
    );
}

async fn holder_of(
    stores: &(Arc<dyn ImmutableStore>, Arc<dyn MutableStore>),
    name: &str,
) -> Option<RepositoryId> {
    repository::id_from_name(server_context(stores, repository_id(99)), name)
        .await
        .ok()
        .filter(|id| *id != RepositoryId::default())
}

#[tokio::test]
async fn a_failed_create_releases_the_name_it_claimed() {
    let stores = stores().await;
    let id = repository_id(1);
    let outcome = repository::create_with_claimed_name(server_context(&stores, id), "a", async {
        Err::<(), &str>("failed")
    })
    .await
    .unwrap();
    assert!(matches!(outcome, ClaimedCreate::Ran(Err("failed"))));
    assert_eq!(holder_of(&stores, "a").await, None);
}

#[tokio::test]
async fn a_create_never_runs_under_a_name_another_repository_holds() {
    let stores = stores().await;
    let (holder, other) = (repository_id(1), repository_id(2));
    repository::claim_name(server_context(&stores, holder), "a", holder)
        .await
        .unwrap();
    let outcome =
        repository::create_with_claimed_name(server_context(&stores, other), "a", async {
            panic!("the create ran under a name another repository holds");
            #[allow(unreachable_code)]
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert!(matches!(outcome, ClaimedCreate::HeldBy(id) if id == holder));
}

/// Two overlapping attempts for one ID: the one that inserted the claim fails while the other is
/// still running. The survivor must end with the name mapped to its repository.
#[tokio::test]
async fn an_overlapping_failed_attempt_does_not_leave_the_name_unmapped() {
    use tokio::sync::oneshot;

    let stores = stores().await;
    let id = repository_id(1);

    let (inserter_started, inserter_running) = oneshot::channel::<()>();
    let (fail_inserter, inserter_fails) = oneshot::channel::<()>();
    let inserter = tokio::spawn(repository::create_with_claimed_name(
        server_context(&stores, id),
        "a",
        async move {
            inserter_started.send(()).unwrap();
            inserter_fails.await.unwrap();
            Err::<(), &str>("failed")
        },
    ));
    inserter_running.await.unwrap();

    let (survivor_started, survivor_running) = oneshot::channel::<()>();
    let (finish_survivor, survivor_finishes) = oneshot::channel::<()>();
    let survivor = tokio::spawn(repository::create_with_claimed_name(
        server_context(&stores, id),
        "a",
        async move {
            survivor_started.send(()).unwrap();
            survivor_finishes.await.unwrap();
            Ok::<(), &str>(())
        },
    ));
    survivor_running.await.unwrap();

    fail_inserter.send(()).unwrap();
    assert!(matches!(
        inserter.await.unwrap().unwrap(),
        ClaimedCreate::Ran(Err("failed"))
    ));
    finish_survivor.send(()).unwrap();
    assert!(matches!(
        survivor.await.unwrap().unwrap(),
        ClaimedCreate::Ran(Ok(()))
    ));
    assert_eq!(holder_of(&stores, "a").await, Some(id));
}

/// The same overlap finishing the other way round: the survivor completes first, then the attempt
/// that inserted the claim fails. The name must stay mapped to the created repository.
#[tokio::test]
async fn a_failure_after_an_overlapping_success_keeps_the_name_mapped() {
    use tokio::sync::oneshot;

    let stores = stores().await;
    let id = repository_id(1);

    let (inserter_started, inserter_running) = oneshot::channel::<()>();
    let (fail_inserter, inserter_fails) = oneshot::channel::<()>();
    let inserter = tokio::spawn(repository::create_with_claimed_name(
        server_context(&stores, id),
        "a",
        async move {
            inserter_started.send(()).unwrap();
            inserter_fails.await.unwrap();
            Err::<(), &str>("failed")
        },
    ));
    inserter_running.await.unwrap();

    let survivor_context = server_context(&stores, id);
    let stored = survivor_context.clone();
    let survivor = repository::create_with_claimed_name(survivor_context, "a", async move {
        repository::metadata_store_hash(stored, Hash::from_context(Context::from([7u8; 16])))
            .await
            .map_err(|err| err.to_string())
    })
    .await
    .unwrap();
    assert!(matches!(survivor, ClaimedCreate::Ran(Ok(()))));

    fail_inserter.send(()).unwrap();
    assert!(matches!(
        inserter.await.unwrap().unwrap(),
        ClaimedCreate::Ran(Err("failed"))
    ));
    assert_eq!(holder_of(&stores, "a").await, Some(id));
}
