// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Changing the view an instance materializes its working tree under, inside a layer mount.
//!
//! A layer draws a subtree of another repository into the working tree at its mount path, and the
//! instance's view decides which of it stands on disk. So a view change is work for every mount at
//! the revision the layer already holds, and a mount is carried the way the tree is: the old view on
//! the from side of the diff and the new one on the to side.
//!
//! A reset that purges leaves a mount to the layer's own walk in the same way: the instance's walk
//! deletes what its revision lacks around a mount and nothing inside it.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::BranchPoint;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::BranchLatestStatus;
    use lore_revision::file::reset;
    use lore_revision::file::reset::ResetMergeSide;
    use lore_revision::file::reset::ResetOptions;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreFileAction;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::layer;
    use lore_revision::layer::Layer;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::revision::sync;
    use lore_revision::revision::sync::SyncError;
    use lore_revision::revision::sync::SyncOptions;
    use serde::Serialize;

    include!("helper.rs");

    /// Where the instance mounts the layer.
    const MOUNT: &str = "mount";
    /// The layer repository's own spelling of what it holds, which the mount re-spells under
    /// [`MOUNT`].
    const SOURCE_KEPT: &str = "kept.txt";
    const SOURCE_DROPPED_DIRECTORY: &str = "drop";
    const SOURCE_DROPPED: &str = "drop/dropped.txt";
    /// The file staged in the layer repository, which a layer holding staged content pins. Below
    /// the layer's root, since that is the staged state the guard has to reach into.
    const SOURCE_STAGED: &str = "drop/staged.txt";
    /// The mounted spelling of the same files, which is what every view here is asked about.
    const MOUNTED_KEPT: &str = "mount/kept.txt";
    const MOUNTED_DROPPED_DIRECTORY: &str = "mount/drop";
    const MOUNTED_DROPPED: &str = "mount/drop/dropped.txt";
    const KEPT_CONTENT: &[u8] = b"drawn by the layer, in view throughout";
    const DROPPED_CONTENT: &[u8] = b"drawn by the layer, dropped by the narrowing";
    /// The instance's own file, outside the mount, which no view here drops.
    const OWN: &str = "own.txt";
    const OWN_CONTENT: &[u8] = b"held by the instance itself";
    /// What the instance's second revision adds, for a sync that moves its own tree alone.
    const SECOND: &str = "second.txt";
    const SECOND_CONTENT: &[u8] = b"added by the instance's second revision";
    /// The rule taking the mount whole out of view, and the one taking a subtree of it.
    const EXCLUDE_MOUNT: &str = "/mount";
    const EXCLUDE_MOUNTED_DIRECTORY: &str = "/mount/drop";
    const NARROW_VIEW: &str = "narrow";
    /// A view file holding no rules, which puts the whole repository in view.
    const WIDE_VIEW: &str = "wide";
    /// A mount two levels below a directory the instance's revision does not hold.
    const MOUNT_BELOW_UNTRACKED: &str = "a/b/mount";
    /// The name of a file no revision holds, written into each directory a purge is to empty.
    const UNTRACKED: &str = "junk.txt";
    const UNTRACKED_CONTENT: &[u8] = b"untracked";

    /// What a reset reports deleting and writing.
    ///
    /// The reset sends a Delete event for each path it is asked to reset that the revision lacks,
    /// and counts each untracked child it purges from a directory it lists without one.
    #[derive(Debug, Default, PartialEq)]
    struct Purged {
        /// The paths of the reset's Delete events, sorted.
        deleted: Vec<String>,
        /// The directories and the files the reset counts deleted.
        deletes: (u64, u64),
        /// The files the reset counts written, which a layer file left in place is not.
        written: u64,
    }

    /// An instance mounting a second repository, each on its own revision.
    struct Fixture {
        instance: TestRepository,
        /// The repository the layer draws from, mounted whole.
        source: TestRepository,
        /// Where the instance mounts the layer: [`MOUNT`], or the path
        /// [`Self::create_mounted_at`] names.
        mount: &'static str,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        views: TempDir,
        /// The revision the layer holds, which no view change moves.
        layer_revision: Hash,
    }

    impl Fixture {
        async fn create(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::create_mounted_at(immutable_store, mutable_store, MOUNT).await
        }

        /// [`Self::create`], with the layer mounted at `mount` in place of [`MOUNT`].
        async fn create_mounted_at(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
            mount: &'static str,
        ) -> Self {
            let source = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            std::fs::create_dir_all(source.path.join(SOURCE_DROPPED_DIRECTORY))
                .expect("Create directory failed");
            test_file_write(source.path.join(SOURCE_KEPT).as_path(), KEPT_CONTENT);
            test_file_write(source.path.join(SOURCE_DROPPED).as_path(), DROPPED_CONTENT);
            let layer_revision = test_commit_tree(&source, "Layer content").await.revision();

            let instance = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            test_file_write(instance.path.join(OWN).as_path(), OWN_CONTENT);
            test_commit_tree(&instance, "First").await;

            // The layer repository's latest on the instance's branch, which is where a sync resolves
            // the revision to carry the layer to.
            let (_revision, instance_branch) =
                lore_revision::instance::load_current_anchor_boxed(&instance.repository)
                    .await
                    .expect("Failed to load current anchor");
            branch::store_latest(
                source.repository.clone(),
                instance_branch,
                Hash::default(),
                layer_revision,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to store the layer repository branch latest");

            let fixture = Self {
                instance,
                source,
                mount,
                immutable_store,
                mutable_store,
                views: generate_tempdir(),
                layer_revision,
            };
            fixture.write_layer_config(Hash::default()).await;
            fixture
        }

        /// Writes the instance's layer set, holding the one layer these tests mount.
        ///
        /// Written rather than added through [`layer::add`], which resolves the layer repository
        /// through the instance's remote and these fixtures have none.
        async fn write_layer_config(&self, staged: Hash) {
            #[derive(Serialize)]
            struct LayerConfig {
                layers: Vec<Layer>,
            }

            lore_revision::util::config::save(
                &LayerConfig {
                    layers: vec![Layer {
                        target_path: self.mount.to_string(),
                        source_path: String::new(),
                        repository: self.source.repository.id,
                        metadata: None,
                        current: self.layer_revision,
                        staged,
                    }],
                },
                layer::layer_config_path(&self.instance.repository).expect("Layer config path"),
            )
            .await
            .expect("Failed to write the layer set");
        }

        /// The one layer the instance mounts, as its config holds it.
        async fn layer(&self) -> Layer {
            layer::list(self.instance.repository.clone())
                .await
                .expect("Failed to read the layer set")
                .pop()
                .expect("The fixture configures one layer")
        }

        /// Commits a second revision in the instance, answering the revision it stood on before.
        ///
        /// The layer repository's branch latest is restored afterwards, and the layer set has to be
        /// written again after this: an instance commit carries its layers along, advancing the
        /// latest a sync resolves the layer's revision from and rewriting the pin the set holds.
        async fn second_revision(&self) -> Hash {
            let (first, branch) =
                lore_revision::instance::load_current_anchor_boxed(&self.instance.repository)
                    .await
                    .expect("Failed to load current anchor");
            test_file_write(self.working(SECOND).as_path(), SECOND_CONTENT);
            test_commit_tree(&self.instance, "Second").await;

            let advanced = branch::load_latest(self.source.repository.clone(), branch)
                .await
                .unwrap_or_default();
            branch::store_latest(
                self.source.repository.clone(),
                branch,
                advanced,
                self.layer_revision,
                BranchLatestStatus::Convergent,
            )
            .await
            .expect("Failed to restore the layer repository branch latest");

            first
        }

        /// Stages a file in the layer repository without committing it, answering the staged state a
        /// layer pins while it holds staged content.
        async fn stage_in_source(&self) -> Hash {
            let path = self.source.path.join(SOURCE_STAGED);
            test_file_write(path.as_path(), b"staged in the layer repository");
            lore_revision::file::stage::stage(
                self.source.repository.clone(),
                &self.source.write_token,
                LoreArray::from_vec(vec![LoreString::from(&path)]),
                lore_revision::stage::StageOptions::default(),
            )
            .await
            .expect("Failed to stage in the layer repository");

            lore_revision::instance::load_staged_revision(&self.source.repository)
                .await
                .expect("Failed to load the layer repository staged anchor")
                .filter(|revision| !revision.is_zero())
                .expect("A staged file anchors a staged state")
        }

        /// Writes the mount's files into the working tree, as a view admitting them leaves it.
        ///
        /// Written directly, so a test carrying a mount out of view does not stand on the path that
        /// carries one in.
        fn materialize_mount(&self) {
            let mount = self.working(self.mount);
            std::fs::create_dir_all(mount.join(SOURCE_DROPPED_DIRECTORY))
                .expect("Create directory failed");
            test_file_write(mount.join(SOURCE_KEPT).as_path(), KEPT_CONTENT);
            test_file_write(mount.join(SOURCE_DROPPED).as_path(), DROPPED_CONTENT);
        }

        /// Creates a branch off the revision the instance is on, moves the instance onto it and
        /// commits [`SECOND`] there, answering the branch it was created from. That branch was last
        /// merged into the new one at its branch point, a revision that lacks the file.
        ///
        /// Created through [`branch::create`], since [`branch::create::create`] creates the branch
        /// in the layer repository as well, which needs branch metadata for the layer's branch that
        /// the instance does not hold.
        async fn branch_off(&self) -> BranchId {
            let (revision, default_branch) =
                lore_revision::instance::load_current_anchor_boxed(&self.instance.repository)
                    .await
                    .expect("Failed to load current anchor");
            let feature = BranchId::from(uuid::Uuid::now_v7());
            branch::create(
                self.instance.repository.clone(),
                &self.instance.write_token,
                feature,
                "feature",
                branch::default_category(),
                "test-user",
                1,
                vec![BranchPoint {
                    branch: default_branch,
                    revision,
                }],
                false,
                false,
            )
            .await
            .expect("Failed to create the branch");
            lore_revision::instance::store_current_anchor_branch(
                &self.instance.repository,
                feature,
            )
            .await
            .expect("Failed to move the instance onto the branch");

            let second = self.working(SECOND);
            test_file_write(second.as_path(), SECOND_CONTENT);
            lore_revision::file::stage::stage(
                self.instance.repository.clone(),
                &self.instance.write_token,
                LoreArray::from_vec(vec![LoreString::from(&second)]),
                lore_revision::stage::StageOptions::default(),
            )
            .await
            .expect("Failed to stage the branch's file");
            test_commit(&self.instance, "Second").await;
            default_branch
        }

        /// Resets `paths`, relative to the working tree's root, with a purge: to the revision the
        /// instance is on, or to the point each path was last merged from `last_merged_from`.
        /// Answers what the reset reports deleting and writing.
        ///
        /// The branch is named by its id, since the instance holds no name for its default branch.
        async fn reset_purging(
            &self,
            paths: &[&str],
            last_merged_from: Option<BranchId>,
        ) -> Purged {
            self.reset_reporting(paths, last_merged_from, true).await
        }

        /// Resets `paths` as [`Self::reset_purging`] does, purging where `purge` says so.
        async fn reset_reporting(
            &self,
            paths: &[&str],
            last_merged_from: Option<BranchId>,
            purge: bool,
        ) -> Purged {
            let paths = LoreArray::from_vec(
                paths
                    .iter()
                    .map(|path| LoreString::from(&self.working(path)))
                    .collect(),
            );
            let options = ResetOptions {
                purge,
                ..Default::default()
            };
            let purged = Arc::new(Mutex::new(Purged::default()));
            let recorded = purged.clone();
            let execution = Arc::new(ExecutionContext::new_client_with_user_id(
                LoreGlobalArgs::default(),
                EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                    let mut purged = recorded.lock().expect("Failed to record the reset");
                    match event {
                        LoreEvent::FileResetFile(file) if file.action == LoreFileAction::Delete => {
                            purged.deleted.push(file.path.to_string());
                        }
                        LoreEvent::FileResetEnd(end) => {
                            purged.deletes = (
                                end.count.directory_delete_count,
                                end.count.file_delete_count,
                            );
                            purged.written = end.count.file_reset_count;
                        }
                        _ => {}
                    }
                }))),
                "test-user".to_string(),
            ));
            LORE_CONTEXT
                .scope(execution.clone(), async {
                    match last_merged_from {
                        Some(branch_id) => {
                            reset::reset_to_last_merged(
                                self.instance.repository.clone(),
                                paths,
                                LoreString::from(branch_id.to_string().as_str()),
                                ResetMergeSide::Resolved,
                                options,
                            )
                            .await
                        }
                        None => {
                            reset::reset(
                                self.instance.repository.clone(),
                                &self.instance.write_token,
                                paths,
                                LoreString::default(),
                                options,
                            )
                            .await
                        }
                    }
                    .expect("Failed to reset");
                    execution.dispatcher.drain().await;
                })
                .await;

            let mut purged = std::mem::take(&mut *purged.lock().expect("Failed to read the reset"));
            purged.deleted.sort();
            purged
        }

        /// Writes the view the instance holds, which the next [`Self::reopened`] reads.
        fn set_view(&self, rules: &[&str]) {
            test_file_write(
                self.instance
                    .path
                    .join(lore_revision::repository::DOT_LORE)
                    .join(lore_revision::repository::VIEW_FILTER)
                    .as_path(),
                format!("{}\n", rules.join("\n")).as_bytes(),
            );
        }

        /// A file holding `rules`, one per line, for a sync to be pointed at.
        ///
        /// Outside the working tree, since a file inside it would be part of the tree a view change
        /// carries.
        fn view_file(&self, name: &str, rules: &[&str]) -> PathBuf {
            let path = self.views.to_path_buf().join(name);
            test_file_write(path.as_path(), rules.join("\n").as_bytes());
            path
        }

        /// The instance's context as opening the repository builds it, with the view file on disk in
        /// the view slot, so a sync through it stands on the view the instance holds.
        fn reopened(&self) -> Arc<RepositoryContext> {
            let filter = lore_revision::repository::load_filter(self.instance.path.as_path())
                .expect("Failed to load the instance filter");
            Arc::new(
                RepositoryContext::new(
                    default_repository_creation_args(
                        self.immutable_store.clone(),
                        self.mutable_store.clone(),
                    )
                    .with_path(&self.instance.path)
                    .with_id(self.instance.repository.id)
                    .with_instance_id(self.instance.repository.instance_id)
                    .with_filter(filter),
                )
                .with_write_token(self.instance.write_token.share()),
            )
        }

        /// Syncs the instance under `options`.
        async fn sync(&self, options: SyncOptions) -> Result<(), SyncError> {
            sync::sync_boxed(self.reopened(), &self.instance.write_token, options).await
        }

        /// Syncs the instance under `options`, answering the largest delete count and delete total
        /// its progress events report.
        async fn sync_counting_deletes(&self, options: SyncOptions) -> (usize, usize) {
            let deletes = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
            let recorded = deletes.clone();
            let execution = Arc::new(ExecutionContext::new_client_with_user_id(
                LoreGlobalArgs::default(),
                EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::RevisionSyncProgress(progress) = event {
                        recorded
                            .0
                            .fetch_max(progress.file_delete, Ordering::Relaxed);
                        recorded
                            .1
                            .fetch_max(progress.file_delete_total, Ordering::Relaxed);
                    }
                }))),
                "test-user".to_string(),
            ));
            LORE_CONTEXT
                .scope(execution.clone(), async {
                    self.sync(options).await.expect("Failed to sync");
                    execution.dispatcher.drain().await;
                })
                .await;
            (
                deletes.0.load(Ordering::Relaxed),
                deletes.1.load(Ordering::Relaxed),
            )
        }

        /// The absolute path of a working tree path.
        fn working(&self, path: &str) -> PathBuf {
            self.instance.path.join(path)
        }

        /// What the working tree holds at `path`, or `None` where it holds nothing.
        fn working_file(&self, path: &str) -> Option<Vec<u8>> {
            std::fs::read(self.working(path)).ok()
        }

        /// The rules the instance's view file holds, or `None` where it has none.
        fn stored_view(&self) -> Option<String> {
            std::fs::read_to_string(
                self.instance
                    .path
                    .join(lore_revision::repository::DOT_LORE)
                    .join(lore_revision::repository::VIEW_FILTER),
            )
            .ok()
        }
    }

    /// A mount entering the view is materialized from the layer repository's store, at the revision
    /// the layer already holds: the layer is enqueued because the view moved, not because it did.
    ///
    /// Asked with one view on both sides this finds nothing to do, since the two revisions it would
    /// compare are the same one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mount_entering_the_view_is_materialized() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.set_view(&[EXCLUDE_MOUNT]);
                let wide = fixture.view_file(WIDE_VIEW, &[]);

                fixture
                    .sync(SyncOptions {
                        view: Some(wide),
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to widen the view over the mount");

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working_file(MOUNTED_DROPPED).as_deref(),
                    ),
                    (Some(KEPT_CONTENT), Some(DROPPED_CONTENT)),
                    "the mount's content is written under the path the layer is mounted at"
                );
                assert_eq!(
                    fixture.layer().await.current,
                    fixture.layer_revision,
                    "the layer is carried to the view alone, at the revision it holds"
                );
                assert_eq!(
                    fixture.stored_view().as_deref(),
                    Some(""),
                    "the view the tree now stands under is published as the instance's own"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A provider that holds its state in a `State` refuses a sync carrying the mount, before the
    /// working tree or the view moves: it holds no files for the layer to be written into.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_holding_its_state_refuses_a_sync_carrying_a_layer() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_MOUNT]);
                let view = fixture.stored_view();

                let (holding, _) = test_holding_context(&fixture.reopened());
                let result = sync::sync_boxed(
                    holding,
                    &fixture.instance.write_token,
                    SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    },
                )
                .await;

                assert!(
                    matches!(result, Err(SyncError::InvalidArguments(_))),
                    "the sync is refused, got {result:?}"
                );
                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_DROPPED).as_deref(),
                        fixture.stored_view()
                    ),
                    (Some(DROPPED_CONTENT), view),
                    "the mount and the view stay as they were"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A mount leaving the view has its content removed, and nothing outside the mount with it.
    ///
    /// The mount path itself is left behind as an empty directory: it is no node of the instance's
    /// own revision, so the walk that carries the mount is rooted at it and has no pair to route.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mount_leaving_the_view_is_removed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_MOUNT]);

                fixture
                    .sync(SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to narrow the view past the mount");

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT),
                        fixture.working_file(MOUNTED_DROPPED),
                        fixture.working(MOUNTED_DROPPED_DIRECTORY).exists(),
                        fixture.working(MOUNT).is_dir(),
                    ),
                    (None, None, false, true),
                    "the mount's content leaves the working tree with the view that admitted it, and \
                     the mount directory stays"
                );
                assert_eq!(
                    fixture.working_file(OWN).as_deref(),
                    Some(OWN_CONTENT),
                    "the instance's own tree is left as it stands"
                );
                assert_eq!(
                    fixture.layer().await.current,
                    fixture.layer_revision,
                    "the layer is carried to the view alone, at the revision it holds"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A narrowing to a view whose re-inclusion opens with a wildcard, so a walk enters each
    /// directory below the mount, removes the excluded directory it empties there and keeps the
    /// mount directory, which it empties too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_narrowing_removes_the_excluded_directories_it_empties_in_the_mount() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let narrow = fixture.view_file(NARROW_VIEW, &["/*", "!*/drop/absent.txt"]);

                fixture
                    .sync(SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to narrow the view inside the mount");

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT),
                        fixture.working(MOUNTED_DROPPED_DIRECTORY).exists(),
                        fixture.working(MOUNT).is_dir(),
                    ),
                    (None, false, true),
                    "the emptied directory leaves the mount, and the mount directory stays"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A narrowing inside the mount to a view whose re-inclusion opens with a wildcard counts the
    /// directory it leaves empty and removes as a delete, done and in total, beside the file it
    /// deletes there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_narrowing_counts_the_directory_it_removes_in_the_mount_as_a_delete() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let narrow =
                    fixture.view_file(NARROW_VIEW, &["/*", "!*/kept.txt", "!*/drop/Saved/Config/"]);

                let deletes = fixture
                    .sync_counting_deletes(SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    })
                    .await;

                assert_eq!(
                    (
                        fixture.working(MOUNTED_DROPPED_DIRECTORY).exists(),
                        fixture.working(MOUNTED_KEPT).is_file(),
                        deletes,
                    ),
                    (false, true, (2, 2)),
                    "the file and the directory it empties are both counted, done and in total"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A view moving below the mount is applied inside it: the subtree the new view drops leaves and
    /// the rest of the mount stays.
    ///
    /// This is the shape a prune would swallow. The mount's content is unchanged between the two
    /// sides, so a walk reading one view would take the whole subtree as matching and never descend.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_moving_below_the_mount_is_applied_inside_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_MOUNTED_DIRECTORY]);

                fixture
                    .sync(SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to narrow the view below the mount");

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working_file(MOUNTED_DROPPED),
                        fixture.working(MOUNTED_DROPPED_DIRECTORY).exists(),
                    ),
                    (Some(KEPT_CONTENT), None, false),
                    "the subtree the view drops leaves the mount and the rest of it stays"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A layer holding staged content blocks a view change, which would otherwise delete the files
    /// that content was staged over. The pin lives in the layer set rather than in the instance
    /// anchor, so the instance's own staged check answers nothing about it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_staged_layer_blocks_a_view_change() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let staged = fixture.stage_in_source().await;
                fixture.write_layer_config(staged).await;
                fixture.materialize_mount();
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_MOUNT]);

                let error = fixture
                    .sync(SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    })
                    .await
                    .expect_err("A view change does not discard a layer's staged content");

                assert!(
                    error.is_invalid_arguments(),
                    "the refusal names the staged layer: {error}"
                );
                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working_file(MOUNTED_DROPPED).as_deref(),
                        fixture.stored_view(),
                    ),
                    (Some(KEPT_CONTENT), Some(DROPPED_CONTENT), None),
                    "the refusal leaves the mount and the instance's view as they stand"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The same layer does not block a sync that leaves its mount where it stands. A sync carrying
    /// only the instance's own tree between revisions has no work for a layer at an unmoved revision
    /// under an unmoved view, so the staged content it holds is not at stake.
    ///
    /// The instance's second revision is committed before the layer is pinned, since a commit takes
    /// a layer's staged content with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_staged_layer_does_not_block_a_sync_that_leaves_the_mount_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let first_revision = fixture.second_revision().await;
                let staged = fixture.stage_in_source().await;
                fixture.write_layer_config(staged).await;

                fixture
                    .sync(SyncOptions {
                        revision: Some(first_revision.to_string()),
                        ..Default::default()
                    })
                    .await
                    .expect("Failed to carry the instance's own tree between revisions");

                assert_eq!(
                    fixture.working_file(SECOND),
                    None,
                    "the instance's own tree is carried to the revision the sync resolved"
                );
                let layer = fixture.layer().await;
                assert_eq!(
                    (layer.current, layer.staged),
                    (fixture.layer_revision, staged),
                    "the layer is left as it stands, staged pin included"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Purges `named` in an instance mounting the layer at [`MOUNT_BELOW_UNTRACKED`], with an
    /// untracked file in each directory on the way to the mount and in it, and asserts that the
    /// purge deletes those files alone and leaves the layer's file in place.
    async fn assert_a_purge_around_a_mount_below_an_untracked_directory(named: &'static str) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let kept = format!("{MOUNT_BELOW_UNTRACKED}/{SOURCE_KEPT}");
                let untracked = ["a", "a/b", MOUNT_BELOW_UNTRACKED]
                    .map(|directory| format!("{directory}/{UNTRACKED}"));
                let fixture = Fixture::create_mounted_at(
                    immutable_store,
                    mutable_store,
                    MOUNT_BELOW_UNTRACKED,
                )
                .await;
                fixture.materialize_mount();
                for path in &untracked {
                    test_file_write(fixture.working(path).as_path(), UNTRACKED_CONTENT);
                }

                let purged = fixture.reset_purging(&[named], None).await;

                let untracked_left: Vec<&String> = untracked
                    .iter()
                    .filter(|path| fixture.working(path).exists())
                    .collect();
                assert_eq!(
                    (untracked_left, fixture.working_file(&kept).as_deref()),
                    (Vec::<&String>::new(), Some(KEPT_CONTENT)),
                    "a purge of {named:?} deletes the untracked files on the way to the mount and \
                     inside it, and keeps the layer's file"
                );
                assert_eq!(
                    purged,
                    Purged {
                        deleted: Vec::new(),
                        deletes: (0, untracked.len() as u64),
                        written: 0,
                    },
                    "a purge of {named:?} deletes the untracked files alone: no directory on the \
                     way to the mount, and no file the layer holds, which it would write back"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A purge of the root deletes the untracked files on the way to a mount two levels below an
    /// untracked directory and inside it, and leaves the layer's file in place.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_of_the_root_deletes_only_the_untracked_files_around_a_mount() {
        assert_a_purge_around_a_mount_below_an_untracked_directory("").await;
    }

    /// A purge of an untracked directory a layer is mounted two levels below deletes the untracked
    /// files on the way to the mount and inside it, and leaves the layer's file in place.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_of_an_untracked_directory_above_a_mount_deletes_only_the_untracked_files() {
        assert_a_purge_around_a_mount_below_an_untracked_directory("a").await;
    }

    /// A purge to the point the root was last merged from a branch walks no layer, so it leaves the
    /// mount's content as it is and purges the rest of the working tree.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_last_merged_purge_leaves_the_mount_and_purges_the_rest() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let default_branch = fixture.branch_off().await;
                test_file_write(fixture.working(UNTRACKED).as_path(), UNTRACKED_CONTENT);

                let purged = fixture.reset_purging(&[""], Some(default_branch)).await;

                assert_eq!(
                    (
                        fixture.working(UNTRACKED).exists(),
                        fixture.working(SECOND).exists(),
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working_file(MOUNTED_DROPPED).as_deref(),
                    ),
                    (false, false, Some(KEPT_CONTENT), Some(DROPPED_CONTENT)),
                    "the purge deletes the untracked file and the file the branch point lacks, and \
                     leaves the mount's content"
                );
                assert_eq!(
                    purged,
                    Purged {
                        deleted: Vec::new(),
                        deletes: (0, 2),
                        written: 0,
                    },
                    "the purge deletes those two files alone, and no directory"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A purge to the point the root was last merged from a branch walks no layer, so it leaves a
    /// file standing at the mount path as it is, as it leaves the mount's content.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_last_merged_purge_leaves_a_file_at_the_mount_path() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let default_branch = fixture.branch_off().await;
                test_file_write(fixture.working(MOUNT).as_path(), UNTRACKED_CONTENT);

                let purged = fixture.reset_purging(&[""], Some(default_branch)).await;

                assert_eq!(
                    (
                        fixture.working_file(MOUNT).as_deref(),
                        fixture.working(SECOND).exists(),
                    ),
                    (Some(UNTRACKED_CONTENT), false),
                    "the purge deletes the file the branch point lacks and leaves the file at \
                     the mount path"
                );
                assert_eq!(
                    purged,
                    Purged {
                        deleted: Vec::new(),
                        deletes: (0, 1),
                        written: 0,
                    },
                    "the purge deletes the one file the branch point lacks alone"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Resets the root, purging where `purge` says so, where a file stands at the mount path, and
    /// asserts that the layer's walk deletes the file and writes the layer's files back.
    async fn assert_a_reset_puts_the_layer_back_where_a_file_stands_at_the_mount_path(purge: bool) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                test_file_write(fixture.working(MOUNT).as_path(), UNTRACKED_CONTENT);

                let reset = fixture.reset_reporting(&[""], None, purge).await;

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working_file(MOUNTED_DROPPED).as_deref(),
                    ),
                    (Some(KEPT_CONTENT), Some(DROPPED_CONTENT)),
                    "purge {purge}: the reset puts the layer's files back at the mount path"
                );
                assert_eq!(
                    reset,
                    Purged {
                        deleted: vec![MOUNT.to_string()],
                        deletes: (0, 1),
                        written: 2,
                    },
                    "purge {purge}: the layer's walk deletes the file at the mount path and \
                     writes the layer's two files"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A purge of the root walks the layer, which puts its content back where a file stands at the
    /// mount path.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_puts_the_layer_back_where_a_file_stands_at_the_mount_path() {
        assert_a_reset_puts_the_layer_back_where_a_file_stands_at_the_mount_path(true).await;
    }

    /// A reset of the root without a purge walks the layer as well, which puts its content back
    /// where a file stands at the mount path.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_puts_the_layer_back_where_a_file_stands_at_the_mount_path() {
        assert_a_reset_puts_the_layer_back_where_a_file_stands_at_the_mount_path(false).await;
    }

    /// Purges a layer's file and an untracked file in its mount, on a branch off the default one:
    /// to the point they were last merged from the default branch where `last_merged` says so, and
    /// to the revision the instance is on otherwise. Asserts that the layer's file is left in
    /// place, and the untracked file deleted only where the purge walks the layer.
    async fn assert_a_purge_of_paths_in_a_mount(last_merged: bool) {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let untracked = format!("{MOUNT}/{UNTRACKED}");
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                fixture.materialize_mount();
                let default_branch = fixture.branch_off().await;
                test_file_write(fixture.working(&untracked).as_path(), UNTRACKED_CONTENT);

                let purged = fixture
                    .reset_purging(
                        &[MOUNTED_KEPT, &untracked],
                        last_merged.then_some(default_branch),
                    )
                    .await;

                assert_eq!(
                    (
                        fixture.working_file(MOUNTED_KEPT).as_deref(),
                        fixture.working(&untracked).exists(),
                    ),
                    (Some(KEPT_CONTENT), last_merged),
                    "last merged {last_merged}: the purge keeps the layer's file, and the \
                     untracked file only where it walks no layer"
                );
                let deleted = if last_merged {
                    Vec::new()
                } else {
                    vec![untracked.clone()]
                };
                assert_eq!(
                    purged,
                    Purged {
                        deletes: (0, deleted.len() as u64),
                        deleted,
                        written: 0,
                    },
                    "last merged {last_merged}: the purge deletes the untracked file alone, where \
                     it deletes anything, and leaves the layer's file in place"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A purge to the point the paths were last merged from a branch walks no layer, so naming a
    /// layer's file and an untracked file in its mount leaves both as they are.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_last_merged_purge_of_paths_in_a_mount_leaves_them_as_they_are() {
        assert_a_purge_of_paths_in_a_mount(true).await;
    }

    /// A purge naming a layer's file and an untracked file in its mount has the layer's own walk
    /// delete the untracked file, and leaves the layer's file in place.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_purge_of_paths_in_a_mount_deletes_only_the_untracked_one() {
        assert_a_purge_of_paths_in_a_mount(false).await;
    }
}
