// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A diff whose two sides filter through different view filters.
//!
//! Three things are held here. Each side is seeded from its own filter, and the walk drops the
//! path it is rooted at only where both sides exclude it. A path one side holds while the other
//! does not is routed by which side holds it: out of the to view it is deleted, into the to view
//! it is added. And what the walk reports of itself says which directories it entered, which is
//! the only place a subtree it skipped differs from one it walked and found unchanged.
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
    use lore_revision::change::sort_by_path;
    use lore_revision::filter::FilterMode;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::lore::RepositoryId;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryContext;
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
    /// The view rule that re-includes [`NESTED_FILE`] below the directory [`EXCLUDE_NESTED`]
    /// drops, so a view holding both materializes that file and nothing else under [`NESTED`].
    const REINCLUDE_NESTED_FILE: &str = "!/sub/nested/deep.txt";
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

    /// A directory holding a different file in each revision, so which side a walk enumerates is
    /// readable in what it names.
    const RESHAPED: &str = "sub/reshaped";
    /// The file [`RESHAPED`] holds in the older revision, and so the one on disk.
    const HELD: &str = "sub/reshaped/held.txt";
    /// The file [`RESHAPED`] holds in the newer revision, which no working tree ever stood on.
    const ARRIVING: &str = "sub/reshaped/arriving.txt";
    /// The view rule that excludes [`RESHAPED`] and everything under it.
    const EXCLUDE_RESHAPED: &str = "/sub/reshaped";

    /// The directory the sibling subtrees sit under, which the walks that measure what a walk
    /// entered are rooted at.
    const TREE: &str = "wide";
    /// How many siblings [`TREE`] holds. Entering them all has to read differently from entering
    /// the one a walk has a reason to, so more than a handful.
    const SIBLINGS: u32 = 20;
    /// The sibling whose content differs between the two revisions, so every walk enters it
    /// whatever the views say.
    const CHANGED: u32 = 3;
    /// A sibling both revisions hold identically, so the views alone decide whether a walk
    /// enters it.
    const UNTOUCHED: u32 = 7;
    /// The file each sibling holds beside [`DROPPED`], which a view can re-include below a
    /// sibling it excludes.
    const KEPT: &str = "keep.txt";
    /// The file each sibling holds beside [`KEPT`], which no re-inclusion names.
    const DROPPED: &str = "drop.txt";

    /// The path of sibling `index` under [`TREE`].
    fn sibling(index: u32) -> String {
        format!("{TREE}/dir{index:02}")
    }

    /// What the newer revision does beyond rewriting [`FILE`].
    #[derive(Clone, Copy)]
    enum Shape {
        Plain,
        Retype,
        Rename,
        Reshape,
        Wide,
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

        /// [`Fixture::create`] where [`RESHAPED`] also holds [`HELD`] in the older revision and
        /// [`ARRIVING`] in the newer.
        async fn create_reshaping(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, Shape::Reshape).await
        }

        /// [`Fixture::create`] beside [`SIBLINGS`] sibling directories under [`TREE`], of which
        /// [`CHANGED`] alone differs between the two revisions.
        async fn create_wide(
            immutable_store: Arc<dyn lore_storage::ImmutableStore>,
            mutable_store: Arc<dyn lore_storage::MutableStore>,
        ) -> Self {
            Self::build(immutable_store, mutable_store, Shape::Wide).await
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
            let held = instance.path.join(HELD);
            match shape {
                Shape::Plain => {}
                Shape::Retype => test_file_write(retyped.as_path(), b"was a file"),
                Shape::Rename => test_file_write(moved.as_path(), b"carried across"),
                Shape::Reshape => {
                    std::fs::create_dir_all(instance.path.join(RESHAPED))
                        .expect("Create directory failed");
                    test_file_write(held.as_path(), b"stood on disk");
                }
                Shape::Wide => {
                    for index in 0..SIBLINGS {
                        let directory = instance.path.join(sibling(index));
                        std::fs::create_dir_all(&directory).expect("Create directory failed");
                        test_file_write(
                            directory.join(KEPT).as_path(),
                            format!("kept {index}").as_bytes(),
                        );
                        test_file_write(
                            directory.join(DROPPED).as_path(),
                            format!("dropped {index}").as_bytes(),
                        );
                    }
                }
            }
            let older = test_commit_tree(&instance, "First").await;

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
                Shape::Reshape => {
                    std::fs::remove_file(&held).expect("Remove file failed");
                    test_file_write(
                        instance.path.join(ARRIVING).as_path(),
                        b"only in the target",
                    );
                }
                Shape::Wide => test_file_write(
                    instance.path.join(sibling(CHANGED)).join(DROPPED).as_path(),
                    b"rewritten",
                ),
            }
            let newer = test_commit_tree(&instance, "Second").await;

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

        /// A context whose ignore slot holds `ignore` and whose view is empty, as a repository
        /// opened over a `.loreignore` and no view file holds one.
        fn ignoring(&self, ignore: &[&str]) -> Arc<RepositoryContext> {
            test_filter_context(
                &self.instance,
                self.immutable_store.clone(),
                self.mutable_store.clone(),
                ignore,
                &[],
            )
        }

        /// A view that excludes sibling `index` and re-includes [`KEPT`] below it, so the
        /// sibling holds something the view keeps and something it does not.
        fn view_excluding_but_for_kept(&self, index: u32) -> Arc<RepositoryContext> {
            let directory = sibling(index);
            self.view(&[&format!("/{directory}"), &format!("!/{directory}/{KEPT}")])
        }

        /// What a walk over [`SUBDIRECTORY`] reports between the two revisions, read the way
        /// every caller in the product reads it: coalesced and sorted.
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
            test_reported(&changes)
        }

        /// What a walk over `path` emitted, and what it reported of itself.
        ///
        /// The changes are the walk's own, ahead of the move coalescing [`Fixture::changes`]
        /// reads them through, and the summary is what `diff_collect` discards.
        async fn walk(
            &self,
            from: Arc<RepositoryContext>,
            to: Arc<RepositoryContext>,
            path: &str,
        ) -> (Vec<(String, String)>, state::DiffWalkStats) {
            let older = self.older.clone();
            let newer = self.newer.clone();
            let path = RelativePath::new_from_initial_path(path).expect("Valid path");
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
            let mut collected = Vec::new();
            while let Some(change) = walk.next().await {
                collected.push(change);
            }
            let stats = walk
                .finish()
                .await
                .expect("Failed to diff the two revisions");
            sort_by_path(&mut collected);
            (test_reported(&collected), stats)
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

    /// `changes` ordered by path and then by action.
    ///
    /// The walk's own order is `change::sort_by_path`, which sorts by path alone and unstably, so
    /// two records standing at one path arrive in no defined order. Imposing one here is what makes
    /// the pair a retype emits readable as a list.
    fn by_path_and_action(mut changes: Vec<(String, String)>) -> Vec<(String, String)> {
        changes.sort_by(|left, right| (&left.1, &left.0).cmp(&(&right.1, &right.0)));
        changes
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

    /// A file the from view re-included below a directory it excluded leaves the tree when the to
    /// view drops that directory whole.
    ///
    /// The verdict the delete fans out under names the line that decided it by index into the view
    /// that produced it, and an excluded verdict is stepped from that line on. The two views hold
    /// different lines at the same index: read against the to view the re-inclusion is not there to
    /// be found, and the one file the working tree holds is never named.
    #[tokio::test]
    async fn a_re_included_file_leaves_with_the_directory_the_to_view_drops() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(
                            fixture.view(&[EXCLUDE_NESTED, REINCLUDE_NESTED_FILE]),
                            fixture.view(&[EXCLUDE_NESTED]),
                        )
                        .await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), NESTED.to_string()),
                        ("D".to_string(), NESTED_FILE.to_string()),
                    ],
                    "the file the from view held below the directory must leave with it"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory leaving the view is emptied of what the from side holds, which is what stands on
    /// disk, and not of what the newer revision would have put there.
    ///
    /// The delete fans out over one tree under one view, and both are the from side's: the to side
    /// names a file no working tree ever stood on, and the to view excludes the subtree whole, so
    /// asking it would name nothing at all.
    #[tokio::test]
    async fn a_directory_leaving_the_view_is_emptied_of_what_the_from_side_holds() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_reshaping(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_RESHAPED]))
                        .await,
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("D".to_string(), RESHAPED.to_string()),
                        ("D".to_string(), HELD.to_string()),
                    ],
                    "the delete names the file the working tree holds, not the one arriving"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A path that is a file in one revision and a directory in the next leaves the working tree
    /// and arrives again: the file is deleted, the directory added at the same path, and what that
    /// directory holds added below it.
    ///
    /// Both records stand at the one path, which no view moves, so this is what a retype costs
    /// before any view has a say -- and the case the exclusion below narrows.
    #[tokio::test]
    async fn a_retype_is_deleted_and_added_at_the_one_path() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_retyping(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);

                assert_eq!(
                    by_path_and_action(fixture.changes(repository.clone(), repository).await),
                    vec![
                        ("M".to_string(), FILE.to_string()),
                        ("A".to_string(), RETYPED.to_string()),
                        ("D".to_string(), RETYPED.to_string()),
                        ("A".to_string(), RETYPED_FILE.to_string()),
                    ],
                    "a retyped path leaves as the file it was and arrives as the directory it \
                     becomes, carrying what that directory holds"
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

    /// The walk reports the directories it entered, which is the only place a subtree it skipped
    /// differs from one it walked: both report the same changes.
    #[tokio::test]
    async fn a_walk_reports_the_directories_it_entered() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;
                let repository = fixture.view(&[]);

                let (changes, stats) = fixture.walk(repository.clone(), repository, TREE).await;

                assert_eq!(
                    changes,
                    vec![("M".to_string(), format!("{}/{DROPPED}", sibling(CHANGED)))],
                    "one file was rewritten between the revisions"
                );
                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    2,
                    "the root and the one sibling whose content differs, of {SIBLINGS}"
                );
                assert_eq!(
                    stats.filter_queries.load(Ordering::Relaxed),
                    u64::from(SIBLINGS) * 2 + 2,
                    "each sibling costs the verdict admitting it and the subtree verdict for the \
                     pair, the two files below the one entered cost the verdict admitting each, \
                     and one filter asks no prune question"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A paired file costs one verdict under one view and two under two, which is the whole of
    /// what the to-side verdict is gated for: a paired file is the commonest node a walk sees.
    #[tokio::test]
    async fn a_paired_file_costs_a_second_verdict_under_two_views() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;
                let one_filter = fixture.view(&[]);
                let files = sibling(CHANGED);

                let (_, single) = fixture.walk(one_filter.clone(), one_filter, &files).await;
                let (_, two) = fixture
                    .walk(fixture.view(&[]), fixture.view(&[]), &files)
                    .await;

                assert_eq!(
                    single.filter_queries.load(Ordering::Relaxed),
                    2,
                    "one filter answers for both sides, so each file costs the verdict admitting \
                     it and nothing more"
                );
                assert_eq!(
                    two.filter_queries.load(Ordering::Relaxed),
                    4,
                    "a second view is a second verdict per paired file"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Two views that cover a content-equal subtree agree about everything in it, so the walk
    /// takes it whole -- the prune the whole design exists to keep.
    #[tokio::test]
    async fn views_that_cover_a_content_equal_subtree_do_not_enter_it() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;

                let (changes, stats) = fixture
                    .walk(
                        fixture.view(&[EXCLUDE_SUBDIRECTORY]),
                        fixture.view(&[EXCLUDE_SUBDIRECTORY]),
                        TREE,
                    )
                    .await;

                assert_eq!(
                    changes,
                    vec![("M".to_string(), format!("{}/{DROPPED}", sibling(CHANGED)))],
                    "rules that reach nothing under the tree change nothing in it"
                );
                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    2,
                    "the root and the one sibling whose content differs, of {SIBLINGS}"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory the to view excludes while re-including below it holds content on either side
    /// of the rule, so the walk has to enter it and route each by the view holding it -- whether or
    /// not its content
    /// differs between the revisions.
    ///
    /// The two halves are the same assertion. Under content equality nothing but the views can
    /// route the walk into the directory, which is what the prune has to account for; with the
    /// content differing the walk would enter it anyway, so the half that passes on its own
    /// proves nothing the other does not disprove.
    #[tokio::test]
    async fn a_re_inclusion_below_an_excluded_directory_is_reached() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;

                for index in [CHANGED, UNTOUCHED] {
                    let directory = sibling(index);
                    let (changes, _) = fixture
                        .walk(
                            fixture.view(&[]),
                            fixture.view_excluding_but_for_kept(index),
                            TREE,
                        )
                        .await;

                    assert!(
                        changes.contains(&("D".to_string(), format!("{directory}/{DROPPED}"))),
                        "{directory} holds a file the to view drops: {changes:?}"
                    );
                    assert!(
                        !changes
                            .iter()
                            .any(|(_, path)| *path == format!("{directory}/{KEPT}")),
                        "{directory} holds a re-included file that stays put: {changes:?}"
                    );
                    assert!(
                        !changes.contains(&("D".to_string(), directory.clone())),
                        "{directory} still holds the re-included file, so it stays: {changes:?}"
                    );
                }
            }))
            .await
            .expect("Test task failed");
    }

    /// Two views that exclude one content-equal directory and re-include different files below it
    /// disagree about everything else in it, so the walk enters it and routes each file by which
    /// view holds it.
    #[tokio::test]
    async fn views_re_including_different_files_below_one_directory_diverge() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;
                let directory = sibling(UNTOUCHED);
                let excluded = format!("/{directory}");

                let (changes, stats) = fixture
                    .walk(
                        fixture.view(&[&excluded, &format!("!/{directory}/{KEPT}")]),
                        fixture.view(&[&excluded, &format!("!/{directory}/{DROPPED}")]),
                        TREE,
                    )
                    .await;

                assert_eq!(
                    changes,
                    vec![
                        ("M".to_string(), format!("{}/{DROPPED}", sibling(CHANGED))),
                        ("A".to_string(), format!("{directory}/{DROPPED}")),
                        ("D".to_string(), format!("{directory}/{KEPT}")),
                    ],
                    "each file is routed by the view that holds it"
                );
                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    3,
                    "the root, the sibling whose content differs, and the one the views diverge in"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// An ignore rule the two sides share cannot make them diverge, so it must not cost the prune.
    ///
    /// A repository opened for real holds name rules in its ignore slot, and a name rule reaches
    /// below every path, so no subtree is ever coverable by that slot. A walk putting the question
    /// to it as well as to the view would answer "the views diverge" under every directory there
    /// is and walk the tree -- the one way this prune becomes a no-op in the product while every
    /// other test still passes.
    #[tokio::test]
    async fn a_shared_ignore_rule_does_not_cost_the_prune() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;

                let (_, stats) = fixture
                    .walk(
                        fixture.ignoring(&["*.tmp"]),
                        fixture.ignoring(&["*.tmp"]),
                        TREE,
                    )
                    .await;

                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    2,
                    "an ignore rule both sides hold leaves every sibling coverable"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A view holding an exclusion that can match at any depth leaves the walk nothing to prune,
    /// so it enters every sibling to find that nothing changed.
    ///
    /// The accepted characteristic, with the number behind it: such a rule belongs in
    /// `.loreignore`, which both sides share and neither can diverge over.
    #[tokio::test]
    async fn an_unanchored_exclusion_costs_the_walk_every_prune() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;

                let (changes, stats) = fixture
                    .walk(fixture.view(&[]), fixture.view(&["*.tmp"]), TREE)
                    .await;

                assert_eq!(
                    changes,
                    vec![("M".to_string(), format!("{}/{DROPPED}", sibling(CHANGED)))],
                    "the rule matches nothing here, so it changes nothing -- only what it cost \
                     to find that out"
                );
                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    u64::from(SIBLINGS) + 1,
                    "a name rule reaches below every path, so no sibling is covered"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// A directory the to view drops whole leaves without being entered: the verdict taken where
    /// the walk pairs it answers for everything under it, and the delete fans out from there.
    #[tokio::test]
    async fn a_directory_the_to_view_drops_whole_is_not_entered() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create_wide(immutable_store, mutable_store).await;
                let directory = sibling(UNTOUCHED);

                let (changes, stats) = fixture
                    .walk(
                        fixture.view(&[]),
                        fixture.view(&[&format!("/{directory}")]),
                        TREE,
                    )
                    .await;

                assert_eq!(
                    changes,
                    vec![
                        ("M".to_string(), format!("{}/{DROPPED}", sibling(CHANGED))),
                        ("D".to_string(), directory.clone()),
                        ("D".to_string(), format!("{directory}/{DROPPED}")),
                        ("D".to_string(), format!("{directory}/{KEPT}")),
                    ],
                    "the directory leaves with everything under it"
                );
                assert_eq!(
                    stats.directories_entered.load(Ordering::Relaxed),
                    2,
                    "the root and the one sibling whose content differs, of {SIBLINGS}"
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
