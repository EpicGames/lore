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
    use std::sync::Mutex;

    use lore_base::types::Hash;
    use lore_base::types::TypedBytes;
    use lore_revision::branch;
    use lore_revision::change::FileAction;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::LORE_CONTEXT;
    use lore_revision::lore::RepositoryId;
    use lore_revision::lore::runtime;
    use lore_revision::node::NodeFlags;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;

    include!("helper.rs");

    /// An offline execution context sending its events to `dispatcher`, for
    /// tests that call `merge_start`.
    ///
    /// `merge_start` reads the remote unless `globals.offline` is set, and the
    /// default test execution context is not offline. `test_store_create` still
    /// runs, because creating the stores seeds `LORE_CONTEXT`. Only the
    /// execution it returns is replaced.
    async fn offline_execution(
        dispatcher: EventDispatcher,
    ) -> Arc<lore_revision::interface::ExecutionContext> {
        let _ = test_store_create().await.expect("Failed to create stores");
        Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            dispatcher,
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
            commit::commit_boxed(self.repository.clone(), &self.write_token, options)
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

        /// Stage at `path` an empty link to a repository of its own, not following branches, so
        /// that the next stage takes the files the working tree holds below `path` into the
        /// linked repository, as staging below a link's mount does. The rest of the working tree
        /// is staged first, with `path` set aside, so the staged tree holds the parent of `path`.
        async fn stage_link(&self, path: &str) {
            let mount = self.repo_path.join(path);
            let aside = TempDir::new("diff3-link-mount-");
            std::fs::rename(&mount, aside.child("mount")).expect("Failed to set the mount aside");
            self.stage_all().await;

            let repository = &self.repository;
            let linked_id = RepositoryId::from(uuid::Uuid::now_v7());
            let linked = repository.to_link_context(linked_id).await;
            let linked_tree = state::State::new();
            linked_tree.set_revision_number(1);
            let pin = linked_tree
                .serialize(linked, &self.write_token)
                .await
                .expect("Failed to serialize the linked tree");

            let (current, staged, _) =
                state::State::deserialize_current_and_staged(repository.clone())
                    .await
                    .expect("Failed to read the staged tree");
            let staged = staged.unwrap_or_else(|| {
                current.set_parent_self(current.revision());
                current
            });
            let (parent, name) = path.rsplit_once('/').expect("a link below a directory");
            let parent = staged
                .find_node_link(repository.clone(), parent)
                .await
                .expect("the staged tree holds the link's parent")
                .node;
            let link = staged
                .node_add(
                    repository.clone(),
                    parent,
                    lore_revision::node::Node {
                        flags: NodeFlags::Link.bits(),
                        name_hash: lore_storage::hash::hash_string(name),
                        child: lore_revision::node::ROOT_NODE,
                        address: lore_base::types::Address {
                            hash: pin,
                            context: linked_id.into(),
                        },
                        ..Default::default()
                    },
                    name,
                )
                .await
                .expect("Failed to add the link");
            staged
                .node_mark(
                    repository,
                    link,
                    state::Mark::staged(NodeFlags::StagedAdd, true),
                )
                .await
                .expect("Failed to stage the link");
            staged
                .link_add(
                    repository.clone(),
                    linked_id,
                    BranchId::default(),
                    pin,
                    link,
                    lore_revision::link::LinkFlags::DisableAutoFollow,
                )
                .await
                .expect("Failed to register the link");
            let signature = staged
                .serialize(repository.clone(), &self.write_token)
                .await
                .expect("Failed to serialize the staged tree");
            lore_revision::instance::store_staged_anchor(repository, signature)
                .await
                .expect("Failed to store the staged anchor");

            std::fs::rename(aside.child("mount"), &mount).expect("Failed to restore the mount");
        }

        /// Remove the link at `path`, with its mount, as `lore link remove` does.
        async fn unlink(&self, path: &str) {
            lore_revision::link::remove::remove_boxed(
                self.repository.clone(),
                &self.write_token,
                lore_revision::util::path::RelativePath::new_from_initial_path(path)
                    .expect("a link path"),
            )
            .await
            .expect("Failed to remove the link");
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
                lore_revision::instance::load_current_anchor_boxed(&self.repository)
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
            .map(|c| (c.path().as_str().to_string(), format!("{:?}", c.action)))
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
                    .filter(|c| c.path().as_str() == "shared.txt")
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
                let paths: Vec<&str> = diff.changes.iter().map(|c| c.path().as_str()).collect();

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

    /// A path both sides change differently is a conflict when it holds glob
    /// syntax, as it is in a walk no source path filter scopes. So is a path the
    /// source renames and changes while the target changes it.
    ///
    /// `dir/plain.txt` and `dir/old.txt` hold none and stand for every ordinary
    /// path.
    #[tokio::test]
    async fn both_sides_changing_a_path_with_glob_syntax_conflict() {
        const GUID: &str = "{3F2504E0-4F89-11D3-9A0C-0305E82C3301}";
        const MOVES: [(&str, &str); 2] =
            [("dir/old.txt", "dir/new.txt"), ("dir/[y].txt", "dir/y.txt")];
        let mut paths = vec![
            "dir/plain.txt".to_owned(),
            "dir/[x].txt".to_owned(),
            "a{b,c}.txt".to_owned(),
            format!("{GUID}/f.txt"),
            format!("{GUID}/[x].txt"),
        ];
        if cfg!(unix) {
            paths.push("**/f.txt".to_owned());
        }

        let (_immutable_store, _mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;
                let write_all = |content: &[u8]| {
                    let moved = MOVES.iter().map(|(from, _)| *from);
                    for path in paths.iter().map(String::as_str).chain(moved) {
                        fixture.write_file(path, content);
                    }
                };

                write_all(b"base\n");
                let base_revision = fixture.stage_and_commit("base").await;

                let source_branch = fixture.create_branch("source").await;
                write_all(b"source\n");
                for (from, to) in MOVES {
                    fixture.stage_move(from, to).await;
                }
                let source_revision = fixture.stage_and_commit("source change").await;

                fixture
                    .switch_to(fixture.main_branch_id, base_revision)
                    .await;
                for (_, to) in MOVES {
                    fixture.delete_file(to);
                }
                let target_branch = fixture.create_branch("target").await;
                write_all(b"target\n");
                let target_revision = fixture.stage_and_commit("target change").await;

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

                let mut conflicts: Vec<(&str, &str)> = diff
                    .conflicts
                    .iter()
                    .map(|(source, target)| (source.path().as_str(), target.path().as_str()))
                    .collect();
                conflicts.sort_unstable();
                let mut expected: Vec<(&str, &str)> = paths
                    .iter()
                    .map(|path| (path.as_str(), path.as_str()))
                    .chain(MOVES.iter().map(|&(from, to)| (to, from)))
                    .collect();
                expected.sort_unstable();
                assert_eq!(
                    conflicts,
                    expected,
                    "changes {:?}",
                    changes_as_summary(&diff.changes)
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
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

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
                let view_repo = std::sync::Arc::new(
                    fixture
                        .repository
                        .to_filter_context(std::sync::Arc::new(view_filter)),
                );

                let merged_rev = Box::pin(lore_revision::branch::merge::merge_start(
                    view_repo.clone(),
                    &fixture.write_token,
                    main_branch,
                    lore_revision::branch::merge::MergeStartOptions {
                        message: "merge main into feature (sparse view)".to_string(),
                        link_messages: std::collections::HashMap::new(),
                        no_commit: false,
                        scope: lore_revision::branch::merge::MergeScope::MainOnly,
                        inherit_metadata: Default::default(),
                    },
                ))
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
                let full_paths: Vec<&str> =
                    full.changes.iter().map(|c| c.path().as_str()).collect();
                let scoped_paths: Vec<&str> =
                    scoped.changes.iter().map(|c| c.path().as_str()).collect();

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

    /// A repository context whose view holds `rules`, a leading `!` re-including
    /// as it does in a view file.
    fn view_context(fixture: &DiffFixture, rules: &[&str]) -> Arc<RepositoryContext> {
        Arc::new(
            fixture
                .repository
                .to_filter_context(Arc::new(test_filter(&[], rules))),
        )
    }

    fn engine_excluded_context(fixture: &DiffFixture) -> Arc<RepositoryContext> {
        view_context(fixture, &["engine", "engine/**"])
    }

    /// Merge `main` into the current branch through `view_repo` and return the
    /// merged revision, or with `no_commit` the staged state it leaves.
    async fn merge_main_under_view(
        view_repo: &Arc<RepositoryContext>,
        fixture: &DiffFixture,
        main_branch: BranchId,
        message: &str,
        no_commit: bool,
    ) -> Hash {
        Box::pin(lore_revision::branch::merge::merge_start(
            view_repo.clone(),
            &fixture.write_token,
            main_branch,
            lore_revision::branch::merge::MergeStartOptions {
                message: message.to_string(),
                link_messages: std::collections::HashMap::new(),
                no_commit,
                scope: lore_revision::branch::merge::MergeScope::MainOnly,
                inherit_metadata: Default::default(),
            },
        ))
        .await
        .expect("merge_start failed")
    }

    /// A chmod is a modification of the executable bit and nothing else, which the content a
    /// merge brings in answers nothing about. The merge writes the content and leaves the bit,
    /// so the change the user made stands rather than being reverted by the write.
    ///
    /// The merge realizes every change it verified rather than the ones the working copy still
    /// needs, so what the verify settles on a change has to reach the realize that follows it.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_merge_keeps_a_local_executable_bit_over_incoming_content() {
        use std::os::unix::fs::PermissionsExt;

        const SCRIPT: &str = "script.sh";
        const FIRST: &[u8] = b"#!/bin/sh\necho first\n";
        const SECOND: &[u8] = b"#!/bin/sh\necho second\n";
        const FEATURE: &str = "feature.txt";

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                fixture.write_file(SCRIPT, FIRST);
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                // The feature branch leaves the script alone, so the merge below carries main's
                // rewrite of it whole.
                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file(FEATURE, b"feature\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;

                fixture.switch_to(main_branch, base_revision).await;
                fixture.delete_file(FEATURE);
                fixture.write_file(SCRIPT, SECOND);
                fixture.stage_and_commit("main rewrites the script").await;

                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file(SCRIPT, FIRST);
                fixture.write_file(FEATURE, b"feature\n");

                let script = fixture.repo_path.join(SCRIPT);
                std::fs::set_permissions(script.as_path(), std::fs::Permissions::from_mode(0o755))
                    .expect("Failed to set the executable bit");

                merge_main_under_view(
                    &fixture.repository,
                    &fixture,
                    main_branch,
                    "merge main into feature",
                    false,
                )
                .await;

                assert_eq!(
                    std::fs::read(script.as_path()).expect("The merged file must be readable"),
                    SECOND,
                    "the merge has to carry the content the incoming revision holds"
                );
                assert_ne!(
                    std::fs::metadata(script.as_path())
                        .expect("The merged file must be readable")
                        .permissions()
                        .mode()
                        & 0o111,
                    0,
                    "the bit the user set has to survive the write the merge makes"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Commits `file` on a base, changes it on a feature branch and on main, and starts a merge
    /// of the feature into main that leaves it in conflict.
    async fn start_conflicted_merge(fixture: &DiffFixture, file: &str) {
        fixture.write_file(file, b"base\n");
        let base_revision = fixture.stage_and_commit("base").await;
        let main_branch = fixture.main_branch_id;

        let feature_branch = fixture.create_branch("feature").await;
        fixture.write_file(file, b"feature\n");
        fixture.stage_and_commit("feature change").await;

        fixture.switch_to(main_branch, base_revision).await;
        fixture.write_file(file, b"main\n");
        fixture.stage_and_commit("main change").await;

        Box::pin(branch::merge::merge_start(
            fixture.repository.clone(),
            &fixture.write_token,
            feature_branch,
            branch::merge::MergeStartOptions {
                message: "merge feature into main".to_string(),
                link_messages: std::collections::HashMap::new(),
                no_commit: true,
                scope: branch::merge::MergeScope::MainOnly,
                inherit_metadata: Default::default(),
            },
        ))
        .await
        .expect("merge_start failed");
    }

    /// `file`'s node in the staged state.
    async fn staged_node(fixture: &DiffFixture, file: &str) -> lore_revision::node::Node {
        let (_current, staged, _branch) =
            lore_revision::state::State::deserialize_current_and_staged(fixture.repository.clone())
                .await
                .expect("the states load");
        let staged = staged.expect("the merge staged its changes");
        let link = staged
            .find_node_link(fixture.repository.clone(), file)
            .await
            .expect("the file is staged");
        staged
            .node(fixture.repository.clone(), link.node)
            .await
            .expect("the file's node reads")
    }

    /// `file` in the working tree, as a path argument.
    fn path_argument(fixture: &DiffFixture, file: &str) -> LoreArray<LoreString> {
        let path = fixture.repo_path.join(file);
        LoreArray::from_vec(vec![LoreString::from(path.to_string_lossy().as_ref())])
    }

    /// Resolving a merge conflict marks its node resolved, and unresolving it marks it a conflict
    /// again, each as the staged state records it.
    #[tokio::test]
    async fn a_merge_conflict_resolved_then_unresolved_records_each_in_the_staged_state() {
        const FILE: &str = "shared.txt";

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;
                start_conflicted_merge(&fixture, FILE).await;
                assert!(
                    staged_node(&fixture, FILE)
                        .await
                        .is_staged_merge_unresolved()
                );

                // Written without conflict markers, which would refuse the resolve.
                fixture.write_file(FILE, b"merged\n");
                branch::merge::merge_resolve(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    path_argument(&fixture, FILE),
                    branch::merge::MergeType::BranchMerge,
                )
                .await
                .expect("merge_resolve failed");
                assert!(staged_node(&fixture, FILE).await.is_staged_merge_resolved());

                branch::merge::merge_unresolve(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    path_argument(&fixture, FILE),
                    branch::merge::MergeType::BranchMerge,
                )
                .await
                .expect("merge_unresolve failed");
                assert!(
                    staged_node(&fixture, FILE)
                        .await
                        .is_staged_merge_unresolved()
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Unresolving a conflict resolved with mine or with theirs takes the side with the
    /// resolution: the node is a plain conflict again, of neither side.
    #[tokio::test]
    async fn a_merge_conflict_resolved_with_a_side_then_unresolved_keeps_no_side() {
        const FILE: &str = "shared.txt";

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                for mine in [true, false] {
                    let fixture = DiffFixture::new().await;
                    start_conflicted_merge(&fixture, FILE).await;

                    let resolved = if mine {
                        branch::merge::merge_resolve_mine(
                            fixture.repository.clone(),
                            &fixture.write_token,
                            path_argument(&fixture, FILE),
                        )
                        .await
                    } else {
                        branch::merge::merge_resolve_theirs(
                            fixture.repository.clone(),
                            &fixture.write_token,
                            path_argument(&fixture, FILE),
                        )
                        .await
                    };
                    resolved.expect("the resolve with a side");
                    let node = staged_node(&fixture, FILE).await;
                    assert!(node.is_staged_merge_resolved(), "mine: {mine}");
                    assert_eq!(
                        (node.is_staged_merge_mine(), node.is_staged_merge_theirs()),
                        (mine, !mine)
                    );

                    branch::merge::merge_unresolve(
                        fixture.repository.clone(),
                        &fixture.write_token,
                        path_argument(&fixture, FILE),
                        branch::merge::MergeType::BranchMerge,
                    )
                    .await
                    .expect("merge_unresolve failed");
                    let node = staged_node(&fixture, FILE).await;
                    assert!(node.is_staged_merge_unresolved(), "mine: {mine}");
                    assert!(
                        !node.is_staged_merge_mine() && !node.is_staged_merge_theirs(),
                        "mine: {mine}: no side stays, flags {:#x}",
                        node.flags
                    );
                }
            }))
            .await
            .expect("Test task failed");
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
            .map(|change| change.path().as_str().to_string())
            .collect()
    }

    /// Every directory in the tree of `revision`, in path order, with its hash and
    /// size. A directory's path ends in `/`, and the root's is `/`.
    async fn directory_hashes(fixture: &DiffFixture, revision: Hash) -> Vec<(String, Hash, u64)> {
        let repository = &fixture.repository;
        let state = state::State::deserialize(repository.clone(), revision)
            .await
            .expect("Failed to read the revision");
        let tree = state
            .tree(repository.clone())
            .await
            .expect("Failed to read the tree");
        let mut directories = vec![("/".to_string(), tree.hash_root, tree.size)];
        let mut pending = vec![("/".to_string(), lore_revision::node::ROOT_NODE)];
        while let Some((path, node_id)) = pending.pop() {
            for child in state
                .node_children(repository.clone(), node_id)
                .await
                .expect("Failed to list a directory")
            {
                let node = state
                    .node(repository.clone(), child)
                    .await
                    .expect("Failed to read a node");
                if node.is_directory() {
                    let name = state
                        .node_name_clone(repository.clone(), child)
                        .await
                        .expect("Failed to read a node name");
                    let child_path = format!("{path}{name}/");
                    directories.push((child_path.clone(), node.address.hash, node.size));
                    pending.push((child_path, child));
                }
            }
        }
        directories.sort_unstable();
        directories
    }

    /// The directories [`directory_hashes`] lists below `engine/`.
    async fn engine_directory_hashes(
        fixture: &DiffFixture,
        revision: Hash,
    ) -> Vec<(String, Hash, u64)> {
        let mut directories = directory_hashes(fixture, revision).await;
        directories.retain(|(path, _, _)| path.starts_with("engine/"));
        directories
    }

    /// The path of each link the link registry of `revision` holds, in path order.
    async fn link_paths(fixture: &DiffFixture, revision: Hash) -> Vec<String> {
        let repository = &fixture.repository;
        let state = state::State::deserialize(repository.clone(), revision)
            .await
            .expect("Failed to read the revision");
        let mut paths = Vec::new();
        for link in state
            .link_list(repository.clone())
            .await
            .expect("Failed to read the link registry")
        {
            paths.push(
                state
                    .node_path(repository.clone(), link.local_node)
                    .await
                    .expect("Failed to read a link path"),
            );
        }
        paths.sort_unstable();
        paths
    }

    /// The changes the delta of `revision` records, in path order, as the path, the
    /// `FileAction` number and the change flags. A deleted node's path is read from
    /// the first parent, the tree that holds it.
    async fn delta_changes(fixture: &DiffFixture, revision: Hash) -> Vec<(String, u16, u16)> {
        let repository = &fixture.repository;
        let merged = state::State::deserialize(repository.clone(), revision)
            .await
            .expect("Failed to read the revision");
        let parent = state::State::deserialize(repository.clone(), merged.parent_self())
            .await
            .expect("Failed to read the first parent");
        let delta = merged
            .delta_block(repository.clone())
            .await
            .expect("Failed to read the delta")
            .to_aligned::<lore_revision::node::NodeDelta>();
        let mut changes = Vec::new();
        for entry in delta.as_type_slice::<lore_revision::node::NodeDelta>() {
            let tree = if entry.action == FileAction::Delete as u16 {
                &parent
            } else {
                &merged
            };
            let path = tree
                .node_path(repository.clone(), entry.node)
                .await
                .expect("Failed to read a delta path");
            changes.push((path, entry.action, entry.flags));
        }
        changes.sort_unstable();
        changes
    }

    /// The history of the file at `path` from `revision`, as the path, action and
    /// path moved from of each entry. It runs in an execution of its own, whose
    /// events it collects: draining an execution's events closes its channel.
    async fn file_history(
        fixture: &DiffFixture,
        revision: Hash,
        path: &str,
    ) -> Vec<(String, String, String)> {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let recorder = recorded.clone();
        let execution = Arc::new(lore_revision::interface::ExecutionContext::new_client(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::FileHistory(entry) = event {
                    recorder.lock().expect("Recorded history poisoned").push((
                        entry.path.to_string(),
                        format!("{:?}", entry.action),
                        entry.from_path.to_string(),
                    ));
                }
            }))),
        ));
        LORE_CONTEXT
            .scope(
                execution.clone(),
                Box::pin(file::history::history(
                    fixture.repository.clone(),
                    fixture.repo_path.join(path).to_string_lossy().into_owned(),
                    file::history::HistoryOptions {
                        revision: Some(revision.to_string()),
                        branch: None,
                        length: 0,
                        depth: 0,
                    },
                )),
            )
            .await
            .expect("Failed to read the file history");
        execution.dispatcher.drain().await;
        std::mem::take(&mut *recorded.lock().expect("Recorded history poisoned"))
    }

    /// A change main makes in an adoption test.
    #[derive(Clone, Copy)]
    enum Edit {
        /// Write a file with the content.
        Write(&'static str, &'static [u8]),
        /// Delete a file.
        Delete(&'static str),
        /// Delete a directory and everything below it.
        DeleteDirectory(&'static str),
        /// Move a file, keeping its identity.
        Move(&'static str, &'static str),
        /// Create an empty directory.
        MakeDirectory(&'static str),
        /// Make the directory the working tree holds at the path a link's mount
        /// ([`DiffFixture::stage_link`]).
        Link(&'static str),
        /// Remove the link at the path ([`DiffFixture::unlink`]).
        Unlink(&'static str),
        /// Commit what main holds so far.
        Commit,
    }

    /// The files of an adoption test's base below `engine/`.
    const ENGINE_BASE: &[(&str, &[u8])] = &[
        ("engine/keep.txt", b"keep\n"),
        ("engine/modify.txt", b"modify v1\n"),
        ("engine/remove.txt", b"remove\n"),
        ("engine/move.txt", b"move\n"),
        ("engine/rename.txt", b"rename v1\n"),
        ("engine/lib/core/util/keep.txt", b"util\n"),
        ("engine/sub/deep/remove_deep.txt", b"remove deep\n"),
        ("engine/sub/gone/remove_gone.txt", b"remove gone\n"),
        (
            "engine/sub/gone/deeper/remove_deeper.txt",
            b"remove deeper\n",
        ),
    ];

    /// What main does to `engine/`: modifies a file, adds three (one in a new
    /// directory, one below three otherwise unchanged directories) and an empty
    /// directory, deletes two files and a directory, leaving `engine/sub/deep` empty,
    /// moves a file into a new directory, and renames a file and changes its content.
    const ENGINE_EDITS: &[Edit] = &[
        Edit::Write("engine/modify.txt", b"modify v2\n"),
        Edit::Write("engine/added.txt", b"added\n"),
        Edit::Write("engine/newdir/added_deep.txt", b"added deep\n"),
        Edit::MakeDirectory("engine/emptydir"),
        Edit::Write("engine/lib/core/util/added.txt", b"added util\n"),
        Edit::Delete("engine/remove.txt"),
        Edit::Delete("engine/sub/deep/remove_deep.txt"),
        Edit::DeleteDirectory("engine/sub/gone"),
        Edit::Move("engine/move.txt", "engine/moved/move.txt"),
        Edit::Move("engine/rename.txt", "engine/renamed.txt"),
        Edit::Write("engine/renamed.txt", b"rename v2\n"),
    ];

    /// The history [`reshape_engine`] builds.
    struct ReshapedEngine {
        base: &'static [(&'static str, &'static [u8])],
        main_branch: BranchId,
        feature_rev: Hash,
        control_branch: BranchId,
    }

    /// Build the history an adoption test merges, and leave `feature` current with
    /// its working tree restored.
    ///
    /// The base holds `game/a.txt` and `base`, with a link at each of `base_links` mounting
    /// the files of `base` below it. `feature` changes the in-view `game/a.txt` only, so
    /// `engine/` matches the base and qualifies for adoption, and `control` starts at the same
    /// revision for the merge with no view. main makes `edits`.
    async fn reshape_engine(
        fixture: &DiffFixture,
        base: &'static [(&'static str, &'static [u8])],
        base_links: &[&str],
        edits: &[Edit],
    ) -> ReshapedEngine {
        fixture.write_file("game/a.txt", b"base\n");
        for (path, content) in base {
            fixture.write_file(path, content);
        }
        for path in base_links {
            fixture.stage_link(path).await;
        }
        let base_revision = fixture.stage_and_commit("base").await;
        let main_branch = fixture.main_branch_id;

        let feature_branch = fixture.create_branch("feature").await;
        fixture.write_file("game/a.txt", b"feature change\n");
        let feature_rev = fixture.stage_and_commit("feature change").await;
        let control_branch = fixture.create_branch("control").await;

        fixture.switch_to(main_branch, base_revision).await;
        fixture.write_file("game/a.txt", b"base\n");
        for edit in edits {
            match *edit {
                Edit::Write(path, content) => fixture.write_file(path, content),
                Edit::Delete(path) => fixture.delete_file(path),
                Edit::DeleteDirectory(path) => fixture.delete_dir(path),
                Edit::Move(from, to) => {
                    fixture.stage_all().await;
                    fixture.stage_move(from, to).await;
                }
                Edit::MakeDirectory(path) => {
                    std::fs::create_dir_all(fixture.repo_path.join(path))
                        .expect("Failed to create a directory");
                }
                Edit::Link(path) => fixture.stage_link(path).await,
                Edit::Unlink(path) => fixture.unlink(path).await,
                Edit::Commit => {
                    fixture
                        .stage_and_commit("main reshapes engine in steps")
                        .await;
                }
            }
        }
        fixture.stage_and_commit("main reshapes engine").await;

        let reshaped = ReshapedEngine {
            base,
            main_branch,
            feature_rev,
            control_branch,
        };
        restore_feature_tree(fixture, &reshaped, feature_branch).await;
        reshaped
    }

    /// Switch to `branch` at feature's revision, with the working tree that revision
    /// holds.
    async fn restore_feature_tree(
        fixture: &DiffFixture,
        reshaped: &ReshapedEngine,
        branch: BranchId,
    ) {
        fixture.switch_to(branch, reshaped.feature_rev).await;
        fixture.delete_dir("engine");
        fixture.write_file("game/a.txt", b"feature change\n");
        for (path, content) in reshaped.base {
            fixture.write_file(path, content);
        }
    }

    /// Merge main into control with no view, from feature's revision and working
    /// tree, and return the merged revision.
    async fn merge_main_into_control(fixture: &DiffFixture, reshaped: &ReshapedEngine) -> Hash {
        restore_feature_tree(fixture, reshaped, reshaped.control_branch).await;
        Box::pin(merge_main_under_view(
            &fixture.repository,
            fixture,
            reshaped.main_branch,
            "merge main with no view",
            false,
        ))
        .await
    }

    /// The action the delta entries in `delta` record for `path`.
    fn recorded_actions(delta: &[(String, u16, u16)], path: &str) -> Vec<u16> {
        delta
            .iter()
            .filter(|(entry, _, _)| entry == path)
            .map(|(_, action, _)| *action)
            .collect()
    }

    /// Adopting an out-of-view subtree must apply every kind of change inside
    /// it, not only modifications, and leave the tree, the delta and the file
    /// history a merge with no view leaves, whether the merge commits or is
    /// staged and committed separately.
    ///
    /// Adoption reconciles the two subtrees: it skips a child whose address
    /// already matches, overwrites one that differs, creates one the other
    /// branch added, moves one the other branch moved, and discards one the
    /// other branch deleted. [`ENGINE_EDITS`] covers each. Every directory, the root
    /// included, must carry the hash and size the merge with no view gives it,
    /// since a later diff takes a directory whose hash matches as unchanged, and
    /// the delta must list the same changes with the same flags, since revision
    /// info and file history read it. Both merges record a directory main adds as
    /// an add, empty or not.
    #[tokio::test]
    async fn merge_adopts_added_and_deleted_paths_in_an_out_of_view_subtree() {
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                for no_commit in [false, true] {
                    let fixture = DiffFixture::new().await;
                    let reshaped = reshape_engine(&fixture, ENGINE_BASE, &[], ENGINE_EDITS).await;
                    let view_repo = engine_excluded_context(&fixture);
                    let mut merged_rev = Box::pin(merge_main_under_view(
                        &view_repo,
                        &fixture,
                        reshaped.main_branch,
                        "merge main",
                        no_commit,
                    ))
                    .await;
                    if no_commit {
                        merged_rev = commit::commit_boxed(
                            view_repo.clone(),
                            &fixture.write_token,
                            CommitOptions {
                                message: "merge main".to_string(),
                                link_messages: std::collections::HashMap::new(),
                                link: None,
                                layer_messages: std::collections::HashMap::new(),
                                layer: None,
                            },
                        )
                        .await
                        .expect("Failed to commit the merge");
                    }
                    let merged = state::State::deserialize(fixture.repository.clone(), merged_rev)
                        .await
                        .expect("the merged State loads");
                    let leaked = test_leaked_slots(&merged, &fixture.repository).await;
                    assert!(
                        leaked.is_empty(),
                        "the merge under the view, no_commit {no_commit}, leaked {leaked:?}"
                    );
                    let control_rev = merge_main_into_control(&fixture, &reshaped).await;

                    assert_eq!(
                        directory_hashes(&fixture, merged_rev).await,
                        directory_hashes(&fixture, control_rev).await,
                        "the merge under the view, no_commit {no_commit}, left directories \
                         the merge with no view does not",
                    );
                    let merged_delta = delta_changes(&fixture, merged_rev).await;
                    let control_delta = delta_changes(&fixture, control_rev).await;
                    for directory in ["engine/emptydir", "engine/newdir"] {
                        for (merge, delta) in [
                            ("under the view", &merged_delta),
                            ("with no view", &control_delta),
                        ] {
                            assert_eq!(
                                recorded_actions(delta, directory),
                                vec![FileAction::Add as u16],
                                "the merge {merge}, no_commit {no_commit}, has to record the \
                                 directory {directory} main adds as an add, delta {delta:?}",
                            );
                        }
                    }
                    assert_eq!(
                        merged_delta, control_delta,
                        "the merge under the view, no_commit {no_commit}, recorded a delta \
                         the merge with no view does not",
                    );
                    for path in ["engine/moved/move.txt", "engine/renamed.txt"] {
                        assert_eq!(
                            file_history(&fixture, merged_rev, path).await,
                            file_history(&fixture, control_rev, path).await,
                            "the merge under the view, no_commit {no_commit}, recorded a \
                             history of {path} the merge with no view does not",
                        );
                    }
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A move across the boundary of an adopted subtree is recorded as an add and a
    /// delete. The diff that pairs moves sees the delete outside the subtree but not
    /// the add inside it, and the reconcile pairs only the changes inside it. The
    /// merge leaves the tree a merge with no view leaves, which records the move.
    #[tokio::test]
    async fn a_move_into_an_adopted_subtree_stays_an_add_and_a_delete() {
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;
                let reshaped = reshape_engine(
                    &fixture,
                    &[("x.txt", b"x\n"), ("engine/keep.txt", b"keep\n")],
                    &[],
                    &[Edit::Move("x.txt", "engine/x.txt")],
                )
                .await;
                let merged_rev = Box::pin(merge_main_under_view(
                    &engine_excluded_context(&fixture),
                    &fixture,
                    reshaped.main_branch,
                    "merge main",
                    false,
                ))
                .await;
                let control_rev = merge_main_into_control(&fixture, &reshaped).await;

                assert_eq!(
                    directory_hashes(&fixture, merged_rev).await,
                    directory_hashes(&fixture, control_rev).await,
                    "the merge under the view left directories the merge with no view does not",
                );
                let actions = |delta: Vec<(String, u16, u16)>| -> Vec<(String, u16)> {
                    delta
                        .into_iter()
                        .filter(|(path, _, _)| path.ends_with("x.txt"))
                        .map(|(path, action, _)| (path, action))
                        .collect()
                };
                assert_eq!(
                    actions(delta_changes(&fixture, merged_rev).await),
                    vec![
                        ("engine/x.txt".to_string(), FileAction::Add as u16),
                        ("x.txt".to_string(), FileAction::Delete as u16),
                    ],
                    "the merge under the view records the move into the subtree as an add \
                     and a delete",
                );
                assert_eq!(
                    actions(delta_changes(&fixture, control_rev).await),
                    vec![("engine/x.txt".to_string(), FileAction::Move as u16)],
                    "the merge with no view records the move",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A move out of a directory the move otherwise leaves unchanged changes that
    /// directory's hash. The merge under the view has to record the move, and leave
    /// `engine/` as main holds it, since the feature branch never touched it.
    ///
    /// A staged move leaves the moved-from directory's hash stale, so
    /// main also adds a file to that directory and deletes it in a later commit,
    /// and its own tree holds the directory's new hash. The merge with no view
    /// relinks the file the same way, and leaves the directory stale; it is not
    /// compared here.
    #[tokio::test]
    async fn a_move_out_of_an_unchanged_directory_in_an_adopted_subtree() {
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;
                let reshaped = reshape_engine(
                    &fixture,
                    &[("engine/a/x.txt", b"x\n"), ("engine/a/y.txt", b"y\n")],
                    &[],
                    &[
                        Edit::Move("engine/a/x.txt", "engine/x.txt"),
                        Edit::Write("engine/a/restage.txt", b"restage\n"),
                        Edit::Commit,
                        Edit::Delete("engine/a/restage.txt"),
                    ],
                )
                .await;
                let merged_rev = Box::pin(merge_main_under_view(
                    &engine_excluded_context(&fixture),
                    &fixture,
                    reshaped.main_branch,
                    "merge main",
                    false,
                ))
                .await;

                let merged = state::State::deserialize(fixture.repository.clone(), merged_rev)
                    .await
                    .expect("Failed to read the merged revision");
                let [_, main_rev] = merged.parents();
                assert_eq!(
                    engine_directory_hashes(&fixture, merged_rev).await,
                    engine_directory_hashes(&fixture, main_rev).await,
                    "the merge under the view left engine/ other than main holds it",
                );
                let delta = delta_changes(&fixture, merged_rev).await;
                assert_eq!(
                    recorded_actions(&delta, "engine/x.txt"),
                    vec![FileAction::Move as u16],
                    "the merge under the view has to record the move, delta {delta:?}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file main replaces with a directory inside an adopted subtree, or a directory it
    /// replaces with a file, reaches the merge as main holds it, a directory holding
    /// subdirectories included. The feature branch never touched `engine/`, so the merge leaves
    /// it as main holds it, as the merge with no view does, and records the type change in the
    /// delta as that merge does.
    #[tokio::test]
    async fn a_type_change_in_an_adopted_subtree_reaches_the_merge() {
        const FILE: &[(&str, &[u8])] =
            &[("engine/keep.txt", b"keep\n"), ("engine/thing", b"file\n")];
        const DIRECTORY: &[(&str, &[u8])] = &[
            ("engine/keep.txt", b"keep\n"),
            ("engine/thing/a.txt", b"directory\n"),
        ];
        const NESTED: &[(&str, &[u8])] = &[
            ("engine/keep.txt", b"keep\n"),
            ("engine/thing/a.txt", b"directory\n"),
            ("engine/thing/sub/b.txt", b"sub\n"),
            ("engine/thing/sub/deeper/c.txt", b"deeper\n"),
        ];
        const TO_DIRECTORY: &[Edit] = &[
            Edit::Delete("engine/thing"),
            Edit::Write("engine/thing/a.txt", b"directory\n"),
        ];
        const TO_FILE: &[Edit] = &[
            Edit::DeleteDirectory("engine/thing"),
            Edit::Write("engine/thing", b"file\n"),
        ];

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                for (case, base, edits, to_directory) in [
                    ("a file to a directory", FILE, TO_DIRECTORY, true),
                    ("a directory to a file", DIRECTORY, TO_FILE, false),
                    ("nested directories to a file", NESTED, TO_FILE, false),
                ] {
                    let fixture = DiffFixture::new().await;
                    let reshaped = reshape_engine(&fixture, base, &[], edits).await;
                    let merged_rev = Box::pin(merge_main_under_view(
                        &engine_excluded_context(&fixture),
                        &fixture,
                        reshaped.main_branch,
                        "merge main",
                        false,
                    ))
                    .await;

                    let repository = fixture.repository.clone();
                    let merged = state::State::deserialize(repository.clone(), merged_rev)
                        .await
                        .expect("Failed to read the merged revision");
                    let thing = merged
                        .find_node(repository.clone(), "engine/thing")
                        .await
                        .expect("the merge holds engine/thing");
                    assert_eq!(
                        thing.is_directory(),
                        to_directory,
                        "the merge under the view has to hold engine/thing as main does, \
                         {case}",
                    );
                    let leaked = test_leaked_slots(&merged, &repository).await;
                    assert!(
                        leaked.is_empty(),
                        "the merge under the view, {case}, leaked {leaked:?}"
                    );
                    let [_, main_rev] = merged.parents();
                    assert_eq!(
                        engine_directory_hashes(&fixture, merged_rev).await,
                        engine_directory_hashes(&fixture, main_rev).await,
                        "the merge under the view left engine/ other than main holds it, \
                         {case}",
                    );

                    let control_rev = merge_main_into_control(&fixture, &reshaped).await;
                    assert_eq!(
                        directory_hashes(&fixture, merged_rev).await,
                        directory_hashes(&fixture, control_rev).await,
                        "the merge under the view, {case}, left \
                         directories the merge with no view does not",
                    );
                    assert_eq!(
                        delta_changes(&fixture, merged_rev).await,
                        delta_changes(&fixture, control_rev).await,
                        "the merge under the view, {case}, recorded a \
                         delta the merge with no view does not",
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A link main replaces with a directory inside an adopted subtree, or a directory it replaces
    /// with a link, reaches the merge as main holds it: the node at the path is of main's kind,
    /// the link registry names the links main's does, and the merge leaves `engine/` as main
    /// holds it.
    #[tokio::test]
    async fn a_link_replaced_by_a_directory_or_the_reverse_in_an_adopted_subtree() {
        const BASE: &[(&str, &[u8])] =
            &[("engine/keep.txt", b"keep\n"), ("engine/lib/y.txt", b"y\n")];
        const TO_DIRECTORY: &[Edit] = &[
            Edit::Unlink("engine/lib"),
            Edit::Commit,
            Edit::Write("engine/lib/z.txt", b"directory\n"),
        ];
        const TO_LINK: &[Edit] = &[
            Edit::DeleteDirectory("engine/lib"),
            Edit::Commit,
            Edit::Write("engine/lib/z.txt", b"linked\n"),
            Edit::Link("engine/lib"),
        ];

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                for (case, base_links, edits, to_link) in [
                    (
                        "a link to a directory",
                        &["engine/lib"][..],
                        TO_DIRECTORY,
                        false,
                    ),
                    ("a directory to a link", &[][..], TO_LINK, true),
                ] {
                    let fixture = DiffFixture::new().await;
                    let reshaped = reshape_engine(&fixture, BASE, base_links, edits).await;
                    let merged_rev = Box::pin(merge_main_under_view(
                        &engine_excluded_context(&fixture),
                        &fixture,
                        reshaped.main_branch,
                        "merge main",
                        false,
                    ))
                    .await;

                    let repository = fixture.repository.clone();
                    let merged = state::State::deserialize(repository.clone(), merged_rev)
                        .await
                        .expect("Failed to read the merged revision");
                    let lib = merged
                        .find_node(repository.clone(), "engine/lib")
                        .await
                        .expect("the merge holds engine/lib");
                    assert_eq!(
                        lib.is_link(),
                        to_link,
                        "the merge under the view has to hold engine/lib as main does, {case}",
                    );
                    let leaked = test_leaked_slots(&merged, &repository).await;
                    assert!(leaked.is_empty(), "the merge, {case}, leaked {leaked:?}");
                    let [_, main_rev] = merged.parents();
                    assert_eq!(
                        engine_directory_hashes(&fixture, merged_rev).await,
                        engine_directory_hashes(&fixture, main_rev).await,
                        "the merge under the view left engine/ other than main holds it, {case}",
                    );
                    assert_eq!(
                        link_paths(&fixture, merged_rev).await,
                        link_paths(&fixture, main_rev).await,
                        "the merge under the view registers other links than main, {case}",
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A file main moves across a type change inside an adopted subtree keeps its identity: into
    /// the directory that replaces it, or up to replace the directory holding it. The reconcile
    /// pairs the file's delete and add into a move, from or to the path the type change takes, and
    /// the merge leaves `engine/` as main holds it and records the move, beside the delete of the
    /// directory the file replaces.
    #[tokio::test]
    async fn a_file_moved_across_a_type_change_in_an_adopted_subtree() {
        const INTO_BASE: &[(&str, &[u8])] =
            &[("engine/keep.txt", b"keep\n"), ("engine/thing", b"file\n")];
        const INTO: &[Edit] = &[
            Edit::Move("engine/thing", "engine/moving"),
            Edit::Commit,
            Edit::Move("engine/moving", "engine/thing/a.txt"),
        ];
        const UP_BASE: &[(&str, &[u8])] = &[
            ("engine/keep.txt", b"keep\n"),
            ("engine/thing/a.txt", b"moved\n"),
            ("engine/thing/b.txt", b"stays\n"),
        ];
        const UP: &[Edit] = &[
            Edit::Move("engine/thing/a.txt", "engine/moving"),
            Edit::Commit,
            Edit::DeleteDirectory("engine/thing"),
            Edit::Move("engine/moving", "engine/thing"),
        ];
        const MOVE: u16 = FileAction::Move as u16;
        const DELETE: u16 = FileAction::Delete as u16;

        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                for (case, base, edits, moved, recorded) in [
                    (
                        "into the directory replacing it",
                        INTO_BASE,
                        INTO,
                        "engine/thing/a.txt",
                        &[MOVE][..],
                    ),
                    (
                        "up to replace the directory holding it",
                        UP_BASE,
                        UP,
                        "engine/thing",
                        &[DELETE, MOVE][..],
                    ),
                ] {
                    let fixture = DiffFixture::new().await;
                    let reshaped = reshape_engine(&fixture, base, &[], edits).await;
                    let merged_rev = Box::pin(merge_main_under_view(
                        &engine_excluded_context(&fixture),
                        &fixture,
                        reshaped.main_branch,
                        "merge main",
                        false,
                    ))
                    .await;

                    let merged = state::State::deserialize(fixture.repository.clone(), merged_rev)
                        .await
                        .expect("Failed to read the merged revision");
                    let [_, main_rev] = merged.parents();
                    assert_eq!(
                        engine_directory_hashes(&fixture, merged_rev).await,
                        engine_directory_hashes(&fixture, main_rev).await,
                        "the merge under the view left engine/ other than main holds it, {case}",
                    );
                    let leaked = test_leaked_slots(&merged, &fixture.repository).await;
                    assert!(leaked.is_empty(), "the merge, {case}, leaked {leaked:?}");
                    let delta = delta_changes(&fixture, merged_rev).await;
                    assert_eq!(
                        recorded_actions(&delta, moved),
                        recorded,
                        "the merge under the view has to record the move, {case}, delta {delta:?}",
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A view that excludes everything and re-includes the root entries and one
    /// directory leaves every directory below another root directory out of
    /// view, since `!/*` matches nothing below the first level. A merge adopts
    /// such a subtree the branch never touched, as it adopts one an exclusion
    /// names.
    ///
    /// The merge applies an adopted subtree before it counts the files it
    /// realizes, so the merge under the view reports no file to update or
    /// delete, where the same merge with no view reports the three files main
    /// writes in `engine/sub` and the two it deletes. The merge under the view
    /// also has to leave the tree the merge with no view leaves, and write
    /// nothing out of view to the working tree.
    #[tokio::test]
    async fn merge_adopts_a_subtree_below_a_shallow_reinclusion() {
        let totals = Arc::new(Mutex::new(Vec::new()));
        let recorded = totals.clone();
        let execution = offline_execution(EventDispatcher::new(Some(Box::new(
            move |event: &LoreEvent| {
                if let LoreEvent::BranchMergeStartEnd(end) = event {
                    recorded
                        .lock()
                        .expect("Recorded totals poisoned")
                        .push((end.stats.file_update_total, end.stats.file_delete_total));
                }
            },
        ))))
        .await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/sub/modify.txt", b"v1\n");
                fixture.write_file("engine/sub/remove.txt", b"v1\n");
                fixture.write_file("engine/sub/deep/remove_deep.txt", b"v1\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("game/a.txt", b"feature change\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;
                let control_branch = fixture.create_branch("control").await;

                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("game/a.txt", b"base\n");
                fixture.write_file("engine/sub/modify.txt", b"v2\n");
                fixture.write_file("engine/sub/added.txt", b"added\n");
                fixture.write_file("engine/sub/newdir/added_deep.txt", b"added deep\n");
                fixture.delete_file("engine/sub/remove.txt");
                fixture.delete_file("engine/sub/deep/remove_deep.txt");
                fixture.stage_and_commit("main reshapes engine/sub").await;

                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file("game/a.txt", b"feature change\n");
                fixture.delete_dir("engine/sub");
                let merged_rev = Box::pin(merge_main_under_view(
                    &view_context(&fixture, &["*", "!/*", "!game/**"]),
                    &fixture,
                    main_branch,
                    "merge main under the view",
                    false,
                ))
                .await;
                assert_eq!(
                    std::fs::read_dir(fixture.repo_path.join("engine"))
                        .expect("engine is in view")
                        .count(),
                    0,
                    "nothing below engine is in view, so the merge writes nothing there"
                );

                fixture.switch_to(control_branch, feature_rev).await;
                fixture.write_file("engine/sub/modify.txt", b"v1\n");
                fixture.write_file("engine/sub/remove.txt", b"v1\n");
                fixture.write_file("engine/sub/deep/remove_deep.txt", b"v1\n");
                let control_rev = Box::pin(merge_main_under_view(
                    &fixture.repository,
                    &fixture,
                    main_branch,
                    "merge main with no view",
                    false,
                ))
                .await;

                execution.dispatcher.drain().await;
                assert_eq!(
                    *totals.lock().expect("Recorded totals poisoned"),
                    vec![(0, 0), (3, 2)],
                    "the merge under the view adopts engine/sub, so it realizes none of the \
                     files the merge with no view updates and deletes there"
                );

                assert_eq!(
                    directory_hashes(&fixture, merged_rev).await,
                    directory_hashes(&fixture, control_rev).await,
                    "the merge under the view left directories the merge with no view does not",
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
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

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
                let feature_rev = Box::pin(merge_main_under_view(
                    &fixture.repository,
                    &fixture,
                    other_branch,
                    "merge other",
                    false,
                ))
                .await;

                // 4. main advances a different out-of-view path.
                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("engine/ours.txt", b"v1\n");
                fixture.write_file("engine/theirs.txt", b"v2\n");
                let main_v2 = fixture.stage_and_commit("main changes engine").await;

                // 5. feature merges main under a view that excludes engine/.
                fixture.switch_to(feature_branch, feature_rev).await;
                let view_repo = engine_excluded_context(&fixture);
                let merged_rev = Box::pin(merge_main_under_view(
                    &view_repo,
                    &fixture,
                    main_branch,
                    "merge main",
                    false,
                ))
                .await;

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
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

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
                let view_repo =
                    Arc::new(fixture.repository.to_filter_context(Arc::new(view_filter)));

                Box::pin(merge_main_under_view(
                    &view_repo,
                    &fixture,
                    main_branch,
                    "merge main",
                    false,
                ))
                .await;

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

    /// A merge bringing in a rename inside a committed directory stages that directory, which the
    /// move's realize marks once it relinks the node and the merge's stage of the moved node
    /// marks again: the merge commit rehashes it, so its hash describes the file under its new
    /// name.
    #[tokio::test]
    async fn merge_rename_inside_a_directory_rehashes_the_directory() {
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let fixture = DiffFixture::new().await;

                fixture.write_file("keep.txt", b"keep v1\n");
                fixture.write_file("dir/a.txt", b"moved content\n");
                fixture.write_file("dir/other.txt", b"other\n");
                let base_revision = fixture.stage_and_commit("base").await;
                let main_branch = fixture.main_branch_id;

                let feature_branch = fixture.create_branch("feature").await;
                fixture.write_file("keep.txt", b"keep v2 on feature\n");
                let feature_rev = fixture.stage_and_commit("feature change").await;

                fixture.switch_to(main_branch, base_revision).await;
                fixture.write_file("keep.txt", b"keep v1\n");
                fixture.stage_all().await;
                fixture.stage_move("dir/a.txt", "dir/b.txt").await;
                let renamed = fixture.commit("main renames a file in dir").await;

                fixture.switch_to(feature_branch, feature_rev).await;
                fixture.write_file("keep.txt", b"keep v2 on feature\n");
                fixture.delete_file("dir/b.txt");
                fixture.write_file("dir/a.txt", b"moved content\n");

                let merged = Box::pin(merge_main_under_view(
                    &fixture.repository,
                    &fixture,
                    main_branch,
                    "merge main",
                    false,
                ))
                .await;

                let repository = fixture.repository.clone();
                let directory_hash = async |revision: Hash| {
                    let state =
                        lore_revision::state::State::deserialize(repository.clone(), revision)
                            .await
                            .expect("the revision's state");
                    assert!(
                        state
                            .find_node_link(repository.clone(), "dir/b.txt")
                            .await
                            .is_ok()
                            != state
                                .find_node_link(repository.clone(), "dir/a.txt")
                                .await
                                .is_ok(),
                        "the revision holds one of the two names"
                    );
                    let link = state
                        .find_node_link(repository.clone(), "dir")
                        .await
                        .expect("dir");
                    state
                        .node(repository.clone(), link.node)
                        .await
                        .expect("dir")
                        .address
                        .hash
                };
                let merged_state =
                    lore_revision::state::State::deserialize(repository.clone(), merged)
                        .await
                        .expect("the merged state");
                assert!(
                    merged_state
                        .find_node_link(repository.clone(), "dir/b.txt")
                        .await
                        .is_ok(),
                    "the merge holds the file under its new name"
                );
                assert_eq!(
                    directory_hash(merged).await,
                    directory_hash(renamed).await,
                    "the merge commit rehashed dir as the rename's commit hashed it"
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
                    .map(|(s, _)| s.path().as_str().to_string())
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
                    c.path().as_str() == "sub" && c.action == FileAction::Delete
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
                    c.path().as_str() == "sub/keep.txt"
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
                        vec![s.path().as_str().to_string(), t.path().as_str().to_string()]
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
        let execution = offline_execution(EventDispatcher::no_dispatch()).await;

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
                let merge_revision = Box::pin(lore_revision::branch::merge::merge_start(
                    fixture.repository.clone(),
                    &fixture.write_token,
                    target_branch,
                    lore_revision::branch::merge::MergeStartOptions {
                        message: "merge target into source".to_string(),
                        link_messages: std::collections::HashMap::new(),
                        no_commit: false,
                        scope: lore_revision::branch::merge::MergeScope::MainOnly,
                        inherit_metadata: Default::default(),
                    },
                ))
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
                    .map(|(s, _)| s.path().as_str().to_string())
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
                    .any(|c| c.path().as_str() == "shared.txt");
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
