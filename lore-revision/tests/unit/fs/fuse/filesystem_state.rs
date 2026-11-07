// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The filesystem State a writable provider builds at mount: the mounted revision with the
//! deletion list applied, then a dirty local-content node for every entry under `write/`.
// The tests plant entries in the external directory as earlier mounts would have left them.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::deletions::DELETION_LIST;
use lore_revision::fs::fuse::deletions::DeletionList;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFileMode;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenWithNameIterator;
use lore_revision::util::path::RelativePath;

use crate::fs::fuse::fuse_fixture;

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: 1 << 17,
        base_time: 1_700_000_000_000,
    }
}

/// An instance's external directory holding `write/` entries and a deletion list.
struct External(TempDir);

impl External {
    fn new() -> Self {
        Self(TempDir::new("lore-fuse-filesystem-state-"))
    }

    fn write_path(&self, path: &str) -> std::path::PathBuf {
        self.0.path().join("write").join(path)
    }

    fn file(&self, path: &str, contents: &[u8]) -> &Self {
        let host = self.write_path(path);
        std::fs::create_dir_all(host.parent().expect("a parent")).expect("parent directories");
        std::fs::write(host, contents).expect("a file under write/");
        self
    }

    fn executable(&self, path: &str, contents: &[u8]) -> &Self {
        use std::os::unix::fs::PermissionsExt as _;
        self.file(path, contents);
        std::fs::set_permissions(
            self.write_path(path),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("an executable mode");
        self
    }

    fn dir(&self, path: &str) -> &Self {
        std::fs::create_dir_all(self.write_path(path)).expect("a directory under write/");
        self
    }

    fn symlink(&self, path: &str, target: &str) -> &Self {
        let host = self.write_path(path);
        std::fs::create_dir_all(host.parent().expect("a parent")).expect("parent directories");
        std::os::unix::fs::symlink(target, host).expect("a symlink under write/");
        self
    }

    fn deleted(&self, paths: &[&str]) -> &Self {
        let list = DeletionList::open(&self.0.path().join(DELETION_LIST)).expect("deletion list");
        for path in paths {
            list.append(&RelativePath::new_from_initial_path(path).expect("a path"))
                .expect("a deletion");
        }
        self
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

/// A writable provider over `fixture`'s revision, built from `external`.
async fn mounted(fixture: &fuse_fixture::FuseFixture, external: &External) -> Arc<FuseFilesystem> {
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new_writable(
                fixture.repository.clone(),
                fixture.state.clone(),
                attr_context(),
                external.path(),
            ),
        )
        .await
        .expect("writable provider construction")
}

/// The node at `path` in `state`, `None` where the State holds nothing there.
async fn node_at(
    state: &State,
    repository: &Arc<RepositoryContext>,
    path: &str,
) -> Option<(NodeID, Node)> {
    match state.find_node_link(repository.clone(), path).await {
        Ok(link) => Some((
            link.node,
            state
                .node(repository.clone(), link.node)
                .await
                .expect("a found node reads"),
        )),
        Err(error) if error.is_node_not_found() => None,
        Err(error) => panic!("looking up {path}: {error}"),
    }
}

async fn node(state: &State, repository: &Arc<RepositoryContext>, path: &str) -> Node {
    node_at(state, repository, path)
        .await
        .unwrap_or_else(|| panic!("the filesystem State holds {path}"))
        .1
}

async fn child_names(
    state: &Arc<State>,
    repository: &Arc<RepositoryContext>,
    parent: NodeID,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut children =
        StateNodeChildrenWithNameIterator::new(state.clone(), repository.clone(), parent)
            .await
            .expect("the children of a directory");
    while let Some((_, _, name)) = children.next().await.expect("the next child") {
        names.push(name.freeze());
    }
    names.sort_unstable();
    names
}

/// Asserts `node` is an added or modified node whose content lives under `write/`.
fn assert_local(path: &str, node: &Node, ext: NodeExtFlags) {
    assert!(node.is_dirty(), "{path} is dirty, flags {:#x}", node.flags);
    assert_eq!(
        node.ext_flags(),
        ext,
        "{path} carries the local-content flags"
    );
    assert!(
        node.address.hash.is_zero(),
        "{path} has the zero address, got {:?}",
        node.address
    );
}

/// A tracked file under `write/`, and nothing else, marks the directories above it dirty with no
/// extension flags of their own, and leaves its siblings clean.
#[tokio::test]
async fn a_mount_marks_the_directories_above_a_changed_tracked_file() {
    let fixture =
        fuse_fixture::committed(&[("a/dir/tracked.txt", b"old"), ("a/dir/other.txt", b"other")])
            .await;
    let external = External::new();
    external.file("a/dir/tracked.txt", b"changed");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let modified = node(state, repository, "a/dir/tracked.txt").await;
    assert_local("a/dir/tracked.txt", &modified, NodeExtFlags::LocalContent);
    assert!(modified.is_dirty_modify());
    for dir in ["a/dir", "a"] {
        let held = node(state, repository, dir).await;
        assert!(held.is_dirty(), "{dir} is marked above the change");
        assert_eq!(held.action_bits(), 0, "{dir} carries no action of its own");
        assert_eq!(held.ext_flags, 0, "{dir} carries no extension flags");
    }
    assert!(!node(state, repository, "a/dir/other.txt").await.is_dirty());
}

#[tokio::test]
async fn a_mount_adds_a_dirty_local_node_for_every_entry_under_write() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k"), ("dir/tracked.txt", b"old")]).await;
    let external = External::new();
    external
        .file("new.txt", b"fresh!")
        .executable("dir/tracked.txt", b"changed")
        .file("dir/sub/deep.txt", b"deep")
        .executable("added/inner/run.sh", b"#!/bin/sh\n");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let added = node(state, repository, "new.txt").await;
    assert_local("new.txt", &added, NodeExtFlags::LocalContent);
    assert!(
        added.is_file() && added.is_dirty_add(),
        "new.txt is an added file"
    );
    assert_eq!(added.size, 6);

    let modified = node(state, repository, "dir/tracked.txt").await;
    assert_local("dir/tracked.txt", &modified, NodeExtFlags::LocalContent);
    assert!(
        modified.is_file() && modified.is_dirty_modify(),
        "a tracked file under write/ is a modified file, flags {:#x}",
        modified.flags
    );
    assert_eq!(modified.size, 7);
    assert_eq!(
        modified.mode & NodeFileMode::Executable.bits(),
        NodeFileMode::Executable.bits(),
        "the tracked file takes the host file's mode"
    );

    for dir in ["dir/sub", "added", "added/inner"] {
        let added_dir = node(state, repository, dir).await;
        assert_local(dir, &added_dir, NodeExtFlags::LocalContent);
        assert!(
            added_dir.is_directory() && added_dir.is_dirty_add(),
            "{dir} is an added directory"
        );
    }
    assert_local(
        "dir/sub/deep.txt",
        &node(state, repository, "dir/sub/deep.txt").await,
        NodeExtFlags::LocalContent,
    );
    let script = node(state, repository, "added/inner/run.sh").await;
    assert_local("added/inner/run.sh", &script, NodeExtFlags::LocalContent);
    assert!(
        script.mode & NodeFileMode::Executable == NodeFileMode::Executable,
        "an executable host file is an executable node, mode {:#x}",
        script.mode
    );

    let tracked_dir = node(state, repository, "dir").await;
    assert!(
        tracked_dir.is_dirty(),
        "a directory holding changes is dirty"
    );
    assert_eq!(
        tracked_dir.ext_flags(),
        NodeExtFlags::NoFlags,
        "a tracked directory keeps its tracked children, so it is no local content"
    );
    assert!(!tracked_dir.address.hash.is_zero());

    let kept = node(state, repository, "keep.txt").await;
    assert!(!kept.is_dirty(), "a file absent from write/ stays clean");
    assert!(!kept.address.hash.is_zero());

    assert!(
        node_at(&fixture.state, repository, "new.txt")
            .await
            .is_none(),
        "the mounted revision's own State is left unchanged"
    );
    for planted in [
        "new.txt",
        "dir/tracked.txt",
        "dir/sub/deep.txt",
        "added/inner/run.sh",
    ] {
        assert!(
            external.write_path(planted).exists(),
            "write/ still holds {planted} once the provider is built"
        );
    }
}

#[tokio::test]
async fn a_mount_removes_every_listed_deletion_the_revision_holds() {
    let fixture = fuse_fixture::committed(&[
        ("a.txt", b"a"),
        ("b.txt", b"b"),
        ("dir/c.txt", b"c"),
        ("dir/d.txt", b"d"),
        ("other/e.txt", b"e"),
        ("other/f.txt", b"f"),
    ])
    .await;
    let external = External::new();
    external.deleted(&["a.txt", "dir", "other/e.txt"]);

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    for gone in ["a.txt", "dir", "dir/c.txt", "other/e.txt"] {
        assert!(
            node_at(state, repository, gone).await.is_none(),
            "{gone} is deleted"
        );
    }
    assert_eq!(
        child_names(state, repository, ROOT_NODE).await,
        vec!["b.txt", "other"]
    );
    let other = node_at(state, repository, "other")
        .await
        .expect("other stays");
    assert!(other.1.is_dirty(), "a directory that lost a child is dirty");
    assert_eq!(child_names(state, repository, other.0).await, vec!["f.txt"]);
    assert!(!node(state, repository, "b.txt").await.is_dirty());
}

#[tokio::test]
async fn write_wins_over_a_deletion_of_the_same_path() {
    let fixture = fuse_fixture::committed(&[("a.txt", b"old"), ("dir/x.txt", b"x")]).await;
    let external = External::new();
    external
        .deleted(&["a.txt", "dir/x.txt", "dir"])
        .file("a.txt", b"new")
        .file("dir/y.txt", b"why");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let recreated = node(state, repository, "a.txt").await;
    assert_local("a.txt", &recreated, NodeExtFlags::LocalContent);
    assert!(
        recreated.is_file() && recreated.is_dirty_modify(),
        "a deleted then recreated tracked file is modified, as an edit is, flags {:#x}",
        recreated.flags
    );
    assert_eq!(
        recreated.address.context,
        node(&fixture.state, repository, "a.txt")
            .await
            .address
            .context,
        "a recreated tracked file keeps its file id"
    );
    assert_eq!(recreated.size, 3);

    let (dir_id, dir) = node_at(state, repository, "dir")
        .await
        .expect("a recreated directory is present");
    assert!(dir.is_directory() && dir.is_dirty());
    assert_eq!(
        dir.ext_flags(),
        NodeExtFlags::NoFlags,
        "a recreated tracked directory stays its tracked node"
    );
    assert_eq!(
        child_names(state, repository, dir_id).await,
        vec!["y.txt"],
        "a recreated directory holds what write/ holds and no deleted child"
    );
}

#[tokio::test]
async fn a_recreated_directory_keeps_its_node_and_its_recreated_files() {
    let fixture = fuse_fixture::committed(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    let external = External::new();
    external
        .deleted(&["dir/a.txt", "dir/b.txt", "dir"])
        .file("dir/a.txt", b"again");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let (dir_id, dir) = node_at(state, repository, "dir")
        .await
        .expect("the recreated directory is present");
    let (tracked_dir_id, _) = node_at(&fixture.state, repository, "dir")
        .await
        .expect("the revision holds dir");
    assert_eq!(
        dir_id, tracked_dir_id,
        "a recreated directory keeps its node"
    );
    assert!(dir.is_directory());
    let a = node(state, repository, "dir/a.txt").await;
    assert_local("dir/a.txt", &a, NodeExtFlags::LocalContent);
    assert!(
        a.is_dirty_modify(),
        "a recreated file is modified, flags {:#x}",
        a.flags
    );
    assert_eq!(
        a.address.context,
        node(&fixture.state, repository, "dir/a.txt")
            .await
            .address
            .context,
        "a recreated file keeps its file id"
    );
    assert_eq!(
        child_names(state, repository, dir_id).await,
        vec!["a.txt"],
        "a listed child write/ does not recreate stays deleted"
    );
}

#[tokio::test]
async fn a_case_only_rename_under_write_renames_the_tracked_node() {
    let fixture = fuse_fixture::committed(&[("Foo.txt", b"foo")]).await;
    let external = External::new();
    external.deleted(&["Foo.txt"]).file("foo.txt", b"renamed");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    assert_eq!(
        child_names(state, repository, ROOT_NODE).await,
        vec!["foo.txt"],
        "the node takes the spelling write/ holds"
    );
    let renamed = node(state, repository, "foo.txt").await;
    assert_local("foo.txt", &renamed, NodeExtFlags::LocalContent);
    assert!(renamed.is_dirty_modify(), "flags {:#x}", renamed.flags);
    assert_eq!(
        renamed.address.context,
        node(&fixture.state, repository, "Foo.txt")
            .await
            .address
            .context,
        "the renamed node keeps its file id"
    );
}

/// A directory under `write/` spelled as no tracked one is, but folding to one, respells the
/// tracked directory in place and marks it dirty, as it stands at a path its revision does not
/// hold; its tracked children stay under it, clean.
#[tokio::test]
async fn a_case_only_rename_of_a_directory_under_write_respells_and_marks_it() {
    let fixture = fuse_fixture::committed(&[("Dir/a.txt", b"a")]).await;
    let external = External::new();
    external.dir("dir");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    assert_eq!(
        child_names(state, repository, ROOT_NODE).await,
        vec!["dir"],
        "the directory takes the spelling write/ holds"
    );
    let (dir_id, dir) = node_at(state, repository, "dir")
        .await
        .expect("the respelled directory is present");
    let (tracked_id, _) = node_at(&fixture.state, repository, "Dir")
        .await
        .expect("the revision holds Dir");
    assert_eq!(dir_id, tracked_id, "a respelled directory keeps its node");
    assert!(dir.is_dirty(), "the respelled directory is dirty");
    assert_eq!(child_names(state, repository, dir_id).await, vec!["a.txt"]);
    let a = node(state, repository, "dir/a.txt").await;
    assert!(
        !a.is_dirty(),
        "a tracked child stays clean, flags {:#x}",
        a.flags
    );
}

#[tokio::test]
async fn a_name_no_node_can_store_is_skipped() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let external = External::new();
    external.file("a\\b", b"backslash").file("ok.txt", b"ok");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    assert_eq!(
        child_names(state, repository, ROOT_NODE).await,
        vec!["keep.txt", "ok.txt"],
        "a name the State cannot store is left out"
    );
}

#[tokio::test]
async fn a_deletion_of_a_path_the_revision_lacks_is_ignored() {
    let fixture =
        fuse_fixture::committed(&[("keep.txt", b"k"), ("gone.txt", b"g"), ("dir/a.txt", b"a")])
            .await;
    let external = External::new();
    external.deleted(&[
        "missing.txt",
        "nodir/also.txt",
        "keep.txt/below",
        "dir/absent.txt",
        "gone.txt",
    ]);

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    assert!(
        node_at(state, repository, "gone.txt").await.is_none(),
        "a listed path the revision holds is deleted past the ones it lacks"
    );
    for path in ["keep.txt", "dir", "dir/a.txt"] {
        let kept = node(state, repository, path).await;
        assert!(!kept.is_dirty(), "{path:?} is unchanged");
    }
}

#[tokio::test]
async fn an_empty_directory_under_write_is_an_added_directory() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let external = External::new();
    external.dir("empty");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let (empty_id, empty) = node_at(state, repository, "empty")
        .await
        .expect("the empty directory is present");
    assert_local("empty", &empty, NodeExtFlags::LocalContent);
    assert!(empty.is_directory() && empty.is_dirty_add());
    assert!(child_names(state, repository, empty_id).await.is_empty());
}

#[tokio::test]
async fn a_symlink_under_write_is_a_symlink_node() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let external = External::new();
    external.symlink("link", "keep.txt");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let link = node(state, repository, "link").await;
    assert_local(
        "link",
        &link,
        NodeExtFlags::LocalContent | NodeExtFlags::Symlink,
    );
    assert!(
        link.is_file() && link.is_dirty_add(),
        "a symlink is a file node"
    );
    assert_eq!(
        link.size,
        "keep.txt".len() as u64,
        "its size is its target's length"
    );
}

#[tokio::test]
async fn case_fold_equal_names_under_write_keep_the_first() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let external = External::new();
    external
        .file("cased/Foo.txt", b"foo")
        .file("cased/foo.txt", b"barbaz");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let (cased_id, _) = node_at(state, repository, "cased")
        .await
        .expect("cased is present");
    assert_eq!(
        child_names(state, repository, cased_id).await,
        vec!["Foo.txt"],
        "one node per folded name, the first in byte order"
    );
    assert_eq!(node(state, repository, "cased/Foo.txt").await.size, 3);
}

#[tokio::test]
async fn a_wide_directory_under_write_is_built_whole() {
    const TRACKED: usize = 10;
    const ADDED: usize = 5000;
    let tracked: Vec<(String, Vec<u8>)> = (0..TRACKED)
        .map(|i| (format!("wide/t{i:04}"), vec![b't'; i + 1]))
        .collect();
    let files: Vec<(&str, &[u8])> = tracked
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_slice()))
        .collect();
    let fixture = fuse_fixture::committed(&files).await;
    let external = External::new();
    for i in 0..ADDED {
        external.file(&format!("wide/n{i:04}"), b"n");
    }
    external.file("wide/t0003", b"changed");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let (wide_id, _) = node_at(state, repository, "wide")
        .await
        .expect("wide is present");
    assert_eq!(
        child_names(state, repository, wide_id).await.len(),
        TRACKED + ADDED
    );
    for added in ["wide/n0000", "wide/n2500", "wide/n4999"] {
        let node = node(state, repository, added).await;
        assert_local(added, &node, NodeExtFlags::LocalContent);
        assert!(node.is_dirty_add(), "{added} is added");
    }
    let modified = node(state, repository, "wide/t0003").await;
    assert_local("wide/t0003", &modified, NodeExtFlags::LocalContent);
    assert!(modified.is_dirty_modify());
    assert!(!node(state, repository, "wide/t0004").await.is_dirty());
}

/// Restores a mode on drop, so the temporary directory can be removed whatever the test did.
struct ModeRestore(std::path::PathBuf);

impl Drop for ModeRestore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[tokio::test]
async fn a_directory_under_write_it_cannot_read_is_skipped() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let external = External::new();
    external
        .file("ok.txt", b"ok")
        .file("locked/inner.txt", b"hidden");
    let locked = external.write_path("locked");
    let _restore = ModeRestore(locked.clone());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("an unreadable directory");
    if std::fs::read_dir(&locked).is_ok() {
        eprintln!("skipped: a directory of mode 0 reads anyway, as for root");
        return;
    }

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    assert_local(
        "ok.txt",
        &node(state, repository, "ok.txt").await,
        NodeExtFlags::LocalContent,
    );
    assert!(
        node_at(state, repository, "locked").await.is_none(),
        "a directory that cannot be read is left out"
    );
}

/// What the provider cannot read from its external directory fails the mount with an internal
/// error naming the step and the path: the directory itself, the deletion list and `write/`.
#[tokio::test]
async fn an_external_directory_the_provider_cannot_read_fails_the_mount() {
    let fixture = fuse_fixture::committed(&[("keep.txt", b"k")]).await;
    let host = TempDir::new("lore-fuse-filesystem-state-unreadable-");
    let file = host.path().join("a-file");
    std::fs::write(&file, b"not a directory").expect("a file");
    let list_a_directory = host.path().join("list-a-directory");
    std::fs::create_dir_all(list_a_directory.join(DELETION_LIST))
        .expect("a directory where the list goes");
    let write_a_file = host.path().join("write-a-file");
    std::fs::create_dir_all(&write_a_file).expect("an external directory");
    std::fs::write(write_a_file.join("write"), b"not a directory")
        .expect("a file where write/ goes");

    for (external, step, named) in [
        (file.join("external"), "creating", file.join("external")),
        (
            list_a_directory.clone(),
            "opening",
            list_a_directory.join(DELETION_LIST),
        ),
        (write_a_file.clone(), "reading", write_a_file.join("write")),
    ] {
        let result = LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                FuseFilesystem::new_writable(
                    fixture.repository.clone(),
                    fixture.state.clone(),
                    attr_context(),
                    &external,
                ),
            )
            .await;
        let error = match result {
            Ok(_) => panic!("{step}: the mount succeeded"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(error.is_internal(), "{step}: {message}");
        assert!(
            message.contains(&format!("{step} {}", named.display())),
            "{step}: {message}"
        );
    }
}

#[tokio::test]
async fn a_write_directory_over_a_tracked_file_replaces_it() {
    let fixture = fuse_fixture::committed(&[("x", b"file")]).await;
    let external = External::new();
    external.file("x/inner.txt", b"in");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let (x_id, x) = node_at(state, repository, "x").await.expect("x is present");
    assert_local("x", &x, NodeExtFlags::LocalContent);
    assert!(
        x.is_directory() && x.is_dirty_add(),
        "x is an added directory"
    );
    assert_eq!(
        child_names(state, repository, x_id).await,
        vec!["inner.txt"]
    );
}

#[tokio::test]
async fn a_write_file_over_a_tracked_directory_replaces_it() {
    let fixture = fuse_fixture::committed(&[("d/a.txt", b"a")]).await;
    let external = External::new();
    external.file("d", b"now a file");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let d = node(state, repository, "d").await;
    assert_local("d", &d, NodeExtFlags::LocalContent);
    assert!(d.is_file() && d.is_dirty_add(), "d is an added file");
    assert!(node_at(state, repository, "d/a.txt").await.is_none());
}

#[tokio::test]
async fn a_write_symlink_over_a_tracked_file_modifies_it() {
    let fixture = fuse_fixture::committed(&[("f", b"data"), ("target", b"t")]).await;
    let external = External::new();
    external.symlink("f", "target");

    let fs = mounted(&fixture, &external).await;
    let state = &fs.state();
    let repository = &fixture.repository;

    let f = node(state, repository, "f").await;
    assert_local("f", &f, NodeExtFlags::LocalContent | NodeExtFlags::Symlink);
    assert!(f.is_file() && f.is_dirty_modify(), "f is a modified file");
    assert_eq!(f.size, "target".len() as u64);
    assert_eq!(
        f.address.context,
        node(&fixture.state, repository, "f").await.address.context,
        "f keeps its file id"
    );
}
