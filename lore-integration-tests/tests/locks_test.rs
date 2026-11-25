// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(all(test, feature = "integration_tests"))]
mod locks_tests {
    use std::error::Error;

    use lore_aws::store::lock_store::DynamoDbLockStore;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::types::Context;
    use lore_base::types::Hash;
    use lore_base::types::LockResource;
    use lore_revision::lock::LockError;
    use lore_revision::lock::LockQuery;
    use lore_revision::lock::LockStore;
    use lore_revision::lore::RepositoryId;
    use rand::Rng;
    use rand::distr::SampleString;

    use crate::common::aws_common::LOCKS_TABLE_NAME;
    use crate::common::aws_common::setup;
    use crate::setup_execution;

    async fn initialize_lock_store() -> Result<DynamoDbLockStore, Box<dyn Error>> {
        let (_, dynamodb, _) = setup(vec![LOCKS_TABLE_NAME]).await?;

        Ok(DynamoDbLockStore::new(dynamodb, LOCKS_TABLE_NAME))
    }

    fn random_repo() -> RepositoryId {
        rand::random::<RepositoryId>()
    }

    fn random_resource(
        branch: Option<Context>,
        hash: Option<Hash>,
        description: Option<impl Into<String>>,
    ) -> LockResource {
        let description = match description {
            Some(desc) => desc.into(),
            None => random_string(),
        };

        LockResource {
            branch: branch.unwrap_or(rand::random::<Context>()),
            hash: hash.unwrap_or(rand::random::<Hash>()),
            description,
        }
    }

    fn random_string() -> String {
        rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 16)
    }

    /// An unlock batch naming one resource twice is refused and releases nothing: a transaction
    /// naming one row twice cannot be applied. `DynamoDB` rejects the request outright, which the
    /// store reports as internal; `DynamoDB` Local cancels it on the repeat's condition, which the
    /// store reports as not found.
    #[tokio::test]
    async fn an_unlock_naming_one_row_twice_is_refused() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();
                let repository = random_repo();

                let resource = random_resource(None, None, Some("src/foo.rs"));
                let resources = vec![
                    resource.clone(),
                    random_resource(None, None, Some("src/bar.rs")),
                    resource.clone(),
                ];

                lock_store
                    .lock_resources(&owner, repository, &resources[..2])
                    .await?;

                let refused = lock_store
                    .unlock_resources(
                        owner.as_str(),
                        /* validate_user */ true,
                        repository,
                        &resources,
                    )
                    .await
                    .expect_err("a batch naming one row twice is not one the service will apply");

                assert!(
                    matches!(refused, LockError::Internal(_) | LockError::LockNotFound(_)),
                    "{refused:?}"
                );

                for resource in &resources[..2] {
                    let locks = lock_store
                        .query_locks(LockQuery::HashRepositoryBranch(
                            resource.hash,
                            repository,
                            resource.branch,
                        ))
                        .await?;

                    assert_eq!(locks.len(), 1, "a refused unlock releases nothing");
                }

                Ok(())
            })
            .await
    }

    /// A batch of exactly the advertised size, through every verb that takes one.
    ///
    /// This is the number the client batches to and the number `TransactWriteItems` stops at, so it
    /// is the one size that has to work end to end. Nothing else here exercises more than twenty.
    #[tokio::test]
    async fn a_full_batch_of_the_advertised_size_round_trips() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let batch = lock_store
                    .max_lock_batch()
                    .expect("the lock store names the size it applies atomically");

                let owner = random_string();
                let repository = random_repo();
                let branch = rand::random::<Context>();
                let resources = (0..batch)
                    .map(|index| {
                        random_resource(Some(branch), None, Some(format!("src/file{index}.rs")))
                    })
                    .collect::<Vec<_>>();

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;
                assert_eq!(locks.len(), batch);

                let status = lock_store
                    .check_locks_status(repository, &resources)
                    .await?;
                assert_eq!(status.len(), batch);

                let queried = lock_store
                    .query_locks(LockQuery::RepositoryBranch(repository, branch))
                    .await?;
                assert_eq!(queried.len(), batch, "a query pages until it has them all");

                let unlocked = lock_store
                    .unlock_resources(owner.as_str(), true, repository, &resources)
                    .await?;
                assert_eq!(unlocked.len(), batch);

                assert!(
                    lock_store
                        .check_locks_status(repository, &resources)
                        .await?
                        .is_empty(),
                    "every lock in the batch is released"
                );

                Ok(())
            })
            .await
    }

    /// One lock held by someone else fails the whole batch, and locks none of the rest.
    ///
    /// This is what "applied atomically" buys, and it is why a request larger than the store takes
    /// is refused rather than split: a split batch could leave a caller holding half of what it
    /// asked for while being told it holds none.
    #[tokio::test]
    async fn a_batch_holding_a_foreign_lock_locks_nothing() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();
                let other = random_string();
                let repository = random_repo();

                let taken = random_resource(None, None, Some("src/taken.rs"));
                let free = random_resource(None, None, Some("src/free.rs"));

                lock_store
                    .lock_resources(&other, repository, std::slice::from_ref(&taken))
                    .await?;

                let refused = lock_store
                    .lock_resources(&owner, repository, &[free.clone(), taken.clone()])
                    .await
                    .expect_err("a resource another owner holds fails the batch");

                assert!(matches!(refused, LockError::LockNotOwned(_)), "{refused:?}");

                let locks = lock_store
                    .query_locks(LockQuery::HashRepositoryBranch(
                        free.hash,
                        repository,
                        free.branch,
                    ))
                    .await?;

                assert!(
                    locks.is_empty(),
                    "the rest of a failed batch must not be left locked"
                );

                Ok(())
            })
            .await
    }

    /// Releasing one lock of someone else's fails the batch and releases none of the caller's own.
    ///
    /// The two refusals a release can earn have to stay apart: a lock held by another owner is
    /// `LockNotOwned`, and only a lock that is not there at all is `LockNotFound`. Reporting the
    /// first as the second would tell a caller its lock had gone when someone else was holding it.
    #[tokio::test]
    async fn releasing_a_batch_holding_a_foreign_lock_releases_nothing()
    -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();
                let other = random_string();
                let repository = random_repo();

                let mine = random_resource(None, None, Some("src/mine.rs"));
                let theirs = random_resource(None, None, Some("src/theirs.rs"));

                lock_store
                    .lock_resources(&owner, repository, std::slice::from_ref(&mine))
                    .await?;
                lock_store
                    .lock_resources(&other, repository, std::slice::from_ref(&theirs))
                    .await?;

                let refused = lock_store
                    .unlock_resources(
                        owner.as_str(),
                        /* validate_user */ true,
                        repository,
                        &[mine.clone(), theirs.clone()],
                    )
                    .await
                    .expect_err("a lock another owner holds fails the release");

                assert!(
                    matches!(refused, LockError::LockNotOwned(_)),
                    "a lock someone else holds is not a lock that is missing: {refused:?}"
                );

                let still_held = lock_store
                    .check_locks_status(repository, std::slice::from_ref(&mine))
                    .await?;

                assert_eq!(
                    still_held.len(),
                    1,
                    "a refused release must not release the rest of the batch"
                );
                assert_eq!(still_held[0].owner, owner);

                Ok(())
            })
            .await
    }

    /// Releasing a lock that is not held.
    #[tokio::test]
    async fn unlocking_what_is_not_locked_reports_not_found() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let refused = lock_store
                    .unlock_resources(
                        random_string().as_str(),
                        /* validate_user */ true,
                        random_repo(),
                        &[random_resource(None, None, Some("src/never-locked.rs"))],
                    )
                    .await
                    .expect_err("there is no lock to release");

                assert!(matches!(refused, LockError::LockNotFound(_)), "{refused:?}");

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_lock_unlock_query_not_owner() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();
                let user = random_string();
                let repository = random_repo();

                let resources = vec![
                    random_resource(None, None, Some("src/foo.rs")),
                    random_resource(None, None, Some("src/bar.rs")),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                // Attempt to unlock resources given the user is not the owner of the lock and
                // it DOES NOT have "admin" or "owner" permissions
                let refused = lock_store
                    .unlock_resources(user.as_str(), true, repository, &resources)
                    .await
                    .expect_err("another owner's locks are not this user's to release");

                assert!(
                    matches!(refused, LockError::LockNotOwned(_)),
                    "a lock someone else holds reports as not owned, not as missing: {refused:?}"
                );

                for resource in &resources {
                    let locks = lock_store
                        .query_locks(LockQuery::HashRepositoryBranch(
                            resource.hash,
                            repository,
                            resource.branch,
                        ))
                        .await?;

                    // All locks still exist
                    assert_eq!(locks.len(), 1);
                }

                // Unlock resources given the user is not the owner of the lock and
                // it DOES have "admin" or "owner" permissions
                lock_store
                    .unlock_resources(user.as_str(), false, repository, &resources)
                    .await?;

                for resource in &resources {
                    let locks = lock_store
                        .query_locks(LockQuery::HashRepositoryBranch(
                            resource.hash,
                            repository,
                            resource.branch,
                        ))
                        .await?;

                    // No result should have been found
                    assert_eq!(locks.len(), 0);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_lock_exists() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();
                let user = random_string();
                let repository = random_repo();
                let resources = vec![random_resource(None, None, Some("src/test.rs"))];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 1);
                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources[0]);
                assert_eq!(&lock.owner, &owner);

                // This should succeed for the owner
                let result = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await;
                let locks = result.unwrap();
                assert!(locks.is_empty());

                // This should fail for not the owner
                let result = lock_store
                    .lock_resources(&user, repository, &resources)
                    .await;
                assert!(matches!(result, Err(LockError::LockNotOwned(_))));

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_hash() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let description = random_string();
                let hash = rand::random::<Hash>();

                // Create resource in 1st repo
                let owner_one = random_string();
                let repo_one = random_repo();
                let resources_one =
                    vec![random_resource(None, Some(hash), Some(description.clone()))];

                let locks = lock_store
                    .lock_resources(&owner_one, repo_one, &resources_one)
                    .await?;

                assert_eq!(locks.len(), 1);
                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources_one[0]);
                assert_eq!(&lock.owner, &owner_one);

                // Create resource in 2nd repo
                let owner_two = random_string();
                let repo_two = random_repo();
                let resources_two =
                    vec![random_resource(None, Some(hash), Some(description.clone()))];

                let locks = lock_store
                    .lock_resources(&owner_two, repo_two, &resources_two)
                    .await?;

                assert_eq!(locks.len(), 1);
                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources_two[0]);
                assert_eq!(&lock.owner, &owner_two);

                // Query by the shared hash
                let locks = lock_store.query_locks(LockQuery::Hash(hash)).await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    let resource = &lock.resource;
                    assert!(resource == &resources_one[0] || resource == &resources_two[0]);
                    assert!(lock.owner == owner_one || lock.owner == owner_two);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_hash_repo() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let description = random_string();
                let hash = rand::random::<Hash>();
                let owner = random_string();
                let repository = random_repo();

                let resources = vec![
                    random_resource(None, Some(hash), Some(description.clone())),
                    random_resource(None, Some(hash), Some(description.clone())),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                let locks = lock_store
                    .query_locks(LockQuery::HashRepository(hash, repository))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_owner() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();

                // Create resource in 1st repo
                let repo_one = random_repo();
                let resources_one = vec![random_resource(None, None, Some("src/foo.rs"))];

                let locks = lock_store
                    .lock_resources(&owner, repo_one, &resources_one)
                    .await?;

                assert_eq!(locks.len(), 1);
                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources_one[0]);
                assert_eq!(&lock.owner, &owner);

                // Create resource in 2nd repo
                let repo_two = random_repo();
                let resources_two = vec![random_resource(None, None, Some("src/bar.rs"))];

                let locks = lock_store
                    .lock_resources(&owner, repo_two, &resources_two)
                    .await?;

                assert_eq!(locks.len(), 1);
                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources_two[0]);
                assert_eq!(&lock.owner, &owner);

                // Query by the shared hash
                let locks = lock_store
                    .query_locks(LockQuery::Owner(owner.clone()))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks {
                    let resource = &lock.resource;
                    assert!(resource == &resources_one[0] || resource == &resources_two[0]);
                    assert!(lock.owner == owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_owner_repo() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();
                let repository = random_repo();

                let resources = vec![
                    random_resource(None, None, None::<String>),
                    random_resource(None, None, None::<String>),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                let locks = lock_store
                    .query_locks(LockQuery::OwnerRepository(owner.clone(), repository))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_owner_repo_branch() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();
                let repository = random_repo();
                let branch = rand::random::<Context>();

                let resources = vec![
                    random_resource(Some(branch), None, None::<String>),
                    random_resource(Some(branch), None, None::<String>),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                let locks = lock_store
                    .query_locks(LockQuery::OwnerRepositoryBranch(
                        owner.clone(),
                        repository,
                        branch,
                    ))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_repo() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();
                let repository = random_repo();

                let resources = vec![
                    random_resource(None, None, None::<String>),
                    random_resource(None, None, None::<String>),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                let locks = lock_store
                    .query_locks(LockQuery::Repository(repository))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_repo_branch() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();
                let repository = random_repo();
                let branch = rand::random::<Context>();

                let resources = vec![
                    random_resource(Some(branch), None, None::<String>),
                    random_resource(Some(branch), None, None::<String>),
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                let locks = lock_store
                    .query_locks(LockQuery::RepositoryBranch(repository, branch))
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_query_repo_branch_desc() -> Result<(), Box<dyn Error>> {
        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                // Shared resource attrs
                let owner = random_string();
                let repository = random_repo();
                let resources = vec![random_resource(None, None, None::<String>)];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 1);

                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources[0]);
                assert_eq!(&lock.owner, &owner);

                let locks = lock_store
                    .query_locks(LockQuery::RepositoryBranchDescription(
                        repository,
                        resources[0].branch,
                        resources[0].description.clone(),
                    ))
                    .await?;

                assert_eq!(locks.len(), 1);

                let lock = locks.first().unwrap();
                assert_eq!(&lock.resource, &resources[0]);
                assert_eq!(&lock.owner, &owner);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_check_lock_status() -> Result<(), Box<dyn Error>> {
        let repository = random_repo();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();

                let resources = (1..=20)
                    .map(|_| random_resource(None, None, None::<String>))
                    .collect::<Vec<_>>();

                let acquired = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                let result = lock_store
                    .check_locks_status(repository, resources.as_slice())
                    .await?;

                for lock in acquired {
                    if !result.contains(&lock) {
                        panic!("Expected to find {lock:?} in the returned statuses, but it was not present.");
                    }
                }

                Ok(())
            })
            .await
    }

    /// A status batch naming one key twice is refused as an internal failure: `BatchGetItem`
    /// refuses a request naming one key twice.
    #[tokio::test]
    async fn a_duplicate_status_key_is_refused() -> Result<(), Box<dyn Error>> {
        let repository = random_repo();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();

                let mut resources = (1..=20)
                    .map(|_| random_resource(None, None, None::<String>))
                    .collect::<Vec<_>>();

                lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                resources.push(resources[0].clone());

                let refused = lock_store
                    .check_locks_status(repository, resources.as_slice())
                    .await
                    .expect_err("a batch naming one key twice is not one the service will serve");

                assert!(matches!(refused, LockError::Internal(_)), "{refused:?}");

                let served = lock_store
                    .check_locks_status(repository, &resources[..20])
                    .await?;

                assert_eq!(served.len(), 20);

                Ok(())
            })
            .await
    }

    #[tokio::test]
    async fn test_check_lock_status_not_all_locked() -> Result<(), Box<dyn Error>> {
        let repository = random_repo();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();

                let mut rng = rand::rng();

                let mut resources = vec![];
                let mut locked_resources = vec![];
                for _ in 1..=20 {
                    let resource = random_resource(None, None, None::<String>);
                    resources.push(resource.clone());

                    let found: bool = rng.random();
                    if found {
                        locked_resources.push(resource);
                    }
                }

                let acquired = lock_store
                    .lock_resources(&owner, repository, locked_resources.as_slice())
                    .await?;

                let result = lock_store
                    .check_locks_status(repository, resources.as_slice())
                    .await?;

                let mut remove_positions = vec![];
                for (pos, lock) in acquired.iter().enumerate() {
                    if !result.contains(lock) {
                        panic!("Expected to find {lock:?} in the returned statuses, but it was not present.");
                    }

                    remove_positions.push(pos);
                }

                let remaining = acquired.iter().enumerate().filter_map(|(p, l)| if !remove_positions.contains(&p) { Some(l.clone()) } else { None }).collect::<Vec<_>>();

                assert!(remaining.is_empty(), "There should not be any additional locks remaining");

                Ok(())
            })
            .await
    }
}
