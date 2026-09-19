// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! A diff whose two sides filter through different view filters.
//!
//! The walk seeds each side from its own filter and drops the path it is rooted at only where both
//! sides exclude it. A subpath-scoped walk is where that matters: its root is a named directory a
//! view can exclude, while a whole-tree walk is rooted at the repository root, which no view
//! excludes.

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
    /// The one file under [`SUBDIRECTORY`], whose content differs between the two revisions.
    const FILE: &str = "sub/file.txt";

    /// A repository holding [`FILE`] in two revisions, over the stores its contexts are built on.
    struct Fixture {
        instance: TestRepository,
        immutable_store: Arc<dyn lore_storage::ImmutableStore>,
        mutable_store: Arc<dyn lore_storage::MutableStore>,
        older: Arc<state::State>,
        newer: Arc<state::State>,
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

            std::fs::create_dir_all(instance.path.join(SUBDIRECTORY))
                .expect("Create directory failed");
            let file = instance.path.join(FILE);
            test_file_write(file.as_path(), b"before");
            let older = commit_tree(&instance, "First").await;
            test_file_write(file.as_path(), b"after");
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
        Box::pin(commit::commit(
            fixture.repository.clone(),
            &fixture.write_token,
            CommitOptions::new(message.to_string()),
        ))
        .await
        .expect("Commit failed");
        let (revision, _branch) = lore_revision::instance::load_current_anchor(&fixture.repository)
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

    /// The to side excludes the walk's root and the from side admits it, so the walk enters it:
    /// the from side still holds what is there, and only a walk says what became of it.
    #[tokio::test]
    async fn a_root_the_to_view_alone_excludes_is_still_walked() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert_eq!(
                    fixture
                        .changes(fixture.view(&[]), fixture.view(&[EXCLUDE_SUBDIRECTORY]))
                        .await,
                    vec![("M".to_string(), FILE.to_string())],
                    "a root the from side admits must be walked whatever the to side says of it"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// The from side excludes the walk's root and the to side admits it, so the from side's
    /// children are dropped by the from view and the to side's arrive alone: the file enters the
    /// view and is added rather than modified.
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
                    vec![("A".to_string(), FILE.to_string())],
                    "a path the from view never held must be added, not modified"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Both views exclude the walk's root, so nothing under it is held by either and the walk
    /// drops it.
    #[tokio::test]
    async fn a_root_both_views_exclude_is_dropped() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;

                assert!(
                    fixture
                        .changes(
                            fixture.view(&[EXCLUDE_SUBDIRECTORY]),
                            fixture.view(&[EXCLUDE_SUBDIRECTORY])
                        )
                        .await
                        .is_empty(),
                    "a root neither side admits must report nothing"
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// One filter on both sides announces a dropped root once, not once per side.
    ///
    /// Only the walk runs under the counting execution. Building the fixture stages and commits,
    /// and a filter reached there would be counted too.
    #[tokio::test]
    async fn a_dropped_root_is_announced_once() {
        let (immutable_store, mutable_store, execution) =
            test_store_create().await.expect("Failed to create stores");
        let announced = Arc::new(AtomicUsize::new(0));
        let counting = counting_execution(announced.clone());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution, async move {
                let fixture = Fixture::create(immutable_store, mutable_store).await;
                let repository = fixture.view(&[EXCLUDE_SUBDIRECTORY]);

                LORE_CONTEXT
                    .scope(counting.clone(), async {
                        assert!(
                            fixture
                                .changes(repository.clone(), repository)
                                .await
                                .is_empty(),
                            "an excluded root must report nothing"
                        );
                        counting.dispatcher.drain().await;
                    })
                    .await;

                assert_eq!(
                    announced.load(Ordering::Relaxed),
                    1,
                    "a dropped root is one exclusion, whichever side was asked about it"
                );
            }))
            .await
            .expect("Test task failed");
    }
}
