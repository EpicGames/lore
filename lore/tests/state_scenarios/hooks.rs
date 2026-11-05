// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

/// The tests drive the node operations through their scheduling points, so the lore-revision
/// they link carries them.
#[test]
fn tests_link_lore_revision_with_its_scheduling_hooks() {
    const { assert!(lore_revision::state::node::HOOKS_ENABLED) };
}

/// Counts the times a task reaches `point`, parking nowhere.
struct Steps {
    point: lore_revision::state::node::hooks::Point,
    count: std::sync::atomic::AtomicUsize,
}

impl lore_revision::state::node::hooks::Scheduler for Steps {
    fn reached(
        &self,
        point: lore_revision::state::node::hooks::Point,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        if point == self.point {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        None
    }

    fn touched(&self, _: lore_revision::state::node::hooks::Resource, _: bool) {}
}

impl Steps {
    fn of(point: lore_revision::state::node::hooks::Point) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            point,
            count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A staged move of a directory to another parent walks its new parent's ancestors once where
/// its caller did not, and not at all where it did; a file's move and a directory's rename in
/// place walk nothing; the C API's move, whose plan walked them, nothing either.
#[tokio::test]
async fn a_staged_move_walks_a_directorys_new_ancestors_once_unless_walked() {
    crate::in_context(async {
        use lore_storage::hash::hash_string;

        let tree = crate::harness::fixtures::Tree::new(&["p/d/f", "q/r/"]).await;
        let handle = crate::capi::Handle::load().await;
        let directory = handle
            .seed(
                lore_revision::node::ROOT_NODE,
                "d",
                lore_revision::interface::LoreNodeType::Directory,
            )
            .await;
        let destination = handle
            .seed(
                lore_revision::node::ROOT_NODE,
                "e",
                lore_revision::interface::LoreNodeType::Directory,
            )
            .await;
        let steps = Steps::of(lore_revision::state::node::hooks::Point::SubtreeStep);
        let _installed = lore_revision::state::node::hooks::install(steps.clone());

        let permit = tree.state.moves_permit().await.expect("the moves permit");
        tree.state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("p/d"),
                tree.id("q/r"),
                ("d", hash_string("d")),
                &permit,
                false,
            )
            .await
            .expect("the move into r");
        assert_eq!(steps.count(), 2, "the walk meets r, then q");
        tree.state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("p/d"),
                tree.id("p"),
                ("d", hash_string("d")),
                &permit,
                true,
            )
            .await
            .expect("the move back");
        assert_eq!(steps.count(), 2, "a move its caller walked walks nothing");
        tree.state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("p/d/f"),
                tree.id("q"),
                ("f", hash_string("f")),
                &permit,
                false,
            )
            .await
            .expect("the file's move");
        assert_eq!(steps.count(), 2, "a file's move walks nothing");
        tree.state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("p/d"),
                tree.id("p"),
                ("dd", hash_string("dd")),
                &permit,
                false,
            )
            .await
            .expect("the rename in place");
        assert_eq!(
            steps.count(),
            2,
            "a directory's rename in place walks nothing"
        );
        drop(permit);

        let events = crate::capi::Events::default();
        let status =
            crate::capi::move_node(handle.tree, directory, destination, "d", &events).await;
        assert_eq!(status, 0, "the C API move succeeds: {:?}", events.all());
        assert_eq!(
            steps.count(),
            2,
            "the C API's apply walks nothing its plan walked"
        );
    })
    .await;
}

/// A C API add and a modify dropped while one of their tasks parks after its climb keep the
/// handle's claim until that task is dropped: a commit's exclusive claim, taken after the drop,
/// finds the task's unfinished change noted, never the climb's marks alone.
#[tokio::test]
async fn a_dropped_call_keeps_its_claim_until_its_tasks_are_gone() {
    crate::in_context(async {
        for point in [
            lore_revision::state::node::hooks::Point::Grabbed,
            lore_revision::state::node::hooks::Point::BeforeRewrite,
        ] {
            let handle = crate::capi::Handle::load().await;
            let directory = handle
                .seed(
                    lore_revision::node::ROOT_NODE,
                    "d",
                    lore_revision::interface::LoreNodeType::Directory,
                )
                .await;
            let file = handle
                .seed(directory, "z", lore_revision::interface::LoreNodeType::File)
                .await;
            handle.settle(&[directory, file]).await;
            let state = handle.state();
            let parks = crate::park::Parks::at(point);
            let _installed = lore_revision::state::node::hooks::install(parks.clone());
            parks.arm(1);
            let events = crate::capi::Events::default();
            let mut call: crate::park::Call<'_> =
                if point == lore_revision::state::node::hooks::Point::Grabbed {
                    Box::pin(crate::capi::add(
                        handle.tree,
                        directory,
                        "x",
                        lore_revision::interface::LoreNodeType::File,
                        &events,
                    ))
                } else {
                    Box::pin(crate::capi::modify(handle.tree, file, &events))
                };
            crate::park::until_parked(&mut call, &parks).await;
            assert!(
                !state.has_unfinished_change(),
                "nothing noted while the task is parked"
            );
            drop(call);
            assert!(
                !state.has_unfinished_change(),
                "the aborted task is not dropped until the runtime polls it"
            );
            let internal = lore::revision_tree::handle::lookup(handle.tree).expect("the handle");
            let _commit_claim = internal.access_exclusive().await;
            assert!(
                state.has_unfinished_change(),
                "the task dropped at {point:?} noted its change before the claim was released"
            );
        }
    })
    .await;
}
