// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Changing the view an instance materializes its working tree under, inside a layer mount.
//!
//! A layer draws a subtree of another repository into the working tree at its mount path, and the
//! instance's view decides which of it stands on disk. So a view change is work for every mount at
//! the revision the layer already holds, and a mount is carried the way the tree is: the old view on
//! the from side of the diff and the new one on the to side.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::path::PathBuf;
    use std::sync::Arc;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::branch::BranchLatestStatus;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::layer;
    use lore_revision::layer::Layer;
    use lore_revision::lore::RepositoryId;
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

    /// An instance mounting a second repository at [`MOUNT`], each on its own revision.
    struct Fixture {
        instance: TestRepository,
        /// The repository the layer draws from, mounted whole.
        source: TestRepository,
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
                        target_path: MOUNT.to_string(),
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
            std::fs::create_dir_all(self.working(MOUNTED_DROPPED_DIRECTORY))
                .expect("Create directory failed");
            test_file_write(self.working(MOUNTED_KEPT).as_path(), KEPT_CONTENT);
            test_file_write(self.working(MOUNTED_DROPPED).as_path(), DROPPED_CONTENT);
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
                    ),
                    (None, None, false),
                    "the mount's content leaves the working tree with the view that admitted it"
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
}
