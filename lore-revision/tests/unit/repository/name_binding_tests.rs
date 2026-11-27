// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! The conditional writes of a repository's name: the binds and unbinds of the name-to-ID
//! mapping, and the swap of the metadata pointer to a copy with a new name.
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_revision::lore::RepositoryId;
use lore_revision::metadata::Metadata;
use lore_revision::repository;
use lore_revision::repository::MetadataSwap;
use lore_revision::repository::NameBind;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryMetadata;
use lore_revision::state::no_link_traversal;

const NAME: &str = "the-repository";
/// A key outside [`RepositoryMetadata`], as `RepositoryMetadataSet` can set.
const EXTRA_KEY: &str = "the-extra-key";
const EXTRA_VALUE: &str = "the extra value";

fn new_id() -> RepositoryId {
    Context::from(uuid::Uuid::now_v7()).into()
}

/// Runs `test` on a server context for a new repository ID over in-memory stores.
async fn with_repository<F, Fut>(test: F)
where
    F: FnOnce(Arc<RepositoryContext>) -> Fut,
    Fut: Future<Output = ()>,
{
    let (immutable, mutable, execution) = crate::helper::test_store_create()
        .await
        .expect("creating the stores");
    let repository = Arc::new(RepositoryContext::new_server_context(
        immutable,
        mutable,
        new_id(),
        no_link_traversal(),
    ));
    Box::pin(LORE_CONTEXT.scope(execution, test(repository))).await;
}

/// The ID `name` maps to, or `None` for an unbound name. Any other failure panics.
async fn bound_id(repository: &Arc<RepositoryContext>, name: &str) -> Option<RepositoryId> {
    match repository::id_from_name(repository.clone(), name).await {
        Ok(id) => Some(id),
        Err(err) if err.is_address_not_found() => None,
        Err(err) => panic!("reading the id of {name}: {err}"),
    }
}

/// Stores the metadata of a repository named [`NAME`] with every key of [`RepositoryMetadata`]
/// and [`EXTRA_KEY`] set, and points the repository at it.
async fn seed_metadata(repository: &Arc<RepositoryContext>) -> (RepositoryMetadata, Hash) {
    let metadata = RepositoryMetadata {
        name: NAME.to_string(),
        description: "the description".to_string(),
        default_branch: Context::from(uuid::Uuid::now_v7()),
        default_branch_name: "main".to_string(),
        creator: "the-creator".to_string(),
        created: 1_700_000_000,
    };
    let hash = repository::metadata_store(repository.clone(), metadata.clone())
        .await
        .expect("storing the metadata");
    let mut blob = Metadata::deserialize(repository.clone(), hash)
        .await
        .expect("reading the metadata");
    blob.set_string(EXTRA_KEY, EXTRA_VALUE)
        .expect("setting the extra key");
    let hash = blob
        .serialize(repository.clone())
        .await
        .expect("storing the metadata with the extra key");
    repository::metadata_store_hash(repository.clone(), hash)
        .await
        .expect("storing the metadata pointer");
    (metadata, hash)
}

#[tokio::test]
async fn binding_an_unbound_name_maps_it_to_the_id() {
    with_repository(|repository| async move {
        let id = repository.id;
        let bind = repository::bind_name_to_id(repository.clone(), NAME, id).await;
        assert_eq!(bind.expect("binding the name"), NameBind::Bound);
        assert_eq!(bound_id(&repository, NAME).await, Some(id));
    })
    .await;
}

#[tokio::test]
async fn binding_a_name_to_the_id_it_holds_succeeds() {
    with_repository(|repository| async move {
        let id = repository.id;
        repository::store_name_to_id(repository.clone(), NAME, id)
            .await
            .expect("binding the name");
        let bind = repository::bind_name_to_id(repository.clone(), NAME, id).await;
        assert_eq!(
            bind.expect("binding the name again"),
            NameBind::AlreadyBound
        );
        assert_eq!(bound_id(&repository, NAME).await, Some(id));
    })
    .await;
}

#[tokio::test]
async fn binding_a_name_another_id_holds_is_refused() {
    with_repository(|repository| async move {
        let other = new_id();
        repository::store_name_to_id(repository.clone(), NAME, other)
            .await
            .expect("binding the name to the other id");
        let bind = repository::bind_name_to_id(repository.clone(), NAME, repository.id).await;
        assert_eq!(
            bind.expect("binding the name"),
            NameBind::HeldByOther(other)
        );
        assert_eq!(bound_id(&repository, NAME).await, Some(other));
    })
    .await;
}

#[tokio::test]
async fn binding_a_name_to_the_zero_id_is_refused() {
    with_repository(|repository| async move {
        let bind =
            repository::bind_name_to_id(repository.clone(), NAME, RepositoryId::default()).await;
        assert!(bind.expect_err("a zero id").is_invalid_arguments());
        assert_eq!(bound_id(&repository, NAME).await, None);
    })
    .await;
}

#[tokio::test]
async fn unbinding_a_name_the_id_holds_clears_it() {
    with_repository(|repository| async move {
        let id = repository.id;
        repository::store_name_to_id(repository.clone(), NAME, id)
            .await
            .expect("binding the name");
        repository::unbind_name_to_id(repository.clone(), NAME, id)
            .await
            .expect("unbinding the name");
        assert_eq!(bound_id(&repository, NAME).await, None);
    })
    .await;
}

#[tokio::test]
async fn unbinding_a_name_another_id_holds_leaves_it() {
    with_repository(|repository| async move {
        let other = new_id();
        repository::store_name_to_id(repository.clone(), NAME, other)
            .await
            .expect("binding the name to the other id");
        repository::unbind_name_to_id(repository.clone(), NAME, repository.id)
            .await
            .expect("unbinding the name");
        assert_eq!(bound_id(&repository, NAME).await, Some(other));
    })
    .await;
}

#[tokio::test]
async fn unbinding_an_unbound_name_leaves_it_unbound() {
    with_repository(|repository| async move {
        repository::unbind_name_to_id(repository.clone(), NAME, repository.id)
            .await
            .expect("unbinding the name");
        assert_eq!(bound_id(&repository, NAME).await, None);
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_keeps_the_other_keys() {
    with_repository(|repository| async move {
        let (metadata, expected) = seed_metadata(&repository).await;
        let swap = repository::metadata_set_name(repository.clone(), expected, "the-new-name")
            .await
            .expect("setting the name");
        let MetadataSwap::Updated(updated) = swap else {
            panic!("the swap from the current hash is lost: {swap:?}");
        };
        let current = repository::metadata_hash(repository.clone())
            .await
            .expect("reading the metadata pointer");
        assert_eq!(current, updated);
        let renamed = repository::metadata(repository.clone(), updated)
            .await
            .expect("reading the renamed metadata");
        assert_eq!(
            renamed,
            RepositoryMetadata {
                name: "the-new-name".to_string(),
                ..metadata
            }
        );
        let blob = Metadata::deserialize(repository.clone(), updated)
            .await
            .expect("reading the renamed blob");
        assert_eq!(blob.get_string(EXTRA_KEY).ok(), Some(EXTRA_VALUE));
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_the_repository_has_keeps_its_metadata() {
    with_repository(|repository| async move {
        let (_, expected) = seed_metadata(&repository).await;
        let swap = repository::metadata_set_name(repository.clone(), expected, NAME)
            .await
            .expect("setting the name");
        assert_eq!(swap, MetadataSwap::Updated(expected));
        let pointer = repository::metadata_hash(repository.clone())
            .await
            .expect("reading the metadata pointer");
        assert_eq!(pointer, expected);
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_of_a_deleted_repository_is_lost() {
    with_repository(|repository| async move {
        let (_, expected) = seed_metadata(&repository).await;
        repository::metadata_store_hash(repository.clone(), Hash::default())
            .await
            .expect("clearing the metadata pointer");
        let swap = repository::metadata_set_name(repository.clone(), expected, "the-new-name")
            .await
            .expect("setting the name");
        assert_eq!(
            swap,
            MetadataSwap::Lost {
                current: Hash::default()
            }
        );
        let pointer = repository::metadata_hash(repository.clone()).await;
        assert!(
            pointer
                .expect_err("no metadata pointer")
                .is_address_not_found()
        );
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_from_a_stale_hash_is_lost() {
    with_repository(|repository| async move {
        let (_, stale) = seed_metadata(&repository).await;
        let MetadataSwap::Updated(current) =
            repository::metadata_set_name(repository.clone(), stale, "the-first-name")
                .await
                .expect("setting the first name")
        else {
            panic!("the first swap is lost");
        };
        let swap = repository::metadata_set_name(repository.clone(), stale, "the-second-name")
            .await
            .expect("setting the second name");
        assert_eq!(swap, MetadataSwap::Lost { current });
        let pointer = repository::metadata_hash(repository.clone())
            .await
            .expect("reading the metadata pointer");
        assert_eq!(pointer, current);
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_again_from_the_same_hash_is_updated() {
    with_repository(|repository| async move {
        let (_, expected) = seed_metadata(&repository).await;
        let first = repository::metadata_set_name(repository.clone(), expected, "the-new-name")
            .await
            .expect("setting the name");
        let second = repository::metadata_set_name(repository.clone(), expected, "the-new-name")
            .await
            .expect("setting the name again");
        assert!(matches!(first, MetadataSwap::Updated(_)), "{first:?}");
        assert_eq!(second, first);
    })
    .await;
}

#[tokio::test]
async fn setting_the_name_from_the_zero_hash_is_refused() {
    with_repository(|repository| async move {
        let swap = repository::metadata_set_name(repository.clone(), Hash::default(), NAME).await;
        assert!(swap.expect_err("a zero hash").is_repository_not_found());
        let pointer = repository::metadata_hash(repository.clone()).await;
        assert!(
            pointer
                .expect_err("no metadata pointer")
                .is_address_not_found()
        );
    })
    .await;
}

/// The binds, unbinds and name updates on a context without a write token fail with
/// `WriteRequired` and leave the name and the metadata as they were.
#[tokio::test]
async fn the_name_writes_need_a_write_token() {
    with_repository(|repository| async move {
        let other = new_id();
        repository::store_name_to_id(repository.clone(), NAME, other)
            .await
            .expect("binding the name to the other id");
        let (_, expected) = seed_metadata(&repository).await;
        let read_only = Arc::new(repository.without_command_locks());

        let bind =
            repository::bind_name_to_id(read_only.clone(), "the-new-name", read_only.id).await;
        let unbind = repository::unbind_name_to_id(read_only.clone(), NAME, other).await;
        let swap = repository::metadata_set_name(read_only.clone(), expected, "the-new-name").await;

        assert!(bind.is_err_and(|err| err.is_write_required()));
        assert!(unbind.is_err_and(|err| err.is_write_required()));
        assert!(swap.is_err_and(|err| err.is_write_required()));
        assert_eq!(bound_id(&repository, "the-new-name").await, None);
        assert_eq!(bound_id(&repository, NAME).await, Some(other));
        let pointer = repository::metadata_hash(repository.clone())
            .await
            .expect("reading the metadata pointer");
        assert_eq!(pointer, expected);
    })
    .await;
}
