// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A push that finds the peer holding its revision while the branch's local latest is behind it,
//! as a commit or a sync killed before its latest reached disk, then a push killed after the peer
//! accepted, leave it.

use lore_base::types::Hash;
use lore_revision::branch;
use lore_revision::branch::BranchLatestStatus;
use lore_revision::branch::push::record_pushed_latest;
use lore_revision::find;

use super::instance::Instance;
use super::instance::create_branch;
use super::instance::current;
use super::instance::merge_branch;
use super::instance::revision_state;
use super::instance::run;
use super::instance::stage_and_commit;
use super::instance::switch_to;
use super::instance::write;

const TREE: [&str; 2] = ["a.txt", "b.txt"];

/// The branch's local latest, whether it is marked divergent, and its last synchronized revision,
/// zero when there is none.
async fn records(instance: &Instance) -> (Hash, bool, Hash) {
    let repository = &instance.repository;
    let latest = branch::load_latest(repository.clone(), instance.main)
        .await
        .expect("Failed to load the branch latest");
    let diverged = branch::load_latest_divergent(repository.clone(), instance.main)
        .await
        .expect("Failed to load the branch latest status");
    let last_sync = branch::load_last_sync(repository.clone(), instance.main)
        .await
        .unwrap_or_default();
    (latest, diverged, last_sync)
}

/// Commits a change to `path`, answering the revision committed.
async fn commit_change(instance: &Instance, path: &str, message: &str) -> Hash {
    write(instance, path, message);
    stage_and_commit(instance, &instance.path, message).await
}

/// Writes `latest` as the branch's local latest in place of `previous`, and as its last
/// synchronized revision, as a commit or a sync whose latest never reached disk leaves them.
async fn store_records_as_on_disk(instance: &Instance, previous: Hash, latest: Hash) {
    let repository = &instance.repository;
    branch::store_latest(
        repository.clone(),
        instance.main,
        previous,
        latest,
        BranchLatestStatus::Convergent,
    )
    .await
    .expect("Failed to store the branch latest");
    branch::store_last_sync(repository.clone(), instance.main, latest).await;
}

/// Records `pushed`, answering whether the local latest is `pushed` or on its history.
async fn record(instance: &Instance, pushed: Hash) -> bool {
    record_pushed_latest(&instance.repository, instance.main, pushed, false)
        .await
        .expect("Failed to record the pushed revision")
}

#[tokio::test]
async fn a_local_latest_behind_the_pushed_revision_is_advanced_to_it() {
    run("interrupted-push-behind", &TREE, |instance| async move {
        let (parent, _, _) = records(&instance).await;
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        store_records_as_on_disk(&instance, pushed, parent).await;

        assert!(record(&instance, pushed).await);

        assert_eq!(records(&instance).await, (pushed, false, pushed));
    })
    .await;
}

#[tokio::test]
async fn a_local_latest_several_revisions_behind_is_advanced_to_the_pushed_revision() {
    run(
        "interrupted-push-far-behind",
        &TREE,
        |instance| async move {
            let (behind, _, _) = records(&instance).await;
            commit_change(&instance, "a.txt", "Second").await;
            commit_change(&instance, "b.txt", "Third").await;
            let pushed = commit_change(&instance, "a.txt", "Fourth").await;
            store_records_as_on_disk(&instance, pushed, behind).await;

            assert!(record(&instance, pushed).await);

            assert_eq!(records(&instance).await, (pushed, false, pushed));
        },
    )
    .await;
}

#[tokio::test]
async fn a_local_latest_merged_into_the_pushed_revision_is_advanced_to_it() {
    run("interrupted-push-merged", &TREE, |instance| async move {
        create_branch(&instance, "feature").await;
        let merged = commit_change(&instance, "b.txt", "Feature").await;
        switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
        commit_change(&instance, "a.txt", "Remote").await;
        assert!(merge_branch(&instance, "feature").await);
        let pushed = current(&instance).await;
        assert_eq!(
            revision_state(&instance, pushed).await.parent_other(),
            merged
        );
        store_records_as_on_disk(&instance, pushed, merged).await;

        assert!(record(&instance, pushed).await);

        assert_eq!(records(&instance).await, (pushed, false, pushed));
    })
    .await;
}

#[tokio::test]
async fn a_zero_local_latest_is_advanced_to_the_pushed_revision() {
    run("interrupted-push-zero", &TREE, |instance| async move {
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        store_records_as_on_disk(&instance, pushed, Hash::default()).await;

        assert!(record(&instance, pushed).await);

        assert_eq!(records(&instance).await, (pushed, false, pushed));
    })
    .await;
}

#[tokio::test]
async fn a_pushed_local_latest_still_marked_divergent_is_marked_convergent() {
    run("interrupted-push-divergent", &TREE, |instance| async move {
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        let (latest, diverged, last_sync) = records(&instance).await;
        assert_eq!(latest, pushed);
        assert!(diverged, "A commit marks the local latest divergent");
        assert_ne!(last_sync, pushed);

        assert!(record(&instance, pushed).await);

        assert_eq!(records(&instance).await, (pushed, false, pushed));
    })
    .await;
}

#[tokio::test]
async fn a_recorded_pushed_revision_is_kept() {
    run("interrupted-push-recorded", &TREE, |instance| async move {
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        assert!(record(&instance, pushed).await);

        assert!(record(&instance, pushed).await);

        assert_eq!(records(&instance).await, (pushed, false, pushed));
    })
    .await;
}

#[tokio::test]
async fn a_dry_run_records_nothing() {
    run("interrupted-push-dry-run", &TREE, |instance| async move {
        let (parent, _, _) = records(&instance).await;
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        store_records_as_on_disk(&instance, pushed, parent).await;
        let before = records(&instance).await;

        let covered = record_pushed_latest(&instance.repository, instance.main, pushed, true)
            .await
            .expect("Failed to check the pushed revision");

        assert!(covered);
        assert_eq!(records(&instance).await, before);
    })
    .await;
}

#[tokio::test]
async fn a_local_latest_ahead_of_the_pushed_revision_is_kept() {
    run("interrupted-push-ahead", &TREE, |instance| async move {
        let pushed = commit_change(&instance, "a.txt", "Second").await;
        assert!(record(&instance, pushed).await);
        let ahead = commit_change(&instance, "b.txt", "Third").await;
        let before = records(&instance).await;
        assert_eq!(before.0, ahead);

        assert!(!record(&instance, pushed).await);

        assert_eq!(records(&instance).await, before);
    })
    .await;
}

#[tokio::test]
async fn a_local_latest_on_another_line_numbered_below_the_pushed_revision_is_kept() {
    run(
        "interrupted-push-other-line",
        &TREE,
        |instance| async move {
            create_branch(&instance, "feature").await;
            let elsewhere = commit_change(&instance, "b.txt", "Feature").await;
            switch_to(&instance, branch::DEFAULT_DEFAULT_NAME).await;
            commit_change(&instance, "a.txt", "Second").await;
            commit_change(&instance, "b.txt", "Third").await;
            let pushed = commit_change(&instance, "a.txt", "Fourth").await;
            store_records_as_on_disk(&instance, pushed, elsewhere).await;
            let before = records(&instance).await;

            assert!(!record(&instance, pushed).await);

            assert_eq!(records(&instance).await, before);
        },
    )
    .await;
}

#[tokio::test]
async fn an_unreadable_local_latest_is_kept() {
    run(
        "interrupted-push-unreadable",
        &TREE,
        |instance| async move {
            let pushed = commit_change(&instance, "a.txt", "Second").await;
            let unknown = Hash::from([7u8; 32]);
            store_records_as_on_disk(&instance, pushed, unknown).await;
            let before = records(&instance).await;

            assert!(!record(&instance, pushed).await);

            assert_eq!(records(&instance).await, before);
        },
    )
    .await;
}

#[tokio::test]
async fn a_history_walk_stops_at_its_search_limit() {
    run(
        "interrupted-push-search-limit",
        &TREE,
        |instance| async move {
            let ancestor = current(&instance).await;
            let number = revision_state(&instance, ancestor).await.revision_number();
            commit_change(&instance, "a.txt", "Second").await;
            commit_change(&instance, "b.txt", "Third").await;
            let latest = commit_change(&instance, "a.txt", "Fourth").await;
            let repository = &instance.repository;

            assert!(
                find::is_on_history(repository.clone(), latest, ancestor, number, Some(4)).await
            );
            assert!(
                !find::is_on_history(repository.clone(), latest, ancestor, number, Some(3)).await
            );
        },
    )
    .await;
}

#[tokio::test]
async fn a_local_latest_past_the_search_limit_is_kept() {
    run(
        "interrupted-push-past-limit",
        &TREE,
        |instance| async move {
            let (behind, _, _) = records(&instance).await;
            let mut pushed = behind;
            for index in 0..branch::MAX_DIVERGENT_HISTORY_LENGTH {
                pushed = commit_change(&instance, "a.txt", &format!("Change {index}")).await;
            }
            store_records_as_on_disk(&instance, pushed, behind).await;
            let before = records(&instance).await;

            assert!(!record(&instance, pushed).await);

            assert_eq!(records(&instance).await, before);
        },
    )
    .await;
}
