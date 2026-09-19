// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A diff whose two sides filter through different view filters.
//!
//! Two things are held here. Each side is seeded from its own filter, and the walk drops the path
//! it is rooted at only where both sides exclude it. And a path one side holds while the other does
//! not is routed by which side holds it: out of the to view it is deleted, into the to view it is
//! added.
//!
//! Every walk here is subpath-scoped, which is what lets the root itself be excluded. A whole-tree
//! walk is rooted at the repository root, and no view excludes that.

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::filter::Filter;
    use lore_revision::filter::FilterMode;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use lore_revision::util::path::RelativePath;

    include!("helper.rs");

    /// The directory the walk is rooted at, which a view can exclude by name.
    const SUBDIRECTORY: &str = "sub";
    /// The view rule that excludes [`SUBDIRECTORY`] and everything under it.
    const EXCLUDE_SUBDIRECTORY: &str = "/sub";
    /// The one file directly under [`SUBDIRECTORY`], whose content differs between the two
    /// revisions.
    const FILE: &str = "sub/file.txt";
    /// A directory under [`SUBDIRECTORY`] whose content is the same in both revisions, so a walk
    /// reaches it only when something other than its content routes it.
    const NESTED: &str = "sub/nested";
    const NESTED_FILE: &str = "sub/nested/deep.txt";
    /// A file under [`SUBDIRECTORY`] whose content is the same in both revisions, so what routes
    /// it can only be the views.
    const STEADY: &str = "sub/steady.txt";
    /// The view rule that excludes [`FILE`].
    const EXCLUDE_FILE: &str = "/sub/file.txt";
    /// The view rule that excludes [`STEADY`].
    const EXCLUDE_STEADY: &str = "/sub/steady.txt";
    /// The view rule that excludes [`NESTED`] and everything under it.
    const EXCLUDE_NESTED: &str = "/sub/nested";
    /// A path that is a file in the older revision and a directory in the newer.
    const RETYPED: &str = "sub/retyped";
    const RETYPED_FILE: &str = "sub/retyped/inner.txt";
    /// A directory-only view rule naming [`RETYPED`], which therefore matches it in the newer
    /// revision alone.
    const EXCLUDE_RETYPED_DIRECTORY: &str = "/sub/retyped/";
    /// A file the older revision holds, which the newer holds at [`RENAMED`] instead.
    const MOVED: &str = "sub/moved.txt";
    const RENAMED: &str = "sub/renamed.txt";
    /// The view rule that excludes [`RENAMED`], the name [`MOVED`] arrives under.
    const EXCLUDE_RENAMED: &str = "/sub/renamed.txt";

    /// What the newer revision does beyond rewriting [`FILE`].
    #[derive(Clone, Copy)]
    enum Shape {
        Plain,
        Retype,
        Rename,
    }

    /// A repository holding [`FILE`] in two revisions, over the stores its contexts are built on.
    struct Fixture {
        instance: TestRepository,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        older: Arc<state::State>,
        newer: Arc<state::State>,
    }

    impl Fixture {
        /// [`FILE`] rewritten between the revisions, beside [`STEADY`] and [`NESTED_FILE`] left
        /// alone.
        async fn create(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, Shape::Plain).await
        }

        /// [`Fixture::create`] where [`RETYPED`] is also a file in the older revision and a
        /// directory holding [`RETYPED_FILE`] in the newer.
        async fn create_retyping(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, Shape::Retype).await
        }

        /// [`Fixture::create`] where [`MOVED`] is also renamed to [`RENAMED`] between the
        /// revisions, carrying its content unchanged.
        async fn create_renaming(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, Shape::Rename).await
        }

        async fn build(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
            shape: Shape,
        ) -> Self {
            let instance = test_repository_create(
                immutable_store.clone(),
                mutable_store.clone(),
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;

            std::fs::create_dir_all(instance.path.join(NESTED)).expect("Create directory failed");
            let file = instance.path.join(FILE);
            test_file_write(file.as_path(), b"before");
            test_file_write(instance.path.join(NESTED_FILE).as_path(), b"deep");
            test_file_write(instance.path.join(STEADY).as_path(), b"steady");
            let retyped = instance.path.join(RETYPED);
            let moved = instance.path.join(MOVED);
            match shape {
                Shape::Plain => {}
                Shape::Retype => test_file_write(retyped.as_path(), b"was a file"),
                Shape::Rename => test_file_write(moved.as_path(), b"carried across"),
            }
            let older = commit_tree(&instance, "First").await;

            test_file_write(file.as_path(), b"after");
            match shape {
                Shape::Plain => {}
                Shape::Retype => {
                    std::fs::remove_file(&retyped).expect("Remove file failed");
                    std::fs::create_dir_all(&retyped).expect("Create directory failed");
                    test_file_write(
                        instance.path.join(RETYPED_FILE).as_path(),
                        b"now a directory",
                    );
                }
                Shape::Rename => {
                    std::fs::rename(&moved, instance.path.join(RENAMED))
                        .expect("Rename file failed");
                }
            }
            let newer = commit_tree(&instance, "Second").await;

            Self {
                instance,
                immutable_store,
                mutable_store,
                older,
                newer,
            }
        }

        /// A context over the repository whose view excludes `globs`.
        ///
        /// Each call mints its own [`Filter`], so two contexts from one fixture hold different
        /// ones whether or not their rules agree.
        fn view(&self, globs: &[&str]) -> Arc<RepositoryContext> {
            let mut filter = Filter::default();
            for glob in globs {
                filter.view.add_exclusion(glob).expect("View exclusion");
            }
            Arc::new(RepositoryContext::new(
                default_repository_creation_args(
                    self.immutable_store.clone(),
                    self.mutable_store.clone(),
                )
                .with_path(&self.instance.path)
                .with_id(self.instance.repository.id)
                .with_instance_id(self.instance.repository.instance_id)
                .with_filter(Arc::new(filter)),
            ))
        }

        /// What a walk over [`SUBDIRECTORY`] reports between the two revisions, as the action
        /// letter against the path.
        ///
        /// The action is carried because the two sides route a path by which of them admits it,
        /// and a path alone does not say which route it took.
        async fn changes(
            &self,
            from: Arc<RepositoryContext>,
            to: Arc<RepositoryContext>,
        ) -> Vec<(String, String)> {
            let changes = state::diff_collect(
                from,
                self.older.clone(),
                to,
                self.newer.clone(),
                Some(RelativePath::new_from_initial_path(SUBDIRECTORY).expect("Valid path")),
                FilterMode::Full,
            )
            .await
            .expect("Failed to diff the two revisions");
            changes
                .iter()
                .map(|change| {
                    (
                        change.action.as_string_short().to_string(),
                        change.path().as_str().to_string(),
                    )
                })
                .collect()
        }

        /// [`Fixture::changes`] with the number of filter-exclude events the walk sent.
        ///
        /// Only the walk runs under the counting execution. Building the fixture stages and
        /// commits, and a filter reached there would be counted too.
        async fn announcements(
            &self,
            from: Arc<RepositoryContext>,
            to: Arc<RepositoryContext>,
        ) -> (Vec<(String, String)>, usize) {
            let announced = Arc::new(AtomicUsize::new(0));
            let counting = counting_execution(announced.clone());
            let changes = LORE_CONTEXT
                .scope(counting.clone(), async {
                    let changes = self.changes(from, to).await;
                    counting.dispatcher.drain().await;
                    changes
                })
                .await;
            (changes, announced.load(Ordering::Relaxed))
        }
    }

    /// Stages and commits the whole working tree, answering the state of the revision it produced.
    async fn commit_tree(fixture: &TestRepository, message: &str) -> Arc<state::State> {
        file::stage::stage(
            fixture.repository.clone(),
            &fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(&fixture.path)]),
            StageOptions {
                scan: true,
                ..Default::default()
            },
        )
        .await
        .expect("Failed to stage the fixture");
        commit::commit_boxed(
            fixture.repository.clone(),
            &fixture.write_token,
            CommitOptions::new(message.to_string()),
        )
        .await
        .expect("Commit failed");
        let (revision, _branch) =
            lore_revision::instance::load_current_anchor_boxed(&fixture.repository)
                .await
                .expect("Failed to load current anchor");
        state::State::deserialize(fixture.repository.clone(), revision)
            .await
            .expect("Failed to deserialize the committed state")
    }

    /// An execution that counts the filter-exclude events sent under it.
    fn counting_execution(counted: Arc<AtomicUsize>) -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new_client_with_user_id(
            LoreGlobalArgs::default(),
            EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                if matches!(event, LoreEvent::FilterExclude(_)) {
                    counted.fetch_add(1, Ordering::Relaxed);
                }
            }))),
            "test-user".to_string(),
        ))
    }

    /// The to side excludes the walk's root and the from side admits it, so the walk enters it and
    /// empties it: everything under the root is held by the from view alone and leaves with it.
    #[tokio::test]
    async fn a_root_the_to_view_alone_excludes_is_walked_and_emptied() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_SUBDIRECTORY]))
                        .await,
                    vec![
                        ("D".to_string(), FILE.to_string()),
                        ("D".to_string(), NESTED.to_string()),
                        ("D".to_string(), NESTED_FILE.to_string()),
                        ("D".to_string(), STEADY.to_string()),
                    ],
                    "a root the from side alone holds must leave the tree path by path"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The from side excludes the walk's root and the to side admits it, so the from side's
    /// children are dropped by the from view and the to side's arrive alone: the subtree enters
    /// the view and is added rather than modified.
    #[tokio::test]
    async fn a_root_the_from_view_alone_excludes_enters_the_view() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[EXCLUDE_SUBDIRECTORY]), fixture.view(&[]))
                        .await,
                    vec![
                        ("A".to_string(), FILE.to_string()),
                        ("A".to_string(), NESTED.to_string()),
                        ("A".to_string(), NESTED_FILE.to_string()),
                        ("A".to_string(), STEADY.to_string()),
                    ],
                    "a subtree the from view never held must be added, not modified"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file the from view holds and the to view drops leaves the working tree, so the walk
    /// reports a delete rather than the modification the content alone would suggest.
    #[tokio::test]
    async fn a_file_leaving_the_view_is_deleted() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_FILE]))
                        .await,
                    vec![("D".to_string(), FILE.to_string())],
                    "a file the to view drops must be deleted, not reported as modified"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file whose content never changed still leaves the working tree when the to view drops it,
    /// which is the case a narrowing is almost entirely made of.
    #[tokio::test]
    async fn an_unchanged_file_leaving_the_view_is_deleted() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_STEADY]))
                        .await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), STEADY.to_string()),
                    ],
                    "the view a file left routes it, not a change to its content"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory the from view holds and the to view drops leaves with everything under it,
    /// which the walk has to spell out per path for the working tree to be emptied.
    #[tokio::test]
    async fn a_directory_leaving_the_view_is_deleted_with_its_contents() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_NESTED]))
                        .await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), NESTED.to_string()),
                        ("D".to_string(), NESTED_FILE.to_string()),
                    ],
                    "a directory the to view drops must take its contents with it"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory-only rule matches a path that becomes a directory and not the file it was, so
    /// one filter alone routes the two sides apart. The delete is the whole change; an add would
    /// name a path the rule keeps off the disk.
    #[tokio::test]
    async fn a_retype_into_an_excluded_directory_is_not_added_back() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_retyping(immutable_store, mutable_store).await;
                let repository = fixture.view(&[EXCLUDE_RETYPED_DIRECTORY]);

                assert_eq!(
                    fixture.changes(repository.clone(), repository).await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), RETYPED.to_string()),
                    ],
                    "a node retyped into an excluded directory is deleted and not added back"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A file renamed into a path the view excludes leaves the working tree, under one view as
    /// much as two: the walk pairs children by name, so a rename arrives as a deletion of the old
    /// name beside an addition of the new one, and the view drops the addition.
    #[tokio::test]
    async fn a_rename_into_an_excluded_path_is_deleted() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_renaming(immutable_store, mutable_store).await;
                let repository = fixture.view(&[EXCLUDE_RENAMED]);

                assert_eq!(
                    fixture.changes(repository.clone(), repository).await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), MOVED.to_string()),
                    ],
                    "a file whose new name the view excludes must leave the tree"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Both views exclude the walk's root, so nothing under it is held by either and the walk drops
    /// it -- announcing that once, though each side was asked separately.
    #[tokio::test]
    async fn a_root_both_views_exclude_is_dropped_and_announced_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                let (changes, announced) = fixture
                    .announcements(
                        fixture.view(&[EXCLUDE_SUBDIRECTORY]),
                        fixture.view(&[EXCLUDE_SUBDIRECTORY]),
                    )
                    .await;

                assert!(
                    changes.is_empty(),
                    "a root neither side admits reports nothing"
                );
                assert_eq!(
                    announced, 1,
                    "two sides asked is still one path dropped, so the to side announces alone"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// One filter answers for both sides, so a dropped root is asked about once and announced once.
    #[tokio::test]
    async fn one_filter_announces_a_dropped_root_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let repository = fixture.view(&[EXCLUDE_SUBDIRECTORY]);

                let (changes, announced) =
                    fixture.announcements(repository.clone(), repository).await;

                assert!(changes.is_empty(), "an excluded root reports nothing");
                assert_eq!(announced, 1, "one answer is one exclusion");
            }))
            .await
            .expect("Test task failed");
    }
}
