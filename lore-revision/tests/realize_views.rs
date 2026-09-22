// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Realizing a revision over a working tree, with a view filter per side.
//!
//! The tree holds what the current view materialized and is left holding what the target view does,
//! so the changes are walked with a context per side and every write is the target context's. A move
//! is where the two views part company over one path — the source is admitted by the view the tree is
//! carried from, which is not the one realize holds — so what decides whether its content still has
//! to be written is the rename that carried it, not a verdict about where it came from. That also
//! makes a realize idempotent over its own result, which is what re-running an interrupted sync asks
//! of it.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::fs::filesystem_provider::FilesystemProvider;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::lore::RepositoryId;
    use lore_revision::node::NodeFlags;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::revision::sync::SyncOptions;
    use lore_revision::stage::StageCaseChange;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;

    include!("helper.rs");

    /// The directory both revisions hold, and the one the move happens inside.
    const DIRECTORY: &str = "dir";
    /// The file the older revision holds, which the newer holds at [`RENAMED`] instead.
    const MOVED: &str = "dir/moved.txt";
    const RENAMED: &str = "dir/renamed.txt";
    /// What [`MOVED`] carries into [`RENAMED`] unchanged, so the rename alone realizes the move.
    const CONTENT: &[u8] = b"carried across";
    /// What [`RENAMED`] holds where the move rewrote it, which no rename can put there.
    const REWRITTEN: &[u8] = b"carried across and rewritten";
    /// A file both revisions hold identically, so a view is the only thing that can drop it.
    const STEADY: &str = "dir/steady.txt";
    /// The view rule that excludes [`MOVED`], the name the file is carried away from.
    const EXCLUDE_MOVED: &str = "/dir/moved.txt";
    /// The view rule that excludes [`STEADY`].
    const EXCLUDE_STEADY: &str = "/dir/steady.txt";

    /// What a realize wrote, as the progress event reports it.
    #[derive(Debug, PartialEq, Eq)]
    struct Written {
        files: usize,
        bytes: u64,
    }

    /// The totals a [`recording_execution`] folds the progress events it sees into.
    #[derive(Default)]
    struct Recorded {
        files: AtomicUsize,
        bytes: AtomicU64,
    }

    impl Recorded {
        fn written(&self) -> Written {
            Written {
                files: self.files.load(Ordering::Relaxed),
                bytes: self.bytes.load(Ordering::Relaxed),
            }
        }
    }

    /// An execution recording the totals of every sync progress event sent under it.
    ///
    /// The totals are monotonic and a realize ends by sending its final ones, so the largest of what
    /// arrived is what it wrote.
    fn recording_execution(recorded: Arc<Recorded>) -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if let LoreEvent::RevisionSyncProgress(progress) = event {
                    recorded
                        .files
                        .fetch_max(progress.file_update, Ordering::Relaxed);
                    recorded
                        .bytes
                        .fetch_max(progress.bytes_update, Ordering::Relaxed);
                }
            }))),
            "test-user".to_string(),
        ))
    }

    /// A repository whose newer revision renames [`MOVED`] to [`RENAMED`] and holds [`STEADY`]
    /// unchanged, with the working tree standing on the older revision.
    struct Fixture {
        instance: TestRepository,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        older: Arc<state::State>,
        newer: Arc<state::State>,
    }

    impl Fixture {
        /// The move carries [`CONTENT`] across unchanged, so the rename is the whole of realizing it.
        async fn create(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, CONTENT).await
        }

        /// The move leaves [`REWRITTEN`] at [`RENAMED`], so the content the rename carries there is
        /// not the content the newer revision holds.
        async fn create_rewriting(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, REWRITTEN).await
        }

        /// The rename is staged as a move rather than scanned, so both revisions hold the one file
        /// node and the walk between them pairs the delete with the add into a move. `arriving` is
        /// what the newer revision holds at [`RENAMED`]. The tree is left standing on the older
        /// revision, which is where a realize of the newer one starts.
        async fn build(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
            arriving: &[u8],
        ) -> Self {
            let instance = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;

            std::fs::create_dir_all(instance.path.join(DIRECTORY))
                .expect("Create directory failed");
            let moved = instance.path.join(MOVED);
            let renamed = instance.path.join(RENAMED);
            test_file_write(moved.as_path(), CONTENT);
            test_file_write(instance.path.join(STEADY).as_path(), b"steady");
            let older = test_commit_tree(&instance, "First").await;

            std::fs::rename(&moved, &renamed).expect("Rename file failed");
            test_file_write(renamed.as_path(), arriving);
            lore_revision::file::stage::stage_move(
                instance.repository.clone(),
                &instance.write_token,
                moved.to_string_lossy().into_owned(),
                renamed.to_string_lossy().into_owned(),
                StageOptions {
                    case_change: StageCaseChange::Error,
                    node_flags: NodeFlags::NoFlags,
                    file_id: None,
                    no_children: false,
                    scan: true,
                },
            )
            .await
            .expect("Failed to stage the move");
            let newer = test_commit(&instance, "Second").await;

            std::fs::remove_file(&renamed).expect("Remove file failed");
            test_file_write(moved.as_path(), CONTENT);

            Self {
                instance,
                immutable_store,
                mutable_store,
                older,
                newer,
            }
        }

        /// A context over the repository whose view holds `globs`.
        fn view(&self, globs: &[&str]) -> Arc<RepositoryContext> {
            test_view_context(
                &self.instance,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                globs,
            )
        }

        /// Realizes the newer revision over the working tree, `current` answering for the tree it
        /// stands on and `target` for the one it is left holding.
        async fn realize(
            &self,
            current: Arc<RepositoryContext>,
            target: Arc<RepositoryContext>,
        ) -> Written {
            let recorded = Arc::new(Recorded::default());
            let recording = recording_execution(recorded.clone());
            let older = self.older.clone();
            let newer = self.newer.clone();
            LORE_CONTEXT
                .scope(recording.clone(), async move {
                    let operation =
                        FilesystemProvider::begin_operation(target.file_system().as_ref())
                            .await
                            .expect("Failed to start filesystem operation");
                    Box::pin(lore_revision::fs::realize::realize_state(
                        current,
                        target,
                        operation.clone(),
                        older,
                        newer,
                        SyncOptions::default(),
                    ))
                    .await
                    .expect("Failed to realize the newer revision");
                    operation
                        .finalize()
                        .await
                        .expect("Failed to finish filesystem operation");
                    recording.dispatcher.drain().await;
                })
                .await;
            recorded.written()
        }

        /// What the working tree holds at `path`, or `None` where it holds nothing.
        fn working_file(&self, path: &str) -> Option<Vec<u8>> {
            std::fs::read(self.instance.path.join(path)).ok()
        }

        /// Marks the working file at `path` executable behind the repository's back, which is what a
        /// user running `chmod +x` leaves: a bit no revision gave the file.
        #[cfg(target_family = "unix")]
        fn make_working_executable(&self, path: &str) {
            use std::os::unix::fs::PermissionsExt;

            let file = self.instance.path.join(path);
            let mut permissions = std::fs::metadata(&file)
                .expect("The working file must be readable")
                .permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            std::fs::set_permissions(&file, permissions).expect("Failed to set the executable bit");
        }

        /// Whether the working file at `path` carries the executable bit.
        #[cfg(target_family = "unix")]
        fn working_executable(&self, path: &str) -> bool {
            use std::os::unix::fs::PermissionsExt;

            std::fs::metadata(self.instance.path.join(path))
                .expect("The working file must be readable")
                .permissions()
                .mode()
                & 0o111
                != 0
        }
    }

    /// A file the target view drops leaves the working tree, which is what the current view's
    /// context is there for: it is the side that admitted the file, and so the side that says the
    /// tree holds one to remove.
    #[tokio::test]
    async fn a_file_the_target_view_drops_leaves_the_working_tree() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                fixture
                    .realize(fixture.view(&[]), fixture.view(&[EXCLUDE_STEADY]))
                    .await;

                assert_eq!(
                    fixture.working_file(STEADY),
                    None,
                    "a file only the current view admits is what a narrowing removes"
                );
                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(CONTENT),
                    "the rest of the revision is realized as it always was"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A move whose source the target view drops is carried by the rename all the same: the source
    /// stands on disk under the view the tree is carried *from*, and the rename reports that it was
    /// there rather than the target view being asked to guess.
    #[tokio::test]
    async fn a_move_whose_source_the_target_view_drops_is_carried_by_the_rename() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                let written = fixture
                    .realize(fixture.view(&[]), fixture.view(&[EXCLUDE_MOVED]))
                    .await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(CONTENT),
                    "the destination holds the content the rename carried there"
                );
                assert_eq!(
                    fixture.working_file(MOVED),
                    None,
                    "the source name is left behind by the move"
                );
                assert_eq!(
                    written,
                    Written { files: 0, bytes: 0 },
                    "a source the two views disagree over costs no write, since the rename answers"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A move whose source the view admits is realized by the rename alone: the content is on disk
    /// already and nothing reads the store, which is the point of reporting a move at all.
    ///
    /// One context on both sides, which is every sync the product runs today, so this is the
    /// optimisation the step has to leave standing.
    #[tokio::test]
    async fn a_move_whose_source_the_view_admits_is_realized_by_the_rename_alone() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);

                let written = fixture.realize(repository.clone(), repository).await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(CONTENT),
                    "the rename carries the content to the destination"
                );
                assert_eq!(
                    fixture.working_file(MOVED),
                    None,
                    "the source name is left behind by the move"
                );
                assert_eq!(
                    written,
                    Written { files: 0, bytes: 0 },
                    "an unchanged move writes nothing"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A move that also rewrites the file is written from the store: the rename carries the content
    /// the older revision held, which is not the content the newer one holds at the destination.
    #[tokio::test]
    async fn a_move_that_rewrites_the_file_is_written_from_the_store() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_rewriting(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);

                let written = fixture.realize(repository.clone(), repository).await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(REWRITTEN),
                    "the destination holds what the newer revision holds, not what the rename carried"
                );
                assert_eq!(
                    written,
                    Written {
                        files: 1,
                        bytes: REWRITTEN.len() as u64
                    },
                    "a move whose content changed is written whatever the rename did"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A bit the user set at the source is the working tree's own, and the rename carries it to the
    /// destination along with the file. Nothing may then apply the mode the revision holds over it.
    ///
    /// The verify has to measure the source to see the bit at all: the destination holds nothing
    /// until the rename has run, so a measurement taken there answers nothing about the mode.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_move_keeps_a_local_executable_bit_the_rename_carries() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);
                fixture.make_working_executable(MOVED);

                let written = fixture.realize(repository.clone(), repository).await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(CONTENT),
                    "the rename carries the content to the destination"
                );
                assert!(
                    fixture.working_executable(RENAMED),
                    "the bit the user set at the source stands at the destination"
                );
                assert_eq!(
                    written,
                    Written { files: 0, bytes: 0 },
                    "a bit of its own is no reason for a move to write the content"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The same where the move rewrites the file, which the rename cannot carry: the content is
    /// written from the store, and writing a node applies the mode it holds. The bit is read off the
    /// file the rename moved and written with the content rather than reverted to the revision's.
    #[cfg(target_family = "unix")]
    #[tokio::test]
    async fn a_move_that_rewrites_the_file_keeps_a_local_executable_bit() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_rewriting(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);
                fixture.make_working_executable(MOVED);

                let written = fixture.realize(repository.clone(), repository).await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(REWRITTEN),
                    "the destination holds what the newer revision holds"
                );
                assert!(
                    fixture.working_executable(RENAMED),
                    "the bit the user set survives the write the rewritten content takes"
                );
                assert_eq!(
                    written,
                    Written {
                        files: 1,
                        bytes: REWRITTEN.len() as u64
                    },
                    "the content still comes from the store"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The same change set realized twice leaves the destination holding the content, which is what
    /// re-running an interrupted sync asks for: the anchor has not moved, so the walk reports the
    /// move again and the second pass meets a tree the rename was already applied to.
    ///
    /// The rename fails there — its source is gone — and the fallback takes the destination with it,
    /// so the content comes back from the store. A skip keyed on anything other than the rename's own
    /// result leaves no file at all.
    #[tokio::test]
    async fn a_move_realized_twice_keeps_its_destination() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);

                let first = fixture
                    .realize(repository.clone(), repository.clone())
                    .await;
                let second = fixture.realize(repository.clone(), repository).await;

                assert_eq!(
                    fixture.working_file(RENAMED).as_deref(),
                    Some(CONTENT),
                    "a move realized over its own result must leave the content in place"
                );
                assert_eq!(
                    (first, second),
                    (
                        Written { files: 0, bytes: 0 },
                        Written {
                            files: 1,
                            bytes: CONTENT.len() as u64
                        }
                    ),
                    "the rename carries the first pass and the store the second"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
