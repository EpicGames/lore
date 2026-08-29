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

    #[tokio::test]
    async fn test_lock_unlock_duplicates() -> Result<(), Box<dyn Error>> {
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
                    resource,
                ];

                let locks = lock_store
                    .lock_resources(&owner, repository, &resources)
                    .await?;

                assert_eq!(locks.len(), 2);

                for lock in locks.iter() {
                    assert!(resources.contains(&lock.resource));
                    assert_eq!(&lock.owner, &owner);
                }

                lock_store
                    .unlock_resources(
                        owner.as_str(),
                        /* validate_user */ true,
                        repository,
                        &resources,
                    )
                    .await?;

                for resource in resources {
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
                let _ = lock_store
                    .unlock_resources(user.as_str(), true, repository, &resources)
                    .await;

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

    #[tokio::test]
    async fn test_check_lock_status_duplicates() -> Result<(), Box<dyn Error>> {
        let repository = random_repo();

        let execution = setup_execution("test".to_string());
        LORE_CONTEXT
            .scope(execution.clone(), async move {
                let lock_store = initialize_lock_store().await?;

                let owner = random_string();

                let mut resources = (1..=20)
                    .map(|_| random_resource(None, None, None::<String>))
                    .collect::<Vec<_>>();

                // Add a duplicate resource
                resources.push(resources[0].clone());

                let acquired = lock_store
                    .lock_resources(&owner, repository, &resources[..20])
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
