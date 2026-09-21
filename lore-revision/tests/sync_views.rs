// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Changing the view an instance materializes its working tree under, through `sync`.
//!
//! A view change mints no revision and rewrites no tree: it carries the working tree from what one
//! view materialized to what another does, which is the difference of the two path sets plus the
//! content of everything in both that the revisions disagree over. So a sync holds two contexts, one
//! per view, and is asked for work at a revision that has not moved. The view file is published
//! last, after the tree and the anchor, so an interrupted apply is re-runnable.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Hash;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::revision::sync;
    use lore_revision::revision::sync::SyncError;
    use lore_revision::revision::sync::SyncOptions;

    include!("helper.rs");

    /// The file every view here admits, so a narrowing that removed it would be removing the wrong
    /// thing.
    const KEPT: &str = "keep/kept.txt";
    const KEPT_CONTENT: &[u8] = b"held by every view";
    /// What the second revision holds at [`KEPT`], which a sync to it has to write.
    const KEPT_REWRITTEN: &[u8] = b"held by every view, rewritten";
    /// The file the second revision adds, in view throughout.
    const ADDED: &str = "keep/added.txt";
    const ADDED_CONTENT: &[u8] = b"added by the second revision";
    /// The directory the narrowing view drops, and the two files in it.
    const DROPPED_DIRECTORY: &str = "drop";
    const DROPPED: &str = "drop/dropped.txt";
    const DROPPED_CONTENT: &[u8] = b"dropped by the narrowing";
    const ALSO_DROPPED: &str = "drop/also-dropped.txt";
    const ALSO_DROPPED_CONTENT: &[u8] = b"dropped by the narrowing as well";
    /// The one rule the narrowing view holds, and the file it is written back out as.
    const EXCLUDE_DROPPED: &str = "/drop";
    const NARROW_VIEW: &str = "narrow";
    /// A view file holding no rules, which puts the whole repository in view.
    const WIDE_VIEW: &str = "wide";

    /// What a sync did to the working tree, as its progress events report it.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Realized {
        files_written: usize,
        bytes_written: u64,
        files_deleted: usize,
    }

    /// The events a [`recording_execution`] folds what it sees into.
    #[derive(Default)]
    struct Recorded {
        files_written: AtomicUsize,
        bytes_written: AtomicU64,
        files_deleted: AtomicUsize,
        reported: Mutex<Vec<String>>,
    }

    impl Recorded {
        /// The largest totals that arrived, which a sync ends by sending.
        fn realized(&self) -> Realized {
            Realized {
                files_written: self.files_written.load(Ordering::Relaxed),
                bytes_written: self.bytes_written.load(Ordering::Relaxed),
                files_deleted: self.files_deleted.load(Ordering::Relaxed),
            }
        }

        /// Every path a per-file event named, with the action it carried, in order.
        fn reported(&self) -> Vec<String> {
            self.reported
                .lock()
                .expect("Recorded paths poisoned")
                .clone()
        }
    }

    /// An execution under `globals` recording every sync event sent under it.
    ///
    /// The progress totals are monotonic, so the largest of them is what the sync did; the per-file
    /// events are kept whole, since what a sync reports of a view change is a property under test.
    fn recording_execution(
        recorded: Arc<Recorded>,
        globals: LoreGlobalArgs,
    ) -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new_client_with_user_id(
            globals,
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| match event {
                LoreEvent::RevisionSyncProgress(progress) => {
                    recorded
                        .files_written
                        .fetch_max(progress.file_update, Ordering::Relaxed);
                    recorded
                        .bytes_written
                        .fetch_max(progress.bytes_update, Ordering::Relaxed);
                    recorded
                        .files_deleted
                        .fetch_max(progress.file_delete, Ordering::Relaxed);
                }
                LoreEvent::RevisionSyncFile(file) => recorded
                    .reported
                    .lock()
                    .expect("Recorded paths poisoned")
                    .push(format!("{:?} {}", file.action, file.path)),
                _ => {}
            }))),
            "test-user".to_string(),
        ))
    }

    /// A repository whose one revision holds [`KEPT`] beside the two files under
    /// [`DROPPED_DIRECTORY`], with the working tree materialized whole.
    struct Fixture {
        instance: TestRepository,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        views: TempDir,
    }

    impl Fixture {
        async fn create(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            let instance = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;

            for directory in ["keep", DROPPED_DIRECTORY] {
                std::fs::create_dir_all(instance.path.join(directory))
                    .expect("Create directory failed");
            }
            for (path, contents) in [
                (KEPT, KEPT_CONTENT),
                (DROPPED, DROPPED_CONTENT),
                (ALSO_DROPPED, ALSO_DROPPED_CONTENT),
            ] {
                test_file_write(instance.path.join(path).as_path(), contents);
            }
            test_commit_tree(&instance, "First").await;

            Self {
                instance,
                immutable_store,
                mutable_store,
                views: generate_tempdir(),
            }
        }

        /// Rewrites [`KEPT`] and adds [`ADDED`] in a second revision, answering it and leaving the
        /// working tree standing on the first.
        async fn second_revision(&self) -> Hash {
            let first = self.anchor().await;
            test_file_write(self.working(KEPT).as_path(), KEPT_REWRITTEN);
            test_file_write(self.working(ADDED).as_path(), ADDED_CONTENT);
            let second = test_commit_tree(&self.instance, "Second").await.revision();

            self.sync(
                self.instance.repository.clone(),
                SyncOptions {
                    revision: Some(first.to_string()),
                    ..Default::default()
                },
            )
            .await
            .0
            .expect("Failed to sync back to the first revision");

            second
        }

        /// A file holding `rules`, one per line, for a sync to be pointed at.
        ///
        /// Outside the working tree, since a file inside it would be part of the tree a view change
        /// carries.
        fn view_file(&self, name: &str, rules: &[&str]) -> PathBuf {
            self.view_file_bytes(name, rules.join("\n").as_bytes())
        }

        /// [`Self::view_file`] written verbatim, for a file whose bytes are the point.
        fn view_file_bytes(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.views.to_path_buf().join(name);
            test_file_write(path.as_path(), bytes);
            path
        }

        /// The instance's context as opening the repository builds it, with the view file on disk in
        /// the view slot, so a sync through it stands on the view the last one published.
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

        /// Syncs `repository` under `options`, answering its result beside what it reported.
        async fn sync(
            &self,
            repository: Arc<RepositoryContext>,
            options: SyncOptions,
        ) -> (Result<(), SyncError>, Arc<Recorded>) {
            self.sync_under(repository, options, LoreGlobalArgs::default())
                .await
        }

        /// [`Self::sync`] under `globals`, for a test whose subject is a global flag.
        async fn sync_under(
            &self,
            repository: Arc<RepositoryContext>,
            options: SyncOptions,
            globals: LoreGlobalArgs,
        ) -> (Result<(), SyncError>, Arc<Recorded>) {
            let recorded = Arc::new(Recorded::default());
            let execution = recording_execution(recorded.clone(), globals);
            let result = LORE_CONTEXT
                .scope(execution.clone(), async {
                    let result =
                        sync::sync_boxed(repository, &self.instance.write_token, options).await;
                    execution.dispatcher.drain().await;
                    result
                })
                .await;
            (result, recorded)
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

        /// The revision the instance stands on.
        async fn anchor(&self) -> Hash {
            lore_revision::instance::load_current_anchor_boxed(&self.instance.repository)
                .await
                .expect("Failed to load current anchor")
                .0
        }

        /// The staged state the instance holds, which a dirty path anchors.
        async fn staged(&self) -> Option<Hash> {
            lore_revision::instance::load_staged_revision(&self.instance.repository)
                .await
                .expect("Failed to load staged anchor")
                .filter(|revision| !revision.is_zero())
        }
    }

    /// A view change at a standing revision is work rather than the no-op a sync to the revision it
    /// already holds is: the revision the tree is materialized from has not moved, and which subset
    /// of it stands on disk has.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_change_at_a_standing_revision_narrows_the_working_tree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let before = fixture.anchor().await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);

                let (result, recorded) = fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to narrow the view");
                assert_eq!(
                    (
                        fixture.working_file(DROPPED),
                        fixture.working_file(ALSO_DROPPED),
                        fixture.working(DROPPED_DIRECTORY).exists(),
                    ),
                    (None, None, false),
                    "the target view drops the directory whole"
                );
                assert_eq!(
                    fixture.working_file(KEPT).as_deref(),
                    Some(KEPT_CONTENT),
                    "a path both views admit is left as it stands"
                );
                assert_eq!(
                    fixture.anchor().await,
                    before,
                    "a view change mints no revision and moves the anchor nowhere"
                );
                assert_eq!(
                    fixture.stored_view().as_deref(),
                    Some("/drop\n"),
                    "the view the tree now stands under is published as the instance's own"
                );
                assert_eq!(
                    recorded.realized(),
                    Realized {
                        files_written: 0,
                        bytes_written: 0,
                        files_deleted: 3,
                    },
                    "a narrowing writes nothing and removes the two files and their directory"
                );
                assert_eq!(
                    recorded.reported(),
                    Vec::<String>::new(),
                    "a successful delete reports no per-file event, which a view narrowing makes \
                     visible: the success arm of the unlink clears the flag the event is gated on"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A view change and a revision change in one sync leave the tree holding exactly what the
    /// target view materializes of the target revision, not one of the two applied to the other.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_change_carries_the_tree_to_the_target_revision_as_well() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let second = fixture.second_revision().await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);

                let (result, _recorded) = fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            revision: Some(second.to_string()),
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to sync the revision and the view together");
                assert_eq!(
                    (
                        fixture.working_file(KEPT).as_deref(),
                        fixture.working_file(ADDED).as_deref(),
                        fixture.working_file(DROPPED),
                    ),
                    (Some(KEPT_REWRITTEN), Some(ADDED_CONTENT), None),
                    "the target revision's content for what the target view admits, and nothing else"
                );
                assert_eq!(
                    fixture.anchor().await,
                    second,
                    "the revision the sync resolved is the one the instance is left on"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Widening the view restores what a narrowing removed, reading it from the store: the tree is
    /// the only place the content went, and the revision never lost it.
    ///
    /// The widening's current side is the instance reopened, so the view it stands on is the one the
    /// narrowing published rather than one the test holds in memory.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_widening_restores_what_the_narrowing_removed() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
                let wide = fixture.view_file(WIDE_VIEW, &[]);
                fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await
                    .0
                    .expect("Failed to narrow the view");

                let (result, recorded) = fixture
                    .sync(
                        fixture.reopened(),
                        SyncOptions {
                            view: Some(wide),
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to widen the view");
                assert_eq!(
                    (
                        fixture.working_file(DROPPED).as_deref(),
                        fixture.working_file(ALSO_DROPPED).as_deref(),
                    ),
                    (Some(DROPPED_CONTENT), Some(ALSO_DROPPED_CONTENT)),
                    "a path entering the view is written from the store"
                );
                assert_eq!(
                    fixture.stored_view().as_deref(),
                    Some(""),
                    "a view holding no rules is published as a file holding none"
                );
                assert_eq!(
                    recorded.realized(),
                    Realized {
                        files_written: 2,
                        bytes_written: (DROPPED_CONTENT.len() + ALSO_DROPPED_CONTENT.len()) as u64,
                        files_deleted: 0,
                    },
                    "a widening writes what enters the view and removes nothing"
                );
                assert_eq!(
                    recorded.reported(),
                    vec![
                        "Add drop".to_string(),
                        "Add drop/also-dropped.txt".to_string(),
                        "Add drop/dropped.txt".to_string(),
                    ],
                    "the directory and the files entering the view are each reported once"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Applying the view the instance already holds carries nothing: the two views admit the same
    /// paths, so the walk finds no difference to realize, whatever it had to walk to find that out.
    #[tokio::test(flavor = "multi_thread")]
    async fn applying_the_view_the_instance_already_holds_changes_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
                fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow.clone()),
                            ..Default::default()
                        },
                    )
                    .await
                    .0
                    .expect("Failed to narrow the view");

                let (result, recorded) = fixture
                    .sync(
                        fixture.reopened(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to re-apply the view the instance holds");
                assert_eq!(
                    recorded.realized(),
                    Realized::default(),
                    "two views admitting the same paths leave the tree alone"
                );
                assert_eq!(
                    (
                        fixture.working_file(KEPT).as_deref(),
                        fixture.working_file(DROPPED),
                        fixture.stored_view().as_deref(),
                    ),
                    (Some(KEPT_CONTENT), None, Some("/drop\n")),
                    "the tree and the view file are what the first apply left"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A reset and a dependency set are each refused with a view change, before anything is read for
    /// the sync: neither carries the tree to the difference between two views, and a refusal is the
    /// only answer that leaves it under a view at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_or_a_dependency_set_refuses_a_view_change() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);

                for options in [
                    SyncOptions {
                        reset: true,
                        view: Some(narrow.clone()),
                        ..Default::default()
                    },
                    SyncOptions {
                        root_files: vec![fixture.working(KEPT).to_string_lossy().into_owned()],
                        view: Some(narrow.clone()),
                        ..Default::default()
                    },
                ] {
                    let (result, recorded) = fixture
                        .sync(fixture.instance.repository.clone(), options.clone())
                        .await;

                    let error = result.expect_err("A view change is not honoured by this sync");
                    assert!(
                        error.is_invalid_arguments(),
                        "{options:?} is refused as an argument error, not attempted: {error}"
                    );
                    assert_eq!(
                        (recorded.realized(), fixture.stored_view()),
                        (Realized::default(), None),
                        "{options:?} leaves the tree and the instance's view untouched"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A view file that cannot be read or cannot be understood is refused. An absent one especially:
    /// read as the empty filter every other loader answers with, it would mean the whole repository
    /// in view and materialize it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_file_that_cannot_be_read_or_parsed_is_refused() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let views = [
                    fixture.views.to_path_buf().join("absent"),
                    // A lone continuation byte, which is no encoding this reads.
                    fixture.view_file_bytes("undecodable", &[b'/', 0x80]),
                    // An inclusion cannot be unanchored: it would put the whole tree in view to
                    // find what it re-includes.
                    fixture.view_file("malformed", &["/*", "!**/keep/kept.txt"]),
                ];

                for view in views {
                    let (result, recorded) = fixture
                        .sync(
                            fixture.instance.repository.clone(),
                            SyncOptions {
                                view: Some(view.clone()),
                                ..Default::default()
                            },
                        )
                        .await;

                    result.expect_err("A view file that cannot be read is not a view");
                    assert_eq!(
                        (
                            recorded.realized(),
                            fixture.stored_view(),
                            fixture.working_file(DROPPED).as_deref(),
                        ),
                        (Realized::default(), None, Some(DROPPED_CONTENT)),
                        "{} leaves the tree and the instance's view untouched",
                        view.display()
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// A dirty flag on a path the target view drops is not carried forward: the file it names is one
    /// the working tree no longer holds, so the flag can only be re-applied to nothing — hidden
    /// while that view holds, since a status asks the same filter, and a phantom local change to a
    /// file nothing touched once the view widens again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dirty_path_the_target_view_drops_is_not_carried_forward() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                fixture
                    .narrow_over_a_dirty_path(LoreGlobalArgs::default())
                    .await;
            }))
            .await
            .expect("Test task failed");
    }

    /// The same, forced. `--force` carries dirty flags the filter excludes, for an operation whose
    /// filter is the one they were recorded under; the filter a view change holds is the one the tree
    /// is left under, and a flag that view excludes can be re-applied to nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dirty_path_the_target_view_drops_is_not_carried_forward_even_when_forced() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                fixture
                    .narrow_over_a_dirty_path(LoreGlobalArgs {
                        force: 1,
                        ..Default::default()
                    })
                    .await;
            }))
            .await
            .expect("Test task failed");
    }

    impl Fixture {
        /// Marks [`DROPPED`] dirty without touching it, narrows the view past it under `globals`, and
        /// asserts the staged state the flag anchored is gone.
        ///
        /// The file is left as the revision holds it, so the delete the narrowing emits is verified
        /// against a file that matches its node and the flag is the only thing stale about it.
        async fn narrow_over_a_dirty_path(&self, globals: LoreGlobalArgs) {
            let narrow = self.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
            lore_revision::file::dirty::dirty(
                self.instance.repository.clone(),
                LoreArray::from_vec(vec![LoreString::from(
                    self.working(DROPPED).to_string_lossy().as_ref(),
                )]),
            )
            .await
            .expect("Failed to mark the path dirty");
            assert!(
                self.staged().await.is_some(),
                "a dirty path anchors a staged state, which is what the sync has to rebase"
            );

            let (result, _recorded) = self
                .sync_under(
                    self.instance.repository.clone(),
                    SyncOptions {
                        view: Some(narrow),
                        ..Default::default()
                    },
                    globals,
                )
                .await;

            result.expect("Failed to narrow the view");
            assert_eq!(
                (self.staged().await, self.working_file(DROPPED)),
                (None, None),
                "the flag leaves with the file it names"
            );
        }
    }

    /// A dry run reports the view change and carries none of it, the view file included. Publishing
    /// it would leave the instance naming a view its working tree was deliberately not carried to.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dry_run_reports_the_view_change_and_publishes_nothing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);

                let (result, recorded) = fixture
                    .sync_under(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                        LoreGlobalArgs {
                            dry_run: 1,
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to report the view change");
                assert_eq!(
                    recorded.realized().files_deleted,
                    3,
                    "a dry run counts the deletes it would make"
                );
                assert_eq!(
                    (
                        fixture.working_file(DROPPED).as_deref(),
                        fixture.stored_view(),
                    ),
                    (Some(DROPPED_CONTENT), None),
                    "the tree and the instance's view are left as they stand"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A narrowing that would delete a locally modified file is refused, and the refusal leaves the
    /// instance where it was: under its old view, with the local work in place. The alternative is a
    /// tree materialized under neither view, which nothing can tell from either.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_narrowing_refused_for_local_modifications_leaves_the_view_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
                let local = b"edited locally, and longer than the revision holds";
                test_file_write(fixture.working(DROPPED).as_path(), local);

                let (result, recorded) = fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await;

                let error = result.expect_err("A narrowing does not delete local work");
                assert!(
                    error.is_local_modifications(),
                    "the refusal names the local modification: {error}"
                );
                assert_eq!(
                    (
                        fixture.working_file(DROPPED).as_deref(),
                        fixture.working_file(ALSO_DROPPED).as_deref(),
                        fixture.stored_view(),
                        recorded.realized(),
                    ),
                    (
                        Some(local.as_slice()),
                        Some(ALSO_DROPPED_CONTENT),
                        None,
                        Realized::default()
                    ),
                    "nothing is carried, since the changes are verified before any of them is realized"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A widening over a local file holding exactly what the target view would write there carries
    /// on: the bytes are already in place, so the file is left alone and counted as realized.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_widening_adopts_a_local_file_holding_the_incoming_content() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                let (result, recorded) = fixture.widen_over_a_local_file(DROPPED_CONTENT).await;

                result.expect("Failed to widen the view over a file holding the incoming content");
                assert_eq!(
                    (
                        fixture.working_file(DROPPED).as_deref(),
                        fixture.working_file(ALSO_DROPPED).as_deref(),
                        fixture.stored_view().as_deref(),
                    ),
                    (Some(DROPPED_CONTENT), Some(ALSO_DROPPED_CONTENT), Some("")),
                    "the file stands where it was and the rest of the view is materialized around it"
                );
                assert_eq!(
                    recorded.realized(),
                    Realized {
                        files_written: 1,
                        bytes_written: ALSO_DROPPED_CONTENT.len() as u64,
                        files_deleted: 0,
                    },
                    "only the path holding nothing yet is written"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A widening that would overwrite a local file is refused, and the refusal leaves the instance
    /// under its old view with the local work in place. The file is the only copy of what it holds,
    /// where the content the view would write is still in the store.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_widening_refused_for_a_local_file_leaves_the_view_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let local = b"written locally while the path was out of view";

                let (result, recorded) = fixture.widen_over_a_local_file(local).await;

                let error = result.expect_err("A widening does not overwrite local work");
                assert!(
                    error.is_local_modifications(),
                    "the refusal names the local modification: {error}"
                );
                assert_eq!(
                    (
                        fixture.working_file(DROPPED).as_deref(),
                        fixture.working_file(ALSO_DROPPED),
                        fixture.stored_view().as_deref(),
                        recorded.realized(),
                    ),
                    (
                        Some(local.as_slice()),
                        None,
                        Some("/drop\n"),
                        Realized::default()
                    ),
                    "nothing is carried and the instance stands under the view it was left under"
                );
            }))
            .await
            .expect("Test task failed");
    }

    impl Fixture {
        /// Narrows past [`DROPPED_DIRECTORY`], writes `local` at [`DROPPED`] by hand, and widens the
        /// view back to the whole repository, answering what the widening did.
        ///
        /// No view the instance stood under admitted the file, so nothing tracks it and nothing has
        /// its content: what the widening does with it is what a sync does with an incoming file
        /// whose path the working tree has already taken.
        async fn widen_over_a_local_file(
            &self,
            local: &[u8],
        ) -> (Result<(), SyncError>, Arc<Recorded>) {
            let narrow = self.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
            let wide = self.view_file(WIDE_VIEW, &[]);
            self.sync(
                self.instance.repository.clone(),
                SyncOptions {
                    view: Some(narrow),
                    ..Default::default()
                },
            )
            .await
            .0
            .expect("Failed to narrow the view");

            std::fs::create_dir_all(self.working(DROPPED_DIRECTORY))
                .expect("Create directory failed");
            test_file_write(self.working(DROPPED).as_path(), local);

            self.sync(
                self.reopened(),
                SyncOptions {
                    view: Some(wide),
                    ..Default::default()
                },
            )
            .await
        }
    }

    /// A directory the view drops survives while it still holds a file of its own, along with the
    /// file: the unlink of a non-empty directory fails and the narrowing keeps what it could not
    /// remove rather than taking local work with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_directory_holding_a_local_file_survives_the_narrowing() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let narrow = fixture.view_file(NARROW_VIEW, &[EXCLUDE_DROPPED]);
                // Named with the extension the ignore filter carries a rule for, so nothing reports
                // it and nothing would stage it either.
                let local = format!(
                    "{DROPPED_DIRECTORY}/local{}",
                    lore_revision::repository::TEMP_FILE_EXTENSION
                );
                test_file_write(fixture.working(&local).as_path(), b"local work");

                let (result, _recorded) = fixture
                    .sync(
                        fixture.instance.repository.clone(),
                        SyncOptions {
                            view: Some(narrow),
                            ..Default::default()
                        },
                    )
                    .await;

                result.expect("Failed to narrow the view");
                assert_eq!(
                    (
                        fixture.working_file(DROPPED),
                        fixture.working_file(ALSO_DROPPED),
                        fixture.working_file(&local).as_deref(),
                    ),
                    (None, None, Some(b"local work".as_slice())),
                    "the tracked files leave the view and the local file stays"
                );
                assert!(
                    fixture.working(DROPPED_DIRECTORY).is_dir(),
                    "the directory is kept for the file it still holds"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
