// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The paths an SWFS instance's working tree leaves out, as a later mount loads them.
use lore_base::runtime::LORE_CONTEXT;
use lore_revision::branch;
use lore_revision::fs::swfs::absent_paths::AbsentPaths;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::repository;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::util::path::RelativePath;

use crate::helper::generate_tempdir;
use crate::helper::setup_test_execution;

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("a relative path")
}

/// A mount made straight after an operation recorded paths absent, as when the service remounts
/// the instance, loads them, though recording them only queued their save: the paths a
/// dependency clone left out stay out.
#[tokio::test]
async fn a_mount_made_straight_after_paths_are_recorded_absent_loads_them() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = generate_tempdir();
            let repository = {
                let token = RepositoryWriteToken::acquire(dir.path()).await;
                repository::create_local(
                    dir.path(),
                    &token,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                    BranchId::from(uuid::Uuid::now_v7()),
                    branch::DEFAULT_DEFAULT_NAME.to_string(),
                    repository::RepositoryConfig::default(),
                    false,
                )
                .await
                .expect("creating a repository")
            };
            let absent = AbsentPaths::load(repository.clone()).await;

            absent.update(&[relative("left/out"), relative("other.txt")], &[]);
            let remounted = AbsentPaths::load(repository.clone()).await;

            assert!(remounted.contains(&relative("left/out/below.txt")));
            assert!(remounted.contains(&relative("other.txt")));
            assert!(!remounted.contains(&relative("kept.txt")));

            absent.update(&[], &[relative("other.txt")]);
            let remounted = AbsentPaths::load(repository).await;

            assert!(!remounted.contains(&relative("other.txt")));
            assert!(remounted.contains(&relative("left/out")));
        })
        .await;
}
