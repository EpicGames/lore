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
                stats: false,
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
        // `branch::merge::merge_start` consults the remote unless
        // globals.offline is set. The default test execution context
        // is not offline, so build one explicitly here. The store
        // initialisation still goes through `test_store_create` for
        // its side effect of seeding LORE_CONTEXT during store
        // creation; the returned execution is replaced before the
        // test body runs.
        let _ = test_store_create().await.expect("Failed to create stores");
        let execution =
            std::sync::Arc::new(lore_revision::interface::ExecutionContext::new_client(
                lore_revision::interface::LoreGlobalArgs::default().set_offline(),
                lore_revision::relay::EventDispatcher::no_dispatch(),
            ));

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
