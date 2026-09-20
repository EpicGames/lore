// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for the walk between two revisions, and what a view change makes it enter.
//!
//! The other benchmarks here measure the filter against a synthesized list of paths. This one
//! holds two revision states and a pair of views, which is what a walk is actually made of, and
//! reports what the walk counted about itself beside the clock: the directories it stood in and
//! the verdicts it asked a filter for.
//!
//! Four walks over one tree, so what separates them is the views and nothing else:
//!
//! - **one view**, the content prune alone, which is every walk in the product today;
//! - **two views diverging in one subtree**, where the walk has to enter that subtree and no
//!   other;
//! - **two views dropping one subtree**, where the walk enters no more than the first and the
//!   delete fans out over what it dropped, naming it path by path;
//! - **two views and an unanchored exclusion**, a rule that can match at any depth and so covers
//!   nothing, leaving the walk to enter the tree whole.
//!
//! The third is the one a narrowing is made of, and the one place the hierarchy walk shows: it
//! reports the directories the walk entered as the first does, and every path it emits is the
//! hierarchy's.
//!
//! The counts are deterministic in the tree and the views, so they are asserted against each
//! other here; the clock is reported. What the counts have to be for a given tree is asserted in
//! `diff_views.rs` instead, where the tree is small enough to name every path.
//!
//! Run with:
//!     `cargo test -p lore-revision --release --test diff_walk_bench -- --ignored --nocapture`

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::filter::FilterMode;
    use lore_revision::lore::RepositoryId;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::state;
    use lore_revision::util::path::RelativePath;

    include!("helper.rs");

    /// The directory the tree sits under, which every walk here is rooted at.
    const TREE: &str = "tree";
    /// Subdirectories under each top-level directory.
    const SUBDIRECTORIES: u32 = 8;
    /// Files in each subdirectory.
    const FILES: u32 = 8;
    /// The top-level directory whose content differs between the two revisions, so every walk
    /// enters it and the commit that produced the second revision has something to record.
    const CHANGED: u32 = 0;
    /// The top-level directory the two views diverge in, which the walk must enter and which no
    /// content change gives it a reason to.
    const DIVERGED: u32 = 1;
    /// The top-level directory the to view drops whole, which leaves the working tree path by path
    /// without the walk entering it.
    const DROPPED: u32 = 2;
    /// An exclusion that can match at any depth, so no subtree is ever covered.
    const UNANCHORED: &str = "*.tmp";
    /// How many times each walk is timed.
    const ROUNDS: u32 = 3;

    /// Workload multiplier, from `LORE_BENCH_SCALE`.
    ///
    /// The default is small enough to run on demand without waiting for the fixture. Scale it up
    /// to read the clock against something the size of a real repository; the counts, which are
    /// what the assertions here rest on, hold at any scale.
    fn scale() -> u32 {
        std::env::var("LORE_BENCH_SCALE")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1)
    }

    /// Top-level directories under [`TREE`], which is what the workload scales in: the prune
    /// question is put once per top-level directory, and each answer stands for a whole subtree.
    fn top_level() -> u32 {
        24 * scale()
    }

    fn top(index: u32) -> String {
        format!("{TREE}/top{index:04}")
    }

    fn subdirectory(index: u32, sub: u32) -> String {
        format!("{}/sub{sub:02}", top(index))
    }

    /// What one configuration cost and what it did.
    struct Measured {
        elapsed: Duration,
        changes: usize,
        stats: state::DiffWalkStats,
    }

    impl Measured {
        fn entered(&self) -> u64 {
            self.stats.directories_entered.load(Ordering::Relaxed)
        }

        fn report(&self, label: &str) {
            println!(
                "  {label:<34} {:>8.2} ms/walk {:>8} directories {:>9} verdicts {:>4} changes",
                self.elapsed.as_secs_f64() * 1_000.0 / f64::from(ROUNDS),
                self.entered(),
                self.stats.filter_queries.load(Ordering::Relaxed),
                self.changes,
            );
        }
    }

    /// One walk of [`TREE`] between the two revisions: how many changes it emitted, and what it
    /// reported of itself.
    async fn walk_once(
        from: Arc<RepositoryContext>,
        to: Arc<RepositoryContext>,
        older: Arc<state::State>,
        newer: Arc<state::State>,
    ) -> (usize, state::DiffWalkStats) {
        let path = RelativePath::new_from_initial_path(TREE).expect("Valid path");
        let mut walk = state::ChangeStream::spawn(async move |changes| {
            state::diff(
                from,
                older,
                to,
                newer,
                Some(path),
                None,
                &changes,
                FilterMode::Full,
            )
            .await
        });
        let mut changes = 0;
        while walk.next().await.is_some() {
            changes += 1;
        }
        (changes, walk.finish().await.expect("Failed to diff"))
    }

    /// [`walk_once`] [`ROUNDS`] times, answering the total time and what the last walk reported.
    ///
    /// `sides` builds the contexts afresh each round, so a filter's ancestor memo is as cold as it
    /// is on a real operation, and a configuration walking under one view says so by answering the
    /// same handle twice. One walk ahead of the rounds is unmeasured, so whichever configuration
    /// is timed first does not also pay for reading the state blocks the rest find cached.
    async fn timed(
        sides: impl Fn() -> (Arc<RepositoryContext>, Arc<RepositoryContext>),
        older: &Arc<state::State>,
        newer: &Arc<state::State>,
    ) -> Measured {
        let (from, to) = sides();
        let _warm = walk_once(from, to, older.clone(), newer.clone()).await;
        let mut total = Duration::ZERO;
        let mut last = None;
        for _ in 0..ROUNDS {
            let (from, to) = sides();
            let start = Instant::now();
            last = Some(walk_once(from, to, older.clone(), newer.clone()).await);
            total += start.elapsed();
        }
        let (changes, stats) = last.expect("at least one round");
        Measured {
            elapsed: total,
            changes,
            stats,
        }
    }

    #[tokio::test]
    #[ignore = "benchmark: run on demand with --ignored"]
    async fn diff_walk_across_views() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let instance = test_repository_create(
                    immutable_store.clone(),
                    mutable_store.clone(),
                    RepositoryId::from(uuid::Uuid::now_v7()),
                )
                .await;
                for index in 0..top_level() {
                    for sub in 0..SUBDIRECTORIES {
                        let directory = instance.path.join(subdirectory(index, sub));
                        std::fs::create_dir_all(&directory).expect("Create directory failed");
                        for file in 0..FILES {
                            test_file_write(
                                directory.join(format!("file{file:02}.bin")).as_path(),
                                format!("{index}/{sub}/{file}").as_bytes(),
                            );
                        }
                    }
                }
                let older = test_commit_tree(&instance, "First").await;
                test_file_write(
                    instance
                        .path
                        .join(subdirectory(CHANGED, 0))
                        .join("file00.bin")
                        .as_path(),
                    b"rewritten",
                );
                let newer = test_commit_tree(&instance, "Second").await;

                let directories = top_level() * (SUBDIRECTORIES + 1) + 1;
                println!(
                    "tree: {} files in {directories} directories, {ROUNDS} rounds each",
                    top_level() * SUBDIRECTORIES * FILES
                );

                let open = || {
                    test_view_context(
                        &instance,
                        immutable_store.clone(),
                        mutable_store.clone(),
                        &[],
                    )
                };
                let diverging = || {
                    let directory = top(DIVERGED);
                    test_view_context(
                        &instance,
                        immutable_store.clone(),
                        mutable_store.clone(),
                        &[
                            &format!("/{directory}"),
                            &format!("!/{directory}/sub00/file00.bin"),
                        ],
                    )
                };
                let dropping = || {
                    test_view_context(
                        &instance,
                        immutable_store.clone(),
                        mutable_store.clone(),
                        &[&format!("/{}", top(DROPPED))],
                    )
                };
                let unanchored = || {
                    test_view_context(
                        &instance,
                        immutable_store.clone(),
                        mutable_store.clone(),
                        &[UNANCHORED],
                    )
                };

                let one_view = timed(
                    || {
                        let one = open();
                        (one.clone(), one)
                    },
                    &older,
                    &newer,
                )
                .await;
                let two_views = timed(|| (open(), diverging()), &older, &newer).await;
                let dropped = timed(|| (open(), dropping()), &older, &newer).await;
                let no_prune = timed(|| (open(), unanchored()), &older, &newer).await;

                one_view.report("one view");
                two_views.report("two views diverging in one subtree");
                dropped.report("two views, one subtree dropped");
                no_prune.report("two views, unanchored exclusion");

                assert_eq!(
                    one_view.changes, no_prune.changes,
                    "a rule matching nothing in the tree changes nothing in it, only what it cost \
                     to find that out"
                );
                assert_eq!(
                    no_prune.entered(),
                    u64::from(directories),
                    "an unanchored exclusion covers nothing, so the walk enters the tree whole"
                );
                assert!(
                    two_views.entered() > one_view.entered(),
                    "the diverging subtree is entered for the views alone"
                );
                assert!(
                    two_views.entered() < no_prune.entered() / 2,
                    "diverging in one subtree must cost a subtree, not the tree"
                );
                assert_eq!(
                    dropped.entered(),
                    one_view.entered(),
                    "the verdict taken where the walk pairs a dropped directory answers for \
                     everything under it, so the delete fans out without the walk entering it"
                );
                assert_eq!(
                    dropped.changes,
                    one_view.changes + (1 + SUBDIRECTORIES + SUBDIRECTORIES * FILES) as usize,
                    "a subtree the to view drops leaves path by path: the directory, its \
                     subdirectories and every file in them"
                );
            }))
            .await
            .expect("Benchmark task failed");
    }
}
