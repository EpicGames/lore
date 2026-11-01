// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A purge holds at most one task more than [`MAX_CONCURRENT_TREE_TASKS`] live, however many
//! untracked entries a directory holds, and a purge whose listing fails awaits the removals it
//! spawned before it reports the failure. Its own binary, because the probe the tests read and arm
//! acts on every purge in the process; the tests take turns, each from a reset probe.
#![allow(clippy::disallowed_methods)] // Test fixtures write to the filesystem outside the repo write-token discipline.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::MAX_CONCURRENT_TREE_TASKS;
    use lore_revision::file::reset;
    use lore_revision::file::reset::PURGE_TASKS;
    use lore_revision::file::reset::ResetError;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;

    include!("helper.rs");

    /// Held by the test using the probe.
    static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Removal tasks spawned before the listing fails.
    const SPAWNED_BEFORE_FAILURE: usize = 2;

    /// Runs `body` on a repository fixture whose one committed file is `kept.txt`, holding
    /// `untracked` untracked files, with the probe reset and to itself.
    async fn with_untracked<F>(
        untracked: usize,
        body: impl FnOnce(TestRepository, Vec<PathBuf>) -> F + Send + 'static,
    ) where
        F: Future<Output = ()> + Send,
    {
        let _turn = TURN.lock().await;
        PURGE_TASKS.reset();
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = test_repository_create(
                    immutable_store,
                    mutable_store,
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                test_file_write(fixture.path.join("kept.txt").as_path(), b"committed");
                test_commit_tree(&fixture, "Initial").await;
                let untracked: Vec<_> = (0..untracked)
                    .map(|index| fixture.path.join(format!("untracked-{index}.tmp")))
                    .collect();
                for path in &untracked {
                    test_file_write(path.as_path(), b"untracked");
                }
                body(fixture, untracked).await;
            }))
            .await
            .expect("Test task failed");
    }

    /// Resets the fixture's root, purging what the revision does not hold.
    async fn reset_purging_root(fixture: &TestRepository) -> Result<(), ResetError> {
        reset::reset(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
            LoreString::default(),
            ResetOptions {
                purge: true,
                ..Default::default()
            },
        )
        .await
    }

    /// A purge of a directory holding more untracked files than the task bound holds no more than
    /// one task beyond it live while every removal is held, and removes every file once released.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_holds_at_most_the_bound_of_removal_tasks_live() {
        with_untracked(MAX_CONCURRENT_TREE_TASKS + 64, async |fixture, untracked| {
            PURGE_TASKS.hold();
            let settled = tokio::spawn(async {
                let deadline = Instant::now() + Duration::from_secs(60);
                let (mut last, mut since) = (0, Instant::now());
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let live = PURGE_TASKS.live();
                    if live != last {
                        (last, since) = (live, Instant::now());
                    } else if live > 0 && since.elapsed() > Duration::from_millis(500) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the purge never settled, {live} tasks live"
                    );
                }
                let peak = PURGE_TASKS.peak();
                PURGE_TASKS.release();
                peak
            });
            reset_purging_root(&fixture)
                .await
                .expect("A reset must succeed");

            let peak = settled.await.expect("The observer completes");
            assert!(
                peak <= MAX_CONCURRENT_TREE_TASKS + 1,
                "the purge held {peak} removal tasks live, the bound being {MAX_CONCURRENT_TREE_TASKS}"
            );
            assert!(
                untracked.iter().all(|path| !path.exists()),
                "the purge left an untracked file"
            );
            assert!(fixture.path.join("kept.txt").exists());
        })
        .await;
    }

    /// A purge whose listing fails once it spawned two removals, both held, reports the failure
    /// only once both have run: the two files are removed, and the files after them stay.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_whose_listing_fails_awaits_the_removals_it_spawned() {
        with_untracked(8, async |fixture, untracked| {
            PURGE_TASKS.fail_entry_after(SPAWNED_BEFORE_FAILURE);
            PURGE_TASKS.hold();
            let settled = tokio::spawn(async {
                let deadline = Instant::now() + Duration::from_secs(60);
                let (mut last, mut since) = (usize::MAX, Instant::now());
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let live = PURGE_TASKS.live();
                    if live != last {
                        (last, since) = (live, Instant::now());
                    } else if PURGE_TASKS.spawned() >= SPAWNED_BEFORE_FAILURE
                        && since.elapsed() > Duration::from_millis(500)
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the purge never settled, {live} tasks live"
                    );
                }
                PURGE_TASKS.release();
            });
            let result = reset_purging_root(&fixture).await;
            settled.await.expect("The observer completes");

            assert!(result.is_err(), "the reset did not report the listing failure");
            assert_eq!(PURGE_TASKS.spawned(), SPAWNED_BEFORE_FAILURE);
            let removed = untracked.iter().filter(|path| !path.exists()).count();
            assert_eq!(
                removed, SPAWNED_BEFORE_FAILURE,
                "the purge removed {removed} files of the {SPAWNED_BEFORE_FAILURE} it spawned removals for"
            );
            assert!(fixture.path.join("kept.txt").exists());
        })
        .await;
    }
}
