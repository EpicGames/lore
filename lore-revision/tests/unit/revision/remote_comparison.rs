// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! How status compares a diverged branch with its remote latest when the sync point recorded for
//! the branch is behind it, as a commit killed before its sync point reached disk leaves it.

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::repository::status::compare_with_remote;

use super::instance::Instance;
use super::instance::create_branch;
use super::instance::current;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage_and_commit;
use super::instance::switch_to;
use super::instance::write;

const TREE: [&str; 1] = ["a.txt"];

/// Commits `content` to `a.txt`, answering the revision.
async fn commit_content(instance: &Instance, content: &str) -> Hash {
    write(instance, "a.txt", content);
    stage_and_commit(instance, &instance.path, content).await
}

/// [`compare_with_remote`] for a diverged branch at `local`, the remote at `remote` and the sync
/// point recorded at `last_sync`.
async fn compare(instance: &Instance, local: Hash, remote: Hash, last_sync: Hash) -> (bool, bool) {
    compare_with_remote(
        &instance.repository,
        instance.main,
        &*revision_state(instance, local).await,
        &*revision_state(instance, remote).await,
        true,
        last_sync,
    )
    .await
}

/// The remote latest is the first parent of the revision after it, the parent a merge of divergent
/// history holds it as, and on the first-parent history of a revision committed on top of that one.
#[tokio::test]
async fn a_remote_latest_on_the_branch_history_is_not_ahead_of_a_stale_sync_point() {
    run("remote-comparison-merged", &TREE, |instance| async move {
        let sync_point = current(&instance).await;
        let remote = commit_content(&instance, "remote").await;
        let merge = commit_content(&instance, "merge").await;
        let after = commit_content(&instance, "after").await;

        assert_eq!(
            compare(&instance, merge, remote, sync_point).await,
            (true, false)
        );
        assert_eq!(
            compare(&instance, after, remote, sync_point).await,
            (true, false)
        );
    })
    .await;
}

#[tokio::test]
async fn a_remote_latest_off_the_branch_history_is_ahead() {
    run(
        "remote-comparison-elsewhere",
        &TREE,
        |instance| async move {
            let sync_point = current(&instance).await;
            create_branch(&instance, "feature").await;
            let elsewhere = commit_content(&instance, "elsewhere").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_content(&instance, "local").await;
            let local = commit_content(&instance, "more").await;

            assert_eq!(
                compare(&instance, local, elsewhere, sync_point).await,
                (true, true)
            );
        },
    )
    .await;
}
