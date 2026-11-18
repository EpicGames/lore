// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! What an SWFS operation realizes on the mount when it finalizes, and what it leaves to the thaw
//! to describe, below a link (ADR-00033, ADR-00036).
// Each test drives an operation over its mount resources and a revision, whose future is
// larger than the library's threshold; a test runs it once.
#![allow(clippy::large_futures)]
// The fixtures write and remove the files a mount holds directly, as a user would.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;

use bytes::Bytes;
use lore_base::test_util::TempDir;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::swfs::api_interface::SwfsInterface;
use lore_revision::fs::swfs::filesystem::SwfsOperation;
use lore_revision::fs::swfs::frozen_files::FrozenSnapshot;
use lore_revision::fs::swfs::modified_tree::Change;
use lore_revision::fs::swfs::modified_tree::Coverage;
use lore_revision::fs::swfs::mount_resources::MountResources;
use lore_revision::fs::swfs::mount_resources::SwfsExecutionToken;
use lore_revision::fs::swfs::paths::MountPath;
use lore_revision::instance::store_current_anchor;
use lore_revision::instance::store_current_anchor_branch;
use lore_revision::instance::store_staged_anchor;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::node::Node;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;

use super::fake_frozen::FakeFrozen;
use super::listing_fixture::Held;
use super::listing_fixture::in_context;
use super::listing_fixture::node;
use super::listing_fixture::revision_state;
use super::listing_fixture::stored;
use super::listing_fixture::writable_test_repository;

struct Fixture {
    operation: SwfsOperation,
    linked: Arc<RepositoryContext>,
    /// `lib.txt` as the link's pin holds it.
    pinned: Node,
    /// `lib.txt` as another revision of the linked repository holds it.
    other: Node,
    mount: TempDir,
}

/// An operation over a mount whose stored current revision links `lib` to a repository whose
/// `src` holds `lib.txt` as `other`, and whose stored staged state repins the link to the revision
/// holding it as `pinned`: what a plain working tree holds after `link update`.
async fn fixture() -> Fixture {
    let mount = TempDir::new("swfs-realize");
    let repository = writable_test_repository().await;
    let linked_id = RepositoryId::from([0x11; 16]);
    let linked = repository.to_link_context(linked_id).await;
    let pinned_state = revision_state(&linked, &[("src/lib.txt", Held::File(b"pinned"))]).await;
    let other_state = revision_state(&linked, &[("src/lib.txt", Held::File(b"other"))]).await;
    let pin = stored(&linked, &pinned_state).await;
    let other_pin = stored(&linked, &other_state).await;
    let file = async |state: &Arc<State>| {
        state
            .find_node(linked.clone(), "src/lib.txt")
            .await
            .expect("finding lib.txt")
    };
    let pinned = file(&pinned_state).await;
    let other = file(&other_state).await;
    let source = async |state: &Arc<State>| {
        state
            .find_node_link(linked.clone(), "src")
            .await
            .expect("finding src")
            .node
    };
    let link_at = |pin, source| {
        [(
            "lib",
            Held::LinkTo {
                repository: linked_id,
                pin,
                source,
            },
        )]
    };
    let current =
        revision_state(&repository, &link_at(other_pin, source(&other_state).await)).await;
    let staged = revision_state(&repository, &link_at(pin, source(&pinned_state).await)).await;
    let current_revision = stored(&repository, &current).await;
    let staged_revision = stored(&repository, &staged).await;
    store_current_anchor(&repository, current_revision)
        .await
        .expect("storing the current anchor");
    store_staged_anchor(&repository, staged_revision)
        .await
        .expect("storing the staged anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository, None, mount_path).await;
    resources.modified().set_trusted(true);
    let operation = SwfsOperation::for_test(SwfsInterface::unmounted(resources), &mount_root);
    Fixture {
        operation,
        linked,
        pinned,
        other,
        mount,
    }
}

#[tokio::test]
async fn a_linked_file_the_pin_holds_is_left_to_the_thaw() {
    in_context(async {
        let fixture = fixture().await;
        fixture
            .operation
            .write_node(
                fixture.linked.clone(),
                &fixture.pinned,
                &node("lib/lib.txt"),
            )
            .await
            .expect("writing the linked file");

        fixture
            .operation
            .realize_for_test()
            .await
            .expect("realizing");

        assert!(
            !fixture.mount.path().join("mount/lib/lib.txt").exists(),
            "the linked file was written to the mount"
        );
        assert_eq!(fixture.operation.described_for_test(), ["lib/lib.txt"]);
    })
    .await;
}

#[tokio::test]
async fn a_linked_file_the_pin_does_not_hold_is_written_to_the_mount() {
    in_context(async {
        let fixture = fixture().await;
        fixture
            .operation
            .write_node(fixture.linked.clone(), &fixture.other, &node("lib/lib.txt"))
            .await
            .expect("writing the linked file");

        fixture
            .operation
            .realize_for_test()
            .await
            .expect("realizing");

        assert_eq!(
            std::fs::read(fixture.mount.path().join("mount/lib/lib.txt")).ok(),
            Some(b"other".to_vec())
        );
        assert!(fixture.operation.described_for_test().is_empty());
    })
    .await;
}

/// An operation begun on a mount serving `lib` at the revision holding `lib.txt` as `pinned`,
/// over which the instance's stored staged state then repins `lib` to a revision holding it as
/// `moved` beside a new `added.txt`, the operation writing nothing below `lib`.
async fn repinned_without_writing() -> (SwfsOperation, TempDir) {
    let mount = TempDir::new("swfs-realize");
    let repository = writable_test_repository().await;
    let linked_id = RepositoryId::from([0x11; 16]);
    let linked = repository.to_link_context(linked_id).await;
    let before = revision_state(&linked, &[("src/lib.txt", Held::File(b"pinned"))]).await;
    let after = revision_state(
        &linked,
        &[
            ("src/lib.txt", Held::File(b"moved")),
            ("src/added.txt", Held::File(b"added")),
        ],
    )
    .await;
    let link_at = async |state: &Arc<State>| {
        let source = state
            .find_node_link(linked.clone(), "src")
            .await
            .expect("finding src")
            .node;
        let pin = stored(&linked, state).await;
        revision_state(
            &repository,
            &[(
                "lib",
                Held::LinkTo {
                    repository: linked_id,
                    pin,
                    source,
                },
            )],
        )
        .await
    };
    let current = link_at(&before).await;
    let staged = link_at(&after).await;
    let current_revision = stored(&repository, &current).await;
    store_current_anchor(&repository, current_revision)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    // What an SWFS mount holds as the operation begins: the old pin's file, which the operation
    // rewrites to keep it as the user's own.
    std::fs::create_dir_all(mount_root.join("lib")).expect("making the link directory");
    std::fs::write(mount_root.join("lib/lib.txt"), b"stale").expect("writing the mounted file");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository.clone(), None, mount_path).await;
    resources.modified().set_trusted(true);
    resources
        .run_with_context(resources.update_state(SwfsExecutionToken::for_test()))
        .await
        .expect("loading what the mount serves when the operation begins");
    let operation = SwfsOperation::for_test(SwfsInterface::unmounted(resources), &mount_root);
    let staged_revision = stored(&repository, &staged).await;
    store_staged_anchor(&repository, staged_revision)
        .await
        .expect("storing the staged anchor");
    (operation, mount)
}

#[tokio::test]
async fn a_repin_that_writes_nothing_below_the_link_keeps_what_the_mount_showed() {
    in_context(async {
        let (operation, mount) = repinned_without_writing().await;

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(
            std::fs::read(mount.path().join("mount/lib/lib.txt")).ok(),
            Some(b"pinned".to_vec()),
            "the file the new pin changed did not keep the old pin's content"
        );
        assert!(
            operation.hides_for_test(&node("lib/added.txt")),
            "the file only the new pin holds is served"
        );
    })
    .await;
}

/// An operation begun on a mount that has described `lib` linked at the revision holding
/// `lib.txt` as `pinned`, after which the instance stores a staged state linking `lib` at the
/// revision holding it as `moved`, or, with `unlinked`, not linking it at all. Returns the
/// operation, the mount, the linked repository and the `moved` revision's `lib.txt`.
async fn described_then_restaged(
    unlinked: bool,
) -> (SwfsOperation, TempDir, Arc<RepositoryContext>, Node) {
    let mount = TempDir::new("swfs-realize");
    let repository = writable_test_repository().await;
    let linked_id = RepositoryId::from([0x11; 16]);
    let linked = repository.to_link_context(linked_id).await;
    let before = revision_state(&linked, &[("src/lib.txt", Held::File(b"pinned"))]).await;
    let after = revision_state(&linked, &[("src/lib.txt", Held::File(b"moved"))]).await;
    let moved = after
        .find_node(linked.clone(), "src/lib.txt")
        .await
        .expect("finding lib.txt");
    let link_at = async |state: &Arc<State>| {
        let source = state
            .find_node_link(linked.clone(), "src")
            .await
            .expect("finding src")
            .node;
        let pin = stored(&linked, state).await;
        revision_state(
            &repository,
            &[
                ("README.txt", Held::File(b"parent")),
                (
                    "lib",
                    Held::LinkTo {
                        repository: linked_id,
                        pin,
                        source,
                    },
                ),
            ],
        )
        .await
    };
    let current = link_at(&before).await;
    let staged = if unlinked {
        revision_state(&repository, &[("README.txt", Held::File(b"parent"))]).await
    } else {
        link_at(&after).await
    };
    let current_revision = stored(&repository, &current).await;
    store_current_anchor(&repository, current_revision)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository.clone(), None, mount_path).await;
    resources.modified().set_trusted(true);
    // What the mount described before the operation: the link at its first pin.
    resources
        .run_with_context(resources.update_state(SwfsExecutionToken::for_test()))
        .await
        .expect("loading what the mount serves");
    let operation = SwfsOperation::for_test(SwfsInterface::unmounted(resources), &mount_root);
    let staged_revision = stored(&repository, &staged).await;
    store_staged_anchor(&repository, staged_revision)
        .await
        .expect("storing the staged anchor");
    (operation, mount, linked, moved)
}

#[tokio::test]
async fn a_repinned_link_is_described_by_its_mount_path_alone() {
    in_context(async {
        let (operation, mount, linked, moved) = described_then_restaged(false).await;
        operation
            .write_node(linked, &moved, &node("lib/lib.txt"))
            .await
            .expect("writing the repinned file");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.described_for_test(), ["lib"]);
        assert!(!mount.path().join("mount/lib/lib.txt").exists());
    })
    .await;
}

#[tokio::test]
async fn a_removed_link_is_described_by_its_mount_path_and_not_deleted_through_the_mount() {
    in_context(async {
        let (operation, mount, _linked, _moved) = described_then_restaged(true).await;
        // What the mount holds of the user's own below the link.
        let users = mount.path().join("mount/lib/user.txt");
        std::fs::create_dir_all(users.parent().expect("a parent")).expect("making lib");
        std::fs::write(&users, b"user").expect("writing a user file");
        operation
            .remove_recursive(&node("lib"))
            .await
            .expect("removing the link's tree");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.described_for_test(), ["lib"]);
        assert!(users.exists(), "the removal went through the mount");
        assert_eq!(operation.removed_subtrees_for_test(), ["lib"]);
    })
    .await;
}

#[tokio::test]
async fn a_removed_link_the_operation_leaves_a_directory_at_is_made_after_the_thaw() {
    in_context(async {
        let (operation, _mount, _linked, _moved) = described_then_restaged(true).await;
        // What removing a link staged for add does: the tree goes, an empty directory stays.
        operation
            .remove_recursive(&node("lib"))
            .await
            .expect("removing the link's tree");
        operation
            .create_dir_all(&node("lib"))
            .await
            .expect("leaving the mount directory");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.described_for_test(), ["lib"]);
        assert_eq!(operation.recreated_mounts_for_test(), ["lib"]);
        assert!(operation.removed_subtrees_for_test().is_empty());
    })
    .await;
}

/// The link's mount path, named as a directory, has SWFS relist it at every depth and drop what
/// the new pin lacks, so the removal needs no description of its own.
#[tokio::test]
async fn a_removal_below_a_repinned_link_of_what_the_new_pin_lacks_is_left_to_the_mount_path() {
    in_context(async {
        let (operation, mount, _linked, _moved) = described_then_restaged(false).await;
        let removed = mount.path().join("mount/lib/gone.txt");
        std::fs::create_dir_all(removed.parent().expect("a parent")).expect("making lib");
        std::fs::write(&removed, b"gone").expect("writing the file");
        operation
            .remove_recursive(&node("lib/gone.txt"))
            .await
            .expect("removing the file");

        operation.realize_for_test().await.expect("realizing");

        assert!(removed.exists(), "the removal went through the mount");
        assert_eq!(operation.described_for_test(), ["lib"]);
    })
    .await;
}

#[tokio::test]
async fn a_removal_below_a_repinned_link_of_what_the_new_pin_serves_is_left_to_the_thaw() {
    in_context(async {
        let (operation, mount, _linked, _moved) = described_then_restaged(false).await;
        let served = mount.path().join("mount/lib/lib.txt");
        std::fs::create_dir_all(served.parent().expect("a parent")).expect("making lib");
        std::fs::write(&served, b"pinned").expect("writing the file");
        operation
            .remove_recursive(&node("lib/lib.txt"))
            .await
            .expect("removing the file");

        operation.realize_for_test().await.expect("realizing");

        assert!(served.exists(), "the removal went through the mount");
        assert_eq!(operation.described_for_test(), ["lib"]);
    })
    .await;
}

#[tokio::test]
async fn a_path_swfs_holds_modified_below_a_repinned_link_is_described_on_its_own() {
    in_context(async {
        let (operation, _mount, linked, moved) = described_then_restaged(false).await;
        // The user edited the file, so SWFS holds it modified, and naming the link's directory
        // would leave it so: the operation restoring it has to name it.
        operation
            .record_for_test()
            .record(Change::Written("lib/lib.txt".into()))
            .await;
        operation
            .write_node(linked, &moved, &node("lib/lib.txt"))
            .await
            .expect("writing the repinned file");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(operation.described_for_test(), ["lib", "lib/lib.txt"]);
    })
    .await;
}

#[tokio::test]
async fn removing_a_file_the_revision_still_serves_is_made_on_the_mount() {
    in_context(async {
        let (operation, mount, _linked, _moved) = described_then_restaged(false).await;
        // The parent's own file, which the stored revision keeps serving after the operation.
        let on_mount = mount.path().join("mount/README.txt");
        std::fs::write(&on_mount, b"parent").expect("holding the file on the mount");

        operation
            .remove(&node("README.txt"))
            .await
            .expect("removing the file");
        assert!(
            on_mount.exists(),
            "the removal went through the mount during the freeze"
        );

        operation.realize_for_test().await.expect("realizing");

        assert!(
            !on_mount.exists(),
            "the removal did not reach the mount when finalizing"
        );
    })
    .await;
}

#[tokio::test]
async fn a_file_deleted_on_the_mount_is_restored_by_the_thaw_not_written() {
    in_context(async {
        let (operation, mount, linked, moved) = described_then_restaged(false).await;
        // The user deleted the file through the mount; SWFS holds the deletion.
        operation
            .record_for_test()
            .record(Change::Deleted {
                path: "lib/lib.txt".into(),
                directory: false,
            })
            .await;
        operation
            .write_node(linked, &moved, &node("lib/lib.txt"))
            .await
            .expect("restoring the file");

        operation.realize_for_test().await.expect("realizing");

        // Named, with the directories leading to it, in the thaw, which brings it back from the
        // source; written through the mount, it would be a copy SWFS holds as the user's.
        assert!(!mount.path().join("mount/lib/lib.txt").exists());
        assert_eq!(operation.described_for_test(), ["lib", "lib/lib.txt"]);
        // No longer held deleted, so a scan compares the file the thaw brings back.
        assert_eq!(
            operation
                .record_for_test()
                .coverage("lib/lib.txt")
                .await
                .expect("asking the record"),
            Coverage::Changed
        );
    })
    .await;
}

/// An operation over a mount serving the parent's own directory `old`, holding `old/a.txt`, as
/// it began, and whose stored current revision then holds `old` only where `kept`: what a plain
/// working tree holds before a sync or switch to a revision that drops it, or keeps it.
async fn serving_old(kept: bool) -> (SwfsOperation, TempDir) {
    let mount = TempDir::new("swfs-realize");
    let repository = writable_test_repository().await;
    let before = revision_state(&repository, &[("old/a.txt", Held::File(b"a"))]).await;
    let after = if kept {
        revision_state(&repository, &[("old/a.txt", Held::File(b"a"))]).await
    } else {
        revision_state(&repository, &[("new.txt", Held::File(b"n"))]).await
    };
    let after_revision = stored(&repository, &after).await;
    store_current_anchor(&repository, after_revision)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(mount_root.join("old")).expect("making old on the mount");
    std::fs::write(mount_root.join("old/a.txt"), b"a").expect("holding old/a.txt on the mount");
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources = MountResources::for_test(repository, Some(before), mount_path).await;
    resources.modified().set_trusted(true);
    let operation = SwfsOperation::for_test(SwfsInterface::unmounted(resources), &mount_root);
    (operation, mount)
}

#[tokio::test]
async fn a_served_tree_the_new_revision_drops_is_described_and_not_removed_through_the_mount() {
    in_context(async {
        let (operation, mount) = serving_old(false).await;
        operation
            .remove_recursive(&node("old"))
            .await
            .expect("removing the tree");

        operation.realize_for_test().await.expect("realizing");

        // Described as holding nothing, which drops the tree in one entry; removed through the
        // mount, each directory in it would be listed there.
        assert!(operation.described_for_test().contains(&"old".to_owned()));
        assert!(mount.path().join("mount/old/a.txt").exists());
        assert_eq!(operation.removed_subtrees_for_test(), ["old"]);
    })
    .await;
}

#[tokio::test]
async fn a_served_tree_the_new_revision_keeps_is_removed_on_the_mount() {
    in_context(async {
        let (operation, mount) = serving_old(true).await;
        operation
            .remove_recursive(&node("old"))
            .await
            .expect("removing the tree");

        operation.realize_for_test().await.expect("realizing");

        // The working tree's own change, which only a removal on the mount makes.
        assert!(!mount.path().join("mount/old").exists());
        assert!(operation.removed_subtrees_for_test().is_empty());
    })
    .await;
}

/// An operation over a mount whose stored current revision holds `revision`, as it did when the
/// operation began, with SWFS holding what `frozen` holds, each of its files on the mount too, and
/// the working tree lacking each of `absent`. Returns the operation, the mount, the repository and
/// the revision's state.
async fn serving_frozen(
    revision: &[(&str, Held<'_>)],
    frozen: FakeFrozen,
    absent: &[&str],
) -> (SwfsOperation, TempDir, Arc<RepositoryContext>, Arc<State>) {
    let mount = TempDir::new("swfs-realize");
    let repository = writable_test_repository().await;
    let state = revision_state(&repository, revision).await;
    let revision_hash = stored(&repository, &state).await;
    store_current_anchor(&repository, revision_hash)
        .await
        .expect("storing the current anchor");
    store_current_anchor_branch(&repository, BranchId::from([5; 16]))
        .await
        .expect("storing the current branch");
    let mount_root = mount.path().join("mount");
    std::fs::create_dir_all(&mount_root).expect("making the mount directory");
    for (path, content) in &frozen.content {
        let on_mount = mount_root.join(path.trim_start_matches('\\').replace('\\', "/"));
        std::fs::create_dir_all(on_mount.parent().expect("a parent")).expect("making a directory");
        std::fs::write(on_mount, content).expect("holding a frozen file on the mount");
    }
    let mount_path = MountPath::new(&mount_root).expect("a mount path");
    let resources =
        MountResources::for_test(repository.clone(), Some(state.clone()), mount_path).await;
    resources.modified().set_trusted(true);
    let absent: Vec<_> = absent.iter().map(|path| node(path)).collect();
    resources.update_absent(&absent, &[]);
    let snapshot = FrozenSnapshot::capture(Arc::new(frozen)).expect("listing the frozen files");
    let operation =
        SwfsOperation::for_test_frozen(SwfsInterface::unmounted(resources), &mount_root, snapshot);
    (operation, mount, repository, state)
}

/// A file of the user's that no revision holds is removed by naming it in the thaw, which deletes
/// it, not through the mount.
#[tokio::test]
async fn removing_a_users_file_the_revision_does_not_hold_names_it_in_the_thaw() {
    in_context(async {
        let (operation, mount, _repository, _state) = serving_frozen(
            &[("tracked.txt", Held::File(b"tracked"))],
            FakeFrozen::default().with_file("\\notes.txt", b"mine"),
            &[],
        )
        .await;
        operation
            .remove(&node("notes.txt"))
            .await
            .expect("removing the user's file");

        operation.realize_for_test().await.expect("realizing");

        assert!(
            mount.path().join("mount/notes.txt").exists(),
            "the removal went through the mount"
        );
        assert_eq!(operation.described_for_test(), ["notes.txt"]);
    })
    .await;
}

/// A directory the revision serves where the user holds a file is described to the thaw, which
/// replaces the file, rather than removed through the mount.
#[tokio::test]
async fn removing_a_users_file_where_the_revision_serves_a_directory_is_left_to_the_thaw() {
    in_context(async {
        let (operation, mount, _repository, _state) = serving_frozen(
            &[("dir/a.txt", Held::File(b"a"))],
            FakeFrozen::default().with_file("\\dir", b"mine"),
            &[],
        )
        .await;
        operation
            .remove_recursive(&node("dir"))
            .await
            .expect("removing the user's file");

        operation.realize_for_test().await.expect("realizing");

        assert!(
            mount.path().join("mount/dir").is_file(),
            "the removal went through the mount"
        );
        assert_eq!(operation.described_for_test(), ["dir"]);
    })
    .await;
}

/// A file the revision holds but the working tree lacks, as one a dependency clone left out, is
/// not served, so the thaw cannot bring it in: it is written to the mount (ADR-00034).
#[tokio::test]
async fn a_file_the_revision_holds_where_the_working_tree_lacks_it_is_written_to_the_mount() {
    in_context(async {
        let (operation, mount, repository, state) = serving_frozen(
            &[("a.txt", Held::File(b"a"))],
            FakeFrozen::default(),
            &["a.txt"],
        )
        .await;
        let held = state
            .find_node(repository.clone(), "a.txt")
            .await
            .expect("finding a.txt");
        operation
            .write_node(repository, &held, &node("a.txt"))
            .await
            .expect("writing the file");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(
            std::fs::read(mount.path().join("mount/a.txt")).ok(),
            Some(b"a".to_vec())
        );
        assert!(operation.described_for_test().is_empty());
    })
    .await;
}

/// A directory the revision still serves, which the operation removed and then wrote into again,
/// on the mount and from a store, loses what it held but keeps what the operation wrote there.
#[tokio::test]
async fn what_the_operation_wrote_into_a_removed_served_directory_survives_finalizing() {
    in_context(async {
        let (operation, mount, repository, state) = serving_frozen(
            &[("old/a.txt", Held::File(b"a"))],
            FakeFrozen::default(),
            &[],
        )
        .await;
        let old = mount.path().join("mount/old");
        std::fs::create_dir_all(&old).expect("making the directory on the mount");
        std::fs::write(old.join("a.txt"), b"a").expect("holding the served file on the mount");
        let held = state
            .find_node(repository.clone(), "old/a.txt")
            .await
            .expect("finding old/a.txt");
        operation
            .remove_recursive(&node("old"))
            .await
            .expect("removing the directory");
        operation
            .write_file(&node("old/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        // Content the revision does not hold at this path, so realizing writes it to the mount.
        operation
            .write_node(repository, &held, &node("old/g.txt"))
            .await
            .expect("writing from the store");

        operation.realize_for_test().await.expect("realizing");

        assert!(
            !old.join("a.txt").exists(),
            "what the directory held stayed"
        );
        assert_eq!(std::fs::read(old.join("f.txt")).ok(), Some(b"f".to_vec()));
        assert_eq!(std::fs::read(old.join("g.txt")).ok(), Some(b"a".to_vec()));
    })
    .await;
}

/// Removing a served directory the operation wrote into again takes from the mount what the
/// directory held, the user's files included, and keeps, with the directories leading there,
/// what the operation wrote or made: a file written to the mount at any depth, a directory moved
/// there through the mount with what it holds, content realized from a store, content and a
/// directory the thaw describes. A served directory with nothing made in it goes whole.
#[tokio::test]
async fn removing_a_served_directory_keeps_each_kind_of_thing_the_operation_made_in_it() {
    in_context(async {
        let (operation, mount, repository, state) = serving_frozen(
            &[
                ("old/a.txt", Held::File(b"a")),
                ("old/held.txt", Held::File(b"held")),
                ("old/sub/b.txt", Held::File(b"b")),
                ("old/plain/c.txt", Held::File(b"c")),
            ],
            FakeFrozen::default().with_file("\\old\\user.txt", b"user"),
            &[],
        )
        .await;
        let root = mount.path().join("mount");
        for (path, content) in [
            ("old/a.txt", b"a".as_slice()),
            ("old/held.txt", b"held"),
            ("old/sub/b.txt", b"b"),
            ("old/plain/c.txt", b"c"),
            ("scratch/inner.txt", b"inner"),
        ] {
            let on_mount = root.join(path);
            std::fs::create_dir_all(on_mount.parent().expect("a parent"))
                .expect("making a directory");
            std::fs::write(on_mount, content).expect("holding a file on the mount");
        }
        let node_at = async |path: &str| {
            state
                .find_node(repository.clone(), path)
                .await
                .expect("finding a served file")
        };
        let a = node_at("old/a.txt").await;
        let held = node_at("old/held.txt").await;
        operation
            .remove_recursive(&node("old"))
            .await
            .expect("removing the directory");
        operation
            .write_file(&node("old/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        std::fs::create_dir_all(root.join("old/deep/x")).expect("making directories on the mount");
        operation
            .write_file(&node("old/deep/x/f.txt"), Bytes::from_static(b"deep"))
            .await
            .expect("writing deep on the mount");
        operation
            .rename(&node("scratch"), &node("old/moved"))
            .await
            .expect("moving a directory through the mount");
        operation
            .write_node(repository.clone(), &a, &node("old/g.txt"))
            .await
            .expect("writing what realizing puts on the mount");
        operation
            .write_node(repository.clone(), &held, &node("old/held.txt"))
            .await
            .expect("writing what the thaw describes");
        operation
            .create_dir_all(&node("old/sub"))
            .await
            .expect("making a directory the thaw describes");

        operation.realize_for_test().await.expect("realizing");

        for gone in ["old/a.txt", "old/user.txt", "old/sub/b.txt", "old/plain"] {
            assert!(!root.join(gone).exists(), "{gone} stayed on the mount");
        }
        for (kept, content) in [
            ("old/f.txt", b"f".as_slice()),
            ("old/deep/x/f.txt", b"deep"),
            ("old/moved/inner.txt", b"inner"),
            ("old/g.txt", b"a"),
            ("old/held.txt", b"held"),
        ] {
            assert_eq!(
                std::fs::read(root.join(kept)).ok().as_deref(),
                Some(content),
                "{kept}"
            );
        }
        assert!(root.join("old/sub").is_dir(), "old/sub went");
    })
    .await;
}

/// A link the user holds in a removed served directory goes, without what it leads to, and a
/// directory the operation means to make something in stays though the mount holds nothing of it
/// yet.
#[tokio::test]
async fn removing_a_served_directory_takes_links_and_keeps_directories_made_into() {
    in_context(async {
        let (operation, mount, repository, state) = serving_frozen(
            &[
                ("old/a.txt", Held::File(b"a")),
                ("old/sub/x.txt", Held::File(b"x")),
            ],
            FakeFrozen::default(),
            &[],
        )
        .await;
        let root = mount.path().join("mount");
        std::fs::create_dir_all(root.join("old/sub")).expect("making directories on the mount");
        std::fs::write(root.join("old/a.txt"), b"a").expect("holding a file on the mount");
        std::fs::create_dir_all(root.join("target")).expect("making a directory to link to");
        std::fs::write(root.join("target/keep.txt"), b"keep").expect("holding a file there");
        std::fs::create_dir_all(root.join("written")).expect("making a directory to link to");
        std::fs::create_dir_all(root.join("old/other")).expect("making a directory on the mount");
        // A junction where Windows makes one without a privilege, a symbolic link elsewhere.
        let junction = |link: &str, target: &str| {
            let (link, target) = (root.join(link), root.join(target));
            #[cfg(windows)]
            crate::util::fs::junction(&link, &target);
            #[cfg(not(windows))]
            std::os::unix::fs::symlink(&target, &link).expect("making a symbolic link");
        };
        junction("old/link", "target");
        // The only entry of a directory the removal takes whole.
        junction("old/other/link", "target");
        // One the operation writes through.
        junction("old/through", "written");
        let x = state
            .find_node(repository.clone(), "old/sub/x.txt")
            .await
            .expect("finding old/sub/x.txt");
        operation
            .remove_recursive(&node("old"))
            .await
            .expect("removing the directory");
        operation
            .write_file(&node("old/f.txt"), Bytes::from_static(b"f"))
            .await
            .expect("writing to the mount");
        operation
            .write_file(&node("old/through/y.txt"), Bytes::from_static(b"y"))
            .await
            .expect("writing through the junction");
        // What the thaw describes, which the mount does not hold.
        operation
            .write_node(repository, &x, &node("old/sub/x.txt"))
            .await
            .expect("writing what the thaw describes");

        operation.realize_for_test().await.expect("realizing");

        assert!(
            std::fs::symlink_metadata(root.join("old/link")).is_err(),
            "the link stayed"
        );
        assert!(
            root.join("target/keep.txt").exists(),
            "what the link led to went"
        );
        assert!(
            !root.join("old/a.txt").exists(),
            "what the directory held stayed"
        );
        assert!(
            root.join("old/sub").is_dir(),
            "the directory made into went"
        );
        assert!(root.join("old/f.txt").exists(), "the file written went");
        assert!(
            !root.join("old/other").exists(),
            "the directory holding a link stayed"
        );
        assert_eq!(
            std::fs::read(root.join("old/through/y.txt")).ok(),
            Some(b"y".to_vec()),
            "the file written through the junction went"
        );
    })
    .await;
}

/// A directory the user moved on the mount, which the operation moves back where the revision
/// holds it, as `lore reset` does, is read, listed and described by what it holds: each file below
/// it reads as the revision holds it at the path it came from, and the thaw names each, so SWFS,
/// which holds the moved files with no bytes of their own, takes the source's for them.
#[tokio::test]
async fn a_directory_moved_back_through_the_mount_carries_what_it_holds() {
    in_context(async {
        let (operation, mount, _repository, _state) = serving_frozen(
            &[
                ("dir/a.txt", Held::File(b"a content")),
                ("dir/sub/b.txt", Held::File(b"b content")),
            ],
            FakeFrozen::default()
                .with_directory("\\moved")
                .with_move("\\dir", "\\moved"),
            &[],
        )
        .await;
        let root = mount.path().join("mount");
        std::fs::create_dir_all(root.join("moved/sub")).expect("making directories on the mount");
        std::fs::write(root.join("moved/a.txt"), b"a content").expect("holding a file");
        std::fs::write(root.join("moved/sub/b.txt"), b"b content").expect("holding a file");
        let record = operation.record_for_test();
        record
            .record(Change::Deleted {
                path: "dir".into(),
                directory: true,
            })
            .await;
        record
            .record(Change::Created {
                path: "moved".into(),
                directory: true,
            })
            .await;

        operation
            .rename(&node("moved"), &node("dir"))
            .await
            .expect("moving the directory back");

        let a = operation
            .file_info(&node("dir/a.txt"))
            .await
            .expect("describing the file");
        assert!(a.is_file() && a.size() == 9, "{a:?}");
        let b = operation
            .file_info(&node("dir/sub/b.txt"))
            .await
            .expect("describing the file");
        assert!(b.is_file() && b.size() == 9, "{b:?}");
        let mut listing = operation
            .read_directory(&node("dir"))
            .await
            .expect("listing the directory");
        let mut names = Vec::new();
        while let Some(entry) = listing.next().await {
            names.push(entry.expect("a listing entry").name);
        }
        names.sort();
        assert_eq!(names, ["a.txt", "sub"]);

        operation.realize_for_test().await.expect("realizing");

        let described = operation.described_for_test();
        for path in ["dir/a.txt", "dir/sub/b.txt"] {
            assert!(
                described.contains(&path.to_owned()),
                "{path} not named: {described:?}"
            );
        }
    })
    .await;
}

/// What the operation reads below a directory it moves through the mount is what it read there
/// before the move, whatever kind it is: a file SWFS holds bytes of its own for stays the mount's,
/// one SWFS froze with none, renamed within the directory, is read as the revision holds it at
/// its origin, what the operation removed stays removed, and what it wrote from a store or means
/// to make moves with the directory.
#[tokio::test]
async fn a_directory_moved_through_the_mount_carries_each_kind_of_thing_below_it() {
    in_context(async {
        let (operation, mount, repository, state) = serving_frozen(
            &[
                ("dir/a.txt", Held::File(b"a content")),
                ("dir/c.txt", Held::File(b"c content!")),
                ("dir/gone.txt", Held::File(b"gone")),
            ],
            FakeFrozen::default()
                .with_directory("\\moved")
                .with_move("\\dir", "\\moved")
                .with_file("\\moved\\edited.txt", b"mine")
                .with_unreadable_frozen_file("\\moved\\renamed.txt", 10)
                .with_move("\\dir\\c.txt", "\\moved\\renamed.txt"),
            &[],
        )
        .await;
        let root = mount.path().join("mount");
        std::fs::create_dir_all(root.join("moved/empty")).expect("making directories on the mount");
        for (path, content) in [
            ("moved/a.txt", b"a content".as_slice()),
            // What the mount answers is not what the file reads as: SWFS holds no bytes of it.
            ("moved/renamed.txt", b"xyz"),
            ("moved/gone.txt", b"gone"),
        ] {
            std::fs::write(root.join(path), content).expect("holding a file on the mount");
        }
        let a = state
            .find_node(repository.clone(), "dir/a.txt")
            .await
            .expect("finding dir/a.txt");
        operation
            .remove_recursive(&node("moved/gone.txt"))
            .await
            .expect("removing a file");
        operation
            .write_node(repository, &a, &node("moved/new.txt"))
            .await
            .expect("writing from the store");
        operation
            .create_dir_all(&node("moved/made"))
            .await
            .expect("making a directory");

        operation
            .rename(&node("moved"), &node("dir"))
            .await
            .expect("moving the directory back");

        let info = async |path: &str| {
            operation
                .file_info(&node(path))
                .await
                .expect("describing a path")
        };
        assert_eq!(
            info("dir/edited.txt").await.size(),
            4,
            "the user's own bytes"
        );
        assert_eq!(
            info("dir/renamed.txt").await.size(),
            10,
            "the bytes at the origin"
        );
        assert_eq!(
            info("dir/new.txt").await.size(),
            9,
            "the file written from the store"
        );
        assert_eq!(info("dir/made").await, FileInfo::Directory);
        assert_eq!(info("dir/empty").await, FileInfo::Directory);
        assert_eq!(info("dir/gone.txt").await, FileInfo::NotExist);
        assert_eq!(info("moved/new.txt").await, FileInfo::NotExist);
    })
    .await;
}

/// A revision holding `dir/a.txt`, with SWFS holding `dir` moved to `moved` and the mount holding
/// `moved/a.txt`, which SWFS holds no bytes of.
async fn moved_away() -> (SwfsOperation, TempDir) {
    let (operation, mount, _repository, _state) = serving_frozen(
        &[("dir/a.txt", Held::File(b"a content"))],
        FakeFrozen::default()
            .with_directory("\\moved")
            .with_move("\\dir", "\\moved"),
        &[],
    )
    .await;
    let root = mount.path().join("mount");
    std::fs::create_dir_all(root.join("moved")).expect("making a directory on the mount");
    std::fs::write(root.join("moved/a.txt"), b"a content").expect("holding a file");
    (operation, mount)
}

/// A directory moved through the mount twice leaves nothing of the operation's at the path it
/// passed through: what it carried there moves on with it.
#[tokio::test]
async fn a_directory_moved_through_the_mount_twice_leaves_nothing_where_it_passed() {
    in_context(async {
        let (operation, mount) = moved_away().await;
        let root = mount.path().join("mount");
        operation
            .rename(&node("moved"), &node("x"))
            .await
            .expect("moving the directory");
        operation
            .rename(&node("x"), &node("y"))
            .await
            .expect("moving it again");

        operation.realize_for_test().await.expect("realizing");

        assert_eq!(
            operation.file_info(&node("x/a.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );
        assert!(
            !root.join("x").exists(),
            "the path passed through was written"
        );
        assert_eq!(
            operation
                .file_info(&node("y/a.txt"))
                .await
                .expect("describing the file")
                .size(),
            9
        );
    })
    .await;
}

/// What the operation made at a path it removed, below a directory it then moves through the
/// mount, is what the operation reads at the new path, not what the mount held there.
#[tokio::test]
async fn a_directory_moved_through_the_mount_carries_what_was_made_over_a_removal() {
    in_context(async {
        let (operation, mount) = moved_away().await;
        std::fs::write(mount.path().join("mount/moved/x"), b"user").expect("holding a file");
        operation
            .remove_recursive(&node("moved/x"))
            .await
            .expect("removing a file");
        operation
            .create_dir_all(&node("moved/x"))
            .await
            .expect("making a directory in its place");

        operation
            .rename(&node("moved"), &node("other"))
            .await
            .expect("moving the directory");

        assert_eq!(
            operation.file_info(&node("other/x")).await.ok(),
            Some(FileInfo::Directory)
        );
    })
    .await;
}

/// A file the operation removed below a directory it then moves through the mount, to a path no
/// revision serves, leaves the mount: nothing realizing does would take it away.
#[tokio::test]
async fn a_removal_carried_where_no_revision_serves_leaves_the_mount() {
    in_context(async {
        let (operation, mount) = moved_away().await;
        let root = mount.path().join("mount");
        std::fs::write(root.join("moved/gone.txt"), b"g").expect("holding a file");
        operation
            .remove_recursive(&node("moved/gone.txt"))
            .await
            .expect("removing a file");

        operation
            .rename(&node("moved"), &node("other"))
            .await
            .expect("moving the directory");
        operation.realize_for_test().await.expect("realizing");

        assert!(
            !root.join("other/gone.txt").exists(),
            "the removed file stayed on the mount"
        );
        assert_eq!(
            operation.file_info(&node("other/gone.txt")).await.ok(),
            Some(FileInfo::NotExist)
        );
    })
    .await;
}
