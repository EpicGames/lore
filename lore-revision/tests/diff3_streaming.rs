// Parity tests for the streaming `revision::diff3` / `branch::diff3` refactor.
//
// Each test builds a small on-disk repo with the production primitives
// (`repository::create_local`, `file::stage::stage`, `commit::commit`,
// `branch::create::create`, `branch::merge::merge_start`), drives a
// 3-way diff through `branch::diff3_collect`, and asserts on the
// drained `DiffResult`. The streaming implementation is what powers
// `diff3_collect` after the refactor — the drain wrapper re-collects
// into `DiffResult` — so set-equality on the drained output is the
// parity contract from spec §"Outputs and observable contract".

#![allow(clippy::disallowed_methods)] // Test fixtures write to the filesystem outside the repo write-token discipline.

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::change::FileAction;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore::runtime;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;

    include!("helper.rs");

    /// An offline execution context, for tests that call `merge_start`.
    ///
    /// `merge_start` reads the remote unless `globals.offline` is set, and the
    /// default test execution context is not offline. `test_store_create` still
    /// runs, because creating the stores seeds `LORE_CONTEXT`. Only the
    /// execution it returns is replaced.
    async fn offline_execution() -> Arc<lore_revision::interface::ExecutionContext> {
        let _ = test_store_create().await.expect("Failed to create stores");
        Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
        ))
    }

    /// Test fixture holding an initialized repository on disk plus a
    /// shared write token. Methods drive the production stage/commit/
    /// branch primitives so the resulting revisions exercise the same
    /// code path as the CLI.
    struct DiffFixture {
        repository: Arc<RepositoryContext>,
        write_token: RepositoryWriteToken,
        repo_path: PathBuf,
        main_branch_id: BranchId,
        _tempdir: TempDir,
    }

    impl DiffFixture {
        async fn new() -> Self {
            // `repository::create_local` builds its own immutable +
            // mutable stores tied to the on-disk repo directory and
            // returns a fully-wired `RepositoryContext` with anchor /
            // branch / metadata state initialised. Reuse that context
            // directly — constructing a parallel `RepositoryContext`
            // over freshly-allocated test stores would diverge from
            // create_local's internal stores and lose visibility of the
            // branch metadata it wrote.
            let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
            let tempdir = generate_tempdir();
            let repo_path = tempdir.to_path_buf();
            std::fs::create_dir_all(repo_path.as_path()).expect("Create repo directory failed");

            let main_branch_id = BranchId::from(uuid::Uuid::now_v7());
            let write_token = repository::RepositoryWriteToken::acquire(repo_path.as_path()).await;
            let repository = repository::create_local(
                repo_path.as_path(),
                &write_token,
                repository_id,
                main_branch_id,
                branch::DEFAULT_DEFAULT_NAME.to_string(),
                repository::RepositoryConfig::default(),
                false,
            )
            .await
            .expect("Failed to initialize repository");

            Self {
                repository,
                write_token,
                repo_path,
                main_branch_id,
                _tempdir: tempdir,
            }
        }

        /// Write `content` to a file relative to the repo root, creating
        /// parent directories as needed.
        fn write_file(&self, relative: &str, content: &[u8]) {
            let absolute = self.repo_path.join(relative);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create parent dir");
            }
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(absolute.as_path())
                .expect("Failed to open file");
            file.write_all(content).expect("Failed to write file");
        }

        /// Delete a file relative to the repo root. Tolerates absence —
        /// callers that need to remove a known file can rely on the
        /// existence precondition themselves.
        #[allow(dead_code)]
        fn delete_file(&self, relative: &str) {
            let absolute = self.repo_path.join(relative);
            let _ = std::fs::remove_file(absolute.as_path());
        }

        /// Recursively delete a directory relative to the repo root.
        #[allow(dead_code)]
        fn delete_dir(&self, relative: &str) {
            let absolute = self.repo_path.join(relative);
            let _ = std::fs::remove_dir_all(absolute.as_path());
        }

        /// Stage the entire repository directory — the production
        /// stager handles all add/modify/delete detection from the
        /// on-disk state.
        async fn stage_all(&self) {
            file::stage::stage(
                self.repository.clone(),
                &self.write_token,
                LoreArray::from_vec(vec![LoreString::from(&self.repo_path)]),
                StageOptions {
                    case_change: stage::StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
            )
            .await
            .expect("Failed to stage repository");
        }

        /// Commit the staged changes with the given message and return
        /// the resulting revision hash.
        async fn commit(&self, message: &str) -> Hash {
            let options = CommitOptions {
                message: message.to_string(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            };
            Box::pin(commit::commit(
                self.repository.clone(),
                &self.write_token,
                options,
            ))
            .await
            .expect("Failed to commit revision")
        }

        /// Convenience: stage and commit in one step.
        async fn stage_and_commit(&self, message: &str) -> Hash {
            self.stage_all().await;
            self.commit(message).await
        }

        /// Rename a file on disk and stage it as a move, so the change carries
        /// `FileAction::Move`. A scan-based stage of the same rename reports a
        /// delete and an add instead.
        async fn stage_move(&self, from: &str, to: &str) {
            let absolute_to = self.repo_path.join(to);
            if let Some(parent) = absolute_to.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create parent dir");
            }
            let absolute_from = self.repo_path.join(from);
            std::fs::rename(&absolute_from, &absolute_to).expect("Failed to rename");
            // `stage_move` resolves a user path against the process working
            // directory, so it takes absolute paths here.
            file::stage::stage_move(
                self.repository.clone(),
                &self.write_token,
                absolute_from.to_string_lossy().into_owned(),
                absolute_to.to_string_lossy().into_owned(),
                StageOptions {
                    case_change: stage::StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
            )
            .await
            .expect("Failed to stage move");
        }

        /// Create a branch starting from the current revision and
        /// switch to it. Returns the new branch ID.
        async fn create_branch(&self, name: &str) -> BranchId {
            branch::create::create(
                self.repository.clone(),
                &self.write_token,
                name.to_string(),
                None,
                String::new(),
                false,
            )
            .await
            .expect("Failed to create branch");
            // create::create stores the new branch as the current
            // anchor branch — read it back so callers can address it.
            let (_revision, branch_id) =
                lore_revision::instance::load_current_anchor(&self.repository)
                    .await
                    .expect("Failed to load current anchor after branch create");
            branch_id
        }

        /// Switch to the given branch at the given revision. Updates
        /// both the anchor branch and the anchor revision.
        async fn switch_to(&self, branch_id: BranchId, revision: Hash) {
            lore_revision::instance::store_current_anchor_branch(&self.repository, branch_id)
                .await
                .expect("Failed to store anchor branch");
            lore_revision::instance::store_current_anchor(&self.repository, revision)
                .await
                .expect("Failed to store anchor revision");
            // Realise the branch's tree to the working directory so
            // subsequent stage operations see the correct baseline.
            // The stage operation re-walks the working directory, so
            // the simplest valid setup is to wipe and rewrite tracked
            // files for each branch in the test scenario.
        }
    }

    /// Collect (path, debug-formatted-action) pairs for set-equality
    /// assertions. The spec calls out set equality rather than
    /// strict-order equality; using the debug form sidesteps
    /// `FileAction`'s missing `Hash` impl.
    fn changes_as_summary(changes: &[lore_revision::change::NodeChange]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = changes
            .iter()
            .map(|c| (c.path.as_str().to_string(), format!("{:?}", c.action)))
            .collect();
        out.sort();
        out
    }

    /// Scenario 4: `include_same` dedup.
    ///
    /// Base has a file. Both source and target modify it to the same
    /// content. With `include_same=true`, today's join emits the
    /// identical change exactly once (deduped against the
    /// most-recently-emitted path). The streaming join preserves this:
    /// it tracks the most-recently-emitted path and skips a duplicate
    /// when source and target produce the same change.
    #[tokio::test]
    async fn include_same_dedup() {
        // `test_store_create` is used here solely to set up a
        // LORE_CONTEXT-scoped execution. The stores it returns are
        // discarded — `repository::create_local` builds its own.
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Base revision: file with original content.
                fixture.write_file("shared.txt", b"original\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Create source branch off base and modify the file.
                let source_branch = fixture.create_branch("source").await;
                fixture.write_file("shared.txt", b"updated\n");
                let source_revision = fixture.stage_and_commit("source change").await;

                // Switch back to main and create target branch off
                // base, making the same modification.
                fixture.switch_to(main_branch, base_revision).await;
                // Restore on-disk state to base content before
                // creating target — otherwise the stager sees source's
                // working-tree state. The simplest restoration is to
                // overwrite the file with the base content explicitly.
                fixture.write_file("shared.txt", b"original\n");
                let target_branch = fixture.create_branch("target").await;
                fixture.write_file("shared.txt", b"updated\n");
                let target_revision = fixture.stage_and_commit("target change").await;

                // 3-way diff with include_same=true. The change should
                // appear exactly once (deduped), as a non-conflict.
                let diff = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    true,  // include_same
                    false, // auto_resolve
                ))
                .await
                .expect("diff3_collect failed");

                let summary = changes_as_summary(&diff.changes);
                let same_path_changes: Vec<_> = diff
                    .changes
                    .iter()
                    .filter(|c| c.path.as_str() == "shared.txt")
                    .collect();
                assert_eq!(
                    same_path_changes.len(),
                    1,
                    "include_same should emit shared.txt exactly once, got {} entries: {:?}",
                    same_path_changes.len(),
                    summary,
                );
                assert!(
                    diff.conflicts.is_empty(),
                    "identical changes should not be a conflict, got {:?}",
                    diff.conflicts,
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A change made only on the target branch, at a path the source branch
    /// never touched, is dropped when the source side has fewer than
    /// `SOURCE_FILTER_THRESHOLD` changes.
    ///
    /// `diff3` derives a view filter from the source changes
    /// (`filter_from_source_changes`) that scopes target's walk to
    /// source-touched paths, so a target-only path is filtered out before the
    /// join. Above the threshold the filter is disabled and such changes
    /// appear. That case is impractical to build here.
    #[tokio::test]
    async fn target_only_change_dropped_by_source_path_filter() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Base revision on main: two files with base content.
                fixture.write_file("alpha.txt", b"base\n");
                fixture.write_file("beta.txt", b"base\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Source branch, the branch being merged: change alpha.txt only.
                let source_branch = fixture.create_branch("feature").await;
                fixture.write_file("alpha.txt", b"changed on feature\n");
                let source_revision = fixture.stage_and_commit("feature change").await;

                // Back to main at base. Restore the working tree to base
                // content, so the target branch stages only its own change.
                // Otherwise feature's alpha edit joins it and both sides
                // change the same path.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("alpha.txt", b"base\n");
                fixture.write_file("beta.txt", b"base\n");

                // Target branch, standing in for `main`: change beta.txt
                // only, a path the source branch never touched.
                let target_branch = fixture.create_branch("main-work").await;
                fixture.write_file("beta.txt", b"changed on main\n");
                let target_revision = fixture.stage_and_commit("main change").await;

                // CLI orientation: source = your branch, target = main.
                let diff = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    false, // include_same
                    false, // auto_resolve
                ))
                .await
                .expect("diff3_collect failed");

                let summary = changes_as_summary(&diff.changes);
                let paths: Vec<&str> = diff.changes.iter().map(|c| c.path.as_str()).collect();

                // The target-only change (beta.txt, untouched by source) is
                // dropped: target's walk is scoped to source-touched paths.
                assert!(
                    !paths.contains(&"beta.txt"),
                    "expected target-only beta.txt to be filtered out by the \
                     source-path scoping, but it appeared: {summary:?}",
                );
                // The source branch's own change is present.
                assert!(
                    paths.contains(&"alpha.txt"),
                    "source-only change (feature's alpha.txt) missing from result: {summary:?}",
                );
                // The paths do not overlap, so there are no conflicts.
                assert!(
                    diff.conflicts.is_empty(),
                    "disjoint changes should not conflict, got {:?}",
                    diff.conflicts,
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A `branch merge` run under a sparse view filter must still merge
    /// changes at out-of-view paths into the tree.
    ///
    /// main changes an out-of-view file. feature merges main under a view
    /// filter that excludes it. After the merge feature must not be divergent
    /// from main there, so a full-tree diff of the merged feature against main
    /// does not list the out-of-view path.
    #[tokio::test]
    async fn merge_under_view_filter_must_not_drop_out_of_view_changes() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // 1. Base revision on main.
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/b.txt", b"v1\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // 2. The feature branch changes the in-view file only.
                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("game/a.txt", b"feature change\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;

                // 3. main advances and changes the out-of-view file only.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/b.txt", b"v2\n");
                let main_v2 = fixture.stage_and_commit("main changes engine").await;

                // 4. Back to feature. Merge main under a view filter that
                //    excludes engine/. Restore feature's working tree first.
                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file("game/a.txt", b"feature change\n");
                fixture.write_file("engine/b.txt", b"v1\n");

                let mut view_filter = lore_revision::filter::Filter::default();
                view_filter
                    .view
                    .add_exclusion("engine")
                    .expect("view exclude");
                view_filter
                    .view
                    .add_exclusion("engine/**")
                    .expect("view exclude");
                // `to_filter_context` drops the write token. Re-attach a
                // shared one, so the view-scoped merge can write anchors.
                let view_repo = std::sync::Arc::new(
                    fixture
                        .repository
                        .to_filter_context(std::sync::Arc::new(view_filter))
                        .with_write_token(fixture.write_token.share()),
                );

                let merged_rev = lore_revision::branch::merge::merge_start(
                    view_repo.clone(),
                    &fixture.write_token,
                    main_branch,
                    lore_revision::branch::merge::MergeStartOptions {
                        message: "merge main into feature (sparse view)".to_string(),
                        no_commit: false,
                        scope: lore_revision::branch::merge::MergeScope::MainOnly,
                    },
                )
                .await
                .expect("merge_start failed");

                // 5a. Full-tree diff, as the server runs it: empty view filter.
                let full = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    feature_branch,
                    merged_rev,
                    main_branch,
                    main_v2,
                    None,
                    false,
                    false,
                ))
                .await
                .expect("full diff3_collect failed");

                // 5b. View-scoped diff, as the CLI runs it: sparse view filter.
                let scoped = Box::pin(branch::diff3_collect(
                    view_repo.clone(),
                    feature_branch,
                    merged_rev,
                    main_branch,
                    main_v2,
                    None,
                    false,
                    false,
                ))
                .await
                .expect("scoped diff3_collect failed");

                let full_summary = changes_as_summary(&full.changes);
                let scoped_summary = changes_as_summary(&scoped.changes);
                let full_paths: Vec<&str> = full.changes.iter().map(|c| c.path.as_str()).collect();
                let scoped_paths: Vec<&str> =
                    scoped.changes.iter().map(|c| c.path.as_str()).collect();

                // The view-scoped diff hides the out-of-view file.
                assert!(
                    !scoped_paths.contains(&"engine/b.txt"),
                    "view-scoped diff should not list engine/b.txt: {scoped_summary:?}",
                );

                // The merge brought main's out-of-view change into the tree,
                // so feature is no longer divergent from main there and the
                // full-tree diff does not list it. This catches a view-scoped
                // merge that drops the change.
                assert!(
                    !full_paths.contains(&"engine/b.txt"),
                    "partial merge: merging main under a sparse view left \
                     engine/b.txt divergent from main (full-tree diff still \
                     lists it): {full_summary:?}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Build a view filter that excludes `directory/` at every depth, and a
    /// repository context that applies it.
    ///
    /// `to_filter_context` drops the write token, so a shared one is re-attached
    /// for the view-scoped merge to write anchors with.
    fn excluded_context(fixture: &DiffFixture, directory: &str) -> Arc<RepositoryContext> {
        let mut view_filter = lore_revision::filter::Filter::default();
        view_filter
            .view
            .add_exclusion(directory)
            .expect("view exclude");
        view_filter
            .view
            .add_exclusion(&format!("{directory}/**"))
            .expect("view exclude");
        Arc::new(
            fixture
                .repository
                .to_filter_context(Arc::new(view_filter))
                .with_write_token(fixture.write_token.share()),
        )
    }

    fn engine_excluded_context(fixture: &DiffFixture) -> Arc<RepositoryContext> {
        excluded_context(fixture, "engine")
    }

    /// Merge `main` into `feature` through `view_repo` and return the merged
    /// revision.
    async fn merge_main_under_view(
        view_repo: &Arc<RepositoryContext>,
        fixture: &DiffFixture,
        main_branch: BranchId,
        message: &str,
    ) -> Hash {
        lore_revision::branch::merge::merge_start(
            view_repo.clone(),
            &fixture.write_token,
            main_branch,
            lore_revision::branch::merge::MergeStartOptions {
                message: message.to_string(),
                no_commit: false,
                scope: lore_revision::branch::merge::MergeScope::MainOnly,
            },
        )
        .await
        .expect("merge_start failed")
    }

    /// Paths where the merged feature branch still differs from main, under a
    /// full-tree diff. Empty means the two agree everywhere.
    async fn paths_divergent_from_main(
        fixture: &DiffFixture,
        feature_branch: BranchId,
        merged_rev: Hash,
        main_branch: BranchId,
        main_rev: Hash,
    ) -> Vec<String> {
        let diff = Box::pin(branch::diff3_collect(
            fixture.repository.clone(),
            feature_branch,
            merged_rev,
            main_branch,
            main_rev,
            None,
            false,
            false,
        ))
        .await
        .expect("diff3_collect failed");
        diff.changes
            .iter()
            .map(|change| change.path.as_str().to_string())
            .collect()
    }

    /// Adopting an out-of-view subtree must apply every kind of change inside
    /// it, not only modifications.
    ///
    /// Adoption reconciles the two subtrees: it skips a child whose address
    /// already matches, overwrites one that differs, creates one the other
    /// branch added, and discards one it no longer has. A merge that only
    /// modifies files exercises the overwrite alone, so this covers the create
    /// and discard paths, including a new directory and a nested delete.
    #[tokio::test]
    async fn merge_adopts_added_and_deleted_paths_in_an_out_of_view_subtree() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // 1. Base revision on main.
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/keep.txt", b"v1\n");
                fixture.write_file("engine/modify.txt", b"v1\n");
                fixture.write_file("engine/remove.txt", b"v1\n");
                fixture.write_file("engine/sub/remove_deep.txt", b"v1\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // 2. feature changes the in-view file only, so engine/ still
                //    matches base and qualifies for adoption.
                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("game/a.txt", b"feature change\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;

                // 3. main modifies one path, adds two (one in a new directory)
                //    and deletes two (one nested).
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/modify.txt", b"v2\n");
                fixture.write_file("engine/added.txt", b"added\n");
                fixture.write_file("engine/newdir/added_deep.txt", b"added deep\n");
                fixture.delete_file("engine/remove.txt");
                fixture.delete_file("engine/sub/remove_deep.txt");
                let main_v2 = fixture.stage_and_commit("main reshapes engine").await;

                // 4. Back to feature, restore its working tree, and merge main
                //    under a view that excludes engine/.
                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file("game/a.txt", b"feature change\n");

                let view_repo = engine_excluded_context(&fixture);
                let merged_rev =
                    merge_main_under_view(&view_repo, &fixture, main_branch, "merge main").await;

                // Every path under engine/ must now agree with main. A missed
                // create or discard shows up here as a divergent path.
                let divergent = paths_divergent_from_main(
                    &fixture,
                    feature_branch,
                    merged_rev,
                    main_branch,
                    main_v2,
                )
                .await;
                let under_engine: Vec<&String> = divergent
                    .iter()
                    .filter(|path| path.starts_with("engine"))
                    .collect();
                assert!(
                    under_engine.is_empty(),
                    "adopted subtree still differs from main at {under_engine:?}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A subtree this branch has changed must not be adopted.
    ///
    /// Adoption replaces the branch's whole version of the subtree, so it is
    /// only sound when the branch left that subtree identical to the base.
    /// `GraftOracle` compares the directory address against the base's to check
    /// that. Here the branch carries its own out-of-view change, picked up by
    /// merging a third branch, so adoption must be refused and both sides' work
    /// must survive.
    #[tokio::test]
    async fn merge_does_not_adopt_a_subtree_the_branch_changed() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // 1. Base revision on main.
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/ours.txt", b"v1\n");
                fixture.write_file("engine/theirs.txt", b"v1\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // 2. A third branch changes one out-of-view path.
                let other_branch = fixture.create_branch("other").await;
                fixture.write_file("engine/ours.txt", b"from other\n");
                fixture.stage_and_commit("other changes engine").await;

                // 3. feature starts at base and takes that change on by merging
                //    `other`. Its engine/ now differs from the base, which is
                //    what must stop the later merge from adopting it.
                fixture.switch_to(main_branch, base_revision).await;
                let feature_branch = fixture.create_branch("feature").await;
                let feature_rev = merge_main_under_view(
                    &fixture.repository,
                    &fixture,
                    other_branch,
                    "merge other",
                )
                .await;

                // 4. main advances a different out-of-view path.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("engine/ours.txt", b"v1\n");
                fixture.write_file("engine/theirs.txt", b"v2\n");
                let main_v2 = fixture.stage_and_commit("main changes engine").await;

                // 5. feature merges main under a view that excludes engine/.
                fixture.switch_to(feature_branch, feature_rev).await;
                let view_repo = engine_excluded_context(&fixture);
                let merged_rev =
                    merge_main_under_view(&view_repo, &fixture, main_branch, "merge main").await;

                let divergent = paths_divergent_from_main(
                    &fixture,
                    feature_branch,
                    merged_rev,
                    main_branch,
                    main_v2,
                )
                .await;

                // main's change has to arrive.
                assert!(
                    !divergent.iter().any(|path| path == "engine/theirs.txt"),
                    "main's out-of-view change was dropped: {divergent:?}",
                );

                // feature's own change has to survive, so it still reads as
                // divergent from main. Adopting the subtree would have replaced
                // it with main's version.
                assert!(
                    divergent.iter().any(|path| path == "engine/ours.txt"),
                    "the branch's own out-of-view change was discarded: {divergent:?}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A merge that renames a file from an out-of-view name to an in-view one
    /// must write it to the working tree.
    ///
    /// Realize handles a move by renaming the file already on disk, then skips
    /// the content write because the rename is taken to have positioned it. A
    /// sparse working tree holds no file at the source name, so the rename
    /// fails and the skip leaves the destination missing. The tree says the
    /// file exists, so the next scan reads the absence as a delete.
    ///
    /// The diff emits `FileAction::Move` only for a rename inside one parent
    /// directory, so the view has to exclude by name rather than by directory
    /// for the two ends to sit on opposite sides of it.
    #[tokio::test]
    async fn merge_rename_from_out_of_view_to_in_view_realizes_the_file() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // 1. Base revision on main.
                fixture.write_file("keep.txt", b"keep v1\n");
                fixture.write_file("hidden.bin", b"moved content\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // 2. feature changes the in-view file only.
                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("keep.txt", b"keep v2 on feature\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;

                // 3. main renames the excluded file to an in-view name, leaving
                //    the content untouched. `stage_move` keeps the node
                //    identity, which is what makes the diff report a move
                //    rather than a delete and an add.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("keep.txt", b"keep v1\n");
                fixture.write_file("hidden.bin", b"moved content\n");
                fixture.stage_all().await;
                fixture.stage_move("hidden.bin", "shown.txt").await;
                fixture.commit("main renames the file into view").await;

                // 4. Back to feature, with the working tree as a sparse
                //    checkout would hold it: the excluded name is absent.
                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file("keep.txt", b"keep v2 on feature\n");
                fixture.delete_file("hidden.bin");
                fixture.delete_file("shown.txt");

                let mut view_filter = lore_revision::filter::Filter::default();
                view_filter
                    .view
                    .add_exclusion("*.bin")
                    .expect("view exclude");
                let view_repo = Arc::new(
                    fixture
                        .repository
                        .to_filter_context(Arc::new(view_filter))
                        .with_write_token(fixture.write_token.share()),
                );

                merge_main_under_view(&view_repo, &fixture, main_branch, "merge main").await;

                let destination = fixture.repo_path.join("shown.txt");
                assert!(
                    destination.exists(),
                    "the rename destination is in view, so the merge must write \
                     it to the working tree: shown.txt",
                );
                assert_eq!(
                    std::fs::read(&destination).expect("read renamed file"),
                    b"moved content\n",
                    "the rename destination holds the wrong content",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Scenario 5 (cap variant): source-side cap fires before target
    /// walk runs.
    ///
    /// Build a fixture where source produces more changes than the
    /// configured cap allows. The streaming `diff3_with_source_cap`
    /// must error out with `BranchError::Oversized` before any target
    /// work happens. The v1 handler uses `is_oversized()` to map the
    /// failure to `Status::resource_exhausted`.
    #[tokio::test]
    async fn source_cap_fires() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Base revision with a single file. Subsequent
                // source-side changes will exceed the configured cap.
                fixture.write_file("base.txt", b"base\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Source branch: add three new files (3 source-side
                // changes). Setting cap = 1 below guarantees the cap
                // fires.
                let source_branch = fixture.create_branch("source").await;
                fixture.write_file("a.txt", b"a\n");
                fixture.write_file("b.txt", b"b\n");
                fixture.write_file("c.txt", b"c\n");
                let source_revision = fixture.stage_and_commit("source add 3").await;

                // Target branch: trivial divergence so the 3-way diff
                // is well-defined. A single unrelated change is
                // enough; the cap fires before target's walk so target
                // content does not matter for the assertion.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("base.txt", b"base\n"); // restore base content
                fixture.delete_file("a.txt");
                fixture.delete_file("b.txt");
                fixture.delete_file("c.txt");
                let target_branch = fixture.create_branch("target").await;
                fixture.write_file("target_only.txt", b"target\n");
                let target_revision = fixture.stage_and_commit("target add 1").await;

                // Drive the streaming branch::diff3_with_source_cap
                // with a cap below source's 3-change count. The cap
                // fires inside revision::diff3 and the error wraps as
                // BranchError::Diff at the branch layer.
                let (tx, mut rx) = tokio::sync::mpsc::channel::<
                    Result<lore_revision::revision::DiffItem, lore_revision::branch::BranchError>,
                >(8);
                let producer = Box::pin(branch::diff3_with_source_cap(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    false, // include_same
                    false, // auto_resolve
                    Some(1),
                    None, // history_walk_concurrency: default
                    None, // graft_view: no grafting
                    tx,
                ));
                // Drain any items the producer emits before erroring.
                // The cap fires before target walk so we expect no
                // items, but draining keeps the channel alive in case
                // the producer happens to emit before erroring.
                let producer_result = producer.await;
                while rx.recv().await.is_some() {}

                let err =
                    producer_result.expect_err("expected source cap to fire and return an error");
                assert!(
                    err.is_oversized(),
                    "expected BranchError::Oversized, got {err:?}: {err}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Scenario 2: directory-delete overlap with conflict.
    ///
    /// Source deletes a directory containing files. Target modifies a
    /// file inside that directory. The 3-way diff's directory-delete
    /// overlap filter (revision.rs §"Post-merge pass 2") should drop
    /// the directory-delete from `changes` because a conflict's path
    /// overlaps it — emitting the directory-delete would shadow the
    /// conflict in a downstream merge UI.
    ///
    /// Today's algorithm and the streaming version both apply the same
    /// `RelativePath::overlaps` retain. The streaming version folds
    /// this into the join's emit step (buffering directory-deletes,
    /// flushing on stream end). Parity: directory-delete absent from
    /// changes, conflict present.
    #[tokio::test]
    async fn directory_delete_overlap_with_conflict() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Base revision: a directory `sub/` with two files.
                fixture.write_file("sub/keep.txt", b"keep base\n");
                fixture.write_file("sub/conflicted.txt", b"conflicted base\n");
                fixture.write_file("root.txt", b"root\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Source branch: delete the entire `sub/` directory.
                let source_branch = fixture.create_branch("source").await;
                fixture.delete_dir("sub");
                let source_revision =
                    fixture.stage_and_commit("source delete sub/").await;

                // Target branch: modify `sub/conflicted.txt`. The
                // modification on target conflicts with source's
                // delete-of-directory (the path overlaps the deleted
                // dir).
                fixture.switch_to(main_branch, base_revision).await;
                // Restore on-disk state to match base before creating
                // target — the previous source-branch work wiped
                // sub/.
                fixture.write_file("sub/keep.txt", b"keep base\n");
                fixture.write_file("sub/conflicted.txt", b"conflicted base\n");
                fixture.write_file("root.txt", b"root\n");
                let target_branch = fixture.create_branch("target").await;
                fixture.write_file("sub/conflicted.txt", b"conflicted on target\n");
                let target_revision =
                    fixture.stage_and_commit("target modify sub/conflicted.txt").await;

                let diff = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    false, // include_same
                    false, // auto_resolve
                ))
                .await
                .expect("diff3_collect failed");

                // The conflict must be reported on the modified file.
                let conflict_paths: Vec<_> = diff
                    .conflicts
                    .iter()
                    .map(|(s, _)| s.path.as_str().to_string())
                    .collect();
                assert!(
                    conflict_paths.iter().any(|p| p == "sub/conflicted.txt"),
                    "expected conflict on sub/conflicted.txt; got conflicts {conflict_paths:?} and changes {:?}",
                    changes_as_summary(&diff.changes),
                );

                // The directory-delete must NOT appear in `changes`
                // because it overlaps the conflict's path. The
                // directory-delete is the parent-dir entry, which we
                // identify by its trailing-slash form. Today's
                // algorithm emits these as a `Delete` action against
                // the directory's path; the overlap filter strips
                // them when a conflict overlaps.
                let dir_delete_in_changes = diff.changes.iter().any(|c| {
                    c.path.as_str() == "sub" && c.action == FileAction::Delete
                });
                assert!(
                    !dir_delete_in_changes,
                    "directory delete for 'sub' should be filtered out by overlap pass; got changes {:?}",
                    changes_as_summary(&diff.changes),
                );

                // The non-overlapping delete (`sub/keep.txt`) should
                // still be present — it is not in conflict, so it is
                // a clean delete and stays in changes.
                let keep_deleted = diff.changes.iter().any(|c| {
                    c.path.as_str() == "sub/keep.txt"
                        && c.action == FileAction::Delete
                });
                assert!(
                    keep_deleted,
                    "non-overlapping delete sub/keep.txt should remain in changes; got {:?}",
                    changes_as_summary(&diff.changes),
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Scenario 1: crossing moves (A → B vs B → A).
    ///
    /// Source moves `a.txt → b.txt`; target moves `b.txt → a.txt`. The
    /// streaming join's Move-vs-Move resolution must pair these as a
    /// conflict, matching today's behaviour (the from-path absorb /
    /// conflict pass at `revision.rs:441-490` historically). The
    /// streaming implementation runs the equivalent logic in
    /// `apply_move_from_path_pass`.
    ///
    /// Parity contract: both `a.txt` and `b.txt` end up flagged in the
    /// output. The exact conflict-vs-absorb classification depends on
    /// content equivalence — when both moves change content the pass
    /// emits two conflicts; when content stays identical the pass
    /// absorbs the secondary change.
    #[tokio::test]
    async fn crossing_moves() {
        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Base revision: two files `a.txt` and `b.txt` with
                // distinct content. Distinct content is necessary so
                // a rename is unambiguous to the stager's move
                // detection.
                fixture.write_file("a.txt", b"alpha content\n");
                fixture.write_file("b.txt", b"beta content\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Source branch: move `a.txt → b.txt`. Achieved by
                // deleting `b.txt` and renaming `a.txt` into its
                // place. Result: `b.txt` now contains `alpha`'s
                // content; `a.txt` no longer exists.
                let source_branch = fixture.create_branch("source").await;
                fixture.delete_file("b.txt");
                std::fs::rename(
                    fixture.repo_path.join("a.txt"),
                    fixture.repo_path.join("b.txt"),
                )
                .expect("rename a.txt -> b.txt failed");
                let source_revision = fixture.stage_and_commit("source: a -> b").await;

                // Target branch: move `b.txt → a.txt`. Same
                // restoration-then-rename pattern.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.delete_file("b.txt");
                fixture.write_file("a.txt", b"alpha content\n");
                fixture.write_file("b.txt", b"beta content\n");
                let target_branch = fixture.create_branch("target").await;
                fixture.delete_file("a.txt");
                std::fs::rename(
                    fixture.repo_path.join("b.txt"),
                    fixture.repo_path.join("a.txt"),
                )
                .expect("rename b.txt -> a.txt failed");
                let target_revision = fixture.stage_and_commit("target: b -> a").await;

                let diff = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    false, // include_same
                    false, // auto_resolve
                ))
                .await
                .expect("diff3_collect failed");

                // The streaming Move-vs-Move resolution must mark both
                // files as either changes or conflicts — the spec
                // does not pin the precise classification, but neither
                // file may silently disappear from the diff.
                let summary = changes_as_summary(&diff.changes);
                let conflict_paths: Vec<_> = diff
                    .conflicts
                    .iter()
                    .flat_map(|(s, t)| {
                        vec![s.path.as_str().to_string(), t.path.as_str().to_string()]
                    })
                    .collect();
                let total_a_mentions = summary.iter().filter(|(p, _)| p == "a.txt").count()
                    + conflict_paths.iter().filter(|p| *p == "a.txt").count();
                let total_b_mentions = summary.iter().filter(|(p, _)| p == "b.txt").count()
                    + conflict_paths.iter().filter(|p| *p == "b.txt").count();
                assert!(
                    total_a_mentions > 0,
                    "a.txt must appear in changes or conflicts after crossing moves; got changes {summary:?} conflicts {conflict_paths:?}",
                );
                assert!(
                    total_b_mentions > 0,
                    "b.txt must appear in changes or conflicts after crossing moves; got changes {summary:?} conflicts {conflict_paths:?}",
                );

                // The diff must report at least one conflict — two
                // independent renames of crossing files cannot be
                // applied cleanly without user input.
                assert!(
                    !diff.conflicts.is_empty(),
                    "crossing moves must produce at least one conflict; got changes {summary:?}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Scenario 3: conflicts resolved via history walk.
    ///
    /// Source picks up target's change via a prior merge, then makes
    /// a further independent modification. A naive 3-way diff would
    /// see both branches modify the same file and flag a conflict,
    /// but `is_last_change_merged` should detect that target's change
    /// is already present in source's history and downgrade the
    /// conflict to a change on source's side.
    ///
    /// Sequence:
    /// 1. base: `shared.txt = "v1"`.
    /// 2. target branch modifies → `"v2"`.
    /// 3. source branch makes an unrelated change so the two diverge.
    /// 4. source merges target into source (auto-commit, no conflict
    ///    because source did not touch `shared.txt` yet).
    /// 5. source modifies `shared.txt` → `"v3"`.
    /// 6. 3-way diff(source, target): `shared.txt` differs (v3 vs v2)
    ///    but target's v2 lives in source's ancestry. History walk
    ///    resolves the conflict — file appears in `changes`, not
    ///    `conflicts`.
    #[tokio::test]
    async fn history_walk_resolves_conflict() {
        let execution = offline_execution().await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                // Step 1: base revision.
                fixture.write_file("shared.txt", b"v1\n");
                fixture.write_file("other.txt", b"unrelated\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // Step 2: target branch modifies shared.txt.
                let target_branch = fixture.create_branch("target").await;
                fixture.write_file("shared.txt", b"v2\n");
                let target_revision = fixture.stage_and_commit("target v2").await;

                // Step 3: switch back to main, create source off base
                // with an unrelated change so the two branches
                // diverge cleanly.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("shared.txt", b"v1\n");
                fixture.write_file("other.txt", b"unrelated\n");
                let source_branch = fixture.create_branch("source").await;
                fixture.write_file("other.txt", b"source diverges\n");
                let _source_diverge_rev =
                    fixture.stage_and_commit("source unrelated change").await;

                // Step 4: merge target into source. Source has not
                // touched shared.txt yet, so the merge resolves
                // cleanly and auto-commits.
                let merge_revision = lore_revision::branch::merge::merge_start(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    target_branch,
                    lore_revision::branch::merge::MergeStartOptions {
                        message: "merge target into source".to_string(),
                        no_commit: false,
                        scope: lore_revision::branch::merge::MergeScope::MainOnly,
                    },
                )
                .await
                .expect("merge_start failed");
                assert_ne!(
                    merge_revision,
                    Hash::default(),
                    "merge_start must return a non-zero revision",
                );

                // Step 5: source modifies shared.txt after the merge.
                // The working tree is now in the post-merge state
                // (shared.txt = v2). Overwrite it to v3 and commit.
                fixture.write_file("shared.txt", b"v3\n");
                let source_revision =
                    fixture.stage_and_commit("source modifies shared after merge").await;

                // Step 6: 3-way diff. Without history-walk resolution
                // the diff would emit a conflict on shared.txt
                // because source and target both modified it. With
                // history-walk resolution the conflict is downgraded
                // because target's v2 lives in source's history.
                let diff = Box::pin(branch::diff3_collect(
                    fixture.repository.clone(),
                    source_branch,
                    source_revision,
                    target_branch,
                    target_revision,
                    None,
                    false, // include_same
                    false, // auto_resolve
                ))
                .await
                .expect("diff3_collect failed");

                let conflict_paths: Vec<_> = diff
                    .conflicts
                    .iter()
                    .map(|(s, _)| s.path.as_str().to_string())
                    .collect();
                assert!(
                    !conflict_paths.iter().any(|p| p == "shared.txt"),
                    "history walk should resolve the conflict on shared.txt; conflicts {conflict_paths:?}, changes {:?}",
                    changes_as_summary(&diff.changes),
                );
                // The file must still appear in changes — source's
                // post-merge modification is a real change relative
                // to target.
                let in_changes = diff
                    .changes
                    .iter()
                    .any(|c| c.path.as_str() == "shared.txt");
                assert!(
                    in_changes,
                    "shared.txt should appear in changes after history-walk resolution; got changes {:?}",
                    changes_as_summary(&diff.changes),
                );
            }))
            .await
            .expect("Test task failed");
    }
}
