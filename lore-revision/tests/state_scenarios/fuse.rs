// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The FUSE provider's commit settle driven through the scheduler: `FuseFilesystem::committed`,
//! with the settle a commit owes from before its revision is current
//! (`FuseFilesystem::owe_settle`), against the FUSE requests that race it, each request's served
//! node operations parking at their scheduling points. Each run mounts a writable provider over a
//! saved tree, its local changes in a directory of its own.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_base::test_util::TempDir;
use lore_fuse::filesystem::Filesystem as _;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::reply::Errno;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::operation::ServedContent;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::util::path::RelativePath;
use lore_storage::hash::hash_string;

use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::invariants::Rules;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

const MAX_READ: u32 = 1 << 20;
const BASE_TIME: u64 = 1_700_000_000_000;

/// A writable provider mounted over a saved tree, and the revision a commit stored, which its
/// settle compares with.
struct Mounted {
    fs: Arc<FuseFilesystem>,
    /// The mounted tree, over the provider's State.
    tree: Tree,
    /// The tree the commit stored, in a repository of its own.
    committed: Tree,
    /// The instance's external directory: `write/` and the deletion list.
    external: TempDir,
    /// Whether the commit's settle owed was armed, its pre-record done: the revision may then
    /// be current.
    owed: AtomicBool,
    /// Whether the commit's settle completed clean and ended its debt.
    released: AtomicBool,
    /// How many of the RENAMEs of a [`renames`] op went through.
    renamed: AtomicUsize,
}

impl Mounted {
    /// The provider over `mounted`, saved, with the directories `local` made under `write/`, and
    /// `committed` as the revision the commit stored.
    async fn new(mounted: &[&str], local: &[&str], committed: &[&str]) -> Self {
        let external = TempDir::new("lore-state-scenarios-fuse-");
        let write = external.path().join("write");
        std::fs::create_dir_all(&write).expect("write/ is made");
        for dir in local {
            std::fs::create_dir_all(write.join(dir)).expect("a local directory is made");
        }
        let tree = Tree::new(mounted).await.reloaded().await;
        let fs = FuseFilesystem::new_writable(
            tree.repository.clone(),
            tree.state.clone(),
            AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
            external.path(),
        )
        .await
        .expect("a writable provider");
        let tree = tree.serving(fs.state());
        Self {
            fs,
            tree,
            committed: Tree::new(committed).await,
            external,
            owed: AtomicBool::new(false),
            released: AtomicBool::new(false),
            renamed: AtomicUsize::new(0),
        }
    }

    /// The lines of the deletion list.
    fn deleted(&self) -> Vec<String> {
        std::fs::read_to_string(self.external.path().join("deleted"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The live node the provider's State holds at `path`, found by name from the root.
    async fn at(&self, path: &str) -> Option<(NodeID, Node)> {
        let (state, repository) = (&self.tree.state, &self.tree.repository);
        let mut node = ROOT_NODE;
        for name in path.split('/') {
            node = state
                .find_subnode(repository.clone(), node, hash_string(name))
                .await
                .ok()?;
        }
        let record = state.node(repository.clone(), node).await.ok()?;
        (!record.is_discarded()).then_some((node, record))
    }

    /// The inode LOOKUP answers for `path`.
    async fn ino(&self, path: &str) -> u64 {
        let mut ino = ROOT_INO;
        for name in path.split('/') {
            ino = self
                .fs
                .lookup(&request(), ino, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("'{path}' is looked up: {errno:?}"))
                .ino;
        }
        ino
    }

    /// Whether the RMDIR of a directory made now, added, records its deletion: only while a
    /// commit reads or owes its settle.
    async fn removal_of_an_added_directory_recorded(&self) -> bool {
        self.fs
            .mkdir(&request(), ROOT_INO, b"fresh", libc::S_IFDIR | 0o755, 0o022)
            .await
            .expect("a fresh directory is made");
        self.fs
            .rmdir(&request(), ROOT_INO, b"fresh")
            .await
            .expect("the fresh directory is removed");
        self.deleted().iter().any(|line| line == "fresh")
    }

    /// Whether the live node at `path` is dirty, and holds no pending clear.
    async fn dirty_with_no_pending_clear(&self, path: &str) -> bool {
        let (node, record) = self
            .at(path)
            .await
            .unwrap_or_else(|| panic!("'{path}' stands"));
        let block = self
            .tree
            .state
            .block(self.tree.repository.clone(), NodeBlock::index(node))
            .await
            .expect("the node's block");
        assert!(
            !block.clear_pending(Node::index(node)),
            "'{path}' holds a pending clear"
        );
        record.is_dirty()
    }

    /// Directory `path` made dirty, as a child made and removed leaves it.
    async fn dirtied(self, path: &str) -> Self {
        let dir = self.ino(path).await;
        self.fs
            .mkdir(&request(), dir, b"made", libc::S_IFDIR | 0o755, 0o022)
            .await
            .expect("a child is made");
        self.fs
            .rmdir(&request(), dir, b"made")
            .await
            .expect("the child is removed");
        self
    }
}

fn request() -> RequestInfo {
    RequestInfo {
        unique: 1,
        nodeid: ROOT_INO,
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

fn paths(paths: &[&str]) -> Vec<RelativePath> {
    paths
        .iter()
        .map(|path| RelativePath::new_from_initial_path(path).expect("a relative path"))
        .collect()
}

/// A commit's settle of `dirs` once its revision is current, against no stored file.
fn settle(dirs: &'static [&'static str]) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            let committed = &c.committed;
            c.fs.committed(
                committed.repository.clone(),
                committed.committed.clone(),
                &ServedContent::default(),
                paths(dirs),
            )
            .await
            .expect("the settle");
        })
    })
}

/// A commit handing over the directories `handed`, as the provider runs it: the settle owed is
/// armed, its pre-record done, before the revision may become current; the settle follows, and
/// ends the debt only where it completed with no error.
fn owed_settle(handed: &'static [&'static str]) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            let owed = c.fs.owe_settle(paths(handed)).await;
            c.owed.store(true, Ordering::SeqCst);
            let committed = &c.committed;
            let clean =
                c.fs.committed(
                    committed.repository.clone(),
                    committed.committed.clone(),
                    &ServedContent::default(),
                    paths(handed),
                )
                .await
                .expect("the settle");
            if clean {
                owed.release();
                c.released.store(true, Ordering::SeqCst);
            }
        })
    })
}

/// An RMDIR of `name` in the root.
fn rmdir(name: &'static str) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            c.fs.rmdir(&request(), ROOT_INO, name.as_bytes())
                .await
                .expect("the RMDIR");
        })
    })
}

/// An UNLINK of `name` in directory `dir`.
fn unlink(dir: &'static str, name: &'static str) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            let dir = c.ino(dir).await;
            c.fs.unlink(&request(), dir, name.as_bytes())
                .await
                .expect("the UNLINK");
        })
    })
}

/// RENAMEs in the root, one after another, each of the first name to the second, until one
/// meets a directory a settle made the revision's, which refuses with `EXDEV`; counted in
/// [`Mounted::renamed`].
fn renames(pairs: &'static [(&'static str, &'static str)]) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            for (from, to) in pairs {
                match c
                    .fs
                    .rename(
                        &request(),
                        ROOT_INO,
                        from.as_bytes(),
                        ROOT_INO,
                        to.as_bytes(),
                        0,
                    )
                    .await
                {
                    Ok(()) => {
                        c.renamed.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(Errno::EXDEV) => return,
                    Err(errno) => panic!("a RENAME of {from}: {errno:?}"),
                }
            }
        })
    })
}

/// A RENAME in the root of `from` to `to`.
fn rename(from: &'static str, to: &'static str) -> Op<Mounted> {
    Box::new(move |c: Arc<Mounted>| {
        Box::pin(async move {
            c.fs.rename(
                &request(),
                ROOT_INO,
                from.as_bytes(),
                ROOT_INO,
                to.as_bytes(),
                0,
            )
            .await
            .expect("the RENAME");
        })
    })
}

async fn invariants_hold(c: &Mounted) {
    invariants::check(
        &c.tree.state,
        &c.tree.repository,
        Rules {
            dirty_closure: true,
            unique_names: true,
            no_ext_flags: false,
            no_builds: false,
        },
    )
    .await
    .unwrap_or_else(|violations| panic!("invariants: {violations:#?}"));
}

/// Explores `ops` from `setup`, dropping the ops `cancellable` names at each point they park at,
/// and runs `check` after each run. Sleep sets stay off: the FUSE requests touch inodes, `write/`
/// and the deletion list, which no hook records.
fn explore(
    name: &'static str,
    setup: crate::harness::scheduler::Setup<Mounted>,
    (ops, cancellable): (Vec<Op<Mounted>>, &[usize]),
    check: crate::harness::scheduler::Check<Mounted>,
) {
    let scenario = Scenario::<Mounted> {
        name,
        setup,
        ops,
        cancellable: cancellable.to_vec(),
        check,
    };
    let covered = scenario.explore(Exploration {
        sleep_sets: false,
        cancellation: !cancellable.is_empty(),
        ..Exploration::default()
    });
    eprintln!("'{name}': {covered:?}");
    assert!(
        covered.complete,
        "'{name}': exploration did not finish: {covered:?}"
    );
}

/// H20: an RMDIR of an empty directory a commit adds, against the commit's settle owed and its
/// settle, which may be dropped at any point once the revision is current. Wherever the RMDIR
/// lands, once the settle is owed the directory's deletion is recorded: by the pre-record, by
/// the RMDIR sampling the debt, or by the RMDIR of a directory that no longer counts as added.
#[test]
fn an_rmdir_of_a_handed_directory_against_the_owed_settle_is_recorded() {
    explore(
        "fuse: an RMDIR of a handed directory against the owed settle",
        Box::new(|| Box::pin(Mounted::new(&["keep"], &["e"], &["keep", "e/"]))),
        (vec![owed_settle(&["e"]), rmdir("e")], &[0]),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                assert!(c.at("e").await.is_none(), "e stands");
                if c.owed.load(Ordering::SeqCst) {
                    assert!(
                        c.deleted().iter().any(|line| line == "e"),
                        "e's deletion went unrecorded: {:?}",
                        c.deleted()
                    );
                }
                // The debt ends with a clean settle alone, and stays once owed.
                let recorded = c.removal_of_an_added_directory_recorded().await;
                if c.released.load(Ordering::SeqCst) {
                    assert!(!recorded, "the released settle left its debt");
                } else if c.owed.load(Ordering::SeqCst) {
                    assert!(recorded, "the dropped settle released its debt");
                }
            })
        }),
    );
}

/// H15: an UNLINK in a directory a commit settles, its other children the committed ones. The
/// settle never clears the directory over the removal, whichever phase the UNLINK lands in, so
/// it ends dirty, and the deletion is recorded.
#[test]
fn an_unlink_against_the_settle_of_its_directory_keeps_it_dirty() {
    explore(
        "fuse: an UNLINK against the settle of its directory",
        Box::new(|| {
            Box::pin(async {
                Mounted::new(&["d/a", "d/b"], &[], &["d/a", "d/b"])
                    .await
                    .dirtied("d")
                    .await
            })
        }),
        (vec![settle(&["d"]), unlink("d", "b")], &[]),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                let (_, d) = c.at("d").await.expect("d stands");
                assert!(d.is_dirty(), "d is clean over the removal of d/b");
                assert!(
                    c.deleted().iter().any(|line| line == "d/b"),
                    "d/b's deletion went unrecorded: {:?}",
                    c.deleted()
                );
            })
        }),
    );
}

/// A respell of a directory a commit settles: whichever phase it lands in, the directory ends
/// dirty under its new spelling, so status reports it.
#[test]
fn a_respell_against_the_settle_of_its_directory_keeps_it_dirty() {
    explore(
        "fuse: a respell against the settle of its directory",
        Box::new(|| {
            Box::pin(async {
                Mounted::new(&["d/a", "d/b"], &[], &["d/a", "d/b"])
                    .await
                    .dirtied("d")
                    .await
            })
        }),
        (vec![settle(&["d"]), rename("d", "D")], &[]),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                let (node, record) = c.at("D").await.expect("D stands");
                let name = c
                    .tree
                    .state
                    .node_name_ref(c.tree.repository.clone(), node)
                    .await
                    .expect("D's name")
                    .to_string();
                assert_eq!(name, "D");
                assert!(record.is_dirty(), "the respelled directory is clean");
            })
        }),
    );
}

/// H16: a settle dropped at every point it parks at, then an UNLINK in its directory. The drop
/// leaves no pending clear, the directory clean only where the settle finished, and the UNLINK
/// marks it dirty again under the served chain rule.
#[test]
fn a_settle_dropped_anywhere_then_an_unlink_marks_its_directory() {
    explore(
        "fuse: a settle dropped anywhere, then an UNLINK",
        Box::new(|| {
            Box::pin(async {
                Mounted::new(&["d/a", "d/b"], &[], &["d/a", "d/b"])
                    .await
                    .dirtied("d")
                    .await
            })
        }),
        (vec![settle(&["d"])], &[0]),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                let (d, _) = c.at("d").await.expect("d stands");
                let block = c
                    .tree
                    .state
                    .block(c.tree.repository.clone(), NodeBlock::index(d))
                    .await
                    .expect("d's block");
                assert!(
                    !block.clear_pending(Node::index(d)),
                    "the dropped settle left d's clear pending"
                );
                let dir = c.ino("d").await;
                c.fs.unlink(&request(), dir, b"b")
                    .await
                    .expect("the UNLINK");
                invariants_hold(&c).await;
                let (_, record) = c.at("d").await.expect("d stands");
                assert!(record.is_dirty(), "d is clean over the removal of d/b");
            })
        }),
    );
}

/// H20 and H19: a RENAME away of an empty directory a commit adds, against the commit's settle
/// owed and its settle, which may be dropped at any point once the revision may be current. Once
/// the settle is owed, the old path's deletion is recorded wherever the RENAME lands: by the
/// pre-record, by the RENAME of a node the settle no longer counts as added, as the move read it,
/// or by the settle finding the path gone. The moved directory ends dirty. A directory the settle
/// cleared refuses the move; empty, it would lose nothing moved, so the refusal's own test is
/// `write_mount`'s `a_rename_of_a_directory_a_commit_settles_after_its_resolution_refuses`.
#[test]
fn a_rename_of_a_handed_directory_against_the_owed_settle_is_recorded() {
    explore(
        "fuse: a RENAME of a handed directory against the owed settle",
        Box::new(|| Box::pin(Mounted::new(&["keep"], &["e"], &["keep", "e/"]))),
        (vec![owed_settle(&["e"]), renames(&[("e", "e2")])], &[0]),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                // Settled before the RENAME read it, or before its move, e is the revision's and
                // stays, its deletion unrecorded.
                if c.renamed.load(Ordering::SeqCst) == 0 {
                    let (_, e) = c.at("e").await.expect("e stands");
                    assert!(
                        !e.is_dirty()
                            && !e.is_dirty_add()
                            && !e.ext_flags().contains(NodeExtFlags::LocalContent),
                        "the RENAME refused a directory that is not the revision's"
                    );
                    assert!(
                        !c.deleted().iter().any(|line| line == "e"),
                        "the refused RENAME recorded e: {:?}",
                        c.deleted()
                    );
                    return;
                }
                assert!(c.at("e").await.is_none(), "e stands");
                assert!(
                    c.dirty_with_no_pending_clear("e2").await,
                    "the moved directory is clean"
                );
                if c.owed.load(Ordering::SeqCst) {
                    assert!(
                        c.deleted().iter().any(|line| line == "e"),
                        "e's deletion went unrecorded: {:?}",
                        c.deleted()
                    );
                }
            })
        }),
    );
}

/// H19: two empty directories a commit adds, one moved away and the other onto its path while
/// the commit's settle runs, which may have read the first at the path. The settle settles
/// whichever directory stands at a path by its own content, re-finding the path where the one it
/// read moved: the directory moved away ends dirty, and no pending clear is left.
#[test]
fn a_directory_moved_onto_a_path_the_settle_reads_is_settled_by_its_own_content() {
    explore(
        "fuse: a directory moved onto a path the settle reads",
        Box::new(|| Box::pin(Mounted::new(&["keep"], &["p", "q"], &["keep", "p/", "q/"]))),
        (
            vec![owed_settle(&["p", "q"]), renames(&[("p", "r"), ("q", "p")])],
            &[],
        ),
        Box::new(|c: Arc<Mounted>| {
            Box::pin(async move {
                invariants_hold(&c).await;
                let renamed = c.renamed.load(Ordering::SeqCst);
                if renamed >= 1 {
                    assert!(
                        c.dirty_with_no_pending_clear("r").await,
                        "the directory moved away is clean"
                    );
                    // The committed p is recorded deleted, or a node at p that no longer counts
                    // as added stands for it.
                    let held = c.at("p").await.is_some_and(|(_, p)| !p.is_dirty_add());
                    assert!(
                        held || c.deleted().iter().any(|line| line == "p"),
                        "p is neither recorded deleted nor the revision's: {:?}",
                        c.deleted()
                    );
                }
                if renamed == 2 {
                    // q's node at p, settled by its own content against the committed p or kept
                    // dirty, with no pending clear either way.
                    let _ = c.dirty_with_no_pending_clear("p").await;
                    assert!(
                        c.deleted().iter().any(|line| line == "q"),
                        "q's deletion went unrecorded: {:?}",
                        c.deleted()
                    );
                }
            })
        }),
    );
}
