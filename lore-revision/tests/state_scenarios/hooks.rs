// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The checks and guards the scheduling hooks carry, each driven one call at a time: the tests
//! link a lore-revision built with the hooks; an add, a build and a staged move trust their
//! caller's name hash, which the hooks check; the C API's staged changes dropped inside their
//! climbs note an unfinished change where they marked a directory (C12), and climb nothing in a
//! directory already staged and dirty; a staged change refused, or whose node moved, between its
//! climb and its change; and a staged move making a cycle ends with an error.

use crate::harness::catalogue::ops::file;

/// The tests drive the node operations through their scheduling points, so the lore-revision
/// they link carries them.
#[test]
fn tests_link_lore_revision_with_its_scheduling_hooks() {
    const { assert!(lore_revision::state::node::HOOKS_ENABLED) };
}

/// A file `name` carrying the hash of another name.
fn misnamed(name: &str) -> lore_revision::node::Node {
    lore_revision::node::Node {
        flags: lore_revision::node::NodeFlags::File.bits(),
        name_hash: lore_storage::hash::hash_string(&format!("not {name}")),
        ..Default::default()
    }
}

/// An add trusts its caller's name hash, which the scheduling hooks check.
#[tokio::test]
#[should_panic(expected = "a node added as 'f' carries another name's hash")]
async fn an_add_of_a_node_carrying_another_names_hash_is_a_bug() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/"]).await;
        let _ = tree
            .state
            .node_add(tree.repository.clone(), tree.id("p"), misnamed("f"), "f")
            .await;
    })
    .await;
}

/// A swap's build trusts its caller's name hash for every node it adds, which the scheduling
/// hooks check.
#[tokio::test]
#[should_panic(expected = "a node added as 'f' carries another name's hash")]
async fn a_build_adding_a_node_carrying_another_names_hash_is_a_bug() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/"]).await;
        let root = lore_revision::node::Node {
            name_hash: lore_storage::hash::hash_string("d"),
            ..Default::default()
        };
        let Ok(lore_revision::state::TreeAttach::Building(mut build)) = tree
            .state
            .swap()
            .node_attach_tree(tree.repository.clone(), tree.id("p"), &root, "d")
            .await
        else {
            panic!("the build begins");
        };
        let _ = build.add(build.root(), &misnamed("f"), "f").await;
    })
    .await;
}

/// A staged move trusts its caller's name hash, which the scheduling hooks check.
#[tokio::test]
#[should_panic(expected = "a node moved as 'y' carries another name's hash")]
async fn a_staged_move_carrying_another_names_hash_is_a_bug() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/x", "q/"]).await;
        let permit = tree.state.moves_permit().await.expect("the moves permit");
        let _ = tree
            .state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("p/x"),
                tree.id("q"),
                ("y", lore_storage::hash::hash_string("not y")),
                &permit,
                false,
            )
            .await;
    })
    .await;
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

/// Parks for good at the `nth` time a task reaches `point`, counting from one, and nowhere else.
struct ParkAt {
    point: lore_revision::state::node::hooks::Point,
    nth: usize,
    seen: std::sync::atomic::AtomicUsize,
}

impl lore_revision::state::node::hooks::Scheduler for ParkAt {
    fn reached(
        &self,
        point: lore_revision::state::node::hooks::Point,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        (point == self.point
            && self.seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == self.nth)
            .then(|| Box::pin(std::future::pending()) as _)
    }

    fn touched(&self, _: lore_revision::state::node::hooks::Resource, _: bool) {}
}

/// Polls `change` until it parks at the `nth` time it reaches `point`, then drops it there.
/// Answers whether `state` then notes an unfinished change.
fn dropped_at(
    state: &lore_revision::state::State,
    change: impl std::future::Future,
    point: lore_revision::state::node::hooks::Point,
    nth: usize,
) -> bool {
    let parked = std::sync::Arc::new(ParkAt {
        point,
        nth,
        seen: std::sync::atomic::AtomicUsize::new(0),
    });
    let _installed = lore_revision::state::node::hooks::install(parked.clone());
    let mut change = Box::pin(change);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(change.as_mut(), &mut context).is_pending(),
        "the change is pending"
    );
    assert_eq!(
        parked.seen.load(std::sync::atomic::Ordering::Relaxed),
        nth,
        "the change parks at {point:?} the {nth}th time it reaches it"
    );
    drop(change);
    state.has_unfinished_change()
}

/// A staged move of `node` into `destination` (the root where `None`) as `name`, dropped at the
/// `nth` time it reaches `point`. Answers whether it noted an unfinished change.
async fn moved_until(
    tree: &crate::harness::fixtures::Tree,
    (node, destination, name): (&str, Option<&str>, &str),
    point: lore_revision::state::node::hooks::Point,
    nth: usize,
) -> bool {
    let permit = tree.state.moves_permit().await.expect("the moves permit");
    let destination = destination.map_or(lore_revision::node::ROOT_NODE, |path| tree.id(path));
    let moving = tree.state.node_move_staged(
        tree.repository.clone(),
        tree.id(node),
        destination,
        (name, lore_storage::hash::hash_string(name)),
        &permit,
        false,
    );
    dropped_at(&tree.state, moving, point, nth)
}

/// A staged move dropped while it marks the directory it leaves, between that directory's mark
/// and its parent's, notes an unfinished change: the guard is armed before the first mark.
#[tokio::test]
async fn a_staged_move_dropped_inside_its_first_mark_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["a/p/x", "q/"]).await;
        let noted = moved_until(
            &tree,
            ("a/p/x", Some("q"), "x"),
            lore_revision::state::node::hooks::Point::ClimbStep,
            2,
        )
        .await;
        let p = tree
            .state
            .node(tree.repository.clone(), tree.id("a/p"))
            .await
            .expect("p reads");
        let unmarked = async |path| {
            !tree
                .state
                .node(tree.repository.clone(), tree.id(path))
                .await
                .expect("the directory reads")
                .is_staged()
        };
        assert!(
            p.is_staged() && unmarked("a").await && unmarked("q").await,
            "the move is dropped inside its first mark, past p and short of a and of q"
        );
        assert!(noted, "the move dropped after a mark notes it");
    })
    .await;
}

/// A staged move dropped before its section notes an unfinished change where it marked a
/// directory, a rename in place in one, and not where it marks none, a rename under the root.
#[tokio::test]
async fn a_staged_move_dropped_before_its_section_notes_it_where_it_marked() {
    crate::in_context(async {
        let point = lore_revision::state::node::hooks::Point::BeforeMoveLock;
        let tree = crate::harness::fixtures::Tree::new(&["p/x"]).await;
        assert!(
            moved_until(&tree, ("p/x", Some("p"), "w"), point, 1).await,
            "a rename in p notes it"
        );
        let tree = crate::harness::fixtures::Tree::new(&["x"]).await;
        assert!(
            !moved_until(&tree, ("x", None, "w"), point, 1).await,
            "a rename under the root notes nothing"
        );
    })
    .await;
}

/// Panics past a number of `Point::ClimbStep`s, parking nowhere: a walk that does not end fails
/// instead of hanging.
struct ClimbLimit(std::sync::atomic::AtomicUsize);

impl lore_revision::state::node::hooks::Scheduler for ClimbLimit {
    fn reached(
        &self,
        point: lore_revision::state::node::hooks::Point,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        if point == lore_revision::state::node::hooks::Point::ClimbStep {
            let steps = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(steps < 1000, "a mark walk does not end");
        }
        None
    }

    fn touched(&self, _: lore_revision::state::node::hooks::Resource, _: bool) {}
}

/// A staged move whose caller wrongly says it walked, of a directory into its own subtree, makes
/// a cycle its subtree marks would walk for good: they answer an internal error instead.
#[tokio::test]
async fn a_staged_move_making_a_cycle_ends_with_an_error() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["d/e/"]).await;
        let _installed = lore_revision::state::node::hooks::install(std::sync::Arc::new(
            ClimbLimit(std::sync::atomic::AtomicUsize::new(0)),
        ));
        let permit = tree.state.moves_permit().await.expect("the moves permit");
        let failure = tree
            .state
            .node_move_staged(
                tree.repository.clone(),
                tree.id("d"),
                tree.id("d/e"),
                ("d", lore_storage::hash::hash_string("d")),
                &permit,
                true,
            )
            .await
            .expect_err("the cycle fails the move");
        assert!(
            failure.is_internal(),
            "the cycle fails the move with an internal error, got {failure}"
        );
    })
    .await;
}

/// Parks the first time a task reaches `point` until released, and nowhere else.
struct ParkUntil {
    point: lore_revision::state::node::hooks::Point,
    parked: std::sync::atomic::AtomicBool,
    release: std::sync::Arc<tokio::sync::Notify>,
}

impl ParkUntil {
    fn at(point: lore_revision::state::node::hooks::Point) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            point,
            parked: std::sync::atomic::AtomicBool::new(false),
            release: std::sync::Arc::new(tokio::sync::Notify::new()),
        })
    }

    fn parked(&self) -> bool {
        self.parked.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl lore_revision::state::node::hooks::Scheduler for ParkUntil {
    fn reached(
        &self,
        point: lore_revision::state::node::hooks::Point,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> {
        if point != self.point || self.parked.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let release = self.release.clone();
        Some(Box::pin(async move { release.notified().await }))
    }

    fn touched(&self, _: lore_revision::state::node::hooks::Resource, _: bool) {}
}

/// The node at `path` of `tree`, read back.
async fn read(tree: &crate::harness::fixtures::Tree, path: &str) -> lore_revision::node::Node {
    tree.state
        .node(tree.repository.clone(), tree.id(path))
        .await
        .expect("the node reads")
}

/// Directory `path` of `tree` marked staged and dirty, as a change below it leaves it.
async fn mark(tree: &crate::harness::fixtures::Tree, path: &str) {
    let mark = lore_revision::state::Mark {
        staged: Some((lore_revision::node::NodeFlags::Staged, false)),
        dirty: Some((
            lore_revision::node::NodeFlags::Dirty,
            lore_revision::state::Ext::Keep,
            false,
        )),
    };
    tree.state
        .node_mark(&tree.repository, tree.id(path), mark)
        .await
        .expect("the mark");
}

/// A C API add dropped while it marks its parent's ancestors, past the parent and short of its
/// parent's parent, notes an unfinished change and has published nothing: the parent is marked
/// before the node publishes (H8).
#[tokio::test]
async fn a_staged_add_dropped_inside_its_climb_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["a/p/"]).await;
        let adding =
            tree.state
                .node_add_staged(tree.repository.clone(), tree.id("a/p"), file("x"), "x");
        let noted = dropped_at(
            &tree.state,
            adding,
            lore_revision::state::node::hooks::Point::ClimbStep,
            2,
        );
        let p = read(&tree, "a/p").await;
        assert!(
            p.is_staged() && p.is_dirty() && !read(&tree, "a").await.is_staged(),
            "the add is dropped inside its climb, past p and short of a"
        );
        assert_eq!(p.child(), None, "nothing published into p");
        assert!(noted, "the add dropped after its first mark notes it");
    })
    .await;
}

/// A C API add that climbs nothing, under the root or into a directory already staged and dirty,
/// notes nothing where it is dropped before its publish.
#[tokio::test]
async fn a_staged_add_that_climbs_nothing_notes_nothing_where_dropped() {
    crate::in_context(async {
        let point = lore_revision::state::node::hooks::Point::Initialized;
        let tree = crate::harness::fixtures::Tree::new(&["p/"]).await;
        let adding = tree.state.node_add_staged(
            tree.repository.clone(),
            lore_revision::node::ROOT_NODE,
            file("x"),
            "x",
        );
        assert!(
            !dropped_at(&tree.state, adding, point, 1),
            "an add under the root notes nothing"
        );
        mark(&tree, "p").await;
        let adding =
            tree.state
                .node_add_staged(tree.repository.clone(), tree.id("p"), file("x"), "x");
        assert!(
            !dropped_at(&tree.state, adding, point, 1),
            "an add into a marked directory notes nothing"
        );
    })
    .await;
}

/// A C API add whose parent a delete stages for deletion between the add's climb and its publish
/// fails, refused by the publish, and notes an unfinished change.
#[tokio::test]
async fn a_staged_add_refused_after_its_climb_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/"]).await;
        let parked = ParkUntil::at(lore_revision::state::node::hooks::Point::Grabbed);
        let _installed = lore_revision::state::node::hooks::install(parked.clone());
        let mut adding = Box::pin(tree.state.node_add_staged(
            tree.repository.clone(),
            tree.id("p"),
            file("x"),
            "x",
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(adding.as_mut(), &mut context).is_pending()
                && parked.parked(),
            "the add parks after its climb"
        );
        assert!(read(&tree, "p").await.is_staged(), "the climb marked p");
        let level = tree
            .state
            .node_delete_level(
                &tree.repository,
                tree.id("p"),
                lore_revision::node::ROOT_NODE,
            )
            .await
            .expect("the delete of p");
        assert!(
            matches!(level, lore_revision::state::LevelMark::Marked(_)),
            "p is staged for deletion: {level:?}"
        );
        assert!(
            !tree.state.has_unfinished_change(),
            "the delete marks under the root and notes nothing"
        );
        parked.release.notify_one();
        adding
            .await
            .expect_err("the publish refuses a parent staged for deletion");
        assert!(
            tree.state.has_unfinished_change(),
            "the add refused after its climb notes it"
        );
    })
    .await;
}

/// A C API modify dropped after its climb, before its rewrite, notes an unfinished change: the
/// parent is marked and the file unchanged. One under the root, which marks nothing, and one in
/// a directory already staged and dirty note nothing.
#[tokio::test]
async fn a_staged_modify_dropped_before_its_rewrite_notes_it_where_it_climbed() {
    crate::in_context(async {
        let point = lore_revision::state::node::hooks::Point::BeforeRewrite;
        let content = lore_revision::state::Content {
            size: 7,
            ..Default::default()
        };
        let tree = crate::harness::fixtures::Tree::new(&["p/z", "y"]).await;
        let modifying = tree
            .state
            .node_modify_staged(&tree.repository, tree.id("p/z"), &content);
        assert!(
            dropped_at(&tree.state, modifying, point, 1),
            "the modify dropped after its climb notes it"
        );
        assert!(
            read(&tree, "p").await.is_staged() && !read(&tree, "p/z").await.is_staged(),
            "p is marked and z unchanged"
        );

        let tree = crate::harness::fixtures::Tree::new(&["p/z", "y"]).await;
        let modifying = tree
            .state
            .node_modify_staged(&tree.repository, tree.id("y"), &content);
        assert!(
            !dropped_at(&tree.state, modifying, point, 1),
            "a modify under the root notes nothing"
        );
        mark(&tree, "p").await;
        let modifying = tree
            .state
            .node_modify_staged(&tree.repository, tree.id("p/z"), &content);
        assert!(
            !dropped_at(&tree.state, modifying, point, 1),
            "a modify in a marked directory notes nothing"
        );
    })
    .await;
}

/// A C API modify whose file another task moves, marking nothing, between the modify's climb and
/// its rewrite climbs from the file's new parent too.
#[tokio::test]
async fn a_staged_modify_of_a_file_moved_meanwhile_marks_its_new_parent() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/z", "q/"]).await;
        let content = lore_revision::state::Content {
            size: 7,
            ..Default::default()
        };
        let parked = ParkUntil::at(lore_revision::state::node::hooks::Point::BeforeRewrite);
        let _installed = lore_revision::state::node::hooks::install(parked.clone());
        let z = tree.id("p/z");
        let mut modifying = Box::pin(tree.state.node_modify_staged(&tree.repository, z, &content));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(modifying.as_mut(), &mut context).is_pending()
                && parked.parked(),
            "the modify parks before its rewrite"
        );
        let own = lore_revision::state::Mark::dirty(
            lore_revision::node::NodeFlags::DirtyMove,
            lore_revision::state::Ext::Set(lore_revision::node::NodeExtFlags::NoFlags),
            true,
        );
        tree.state
            .node_move(
                tree.repository.clone(),
                z,
                tree.id("q"),
                lore_revision::state::NameChange::Keep,
                own,
            )
            .await
            .expect("the move of z into q");
        assert!(
            !read(&tree, "q").await.is_staged(),
            "the move marked nothing"
        );
        parked.release.notify_one();
        modifying.await.expect("the modify");
        let q = read(&tree, "q").await;
        assert!(
            q.is_staged() && q.is_dirty(),
            "the modify marked q, the file's parent when it wrote"
        );
        assert!(
            !tree.state.has_unfinished_change(),
            "a modify that wrote notes nothing"
        );
    })
    .await;
}

/// A restore whose node other tasks restore, move and stage for deletion again between the
/// restore's read of the node's parent and its locks finds the node moved under its locks and
/// restores it under its new parent. Its old parent is staged for deletion meanwhile, whose word
/// would refuse the restore.
#[tokio::test]
async fn a_restore_of_a_node_moved_meanwhile_restores_it_under_its_new_parent() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/z", "q/"]).await;
        let deleted =
            lore_revision::state::Mark::staged(lore_revision::node::NodeFlags::StagedDelete, true);
        let content = lore_revision::state::Content::default();
        let z = tree.id("p/z");
        tree.state
            .node_mark(&tree.repository, z, deleted)
            .await
            .expect("z staged for deletion");
        let parked = ParkUntil::at(lore_revision::state::node::hooks::Point::RestoreLock);
        let _installed = lore_revision::state::node::hooks::install(parked.clone());
        let mut restoring = Box::pin(tree.state.node_restore(&tree.repository, z, &content));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(restoring.as_mut(), &mut context).is_pending()
                && parked.parked(),
            "the restore parks before its locks, its parent read"
        );
        tree.state
            .node_restore(&tree.repository, z, &content)
            .await
            .expect("another restore of z");
        tree.state
            .node_move(
                tree.repository.clone(),
                z,
                tree.id("q"),
                lore_revision::state::NameChange::Keep,
                lore_revision::state::Mark::staged(
                    lore_revision::node::NodeFlags::StagedMove,
                    true,
                ),
            )
            .await
            .expect("the move of z into q");
        for node in [z, tree.id("p")] {
            tree.state
                .node_mark(&tree.repository, node, deleted)
                .await
                .expect("staged for deletion again");
        }
        parked.release.notify_one();
        restoring
            .await
            .expect("the restore finds z under q, not under p");
        let restored = tree
            .state
            .node(tree.repository.clone(), z)
            .await
            .expect("z reads");
        assert_eq!(restored.parent, tree.id("q"));
        assert!(!restored.is_staged_delete(), "z is restored");
    })
    .await;
}

/// A C API delete's mark of a level dropped while it marks the node's ancestors, past the parent
/// and short of its parent's parent, notes an unfinished change and leaves the node unmarked: the
/// parent is marked before the node (H8). One under the root marks the node and notes nothing.
#[tokio::test]
async fn a_level_mark_dropped_inside_its_climb_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["a/p/x", "y"]).await;
        let marking =
            tree.state
                .node_delete_level(&tree.repository, tree.id("a/p/x"), tree.id("a/p"));
        let noted = dropped_at(
            &tree.state,
            marking,
            lore_revision::state::node::hooks::Point::ClimbStep,
            2,
        );
        assert!(
            read(&tree, "a/p").await.is_staged() && !read(&tree, "a").await.is_staged(),
            "the mark is dropped inside its climb, past p and short of a"
        );
        assert!(
            !read(&tree, "a/p/x").await.is_staged(),
            "x is not marked before its parents"
        );
        assert!(
            noted,
            "the mark dropped after its first climb step notes it"
        );

        let tree = crate::harness::fixtures::Tree::new(&["a/p/x", "y"]).await;
        let level = tree
            .state
            .node_delete_level(
                &tree.repository,
                tree.id("y"),
                lore_revision::node::ROOT_NODE,
            )
            .await
            .expect("the delete of y");
        assert!(
            matches!(level, lore_revision::state::LevelMark::Marked(None)),
            "y is staged for deletion: {level:?}"
        );
        assert!(
            read(&tree, "y").await.is_staged_delete() && !tree.state.has_unfinished_change(),
            "a delete under the root marks y and notes nothing"
        );
    })
    .await;
}

/// A C API add, modify and delete in a directory already staged and dirty climb nothing: none
/// reaches a climb step.
#[tokio::test]
async fn staged_changes_in_a_marked_directory_climb_nothing() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/z", "p/y"]).await;
        mark(&tree, "p").await;
        let steps = Steps::of(lore_revision::state::node::hooks::Point::ClimbStep);
        let _installed = lore_revision::state::node::hooks::install(steps.clone());
        tree.state
            .node_add_staged(tree.repository.clone(), tree.id("p"), file("x"), "x")
            .await
            .expect("the add");
        let content = lore_revision::state::Content {
            size: 7,
            ..Default::default()
        };
        tree.state
            .node_modify_staged(&tree.repository, tree.id("p/z"), &content)
            .await
            .expect("the modify");
        let level = tree
            .state
            .node_delete_level(&tree.repository, tree.id("p/y"), tree.id("p"))
            .await
            .expect("the delete of y");
        assert_eq!(level, lore_revision::state::LevelMark::Marked(None));
        assert_eq!(steps.count(), 0, "no change climbs from a marked directory");

        let tree = crate::harness::fixtures::Tree::new(&["p/z"]).await;
        tree.state
            .node_modify_staged(&tree.repository, tree.id("p/z"), &content)
            .await
            .expect("the modify");
        assert_eq!(
            steps.count(),
            1,
            "a modify in a clean directory climbs one level"
        );
    })
    .await;
}

/// A C API delete's mark of a level whose node another task moves out of its parent, marking
/// nothing, during the level's climb answers `Other` and notes an unfinished change: the climb
/// staged the parent, which may hold no change.
#[tokio::test]
async fn a_level_mark_whose_node_left_during_its_climb_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/x", "q/"]).await;
        let parked = ParkUntil::at(lore_revision::state::node::hooks::Point::ClimbStep);
        let _installed = lore_revision::state::node::hooks::install(parked.clone());
        let x = tree.id("p/x");
        let mut marking = Box::pin(
            tree.state
                .node_delete_level(&tree.repository, x, tree.id("p")),
        );
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(marking.as_mut(), &mut context).is_pending()
                && parked.parked(),
            "the mark parks in its climb"
        );
        let own = lore_revision::state::Mark::dirty(
            lore_revision::node::NodeFlags::DirtyMove,
            lore_revision::state::Ext::Set(lore_revision::node::NodeExtFlags::NoFlags),
            true,
        );
        tree.state
            .node_move(
                tree.repository.clone(),
                x,
                tree.id("q"),
                lore_revision::state::NameChange::Keep,
                own,
            )
            .await
            .expect("the move of x into q");
        parked.release.notify_one();
        let level = marking.await.expect("the level's mark");
        assert_eq!(level, lore_revision::state::LevelMark::Other);
        assert!(read(&tree, "p").await.is_staged(), "the climb staged p");
        let moved = tree
            .state
            .node(tree.repository.clone(), x)
            .await
            .expect("x reads");
        assert!(!moved.is_staged_delete(), "x is not marked");
        assert!(
            tree.state.has_unfinished_change(),
            "a level that climbed and marked nothing notes it"
        );
    })
    .await;
}

/// A C API modify whose file a delete stages for deletion between the modify's climb and its
/// rewrite fails, refused under the lock, and notes an unfinished change.
#[tokio::test]
async fn a_staged_modify_refused_after_its_climb_notes_it() {
    crate::in_context(async {
        let tree = crate::harness::fixtures::Tree::new(&["p/z"]).await;
        let content = lore_revision::state::Content {
            size: 7,
            ..Default::default()
        };
        let parked = ParkUntil::at(lore_revision::state::node::hooks::Point::BeforeRewrite);
        let _installed = lore_revision::state::node::hooks::install(parked.clone());
        let z = tree.id("p/z");
        let mut modifying = Box::pin(tree.state.node_modify_staged(&tree.repository, z, &content));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(modifying.as_mut(), &mut context).is_pending()
                && parked.parked(),
            "the modify parks before its rewrite"
        );
        assert!(read(&tree, "p").await.is_staged(), "the climb marked p");
        let level = tree
            .state
            .node_delete_level(&tree.repository, z, tree.id("p"))
            .await
            .expect("the delete of z");
        assert_eq!(level, lore_revision::state::LevelMark::Marked(None));
        assert!(
            !tree.state.has_unfinished_change(),
            "the delete climbs nothing"
        );
        parked.release.notify_one();
        let refused = modifying
            .await
            .expect_err("the rewrite refuses a file staged for deletion");
        assert!(refused.is_invalid_arguments(), "{refused}");
        assert!(
            tree.state.has_unfinished_change(),
            "the modify refused after its climb notes it"
        );
    })
    .await;
}

/// A C API modify of a file in another block than its parent's, on a State loaded afresh, reads
/// the parent's flags from the parent's block, loading it: it climbs where the parent lacks a
/// base bit, and not where the parent holds both.
#[tokio::test]
async fn a_staged_modify_reads_a_parent_in_another_block() {
    crate::in_context(async {
        let content = lore_revision::state::Content {
            size: 7,
            ..Default::default()
        };
        for marked in [false, true] {
            // Marked before the save, so the State loaded afresh holds neither block, and the
            // modify loads p's block in both halves.
            let tree = crate::harness::fixtures::Tree::spread(&["p/"], "f", &["p/z"]).await;
            if marked {
                mark(&tree, "p").await;
            }
            let tree = tree.reloaded().await;
            assert_ne!(
                lore_revision::node::NodeBlock::index(tree.id("p")),
                lore_revision::node::NodeBlock::index(tree.id("p/z")),
                "p and z lie in different blocks"
            );
            let steps = Steps::of(lore_revision::state::node::hooks::Point::ClimbStep);
            let _installed = lore_revision::state::node::hooks::install(steps.clone());
            tree.state
                .node_modify_staged(&tree.repository, tree.id("p/z"), &content)
                .await
                .expect("the modify");
            let p = read(&tree, "p").await;
            assert!(p.is_staged() && p.is_dirty(), "p is marked");
            assert_eq!(
                steps.count(),
                usize::from(!marked),
                "the modify climbs from p only where p lacks a base bit"
            );
        }
    })
    .await;
}
