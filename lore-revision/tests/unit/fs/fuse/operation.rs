// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `FuseOperation`: the `InstanceOperation` a FUSE-mounted instance answers from its filesystem
//! State, never through the kernel, and the diff of that State that `status` and `diff` read.
// The tests plant a file under `write/` behind the provider's back.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use bytes::Bytes;
use lore_base::error::NotSupported;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_error_set::FfiError as _;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;
use lore_revision::change::FileAction;
use lore_revision::change::NodeChange;
use lore_revision::filter::FilterMode;
use lore_revision::filter::FilterStates;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::FilesystemDiffContext;
use lore_revision::fs::filesystem_provider::FilesystemDiffIntent;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::FsError;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StageIntent;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::filesystem::WriteStep;
use lore_revision::fs::fuse::inode::InodeKind;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::operation::DeletionsRead;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::fs::fuse::swap::NotificationSink;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreFileAction;
use lore_revision::interface::LoreString;
use lore_revision::node::Node;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::stage::StageCaseChange;
use lore_revision::stage::StageOptions;
use lore_revision::state::Content;
use lore_revision::state::Ext;
use lore_revision::state::FilesystemDiffStats;
use lore_revision::state::Mark;
use lore_revision::state::NodeComparison;
use lore_revision::state::NodeMapping;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_revision::state::step_counting;
use lore_revision::util::path::RelativePath;
use lore_storage::ContentSource;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use lore_storage::content::VirtualContent;
use lore_storage::hash::hash_string;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt as _;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::fuse_test_util::req;
use crate::fs::fuse::staged_state;
use crate::fs::fuse::staged_state::Action;

#[test]
fn a_deletion_read_holds_a_listed_path_and_what_is_below_it_case_folded() {
    let paths = ["Dir/Sub", "a.txt", "dir2/b.txt"]
        .into_iter()
        .map(|path| RelativePath::new_from_initial_path(path).expect("a path"))
        .collect();
    let read = DeletionsRead::new(0, paths);
    assert!(read.holds("a.txt"));
    assert!(read.holds("dir/sub"));
    assert!(read.holds("dir/sub/deep/c.txt"));
    assert!(
        !read.holds("dir"),
        "a directory above a listed path is not listed"
    );
    assert!(
        !read.holds("dir/subway.txt"),
        "a listed name is not a prefix of another"
    );
    assert!(!read.holds("dir2"));
    assert!(!read.holds("b.txt"));
}

const BASE_TIME: u64 = 1_700_000_000_000;

const README: &[u8] = b"# readme\n";
const A_TXT: &[u8] = b"alpha contents";
const BINARY: &[u8] = &[0, 159, 146, 150, 0, 1, 2, 3, 0, 0];

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: 1 << 17,
        base_time: BASE_TIME,
    }
}

fn path(value: &str) -> RelativePath {
    if value.is_empty() {
        return RelativePath::new();
    }
    RelativePath::new_from_initial_path(value).expect("relative path")
}

struct Mounted {
    fixture: fuse_fixture::FuseFixture,
    fs: Arc<FuseFilesystem>,
    operation: Arc<InstanceOperationImpl>,
}

async fn mounted() -> Mounted {
    let fixture = Box::pin(fuse_fixture::committed(&[
        ("README.md", README),
        ("dir/a.txt", A_TXT),
        ("dir/sub/b.txt", b"bravo"),
        ("dir/sub/empty.txt", b""),
        ("blob.bin", BINARY),
    ]))
    .await;
    let fs = FuseFilesystem::new(
        fixture.repository.clone(),
        fixture.state.clone(),
        attr_context(),
    )
    .await
    .expect("provider construction");
    let operation = Arc::new(InstanceOperationImpl::new(
        StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
        true,
    ));
    Mounted {
        fixture,
        fs,
        operation,
    }
}

/// Boxed: the fixture's commit makes every test body's future large.
async fn in_context(work: impl std::future::Future<Output = ()>) {
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), Box::pin(work))
        .await;
}

#[tokio::test]
async fn file_info_answers_from_snapshot() {
    in_context(async {
        let m = mounted().await;
        // A time recorded for the path, which the mount does not report. Stored here: the
        // fixture's commit records none for a file written in the clock tick it reads.
        lore_revision::state::file_modified_time_store(
            m.fixture.repository.clone(),
            &path("dir/a.txt"),
            BASE_TIME + 3_600_000,
        )
        .await;
        let recorded = lore_revision::state::file_modified_time(
            m.fixture.repository.clone(),
            &path("dir/a.txt"),
        )
        .await;
        assert_ne!(recorded, 0, "a time is recorded for the path");
        assert_eq!(
            m.operation
                .file_info(&path("dir/a.txt"))
                .await
                .expect("info"),
            FileInfo::File {
                executable: Some(false),
                size: A_TXT.len() as u64,
                mtime: BASE_TIME,
            },
            "only a swap records a time a mount reports"
        );
        assert_eq!(
            m.operation.file_info(&path("dir/sub")).await.expect("info"),
            FileInfo::Directory
        );
        assert_eq!(
            m.operation.file_info(&path("")).await.expect("info"),
            FileInfo::Directory
        );
        assert_eq!(
            m.operation
                .file_info(&path("dir/missing"))
                .await
                .expect("info"),
            FileInfo::NotExist
        );
        assert_eq!(
            m.operation
                .file_info(&path("README.md/below"))
                .await
                .expect("info"),
            FileInfo::NotExist
        );
        assert_eq!(
            m.fs.inodes().len(),
            1,
            "answering from the snapshot must not create inodes the kernel never looked up"
        );
    })
    .await;
}

#[tokio::test]
async fn untracked_file_info_is_not_exist() {
    in_context(async {
        let m = mounted().await;
        for tracked in ["README.md", "dir", "nothing-here"] {
            assert_eq!(
                m.operation
                    .untracked_file_info(&path(tracked))
                    .await
                    .expect("info"),
                FileInfo::NotExist,
                "{tracked}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn read_directory_yields_entries_with_name_hash() {
    in_context(async {
        let m = mounted().await;
        let mut listing = m
            .operation
            .read_directory(&path("dir"))
            .await
            .expect("listing");
        let mut seen = Vec::new();
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("entry");
            assert_eq!(entry.name_hash, hash_string(&entry.name), "{}", entry.name);
            seen.push((entry.name, entry.info));
        }
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            seen,
            vec![
                (
                    "a.txt".to_string(),
                    FileInfo::File {
                        executable: Some(false),
                        size: A_TXT.len() as u64,
                        mtime: BASE_TIME,
                    }
                ),
                ("sub".to_string(), FileInfo::Directory),
            ]
        );

        assert!(
            m.operation
                .read_directory(&path("README.md"))
                .await
                .is_err(),
            "a file is not a directory to list"
        );
        assert!(m.operation.read_directory(&path("gone")).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn holds_name_exactly_true_only_for_exact_spelling() {
    in_context(async {
        let m = mounted().await;
        assert_eq!(
            m.operation.holds_name_exactly(&path("README.md")).await,
            Some(true)
        );
        assert_eq!(
            m.operation.holds_name_exactly(&path("readme.md")).await,
            Some(false)
        );
        assert_eq!(
            m.operation.holds_name_exactly(&path("dir/SUB")).await,
            Some(false)
        );
        assert_eq!(
            m.operation.holds_name_exactly(&path("dir/sub/b.txt")).await,
            Some(true)
        );
        assert_eq!(
            m.operation.holds_name_exactly(&path("missing")).await,
            Some(false)
        );
    })
    .await;
}

/// Two spellings of one path name its one entry, which the served tree finds by any spelling;
/// another entry, and a path naming nothing, do not, and a first path naming nothing is answered
/// as naming nothing.
#[tokio::test]
async fn names_one_entry_only_for_spellings_of_one_path() {
    in_context(async {
        let m = mounted().await;
        for (from, to, one) in [
            ("README.md", "readme.MD", Some(true)),
            ("dir/sub", "DIR/SUB", Some(true)),
            ("README.md", "dir/sub/b.txt", Some(false)),
            ("README.md", "missing", Some(false)),
            ("missing", "README.md", None),
        ] {
            assert_eq!(
                m.operation
                    .names_one_entry(&path(from), &path(to))
                    .await
                    .expect("an answer"),
                one,
                "{from} and {to}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn names_folding_to_lists_case_variants() {
    in_context(async {
        let m = mounted().await;
        assert_eq!(
            m.operation
                .names_folding_to(&path(""), "readme.MD")
                .await
                .expect("names"),
            vec!["README.md".to_string()]
        );
        assert_eq!(
            m.operation
                .names_folding_to(&path("dir"), "SUB")
                .await
                .expect("names"),
            vec!["sub".to_string()]
        );
        assert!(
            m.operation
                .names_folding_to(&path("dir"), "none")
                .await
                .expect("names")
                .is_empty()
        );
    })
    .await;
}

#[tokio::test]
async fn every_write_method_on_a_read_only_mount_returns_read_only() {
    in_context(async {
        let m = mounted().await;
        let op = &m.operation;
        let repository = m.fixture.repository.clone();
        let node = Node::default();
        let target = path("dir/a.txt");
        let results: Vec<(&str, Result<(), FsError>)> = vec![
            ("make_executable", op.make_executable(&target, true).await),
            ("create_dir_all", op.create_dir_all(&path("new/dir")).await),
            (
                "write_file",
                op.write_file(&path("new.txt"), Bytes::new()).await,
            ),
            ("rename", op.rename(&target, &path("dir/A.txt")).await),
            ("remove", op.remove(&target).await),
            ("remove_recursive", op.remove_recursive(&path("dir")).await),
            (
                "write_node",
                op.write_node(repository.clone(), &node, &target)
                    .await
                    .map(|_| ()),
            ),
            (
                "set_file_to_immutable_store_contents",
                op.set_file_to_immutable_store_contents(
                    repository.clone(),
                    Address::default(),
                    &target,
                )
                .await
                .map(|_| ()),
            ),
            ("copy_file", op.copy_file(&target, &path("copy.txt")).await),
        ];
        for (method, result) in results {
            let error = result.expect_err(method);
            assert!(
                error.is_not_supported(),
                "{method} must surface NotSupported, got {error}"
            );
            assert_eq!(
                error.ffi_code(),
                NotSupported {
                    operation: String::new()
                }
                .ffi_code(),
                "{method}"
            );
            let outward = lore_revision::revision::sync::SyncError::from(error);
            assert!(
                outward.is_not_supported(),
                "{method} must stay NotSupported past the filesystem boundary, got {outward}"
            );
        }
    })
    .await;
}

/// The inode at `at`, looked up name by name; `None` where nothing stands there.
async fn ino_at(fs: &FuseFilesystem, at: &str) -> Option<u64> {
    let mut ino = ROOT_INO;
    for name in at.split('/') {
        ino = lookup_ino(fs, ino, name).await?;
    }
    Some(ino)
}

/// The bytes served at `at`.
async fn served_at(fs: &FuseFilesystem, at: &str) -> Vec<u8> {
    let ino = ino_at(fs, at)
        .await
        .unwrap_or_else(|| panic!("{at} is served"));
    read_all(fs, ino, 64).await
}

/// The mode served at `at`.
async fn mode_at(fs: &FuseFilesystem, at: &str) -> u32 {
    let ino = ino_at(fs, at)
        .await
        .unwrap_or_else(|| panic!("{at} is served"));
    fs.getattr(&req(), ino).await.expect("getattr").attr.mode
}

/// Each instance write method changes the filesystem State and `write/` as the same request
/// through the kernel does, and a remount keeps what it wrote.
#[tokio::test(flavor = "multi_thread")]
async fn every_write_method_changes_the_state_and_write() {
    in_context(async {
        let w = working(&[
            ("README.md", README),
            ("dir/a.txt", b"alpha"),
            ("dir/b.txt", b"bravo"),
            ("gone/x.txt", b"x"),
            ("gone/y/z.txt", b"z"),
        ])
        .await;
        let op = operation_of(&w.fs);
        let entry = {
            let served = w.fs.served();
            w.fs.entry_at_path(&served, &path("README.md"))
                .await
                .expect("the snapshot reads")
                .expect("README.md")
        };
        let readme = Node {
            address: entry.address,
            size: entry.size,
            ..Node::default()
        };

        op.make_executable(&path("dir/a.txt"), true)
            .await
            .expect("make_executable");
        op.create_dir_all(&path("new/deep"))
            .await
            .expect("create_dir_all");
        op.write_file(&path("new/deep/n.txt"), Bytes::from_static(b"fresh"))
            .await
            .expect("write_file");
        op.rename(&path("dir/b.txt"), &path("dir/c.txt"))
            .await
            .expect("rename");
        op.remove(&path("gone/x.txt")).await.expect("remove");
        op.remove_recursive(&path("gone"))
            .await
            .expect("remove_recursive");
        let info = op
            .write_node(w.repository.clone(), &readme, &path("dir/w.txt"))
            .await
            .expect("write_node");
        assert_eq!(info.size(), README.len() as u64);
        op.set_file_to_immutable_store_contents(
            w.repository.clone(),
            entry.address,
            &path("s.txt"),
        )
        .await
        .expect("set_file_to_immutable_store_contents");
        op.copy_file(&path("dir/a.txt"), &path("copy.txt"))
            .await
            .expect("copy_file");
        // As on an ordinary clone, removing what is not there leaves what was asked for.
        op.remove(&path("missing.txt"))
            .await
            .expect("remove of a missing file");
        op.remove_recursive(&path("missing/below"))
            .await
            .expect("remove_recursive of a missing directory");

        assert_eq!(
            std::fs::read(w.write_dir().join("new/deep/n.txt")).expect("write/new/deep/n.txt"),
            b"fresh"
        );
        for fs in [w.fs.clone(), w.reopen().await] {
            assert_ne!(
                mode_at(&fs, "dir/a.txt").await & 0o111,
                0,
                "dir/a.txt is executable"
            );
            assert_eq!(served_at(&fs, "dir/a.txt").await, b"alpha");
            assert_eq!(
                listed(&fs, ino_at(&fs, "new/deep").await.expect("new/deep")).await,
                vec!["n.txt"]
            );
            assert_eq!(served_at(&fs, "new/deep/n.txt").await, b"fresh");
            assert_eq!(ino_at(&fs, "dir/b.txt").await, None);
            assert_eq!(served_at(&fs, "dir/c.txt").await, b"bravo");
            assert_eq!(ino_at(&fs, "gone").await, None);
            assert_eq!(served_at(&fs, "dir/w.txt").await, README);
            assert_eq!(served_at(&fs, "s.txt").await, README);
            assert_eq!(served_at(&fs, "copy.txt").await, b"alpha");
        }
    })
    .await;
}

/// An instance rename of a tracked directory, which RENAME refuses with `EXDEV`, moves it child
/// by child: its files are served at the new path, and nothing stands at the old one.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_rename_of_a_tracked_directory_moves_its_files() {
    in_context(async {
        let w = working(&[("dir/a.txt", b"alpha"), ("dir/sub/b.txt", b"bravo")]).await;
        let op = operation_of(&w.fs);

        op.rename(&path("dir"), &path("moved"))
            .await
            .expect("rename of a tracked directory");

        for fs in [w.fs.clone(), w.reopen().await] {
            assert_eq!(ino_at(&fs, "dir").await, None);
            assert_eq!(served_at(&fs, "moved/a.txt").await, b"alpha");
            assert_eq!(served_at(&fs, "moved/sub/b.txt").await, b"bravo");
        }
    })
    .await;
}

/// An instance `remove_recursive` of a directory holding a child the view hides removes the
/// child with it, as an ordinary clone holds no such child to keep: RMDIR's emptiness check skips
/// what the view hides, and the detach takes it along. The directory is gone, after a remount
/// too, and `status --scan` reports the directory and the file the view shows, unstaged, never
/// the hidden one; a plain status reports the directory alone, as on an ordinary clone.
#[tokio::test(flavor = "multi_thread")]
async fn remove_recursive_removes_a_child_the_view_hides() {
    in_context(async {
        let fuse_fixture::ReleasedFixture {
            repository,
            immutable,
            mutable,
            path: root,
            state,
            directory: _directory,
        } = Box::pin(fuse_fixture::released(&[
            ("dir/a.txt", b"alpha"),
            ("dir/secret.txt", b"hidden"),
            ("keep.txt", b"keep"),
        ]))
        .await;
        let mut filter = lore_revision::filter::Filter::default();
        filter
            .view
            .add_exclusion("dir/secret.txt")
            .expect("filter exclusion");
        let filter = Arc::new(filter);
        let viewed = Arc::new(RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(immutable.clone(), mutable.clone())
                .with_path(&root)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id)
                .with_filter(filter.clone()),
        ));
        let external = TempDir::new("lore-fuse-operation-");
        let fs = writable_provider(&viewed, &state, &external).await;
        assert_eq!(
            listed(&fs, ino_at(&fs, "dir").await.expect("dir")).await,
            vec!["a.txt"]
        );

        operation_of(&fs)
            .remove_recursive(&path("dir"))
            .await
            .expect("remove_recursive of a directory holding a hidden child");

        for fs in [
            fs.clone(),
            writable_provider(&viewed, &state, &external).await,
        ] {
            assert_eq!(ino_at(&fs, "dir").await, None);
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt"]);
        }
        let token = RepositoryWriteToken::acquire(&root).await;
        let context = Arc::new(
            RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(immutable, mutable)
                    .with_path(&root)
                    .with_id(repository.id)
                    .with_instance_id(repository.instance_id)
                    .with_filter(filter)
                    .with_filesystem_provider(Arc::new(MountedProvider(fs.clone()))),
            )
            .with_write_token(token),
        );
        let plain = staged_state::status_files(context.clone(), false).await;
        assert_eq!(
            plain.into_iter().collect::<Vec<_>>(),
            vec![("dir".to_string(), (LoreFileAction::Delete, true))],
            "a plain status reports the deleted directory as one entry"
        );
        let scanned = staged_state::status_files(context, true).await;
        assert_eq!(
            scanned.into_iter().collect::<Vec<_>>(),
            vec![
                ("dir".to_string(), (LoreFileAction::Delete, true)),
                ("dir/a.txt".to_string(), (LoreFileAction::Delete, true)),
            ]
        );
    })
    .await;
}

/// An instance rename of a tracked directory that changes only its case respells it in place, as
/// on an ordinary clone: its files stay below the new spelling, after a remount too, and status
/// reports the directory moved to it, as an ordinary clone's diff reports an unmodified entry
/// under another spelling.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_case_rename_of_a_tracked_directory_respells_it() {
    in_context(async {
        let w = working(&[("dir/a.txt", b"alpha"), ("dir/sub/b.txt", b"bravo")]).await;

        operation_of(&w.fs)
            .rename(&path("dir"), &path("Dir"))
            .await
            .expect("a case-only rename of a tracked directory");

        for fs in [w.fs.clone(), w.reopen().await] {
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"alpha");
            assert_eq!(served_at(&fs, "Dir/sub/b.txt").await, b"bravo");
        }
        let status: Vec<(String, LoreFileAction)> = w
            .status()
            .await
            .into_iter()
            .map(|(path, (action, _))| (path, action))
            .collect();
        assert_eq!(status, vec![("Dir".to_string(), LoreFileAction::Move)]);
    })
    .await;
}

/// An instance rename of a tracked directory holding a child the view hides moves it child by
/// child: the children the view shows arrive, the source goes, and nothing leaves the
/// destination.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_rename_of_a_directory_holding_a_hidden_child_moves_it() {
    in_context(async {
        let fuse_fixture::ReleasedFixture {
            repository,
            immutable,
            mutable,
            path: root,
            state,
            directory: _directory,
        } = Box::pin(fuse_fixture::released(&[
            ("dir/a.txt", b"alpha"),
            ("dir/secret.txt", b"hidden"),
            ("dir/sub/b.txt", b"bravo"),
        ]))
        .await;
        let mut filter = lore_revision::filter::Filter::default();
        filter
            .view
            .add_exclusion("dir/secret.txt")
            .expect("filter exclusion");
        let viewed = Arc::new(RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(immutable, mutable)
                .with_path(&root)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id)
                .with_filter(Arc::new(filter)),
        ));
        let external = TempDir::new("lore-fuse-operation-");
        let fs = writable_provider(&viewed, &state, &external).await;

        operation_of(&fs)
            .rename(&path("dir"), &path("moved"))
            .await
            .expect("rename of a directory holding a hidden child");

        for fs in [
            fs.clone(),
            writable_provider(&viewed, &state, &external).await,
        ] {
            assert_eq!(ino_at(&fs, "dir").await, None);
            assert_eq!(served_at(&fs, "moved/a.txt").await, b"alpha");
            assert_eq!(served_at(&fs, "moved/sub/b.txt").await, b"bravo");
        }
    })
    .await;
}

/// An instance copy keeps the source's executable bit, as `std::fs::copy` does on an ordinary
/// clone, from a tracked source and from a local one.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_copy_keeps_the_sources_executable_bit() {
    in_context(async {
        let w = working(&[("run.sh", b"#!/bin/sh\n"), ("plain.txt", b"plain")]).await;
        let op = operation_of(&w.fs);
        op.make_executable(&path("run.sh"), true)
            .await
            .expect("make_executable");

        op.copy_file(&path("run.sh"), &path("run.sh~mine"))
            .await
            .expect("copy of an executable");
        op.copy_file(&path("plain.txt"), &path("plain.txt~mine"))
            .await
            .expect("copy of a plain file");

        assert_ne!(
            mode_at(&w.fs, "run.sh~mine").await & 0o111,
            0,
            "the copy is executable"
        );
        assert_eq!(
            mode_at(&w.fs, "plain.txt~mine").await & 0o111,
            0,
            "the copy is plain"
        );
        assert_eq!(served_at(&w.fs, "run.sh~mine").await, b"#!/bin/sh\n");
    })
    .await;
}

/// An instance rename of a directory onto one that exists hands its children over: a file the
/// destination holds under the same name takes the source's, the destination's other children
/// stay, and the source goes, as on an ordinary clone.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_rename_onto_an_existing_directory_merges_into_it() {
    in_context(async {
        let w = working(&[("dst/x.txt", b"old x"), ("dst/y.txt", b"y")]).await;
        let op = operation_of(&w.fs);
        op.write_file(&path("src/x.txt"), Bytes::from_static(b"new x"))
            .await
            .expect("write src/x.txt");
        op.write_file(&path("src/z.txt"), Bytes::from_static(b"z"))
            .await
            .expect("write src/z.txt");

        op.rename(&path("src"), &path("dst"))
            .await
            .expect("rename onto an existing directory");

        for fs in [w.fs.clone(), w.reopen().await] {
            assert_eq!(ino_at(&fs, "src").await, None);
            let dst = ino_at(&fs, "dst").await.expect("dst");
            assert_eq!(listed(&fs, dst).await, vec!["x.txt", "y.txt", "z.txt"]);
            assert_eq!(served_at(&fs, "dst/x.txt").await, b"new x");
            assert_eq!(served_at(&fs, "dst/y.txt").await, b"y");
            assert_eq!(served_at(&fs, "dst/z.txt").await, b"z");
        }
    })
    .await;
}

#[tokio::test]
async fn content_source_is_virtual_immutable() {
    in_context(async {
        let m = mounted().await;
        let file = path("dir/a.txt");
        let expected = m
            .fixture
            .state
            .find_node(m.fixture.repository.clone(), file.as_str())
            .await
            .expect("committed node")
            .address;
        // Nothing resolved the path first, so no listing below the root is cached.
        match m.operation.content_source(&file).await.expect("source") {
            ContentSource::Virtual(VirtualContent::Immutable { address, size }) => {
                assert_eq!(address.hash, expected.hash);
                assert_eq!(size, A_TXT.len() as u64);
            }
            other => panic!("expected virtual immutable content, got {other}"),
        }
        match m
            .operation
            .content_source(&path("dir/sub/empty.txt"))
            .await
            .expect("source")
        {
            ContentSource::Virtual(VirtualContent::Immutable { address, size }) => {
                assert_eq!(size, 0);
                assert_ne!(
                    address,
                    Address::default(),
                    "an empty file still names content"
                );
            }
            other => panic!("expected virtual immutable content, got {other}"),
        }
    })
    .await;
}

#[tokio::test]
async fn content_source_of_a_path_naming_no_file_is_an_error() {
    in_context(async {
        let m = mounted().await;
        for missing in ["dir/missing.txt", "dir/sub", "README.md/below"] {
            let result = m.operation.content_source(&path(missing)).await;
            let error = result.map(|s| s.to_string()).expect_err(missing);
            assert!(
                matches!(error, FsError::FileNotFound(_)),
                "{missing}: a path naming no file is not found, got {error}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn file_holds_content_compares_addresses_without_reading() {
    in_context(async {
        let m = mounted().await;
        let file = path("dir/a.txt");
        let node = m
            .fixture
            .state
            .find_node(m.fixture.repository.clone(), file.as_str())
            .await
            .expect("committed node");
        let established = lore_storage::ContentHashes::default();
        assert!(matches!(
            m.operation
                .file_holds_content(
                    m.fixture.repository.clone(),
                    &file,
                    node.address,
                    node.size,
                    &established,
                )
                .await
                .expect("compare"),
            NodeComparison::Matches
        ));
        let mut other = node.address;
        other.hash = lore_base::types::Hash::from_u64(7);
        assert!(matches!(
            m.operation
                .file_holds_content(
                    m.fixture.repository.clone(),
                    &file,
                    other,
                    node.size,
                    &established,
                )
                .await
                .expect("compare"),
            NodeComparison::Differs
        ));
        assert!(matches!(
            m.operation
                .file_holds_content(
                    m.fixture.repository.clone(),
                    &path("dir/missing"),
                    node.address,
                    node.size,
                    &established,
                )
                .await
                .expect("compare"),
            NodeComparison::Differs
        ));
    })
    .await;
}

#[tokio::test]
async fn infer_is_diffable_reads_first_4k() {
    in_context(async {
        let m = mounted().await;
        assert!(
            m.operation
                .infer_is_diffable(&path("README.md"))
                .await
                .expect("text")
        );
        assert!(
            !m.operation
                .infer_is_diffable(&path("blob.bin"))
                .await
                .expect("binary")
        );
        assert!(
            !m.operation
                .infer_is_diffable(&path("missing"))
                .await
                .expect("a path holding no file is an answer")
        );
    })
    .await;
}

#[tokio::test]
async fn finalize_is_ok_and_holds_state_is_true() {
    in_context(async {
        let m = mounted().await;
        assert!(m.operation.holds_state());
        m.operation.finalize().await.expect("finalize");
    })
    .await;
}

/// A provider whose operations answer from one mounted filesystem.
struct MountedProvider(Arc<FuseFilesystem>);

#[async_trait::async_trait]
impl FilesystemProvider for MountedProvider {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Ok(operation_of(&self.0))
    }

    fn holds_state(&self) -> bool {
        true
    }
}

/// How long the held-invalidation test waits on each step before calling it stuck.
const HELD_WAIT: Duration = Duration::from_secs(60);

/// Records each invalidation, holding the first until released, as the kernel holds an entry
/// invalidation beside a LOOKUP it is waiting on.
struct HeldSink {
    entries: Mutex<Vec<(u64, Vec<u8>)>>,
    entered: mpsc::Sender<()>,
    released: Mutex<Option<mpsc::Receiver<()>>>,
}

impl HeldSink {
    fn hold(&self) {
        let _ = self.entered.send(());
        let released = self.released.lock().expect("released").take();
        // Until the test releases it, or drops its sender on a failure.
        if let Some(released) = released {
            let _ = released.recv();
        }
    }
}

impl NotificationSink for HeldSink {
    fn inval_inode(&self, _ino: u64) {
        self.hold();
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        self.entries
            .lock()
            .expect("entries")
            .push((parent, name.to_vec()));
        self.hold();
    }
}

/// An instance operation's invalidations go out in one batch from the mount's notification
/// thread: while the kernel holds one, the one runtime worker stays free to answer the LOOKUP it
/// waits on.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn an_instance_operations_held_invalidation_leaves_the_runtime_worker_free() {
    in_context(async {
        let w = working(&[
            ("dir/a.txt", b"a"),
            ("dir/sub/b.txt", b"b"),
            ("dir/sub/c.txt", b"c"),
        ])
        .await;
        let dir = ino_at(&w.fs, "dir").await.expect("dir");
        let sub = ino_at(&w.fs, "dir/sub").await.expect("dir/sub");
        let (entered, entered_now) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let sink = Arc::new(HeldSink {
            entries: Mutex::new(Vec::new()),
            entered,
            released: Mutex::new(Some(released)),
        });
        w.fs.set_notifications(sink.clone())
            .expect("the notification thread");
        let op = operation_of(&w.fs);
        let (done, done_now) = mpsc::channel();
        // The tasks carry their own execution scope, or need none.
        #[allow(clippy::disallowed_methods)]
        let _remove = tokio::spawn(LORE_CONTEXT.scope(
            fuse_fixture::setup_test_execution(),
            async move {
                let removed = op.remove_recursive(&path("dir/sub")).await;
                let _ = done.send(removed.is_ok());
            },
        ));
        let held = entered_now.recv_timeout(HELD_WAIT);
        let (probe, probed) = mpsc::channel();
        #[allow(clippy::disallowed_methods)]
        let _probe = tokio::spawn(async move {
            let _ = probe.send(());
        });
        let free = probed.recv_timeout(HELD_WAIT);
        let pending = done_now.try_recv();
        let _ = release.send(());
        let removed = done_now.recv_timeout(HELD_WAIT);
        held.expect("the operation's invalidation reaches the mount's sink");
        free.expect("the runtime worker runs another task while the invalidation is held");
        assert!(
            pending.is_err(),
            "the operation waits for its invalidations while they are held"
        );
        assert_eq!(removed, Ok(true), "the removal completes once released");
        assert_eq!(
            w.fs.notifications().expect("the mount's queue").batches(),
            1,
            "one batch for the operation"
        );
        let entries = sink.entries.lock().expect("entries").clone();
        for entry in [
            (sub, b"b.txt".to_vec()),
            (sub, b"c.txt".to_vec()),
            (dir, b"sub".to_vec()),
        ] {
            assert!(entries.contains(&entry), "{entry:?} in {entries:?}");
        }
    })
    .await;
}

/// An instance operation dropped after its first change still invalidates what it changed.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_operation_dropped_after_a_change_still_invalidates_it() {
    in_context(async {
        let w = working(&[
            ("dir/sub/b.txt", b"b"),
            ("dir/sub/c.txt", b"c"),
            ("dir/other/keep.txt", b"k"),
        ])
        .await;
        let sub = ino_at(&w.fs, "dir/sub").await.expect("dir/sub");
        let sub_node = match w.fs.inodes().get(sub).expect("sub's inode").kind {
            InodeKind::Dir { node, .. } => node,
            InodeKind::File { .. } | InodeKind::Symlink => panic!("dir/sub is a directory"),
        };
        let (entered, entered_now) = mpsc::channel();
        let sink = Arc::new(HeldSink {
            entries: Mutex::new(Vec::new()),
            entered,
            released: Mutex::new(None),
        });
        w.fs.set_notifications(sink.clone())
            .expect("the notification thread");
        // A rename onto a non-empty directory moves child by child. Once b.txt has moved, sub's
        // listing goes, so the lookup of c.txt builds it; that build holds the rename for good.
        let armed = Arc::new(AtomicBool::new(false));
        let reached = Arc::new(tokio::sync::Notify::new());
        {
            let (fs, armed) = (Arc::downgrade(&w.fs), armed.clone());
            w.fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::NodeMoved { file: true, .. })
                    && let Some(fs) = fs.upgrade()
                    && !armed.swap(true, Ordering::SeqCst)
                {
                    fs.served().listings.remove(sub_node);
                }
                true
            }));
        }
        {
            let (armed, reached) = (armed.clone(), reached.clone());
            w.fs.set_listing_hook(Arc::new(
                move |node| -> lore_revision::BoxFuture<'static, ()> {
                    if node != sub_node || !armed.load(Ordering::SeqCst) {
                        return Box::pin(async {});
                    }
                    let reached = reached.clone();
                    Box::pin(async move {
                        reached.notify_one();
                        std::future::pending::<()>().await;
                    })
                },
            ));
        }
        let op = operation_of(&w.fs);
        let (from, to) = (path("dir/sub"), path("dir/other"));
        tokio::select! {
            _ = op.rename(&from, &to) => panic!("the rename waits in the hook"),
            () = reached.notified() => {}
        }
        entered_now
            .recv_timeout(HELD_WAIT)
            .expect("the dropped operation's invalidation reaches the sink");
        let entries = sink.entries.lock().expect("entries").clone();
        assert!(entries.contains(&(sub, b"b.txt".to_vec())), "{entries:?}");
    })
    .await;
}

/// A change that leaves the kernel's other spellings of a name stale invalidates them from the
/// mount's notification thread, not from a shared pool.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_invalidates_other_spellings_from_the_mounts_thread() {
    in_context(async {
        let w = working(&[("README.md", README)]).await;
        let (entered, _entered_now) = mpsc::channel();
        let sink = Arc::new(HeldSink {
            entries: Mutex::new(Vec::new()),
            entered,
            released: Mutex::new(None),
        });
        w.fs.set_notifications(sink.clone())
            .expect("the notification thread");
        // A negative LOOKUP the kernel may cache under this spelling.
        assert_eq!(lookup_ino(&w.fs, ROOT_INO, "NEW").await, None);
        operation_of(&w.fs)
            .create_dir_all(&path("new"))
            .await
            .expect("create_dir_all");
        let queue = w.fs.notifications().expect("the mount's queue");
        // Queued behind the spellings, so they are out once this returns.
        queue
            .send(lore_revision::fs::fuse::notifications::Invalidations {
                entries: Vec::new(),
                inodes: vec![ROOT_INO],
            })
            .await;
        let entries = sink.entries.lock().expect("entries").clone();
        assert!(
            entries.contains(&(ROOT_INO, b"NEW".to_vec())),
            "{entries:?}"
        );
    })
    .await;
}

fn operation_of(fs: &Arc<FuseFilesystem>) -> Arc<InstanceOperationImpl> {
    Arc::new(InstanceOperationImpl::new(
        StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
        true,
    ))
}

/// Staging a name spelled in another case keeps the tree's spelling by renaming on the
/// filesystem, which a mount refuses as read-only rather than as an internal failure.
#[tokio::test]
async fn staging_a_case_change_on_a_mount_reports_read_only() {
    in_context(async {
        let m = mounted().await;
        let repository = Arc::new(
            RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    m.fixture.repository.immutable_store(),
                    m.fixture
                        .repository
                        .try_mutable_store_arc()
                        .expect("fixture holds a write token"),
                )
                .with_path(&m.fixture.path)
                .with_id(m.fixture.repository.id)
                .with_instance_id(m.fixture.repository.instance_id)
                .with_filesystem_provider(Arc::new(MountedProvider(m.fs.clone()))),
            )
            .with_write_token(m.fixture.write_token.share()),
        );
        let staged = lore_revision::file::stage::stage(
            repository,
            &m.fixture.write_token,
            LoreArray::from_vec(vec![LoreString::from(&m.fixture.path.join("dir/A.txt"))]),
            StageOptions {
                case_change: StageCaseChange::Keep,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: false,
            },
        )
        .await;
        let error = staged.expect_err("a mount renames nothing");
        assert!(error.is_not_supported(), "got {error}");
    })
    .await;
}

const FILE_MODE: u32 = libc::S_IFREG | 0o644;
const DIR_MODE: u32 = 0o755;
const UMASK: u32 = 0o022;
const CREATE_FLAGS: u32 = (libc::O_CREAT | libc::O_RDWR) as u32;
const NO_HANDLE: u64 = 0;

/// A writable provider over a committed fixture whose write token is released, as the service
/// holds one: requests go to the provider directly, with no mount.
struct Working {
    fs: Arc<FuseFilesystem>,
    /// The instance's context, holding no write token.
    repository: Arc<RepositoryContext>,
    immutable: Arc<dyn ImmutableStore>,
    mutable: Arc<dyn MutableStore>,
    current: Arc<State>,
    path: PathBuf,
    external: TempDir,
}

impl Working {
    fn write_dir(&self) -> PathBuf {
        self.external.path().join("write")
    }

    /// Another provider over the same revision and instance directory, as a remount builds.
    async fn reopen(&self) -> Arc<FuseFilesystem> {
        writable_provider(&self.repository, &self.current, &self.external).await
    }

    /// The instance's context with `fs` as its filesystem, as a command relayed to the service
    /// opens it, holding `token` if given.
    fn command_context(
        &self,
        fs: &Arc<FuseFilesystem>,
        token: Option<RepositoryWriteToken>,
    ) -> Arc<RepositoryContext> {
        let context = RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(
                self.immutable.clone(),
                self.mutable.clone(),
            )
            .with_path(&self.path)
            .with_id(self.repository.id)
            .with_instance_id(self.repository.instance_id)
            .with_filesystem_provider(Arc::new(MountedProvider(fs.clone()))),
        );
        Arc::new(match token {
            Some(token) => context.with_write_token(token),
            None => context,
        })
    }

    /// Runs `status` without `--scan` under the instance's write token, which records what the
    /// provider's diff finds in the staged State.
    async fn status(&self) -> BTreeMap<String, (LoreFileAction, bool)> {
        let token = RepositoryWriteToken::acquire(&self.path).await;
        staged_state::status_files(self.command_context(&self.fs, Some(token)), false).await
    }

    /// [`Self::status`] of `at` alone, or of the whole tree for `None`.
    async fn status_at(&self, at: Option<&str>) -> BTreeMap<String, (LoreFileAction, bool)> {
        let token = RepositoryWriteToken::acquire(&self.path).await;
        crate::helper::test_status_files_at(
            self.command_context(&self.fs, Some(token)),
            at.map(|at| vec![path(at)]),
            false,
        )
        .await
    }

    /// The staged State the instance's anchor names.
    async fn staged(&self) -> Arc<State> {
        let staged = lore_revision::instance::load_staged_revision(&self.repository)
            .await
            .expect("the staged anchor reads")
            .expect("a staged anchor is stored");
        assert_ne!(
            staged,
            self.current.revision(),
            "the staged State is not the current one"
        );
        State::deserialize(self.repository.clone(), staged)
            .await
            .expect("the staged State loads")
    }

    /// The provider's diff of the whole tree against `from`, sorted by path.
    async fn diff(&self, from: &Arc<State>, intent: FilesystemDiffIntent) -> Vec<NodeChange> {
        self.diff_at(from, "", intent).await
    }

    /// The provider's diff of the tree below `at` against `from`, sorted by path.
    async fn diff_at(
        &self,
        from: &Arc<State>,
        at: &str,
        intent: FilesystemDiffIntent,
    ) -> Vec<NodeChange> {
        self.counted_diff_at(from, at, intent).await.0
    }

    /// [`Self::diff`], with what the diff counted.
    async fn counted_diff(
        &self,
        from: &Arc<State>,
        intent: FilesystemDiffIntent,
    ) -> (Vec<NodeChange>, FilesystemDiffStats) {
        self.counted_diff_at(from, "", intent).await
    }

    /// [`Self::diff_at`], with what the diff counted.
    async fn counted_diff_at(
        &self,
        from: &Arc<State>,
        at: &str,
        intent: FilesystemDiffIntent,
    ) -> (Vec<NodeChange>, FilesystemDiffStats) {
        let from = self.mapping(from, at).await;
        self.diff_from(from, at, intent).await
    }

    /// `state`'s node at `at`, found by name.
    async fn mapping(&self, state: &Arc<State>, at: &str) -> NodeMapping {
        NodeMapping {
            repository: self.repository.clone(),
            state: state.clone(),
            path: path(at),
            node: state
                .find_node_link(self.repository.clone(), at)
                .await
                .map_or(lore_revision::node::INVALID_NODE, |link| link.node),
        }
    }

    /// The provider's diff of the tree below `at` against `from` as given, sorted by path, with
    /// what it counted.
    async fn diff_from(
        &self,
        from: NodeMapping,
        at: &str,
        intent: FilesystemDiffIntent,
    ) -> (Vec<NodeChange>, FilesystemDiffStats) {
        let operation = operation_of(&self.fs);
        let diff = FilesystemDiffContext {
            operation: operation.clone(),
            from,
            current: self.mapping(&self.current, at).await,
            filesystem_path: path(at),
            states: FilterStates::ROOT,
            from_states: FilterStates::ROOT,
            named_path: None,
            filter_mode: FilterMode::Full,
            intent,
            layer_mounts: Arc::new(Vec::new()),
            link_mounts: Arc::new(Vec::new()),
            discards: None,
        };
        let mut stream = operation.changes_from_filesystem_to_state(diff);
        let mut changes = Vec::new();
        while let Some(change) = stream.next().await {
            changes.push(change);
        }
        let stats = stream.finish().await.expect("the diff walks");
        lore_revision::change::sort_by_path(&mut changes);
        (changes, stats)
    }
}

/// Each change as its path, its action and whether it measured a modification.
fn summary(changes: &[NodeChange]) -> Vec<(String, FileAction, bool)> {
    changes
        .iter()
        .map(|change| {
            (
                change.path().to_string(),
                change.action,
                change.flags.contains(lore_revision::change::Flags::Modify),
            )
        })
        .collect()
}

async fn writable_provider(
    repository: &Arc<RepositoryContext>,
    current: &Arc<State>,
    external: &TempDir,
) -> Arc<FuseFilesystem> {
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new_writable(
                repository.clone(),
                current.clone(),
                attr_context(),
                external.path(),
            ),
        )
        .await
        .expect("writable provider construction")
}

async fn working(files: &[(&str, &[u8])]) -> Working {
    let fuse_fixture::ReleasedFixture {
        repository,
        immutable,
        mutable,
        path,
        state,
        ..
    } = Box::pin(fuse_fixture::released(files)).await;
    let external = TempDir::new("lore-fuse-operation-");
    let fs = writable_provider(&repository, &state, &external).await;
    Working {
        fs,
        repository,
        immutable,
        mutable,
        current: state,
        path,
        external,
    }
}

async fn create(fs: &FuseFilesystem, parent: u64, name: &str) -> (u64, u64) {
    let created = fs
        .create(
            &req(),
            parent,
            name.as_bytes(),
            FILE_MODE,
            UMASK,
            CREATE_FLAGS,
        )
        .await
        .unwrap_or_else(|errno| panic!("create {name}: {errno:?}"));
    (created.entry.ino, created.open.fh)
}

async fn write_all(fs: &FuseFilesystem, ino: u64, fh: u64, offset: u64, data: &[u8]) {
    let written = fs
        .write(&req(), ino, fh, offset, data, 0)
        .await
        .unwrap_or_else(|errno| panic!("write to {ino}: {errno:?}"));
    assert_eq!(written, data.len());
}

async fn write_new(fs: &FuseFilesystem, parent: u64, name: &str, data: &[u8]) -> u64 {
    let (ino, fh) = create(fs, parent, name).await;
    write_all(fs, ino, fh, 0, data).await;
    ino
}

async fn read_all(fs: &FuseFilesystem, ino: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    match fs.read(&req(), ino, 0, 0, &mut out, true).await {
        Ok(read) => out.truncate(read),
        Err(ReadFault::Errno(errno)) => panic!("read of {ino}: {errno:?}"),
        Err(ReadFault::WouldFetch) => panic!("read of {ino} deferred"),
    }
    out
}

async fn mkdir(fs: &FuseFilesystem, parent: u64, name: &str) -> u64 {
    fs.mkdir(&req(), parent, name.as_bytes(), DIR_MODE, UMASK)
        .await
        .unwrap_or_else(|errno| panic!("mkdir {name}: {errno:?}"))
        .ino
}

async fn lookup_ino(fs: &FuseFilesystem, parent: u64, name: &str) -> Option<u64> {
    match fs.lookup(&req(), parent, name.as_bytes()).await {
        Ok(reply) if reply.ino != 0 => Some(reply.ino),
        Ok(_) | Err(Errno::ENOENT) => None,
        Err(errno) => panic!("lookup {name}: {errno:?}"),
    }
}

async fn rename(fs: &FuseFilesystem, parent: u64, name: &str, newparent: u64, newname: &str) {
    fs.rename(
        &req(),
        parent,
        name.as_bytes(),
        newparent,
        newname.as_bytes(),
        0,
    )
    .await
    .unwrap_or_else(|errno| panic!("rename {name} to {newname}: {errno:?}"));
}

async fn listed(fs: &FuseFilesystem, ino: u64) -> Vec<String> {
    let mut buffer = vec![0u8; 64 << 10];
    let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut buffer);
    fs.readdir(&req(), ino, NO_HANDLE, 0, &mut writer)
        .await
        .expect("readdir");
    let used = writer.finish();
    let mut names: Vec<String> = fuse_test_util::dirents(&buffer[..used], false)
        .filter(|dirent| dirent.name != b"." && dirent.name != b"..")
        .map(|dirent| String::from_utf8(dirent.name.to_vec()).expect("utf-8 name"))
        .collect();
    names.sort();
    names
}

const FIXTURE: &[(&str, &[u8])] = &[
    ("a.txt", b"alpha"),
    ("b.txt", b"bee"),
    ("dir/c.txt", b"sea"),
    ("old.txt", b"old"),
];

/// Rewrites `a.txt`, creates `new.txt` and unlinks `b.txt` through `w`'s provider.
async fn write_add_and_delete(w: &Working) {
    let a = lookup_ino(&w.fs, ROOT_INO, "a.txt").await.expect("a.txt");
    write_all(&w.fs, a, 0, 0, b"ALPHA").await;
    write_new(&w.fs, ROOT_INO, "new.txt", b"new").await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");
}

#[tokio::test]
async fn changes_from_filesystem_to_state_reports_a_written_an_added_and_a_deleted_file() {
    in_context(async {
        let w = working(FIXTURE).await;
        write_add_and_delete(&w).await;

        let changes = w.diff(&w.current, FilesystemDiffIntent::Report).await;
        assert_eq!(
            summary(&changes),
            vec![
                ("a.txt".to_string(), FileAction::Keep, true),
                ("b.txt".to_string(), FileAction::Delete, false),
                ("new.txt".to_string(), FileAction::Add, false),
            ]
        );
        let added = &changes[2];
        assert!(added.to.flags.contains(NodeFlags::File));
        assert_eq!(
            added.to.observed.map(|observed| observed.size()),
            Some(3),
            "an added file reports the size its host file holds"
        );
    })
    .await;
}

#[tokio::test]
async fn the_diff_marks_the_from_state_dirty_when_asked_and_clears_a_settled_node() {
    in_context(async {
        let w = working(FIXTURE).await;
        write_add_and_delete(&w).await;
        // A fresh copy of the revision stands for the staged State lore passes as `from`.
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        let settled = from
            .find_node_link(w.repository.clone(), "dir/c.txt")
            .await
            .expect("dir/c.txt")
            .node;
        from.node_mark(
            &w.repository,
            settled,
            Mark::dirty(
                NodeFlags::DirtyModify,
                Ext::Set(NodeExtFlags::NoFlags),
                false,
            ),
        )
        .await
        .expect("an earlier status's mark");
        let planted = BTreeMap::from([(
            "dir/c.txt".to_string(),
            (Action::Modify, NodeExtFlags::NoFlags),
        )]);

        w.diff(&from, FilesystemDiffIntent::Report).await;
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from).await,
            planted,
            "a report marks nothing"
        );

        let changes = w.diff(&from, FilesystemDiffIntent::MarkDirty).await;
        assert!(
            changes.iter().all(|change| change.flags.is_dirty()),
            "each change carries the mark it left"
        );
        let none = NodeExtFlags::NoFlags;
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from).await,
            BTreeMap::from([
                ("a.txt".to_string(), (Action::Modify, none)),
                ("b.txt".to_string(), (Action::Delete, none)),
                ("new.txt".to_string(), (Action::Add, none)),
            ])
        );
        let dir = from
            .find_node_link(w.repository.clone(), "dir")
            .await
            .expect("dir")
            .node;
        assert!(
            !from
                .node(w.repository.clone(), dir)
                .await
                .expect("dir")
                .is_dirty(),
            "clearing the settled node clears the directory it alone dirtied"
        );
    })
    .await;
}

/// A provider diff asked to stage marks as a marking one does and stages nothing: lore's stage
/// walk stages what the diff marked.
#[tokio::test]
async fn a_provider_diff_asked_to_stage_marks_and_stages_nothing() {
    in_context(async {
        let w = working(FIXTURE).await;
        write_add_and_delete(&w).await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");

        w.diff(&from, FilesystemDiffIntent::Stage(StageIntent::default()))
            .await;
        let none = NodeExtFlags::NoFlags;
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from).await,
            BTreeMap::from([
                ("a.txt".to_string(), (Action::Modify, none)),
                ("b.txt".to_string(), (Action::Delete, none)),
                ("new.txt".to_string(), (Action::Add, none)),
            ])
        );
        for at in ["a.txt", "b.txt", "new.txt"] {
            let node = from
                .find_node_link(w.repository.clone(), at)
                .await
                .expect("a marked node")
                .node;
            assert!(
                !from
                    .node(w.repository.clone(), node)
                    .await
                    .expect("a marked node")
                    .is_staged(),
                "{at} is marked, not staged"
            );
        }
    })
    .await;
}

/// A file renamed only in case is reported under its new spelling as a move, and a marking diff
/// marks it moved.
#[tokio::test]
async fn a_marking_diff_marks_a_case_only_rename_as_a_move() {
    in_context(async {
        let w = working(FIXTURE).await;
        rename(&w.fs, ROOT_INO, "a.txt", ROOT_INO, "A.txt").await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");

        assert_eq!(
            summary(&w.diff(&from, FilesystemDiffIntent::MarkDirty).await),
            vec![("A.txt".to_string(), FileAction::Move, true)]
        );
        let a = from
            .find_node_link(w.repository.clone(), "a.txt")
            .await
            .expect("a.txt")
            .node;
        let marked = from.node(w.repository.clone(), a).await.expect("a.txt");
        assert!(
            marked.is_dirty_move(),
            "the renamed file is marked moved, flags {:#x}",
            marked.flags
        );
    })
    .await;
}

/// A file renamed only in case below a directory is reported under its new spelling as a move,
/// as one at the root is: the walk reaches the directory holding it.
#[tokio::test]
async fn a_case_only_rename_below_a_directory_is_reported_as_a_move() {
    in_context(async {
        let w = working(FIXTURE).await;
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        rename(&w.fs, dir, "c.txt", dir, "C.txt").await;

        assert_eq!(
            summary(&w.diff(&w.current, FilesystemDiffIntent::Report).await),
            vec![("dir/C.txt".to_string(), FileAction::Move, true)]
        );
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        assert_eq!(
            summary(&w.diff(&from, FilesystemDiffIntent::MarkDirty).await),
            vec![("dir/C.txt".to_string(), FileAction::Move, true)]
        );
    })
    .await;
}

/// A tracked file whose mode alone changed is a modification.
#[tokio::test]
async fn a_diff_reports_a_tracked_file_whose_mode_alone_changed() {
    in_context(async {
        let w = working(FIXTURE).await;
        operation_of(&w.fs)
            .make_executable(&path("a.txt"), true)
            .await
            .expect("make_executable");

        assert_eq!(
            summary(&w.diff(&w.current, FilesystemDiffIntent::Report).await),
            vec![("a.txt".to_string(), FileAction::Keep, true)]
        );
    })
    .await;
}

#[tokio::test]
async fn a_marking_diff_rooted_in_a_new_directory_adds_it_with_its_files() {
    in_context(async {
        let w = working(FIXTURE).await;
        let saved = mkdir(&w.fs, ROOT_INO, "Saved").await;
        write_new(&w.fs, saved, "log.txt", b"log").await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");

        let changes = w
            .diff_at(&from, "Saved", FilesystemDiffIntent::MarkDirty)
            .await;
        assert_eq!(
            summary(&changes),
            vec![("Saved/log.txt".to_string(), FileAction::Add, false)]
        );
        let none = NodeExtFlags::NoFlags;
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from).await,
            BTreeMap::from([
                ("Saved".to_string(), (Action::Add, none)),
                ("Saved/log.txt".to_string(), (Action::Add, none)),
            ])
        );
    })
    .await;
}

/// An add the walk reports below a directory it did not list in `from` is attached under the
/// directory `from` holds by that name; a name `from` holds there already is marked on the node
/// holding it, and a delete staged for it is taken back, rather than a second node made.
#[tokio::test]
async fn an_add_below_a_directory_the_walk_did_not_list_marks_the_node_holding_its_name() {
    in_context(async {
        let w = working(FIXTURE).await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        let c = from
            .find_node_link(w.repository.clone(), "dir/c.txt")
            .await
            .expect("dir/c.txt")
            .node;
        from.node_mark(
            &w.repository,
            c,
            Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
        )
        .await
        .expect("a staged delete");
        let unlisted = NodeMapping {
            repository: w.repository.clone(),
            state: from.clone(),
            path: path("dir"),
            node: lore_revision::node::INVALID_NODE,
        };

        let (changes, _) = w
            .diff_from(unlisted, "dir", FilesystemDiffIntent::MarkDirty)
            .await;
        assert_eq!(
            summary(&changes)
                .into_iter()
                .map(|(path, action, _)| (path, action))
                .collect::<Vec<_>>(),
            vec![("dir/c.txt".to_string(), FileAction::Add)]
        );
        assert_eq!(
            children_named(&w.repository, &from, "dir", "c.txt").await,
            1,
            "the name stays held by one node"
        );
        let held = from.node(w.repository.clone(), c).await.expect("c.txt");
        assert!(
            !held.is_staged_delete(),
            "the delete staged for the name is taken back, flags {:#x}",
            held.flags
        );
        assert!(
            held.is_dirty_modify(),
            "the node holding the name is marked modified, flags {:#x}",
            held.flags
        );
    })
    .await;
}

#[tokio::test]
async fn a_local_content_node_with_the_from_address_is_not_a_change() {
    in_context(async {
        let w = working(FIXTURE).await;
        // What a command writing content it knows the address of leaves: the bytes under
        // `write/` and a local-content node at that address, not dirty.
        std::fs::write(w.write_dir().join("a.txt"), b"alpha").expect("the host copy");
        let served = w.fs.served();
        let a = served
            .state
            .find_node_link(served.repository.clone(), "a.txt")
            .await
            .expect("a.txt")
            .node;
        let committed = served
            .state
            .node(served.repository.clone(), a)
            .await
            .expect("a.txt");
        served
            .state
            .node_rewrite(
                &served.repository,
                a,
                &Content {
                    mode: committed.mode,
                    size: committed.size,
                    address: committed.address,
                },
                Rewrite::Dirty(NodeFlags::NoFlags, NodeExtFlags::LocalContent),
            )
            .await
            .expect("a local copy of the committed content");
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        let c = lookup_ino(&w.fs, dir, "c.txt").await.expect("c.txt");
        write_all(&w.fs, c, 0, 0, b"SEA").await;

        assert_eq!(
            summary(&w.diff(&w.current, FilesystemDiffIntent::Report).await),
            vec![("dir/c.txt".to_string(), FileAction::Keep, true)]
        );
    })
    .await;
}

/// A dirty local-content node at the from address is compared by address: its host file is not
/// read.
#[tokio::test]
async fn a_dirty_local_content_node_with_the_from_address_is_compared_by_address() {
    in_context(async {
        let w = working(FIXTURE).await;
        std::fs::write(w.write_dir().join("a.txt"), b"alpha").expect("the host copy");
        let served = w.fs.served();
        let a = served
            .state
            .find_node_link(served.repository.clone(), "a.txt")
            .await
            .expect("a.txt")
            .node;
        let committed = served
            .state
            .node(served.repository.clone(), a)
            .await
            .expect("a.txt");
        served
            .state
            .node_rewrite(
                &served.repository,
                a,
                &Content {
                    mode: committed.mode,
                    size: committed.size,
                    address: committed.address,
                },
                Rewrite::Dirty(NodeFlags::DirtyModify, NodeExtFlags::LocalContent),
            )
            .await
            .expect("a dirty local copy of the committed content");
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        let c = lookup_ino(&w.fs, dir, "c.txt").await.expect("c.txt");
        write_all(&w.fs, c, 0, 0, b"SEA").await;

        let (changes, stats) = w
            .counted_diff(&w.current, FilesystemDiffIntent::Report)
            .await;
        assert_eq!(
            summary(&changes),
            vec![("dir/c.txt".to_string(), FileAction::Keep, true)]
        );
        assert_eq!(
            stats.file_hash.load(Ordering::Relaxed),
            1,
            "only dir/c.txt, whose content has no address, is read"
        );
    })
    .await;
}

/// Creates, copies up, deletes and renames through `w`'s provider, each kind of namespace
/// change once: a new file, a new directory holding a file, a rewritten tracked file, an
/// unlinked tracked file, a tracked file renamed into a tracked directory, a new file renamed,
/// and a rewrite of a file already added.
async fn change_every_way(w: &Working) {
    let fs = &w.fs;
    let new = write_new(fs, ROOT_INO, "new.txt", b"new").await;
    let saved = mkdir(fs, ROOT_INO, "Saved").await;
    write_new(fs, saved, "log.txt", b"log").await;
    let a = lookup_ino(fs, ROOT_INO, "a.txt").await.expect("a.txt");
    write_all(fs, a, 0, 0, b"ALPHA").await;
    fs.unlink(&req(), ROOT_INO, b"b.txt").await.expect("unlink");
    let dir = lookup_ino(fs, ROOT_INO, "dir").await.expect("dir");
    rename(fs, ROOT_INO, "old.txt", dir, "moved.txt").await;
    write_new(fs, ROOT_INO, "tmp.o", b"object").await;
    rename(fs, ROOT_INO, "tmp.o", ROOT_INO, "final.o").await;
    write_all(fs, new, 0, 0, b"NEW").await;
}

fn expected_actions() -> BTreeMap<String, (Action, NodeExtFlags)> {
    let none = NodeExtFlags::NoFlags;
    [
        ("Saved", (Action::Add, none)),
        ("Saved/log.txt", (Action::Add, none)),
        ("a.txt", (Action::Modify, none)),
        ("b.txt", (Action::Delete, none)),
        ("dir/moved.txt", (Action::Add, none)),
        ("final.o", (Action::Add, none)),
        ("new.txt", (Action::Add, none)),
        ("old.txt", (Action::Delete, none)),
    ]
    .into_iter()
    .map(|(path, action)| (path.to_string(), action))
    .collect()
}

#[tokio::test]
async fn status_reports_the_mount_writes_with_and_without_a_scan() {
    let w = working(FIXTURE).await;
    change_every_way(&w).await;

    let context = w.command_context(&w.fs, None);
    let reported = staged_state::status_files(context.clone(), false).await;
    let files: BTreeMap<&str, (LoreFileAction, bool)> = reported
        .iter()
        .map(|(path, change)| (path.as_str(), *change))
        .collect();
    assert_eq!(
        files,
        BTreeMap::from([
            ("Saved", (LoreFileAction::Add, true)),
            ("Saved/log.txt", (LoreFileAction::Add, true)),
            ("a.txt", (LoreFileAction::Keep, true)),
            ("b.txt", (LoreFileAction::Delete, true)),
            ("dir/moved.txt", (LoreFileAction::Add, true)),
            ("final.o", (LoreFileAction::Add, true)),
            ("new.txt", (LoreFileAction::Add, true)),
            ("old.txt", (LoreFileAction::Delete, true)),
        ])
    );

    let token = RepositoryWriteToken::acquire(&w.path).await;
    let scanned = staged_state::status_files(w.command_context(&w.fs, Some(token)), true).await;
    assert_eq!(
        scanned, reported,
        "a scan reports what the provider's diff finds"
    );
}

#[tokio::test]
async fn a_status_after_one_that_recorded_the_writes_reports_them_again() {
    let w = working(FIXTURE).await;
    change_every_way(&w).await;

    let recorded = w.status().await;
    assert_eq!(recorded.len(), expected_actions().len());
    assert_eq!(
        w.status().await,
        recorded,
        "a status over the staged State the first recorded reports the same writes"
    );
}

#[tokio::test]
async fn a_staged_rebase_keeps_the_dirty_actions() {
    let w = working(FIXTURE).await;
    change_every_way(&w).await;
    w.status().await;
    let staged = w.staged().await;

    let token = RepositoryWriteToken::acquire(&w.path).await;
    let context = w.command_context(&w.fs, Some(token));
    let rebased = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::state::rebase_staged_state(
                context.clone(),
                staged.revision(),
                w.current.revision(),
                false,
            ),
        )
        .await
        .expect("the rebase applies")
        .expect("the rebase leaves something staged");
    let rebased = State::deserialize(context.clone(), rebased)
        .await
        .expect("the rebased State loads");
    assert_eq!(
        staged_state::dirty_actions(&context, &rebased).await,
        expected_actions()
    );
}

#[tokio::test]
async fn a_rewrite_of_an_added_file_leaves_the_staged_state_as_it_was() {
    let w = working(FIXTURE).await;
    let new = write_new(&w.fs, ROOT_INO, "new.txt", b"new").await;
    w.status().await;
    let before = w.staged().await.revision();

    write_all(&w.fs, new, 0, 0, b"rewritten").await;
    w.status().await;
    assert_eq!(w.staged().await.revision(), before);
}

#[tokio::test]
async fn a_later_provider_rebuilds_the_writes_from_write_and_the_deletion_list() {
    let w = working(FIXTURE).await;
    change_every_way(&w).await;
    std::fs::write(w.write_dir().join("stray.txt"), b"not through the mount").expect("plant");
    let saved_before = lookup_ino(&w.fs, ROOT_INO, "Saved").await.expect("Saved");

    let fs = w.reopen().await;

    assert_eq!(
        listed(&fs, ROOT_INO).await,
        vec!["Saved", "a.txt", "dir", "final.o", "new.txt", "stray.txt"],
        "the rebuild serves what write/ holds, less what the deletion list removes"
    );
    assert_eq!(lookup_ino(&fs, ROOT_INO, "b.txt").await, None);
    assert_eq!(lookup_ino(&fs, ROOT_INO, "old.txt").await, None);
    let a = lookup_ino(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
    assert_eq!(read_all(&fs, a, 16).await, b"ALPHA");
    let new = lookup_ino(&fs, ROOT_INO, "new.txt").await.expect("new.txt");
    assert_eq!(read_all(&fs, new, 16).await, b"NEW");
    let dir = lookup_ino(&fs, ROOT_INO, "dir").await.expect("dir");
    assert_eq!(listed(&fs, dir).await, vec!["c.txt", "moved.txt"]);
    let moved = lookup_ino(&fs, dir, "moved.txt").await.expect("moved.txt");
    assert_eq!(read_all(&fs, moved, 16).await, b"old");
    let saved = lookup_ino(&fs, ROOT_INO, "Saved").await.expect("Saved");
    assert_eq!(
        saved, saved_before,
        "a created directory keeps its number across a remount"
    );
    let log = lookup_ino(&fs, saved, "log.txt").await.expect("log.txt");
    assert_eq!(read_all(&fs, log, 16).await, b"log");
    assert!(
        !w.external.path().join("whiteouts").exists(),
        "no whiteout log is kept beside write/"
    );

    write_new(&fs, dir, "more.o", b"more").await;
    let token = RepositoryWriteToken::acquire(&w.path).await;
    staged_state::status_files(w.command_context(&fs, Some(token)), false).await;
    let staged = w.staged().await;
    let mut expected = expected_actions();
    for added in ["dir/more.o", "stray.txt"] {
        expected.insert(added.to_string(), (Action::Add, NodeExtFlags::NoFlags));
    }
    assert_eq!(
        staged_state::dirty_actions(&w.repository, &staged).await,
        expected
    );
}

#[tokio::test]
async fn the_operation_answers_a_whiteout_below_a_tracked_directory_as_absent() {
    let w = working(FIXTURE).await;
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    w.fs.unlink(&req(), dir, b"c.txt").await.expect("unlink");

    let operation = FuseOperation::new(w.fs.clone());
    assert_eq!(
        operation.file_info(&path("dir/c.txt")).await.expect("info"),
        FileInfo::NotExist
    );
    assert_eq!(
        operation.file_info(&path("DIR/C.TXT")).await.expect("info"),
        FileInfo::NotExist
    );
}

/// The address the mounted snapshot holds for the file at `at`.
async fn snapshot_address(fs: &FuseFilesystem, at: &str) -> lore_base::types::Address {
    let snapshot = fs.served();
    fs.entry_at_path(&snapshot, &path(at))
        .await
        .expect("the snapshot reads")
        .unwrap_or_else(|| panic!("{at} is in the snapshot"))
        .address
}

/// The bytes the operation names as the content at `at`: a host file's.
async fn host_content(operation: &FuseOperation, at: &str) -> Vec<u8> {
    let source = operation
        .content_source(&path(at))
        .await
        .unwrap_or_else(|error| panic!("content source of {at}: {error}"));
    let file = source
        .file_path()
        .unwrap_or_else(|| panic!("{at} is read from write/"));
    std::fs::read(file).expect("the host file reads")
}

#[tokio::test]
async fn the_operation_reads_local_content_from_write() {
    let w = working(FIXTURE).await;
    let a_before = snapshot_address(&w.fs, "a.txt").await;
    let b_before = snapshot_address(&w.fs, "b.txt").await;
    let old_address = snapshot_address(&w.fs, "old.txt").await;
    let a = lookup_ino(&w.fs, ROOT_INO, "a.txt").await.expect("a.txt");
    write_all(&w.fs, a, 0, 0, b"ALPHA").await;
    let old = lookup_ino(&w.fs, ROOT_INO, "old.txt")
        .await
        .expect("old.txt");
    write_all(&w.fs, old, 0, 0, b"old").await;
    write_new(&w.fs, ROOT_INO, "new.txt", b"new content").await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");

    let operation = FuseOperation::new(w.fs.clone());
    assert_eq!(host_content(&operation, "a.txt").await, b"ALPHA");
    assert_eq!(host_content(&operation, "new.txt").await, b"new content");
    assert!(matches!(
        operation.content_source(&path("b.txt")).await,
        Err(FsError::FileNotFound(_))
    ));
    assert!(
        operation
            .content_source(&path("dir/c.txt"))
            .await
            .expect("a tracked file's source")
            .file_path()
            .is_none(),
        "an untouched tracked file is read from the store"
    );

    let compare = async |at: &str, previous: lore_base::types::Address, size: u64| {
        operation
            .file_holds_content(
                w.repository.clone(),
                &path(at),
                previous,
                size,
                &lore_storage::ContentHashes::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("comparing {at}: {error}"))
    };
    assert!(matches!(
        compare("a.txt", a_before, 5).await,
        NodeComparison::Differs
    ));
    assert!(matches!(
        compare("old.txt", old_address, 3).await,
        NodeComparison::Matches
    ));
    assert!(matches!(
        compare("b.txt", b_before, 3).await,
        NodeComparison::Differs
    ));

    assert!(
        operation
            .infer_is_diffable(&path("new.txt"))
            .await
            .expect("new.txt")
    );
    assert!(
        !operation
            .infer_is_diffable(&path("b.txt"))
            .await
            .expect("a deleted file is an answer")
    );
}

/// What `write/` holds answers for untracked content, from the host: a copied-up and a created
/// file, and a created directory. A tracked, a deleted and an absent path hold none.
#[tokio::test]
async fn the_operation_answers_untracked_info_of_local_content_from_write() {
    let w = working(FIXTURE).await;
    let a = lookup_ino(&w.fs, ROOT_INO, "a.txt").await.expect("a.txt");
    write_all(&w.fs, a, 0, 0, b"ALPHA!").await;
    write_new(&w.fs, ROOT_INO, "new.txt", b"new content").await;
    mkdir(&w.fs, ROOT_INO, "made").await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");

    let operation = FuseOperation::new(w.fs.clone());
    let info = async |at: &str| {
        operation
            .untracked_file_info(&path(at))
            .await
            .unwrap_or_else(|error| panic!("untracked info of {at}: {error}"))
    };
    assert!(matches!(
        info("a.txt").await,
        FileInfo::File { size: 6, .. }
    ));
    assert!(matches!(
        info("new.txt").await,
        FileInfo::File { size: 11, .. }
    ));
    assert!(matches!(info("made").await, FileInfo::Directory));
    for untracked_none in ["dir/c.txt", "b.txt", "nothing-here"] {
        assert!(
            matches!(info(untracked_none).await, FileInfo::NotExist),
            "{untracked_none}"
        );
    }
}

#[tokio::test]
async fn the_operation_resolves_local_and_deleted_names() {
    let w = working(FIXTURE).await;
    write_new(&w.fs, ROOT_INO, "New.txt", b"new").await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");

    let operation = FuseOperation::new(w.fs.clone());
    assert_eq!(
        operation.holds_name_exactly(&path("New.txt")).await,
        Some(true)
    );
    assert_eq!(
        operation.holds_name_exactly(&path("new.txt")).await,
        Some(false)
    );
    assert_eq!(
        operation.holds_name_exactly(&path("b.txt")).await,
        Some(false)
    );
    assert_eq!(
        operation.holds_name_exactly(&path("a.txt")).await,
        Some(true)
    );
    assert_eq!(
        operation
            .names_folding_to(&path(""), "NEW.TXT")
            .await
            .expect("names"),
        vec!["New.txt".to_string()]
    );
    assert!(
        operation
            .names_folding_to(&path(""), "b.txt")
            .await
            .expect("names")
            .is_empty()
    );
    assert_eq!(
        operation
            .names_folding_to(&path(""), "A.TXT")
            .await
            .expect("names"),
        vec!["a.txt".to_string()]
    );
}

/// A commit through a writable provider stores what `write/` holds for a copied-up
/// and an added file.
#[tokio::test]
async fn a_commit_stores_the_local_content_of_copied_up_and_added_files() {
    let w = working(FIXTURE).await;
    let a = lookup_ino(&w.fs, ROOT_INO, "a.txt").await.expect("a.txt");
    write_all(&w.fs, a, 0, 0, b"ALPHA").await;
    write_new(&w.fs, ROOT_INO, "new.txt", b"new content").await;

    let token = RepositoryWriteToken::acquire(&w.path).await;
    let context = w.command_context(&w.fs, Some(token.share()));
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(vec![LoreString::from(&w.path)]),
                StageOptions::default(),
            )
            .await
            .expect("the writes stage");
            lore_revision::commit::commit_boxed(
                context.clone(),
                &token,
                lore_revision::commit::CommitOptions::new("mount writes".to_string()),
            )
            .await
            .expect("the commit succeeds");
        })
        .await;
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&context)
        .await
        .expect("the committed anchor");
    drop(token);
    let committed = State::deserialize(context.clone(), revision)
        .await
        .expect("the committed State loads");

    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(w.repository.clone(), committed, attr_context()),
        )
        .await
        .expect("a provider over the committed revision");
    let a = lookup_ino(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
    assert_eq!(read_all(&fs, a, 16).await, b"ALPHA");
    let new = lookup_ino(&fs, ROOT_INO, "new.txt").await.expect("new.txt");
    assert_eq!(read_all(&fs, new, 16).await, b"new content");
}

/// Stages `paths` of `w` and commits them, answering the committed State.
async fn stage_and_commit(w: &Working, paths: &[PathBuf]) -> Arc<State> {
    let token = RepositoryWriteToken::acquire(&w.path).await;
    let context = w.command_context(&w.fs, Some(token.share()));
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
                StageOptions::default(),
            )
            .await
            .expect("the change stages");
            lore_revision::commit::commit_boxed(
                context.clone(),
                &token,
                lore_revision::commit::CommitOptions::new("replaced".to_string()),
            )
            .await
            .expect("the commit succeeds");
        })
        .await;
    let (revision, _branch) = lore_revision::instance::load_current_anchor_boxed(&context)
        .await
        .expect("the committed anchor");
    drop(token);
    State::deserialize(context, revision)
        .await
        .expect("the committed State loads")
}

/// A tracked file replaced through the mount by a directory holding a file, with status and stage
/// of `scope`, or of the whole tree for `None`, commits as an ordinary clone's staging commits it:
/// the file's delete and the directory's adds, after which status is clean.
async fn replace_a_tracked_file_by_a_directory_and_commit(scope: Option<&str>) {
    let w = working(&[("x", b"a file first"), ("keep.txt", b"keep")]).await;
    w.fs.unlink(&req(), ROOT_INO, b"x").await.expect("unlink");
    let x = mkdir(&w.fs, ROOT_INO, "x").await;
    write_new(&w.fs, x, "f.txt", b"content below").await;

    let reported = w.status_at(scope).await;
    assert!(
        reported.contains_key("x/f.txt"),
        "status reports the directory's content: {reported:?}"
    );
    let staged = match scope {
        Some(at) => w.path.join(at),
        None => w.path.clone(),
    };
    let committed = stage_and_commit(&w, &[staged]).await;

    assert_eq!(
        children_named(&w.repository, &committed, "", "x").await,
        1,
        "the revision holds one node named x"
    );
    let x = committed
        .find_node_link(w.repository.clone(), "x")
        .await
        .expect("x in the revision");
    let node = committed
        .node(w.repository.clone(), x.node)
        .await
        .expect("x's node");
    assert!(node.is_directory(), "x is a directory in the revision");
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(w.repository.clone(), committed, attr_context()),
        )
        .await
        .expect("a provider over the committed revision");
    let x = lookup_ino(&fs, ROOT_INO, "x").await.expect("x");
    let f = lookup_ino(&fs, x, "f.txt").await.expect("x/f.txt");
    assert_eq!(read_all(&fs, f, 32).await, b"content below");
    assert_eq!(
        w.status().await,
        BTreeMap::new(),
        "status is clean after the commit"
    );
}

#[tokio::test]
async fn a_tracked_file_replaced_by_a_directory_commits_as_the_directory() {
    Box::pin(replace_a_tracked_file_by_a_directory_and_commit(None)).await;
}

#[tokio::test]
async fn a_tracked_file_replaced_by_a_directory_commits_when_status_and_stage_name_it() {
    Box::pin(replace_a_tracked_file_by_a_directory_and_commit(Some("x"))).await;
}

/// A tracked directory replaced through the mount by a file commits as the file: the directory's
/// deletes and the file's add, after which status is clean.
#[tokio::test]
async fn a_tracked_directory_replaced_by_a_file_commits_as_the_file() {
    let w = working(&[("x/f.txt", b"content below"), ("keep.txt", b"keep")]).await;
    let x = lookup_ino(&w.fs, ROOT_INO, "x").await.expect("x");
    w.fs.unlink(&req(), x, b"f.txt").await.expect("unlink");
    w.fs.rmdir(&req(), ROOT_INO, b"x").await.expect("rmdir");
    write_new(&w.fs, ROOT_INO, "x", b"a file now").await;

    let committed = stage_and_commit(&w, std::slice::from_ref(&w.path)).await;

    assert_eq!(
        children_named(&w.repository, &committed, "", "x").await,
        1,
        "the revision holds one node named x"
    );
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(w.repository.clone(), committed, attr_context()),
        )
        .await
        .expect("a provider over the committed revision");
    let x = lookup_ino(&fs, ROOT_INO, "x").await.expect("x");
    assert_eq!(read_all(&fs, x, 32).await, b"a file now");
    assert_eq!(
        w.status().await,
        BTreeMap::new(),
        "status is clean after the commit"
    );
}

#[tokio::test]
async fn the_operation_lists_directories_with_their_local_entries() {
    let w = working(FIXTURE).await;
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    write_new(&w.fs, dir, "new.o", b"object").await;
    let saved = mkdir(&w.fs, ROOT_INO, "Saved").await;
    write_new(&w.fs, saved, "log.txt", b"log").await;
    mkdir(&w.fs, saved, "Logs").await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");

    let operation = FuseOperation::new(w.fs.clone());
    let entries = async |at: &str| {
        let mut listing = operation
            .read_directory(&path(at))
            .await
            .unwrap_or_else(|error| panic!("listing {at}: {error}"));
        let mut entries = Vec::new();
        while let Some(entry) = listing.next().await {
            let entry = entry.expect("entry");
            entries.push((entry.name, entry.info));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    };

    let root: Vec<String> = entries("")
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(root, vec!["Saved", "a.txt", "dir", "old.txt"]);
    let in_dir = entries("dir").await;
    assert_eq!(
        in_dir
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["c.txt", "new.o"]
    );
    assert!(matches!(in_dir[1].1, FileInfo::File { size: 6, .. }));
    let in_saved = entries("Saved").await;
    assert_eq!(
        in_saved
            .iter()
            .map(|(name, info)| (name.as_str(), *info == FileInfo::Directory))
            .collect::<Vec<_>>(),
        vec![("Logs", true), ("log.txt", false)]
    );
    assert!(operation.read_directory(&path("b.txt")).await.is_err());
}

/// How many children of the directory at `dir` in `state` are named `name`.
async fn children_named(
    repository: &Arc<RepositoryContext>,
    state: &Arc<State>,
    dir: &str,
    name: &str,
) -> usize {
    let dir_node = if dir.is_empty() {
        lore_revision::node::ROOT_NODE
    } else {
        state
            .find_node_link(repository.clone(), dir)
            .await
            .expect("the directory")
            .node
    };
    let mut count = 0;
    for child in state
        .node_children(repository.clone(), dir_node)
        .await
        .expect("the directory's children")
    {
        if state
            .node_name_clone(repository.clone(), child)
            .await
            .expect("a child's name")
            == name
        {
            count += 1;
        }
    }
    count
}

/// `dir/` of 400 tracked files and `dir/target.txt`, so a walk of it takes long enough for a
/// save to land inside it.
fn saved_directory() -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = (0..400)
        .map(|i| (format!("dir/f{i:04}.txt"), format!("file {i}").into_bytes()))
        .collect();
    files.push(("dir/target.txt".to_string(), b"target".to_vec()));
    files
}

impl Working {
    /// The provider's diff of the whole tree against `from`, and whether it met the filesystem
    /// State changed under it, a race test's window: a child moved since the walk listed or
    /// collected its directory, or a delete confirmed since the directory's chain changed.
    async fn raced_diff(
        &self,
        from: &Arc<State>,
        intent: FilesystemDiffIntent,
    ) -> (Vec<NodeChange>, bool) {
        let filesystem = self.fs.served().state.clone();
        let walk_moves = filesystem.walk_moves();
        let (changes, stats) = self.counted_diff(from, intent).await;
        let raced = stats.moved_under_walk.load(Ordering::Relaxed) > 0
            || filesystem.walk_moves() > walk_moves;
        (changes, raced)
    }
}

/// Saves `dir/target.txt` of `w` through a temporary until `stop` is set, as editors save.
fn save_through_temporaries(
    w: &Arc<Working>,
    dir: u64,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<u64> {
    let (w, stop) = (w.clone(), stop.clone());
    tokio::spawn(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let mut saves = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let tmp = format!("tmp{}", saves % 4);
                write_new(&w.fs, dir, &tmp, b"saved").await;
                rename(&w.fs, dir, &tmp, dir, "target.txt").await;
                saves += 1;
            }
            saves
        }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_marking_diff_raced_by_saves_through_a_temporary_never_duplicates_or_deletes_the_file() {
    const WINDOWS: usize = 20;
    const MAX_ROUNDS: usize = 2000;
    let owned = saved_directory();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let w = Arc::new(Box::pin(working(&files)).await);
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saver = save_through_temporaries(&w, dir, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS {
            let from = State::deserialize(w.repository.clone(), w.current.revision())
                .await
                .expect("the revision loads");
            let (changes, raced) = w.raced_diff(&from, FilesystemDiffIntent::MarkDirty).await;
            rounds += 1;
            if !raced {
                continue;
            }
            windows += 1;
            if changes.iter().any(|change| {
                change.path().as_str() == "dir/target.txt" && change.action == FileAction::Delete
            }) {
                failures.push((rounds, "reported deleted"));
            }
            if matches!(
                staged_state::dirty_actions(&w.repository, &from)
                    .await
                    .get("dir/target.txt"),
                Some((Action::Delete, _))
            ) {
                failures.push((rounds, "marked deleted"));
            }
            if children_named(&w.repository, &from, "dir", "target.txt").await != 1 {
                failures.push((rounds, "duplicated"));
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    saver.await.expect("the saver");
    assert!(
        windows >= WINDOWS,
        "a save landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(failures.is_empty(), "{failures:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_diffs_raced_by_saves_through_a_temporary_name_only_paths_the_mount_serves() {
    const WINDOWS: usize = 20;
    const MAX_ROUNDS: usize = 2000;
    let owned = saved_directory();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let w = Arc::new(Box::pin(working(&files)).await);
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    let served: std::collections::BTreeSet<String> = owned
        .iter()
        .map(|(path, _)| path.clone())
        .chain((0..4).map(|i| format!("dir/tmp{i}")))
        .collect();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saver = save_through_temporaries(&w, dir, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS {
            let (changes, raced) = w.raced_diff(&w.current, FilesystemDiffIntent::Report).await;
            rounds += 1;
            if !raced {
                continue;
            }
            windows += 1;
            let mut paths = std::collections::BTreeSet::new();
            for change in &changes {
                let path = change.path().to_string();
                if !served.contains(&path) {
                    failures.push((rounds, path.clone(), "never served"));
                }
                if path == "dir/target.txt" && change.action == FileAction::Delete {
                    failures.push((rounds, path.clone(), "reported deleted"));
                }
                if !paths.insert(path.clone()) {
                    failures.push((rounds, path, "reported twice"));
                }
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    saver.await.expect("the saver");
    assert!(
        windows >= WINDOWS,
        "a save landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(failures.is_empty(), "{failures:?}");
}

/// Saves each tracked file of `dir/` of `w` in turn through a temporary until `stop` is set, so
/// each save replaces a file deep in the directory's chain, which a listing under way reaches late.
fn save_over_older_files(
    w: &Arc<Working>,
    dir: u64,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<u64> {
    let (w, stop) = (w.clone(), stop.clone());
    tokio::spawn(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let mut saves = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let tmp = format!("tmp{}", saves % 4);
                write_new(&w.fs, dir, &tmp, b"saved").await;
                rename(&w.fs, dir, &tmp, dir, &format!("f{:04}.txt", saves % 400)).await;
                saves += 1;
            }
            saves
        }),
    )
}

/// A listing under way that a save passes by, replacing a file the listing has yet to reach, lacks
/// the file; the diff confirms by name that the file is held before it reports a delete. A window
/// is a diff that made that find: every name it finds is held, as saves only replace files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_diffs_raced_by_saves_over_older_files_never_report_a_held_file_deleted() {
    const WINDOWS: usize = 10;
    const MAX_ROUNDS: usize = 4000;
    let owned = saved_directory();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let w = Arc::new(Box::pin(working(&files)).await);
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saver = save_over_older_files(&w, dir, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS && failures.len() <= 4 {
            let (changes, stats) = w
                .counted_diff(&w.current, FilesystemDiffIntent::Report)
                .await;
            rounds += 1;
            if stats.finds_after_change.load(Ordering::Relaxed) == 0 {
                continue;
            }
            windows += 1;
            for change in &changes {
                if change.action == FileAction::Delete {
                    failures.push((rounds, change.path().to_string()));
                }
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    saver.await.expect("the saver");
    assert!(
        windows >= WINDOWS || !failures.is_empty(),
        "a find after a save landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(failures.is_empty(), "reported deleted: {failures:?}");
}

/// Creates and unlinks a temporary in `dir/` of `w` until `stop` is set, which moves the
/// directory's chain under a walk without touching a name the walk pairs.
fn churn_temporaries(
    w: &Arc<Working>,
    dir: u64,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<u64> {
    let (w, stop) = (w.clone(), stop.clone());
    tokio::spawn(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let mut churns = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let tmp = format!("tmp{}", churns % 4);
                write_new(&w.fs, dir, &tmp, b"tmp").await;
                w.fs.unlink(&req(), dir, tmp.as_bytes())
                    .await
                    .expect("unlink the temporary");
                churns += 1;
            }
            churns
        }),
    )
}

/// A tracked file a symlink replaced is deleted even where the directory's chain moved under the
/// walk and the diff confirms the name by a find: the find meets the symlink, which the listing
/// skipped, and takes it as nothing held. A window is a diff that made that find.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_diffs_raced_by_churn_report_a_file_a_symlink_replaced_deleted() {
    const WINDOWS: usize = 10;
    const MAX_ROUNDS: usize = 4000;
    let owned = saved_directory();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let w = Arc::new(Box::pin(working(&files)).await);
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    in_context(async {
        w.fs.unlink(&req(), dir, b"target.txt")
            .await
            .expect("unlink target.txt");
        w.fs.symlink(&req(), dir, b"target.txt", b"f0000.txt")
            .await
            .expect("symlink at target.txt");
    })
    .await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churner = churn_temporaries(&w, dir, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS && failures.len() <= 4 {
            let (changes, stats) = w
                .counted_diff(&w.current, FilesystemDiffIntent::Report)
                .await;
            rounds += 1;
            if stats.finds_after_change.load(Ordering::Relaxed) == 0 {
                continue;
            }
            windows += 1;
            if !changes.iter().any(|change| {
                change.path().as_str() == "dir/target.txt" && change.action == FileAction::Delete
            }) {
                failures.push(rounds);
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    churner.await.expect("the churner");
    assert!(
        windows >= WINDOWS || !failures.is_empty(),
        "a find after a churn landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(
        failures.is_empty(),
        "dir/target.txt not reported deleted in rounds {failures:?}"
    );
}

/// Stages the named `paths` of `w`, with `scan` asking for `--scan`.
async fn stage_paths(w: &Working, paths: &[PathBuf], scan: bool) {
    let token = RepositoryWriteToken::acquire(&w.path).await;
    let context = w.command_context(&w.fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::file::stage::stage(
                context,
                &token,
                LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
                StageOptions {
                    scan,
                    ..StageOptions::default()
                },
            ),
        )
        .await
        .expect("the stage succeeds");
}

/// What the staged State holds at each of `paths`: how many nodes of the name, and the staged
/// action of the first.
async fn staged_at(w: &Working, paths: &[&str]) -> Vec<(String, usize, &'static str)> {
    let staged = w.staged().await;
    let mut found = Vec::new();
    for at in paths {
        let parent = path(at).parent_path();
        let count = children_named(&w.repository, &staged, parent.as_str(), path(at).name()).await;
        let action = match staged.find_node_link(w.repository.clone(), at).await {
            Ok(link) if link.is_valid() => {
                let node = staged
                    .node(w.repository.clone(), link.node)
                    .await
                    .expect("a staged node");
                if node.is_staged_delete() {
                    "delete"
                } else if node.is_staged_add() {
                    "add"
                } else if node.is_staged_modify() {
                    "modify"
                } else {
                    "none"
                }
            }
            _ => "absent",
        };
        found.push((at.to_string(), count, action));
    }
    found
}

#[tokio::test]
async fn a_file_recreated_after_its_delete_was_staged_pairs_with_the_deleted_node() {
    let w = working(FIXTURE).await;
    w.fs.unlink(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");
    stage_paths(&w, &[w.path.join("b.txt")], false).await;
    write_new(&w.fs, ROOT_INO, "b.txt", b"BEE again").await;

    let reported = w.status().await;
    assert_eq!(
        reported.get("b.txt"),
        Some(&(LoreFileAction::Keep, true)),
        "{reported:?}"
    );
    let staged = w.staged().await;
    assert_eq!(children_named(&w.repository, &staged, "", "b.txt").await, 1);
}

/// A directory `from` holds staged for delete that the tree holds again has its delete taken back
/// by a marking diff.
#[tokio::test]
async fn a_marking_diff_takes_back_the_delete_staged_for_a_directory_the_tree_holds() {
    in_context(async {
        let w = working(FIXTURE).await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        let dir = from
            .find_node_link(w.repository.clone(), "dir")
            .await
            .expect("dir")
            .node;
        from.node_mark(
            &w.repository,
            dir,
            Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
        )
        .await
        .expect("a staged delete");

        w.diff(&from, FilesystemDiffIntent::MarkDirty).await;
        let held = from.node(w.repository.clone(), dir).await.expect("dir");
        assert!(
            !held.is_staged_delete(),
            "the tree holds the directory, so its staged delete is taken back, flags {:#x}",
            held.flags
        );
    })
    .await;
}

/// Where `from` holds a name both live and staged for delete, the tree's file of that name pairs
/// with the live node, whichever comes first in the chain.
#[tokio::test]
async fn a_name_from_holds_live_and_staged_for_delete_pairs_with_the_live_node() {
    in_context(async {
        let w = working(FIXTURE).await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        let deleted = from
            .node_add(
                w.repository.clone(),
                lore_revision::node::ROOT_NODE,
                Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string("b.txt"),
                    ..Default::default()
                },
                "b.txt",
            )
            .await
            .expect("a second b.txt");
        from.node_mark(
            &w.repository,
            deleted,
            Mark::staged_and_dirty(NodeFlags::StagedDelete, NodeFlags::DirtyDelete),
        )
        .await
        .expect("a staged delete");
        assert_eq!(children_named(&w.repository, &from, "", "b.txt").await, 2);

        assert_eq!(
            summary(&w.diff(&from, FilesystemDiffIntent::Report).await),
            vec![]
        );
    })
    .await;
}

#[tokio::test]
async fn stage_scan_of_the_root_stages_a_written_a_created_a_deleted_and_a_recreated_file() {
    let w = working(FIXTURE).await;
    w.fs.unlink(&req(), ROOT_INO, b"old.txt")
        .await
        .expect("unlink");
    stage_paths(&w, &[w.path.join("old.txt")], false).await;
    write_new(&w.fs, ROOT_INO, "old.txt", b"old again").await;
    write_add_and_delete(&w).await;

    stage_paths(&w, std::slice::from_ref(&w.path), true).await;
    assert_eq!(
        staged_at(&w, &["a.txt", "b.txt", "dir/c.txt", "new.txt", "old.txt"]).await,
        vec![
            ("a.txt".to_string(), 1, "modify"),
            ("b.txt".to_string(), 1, "delete"),
            ("dir/c.txt".to_string(), 1, "none"),
            ("new.txt".to_string(), 1, "add"),
            ("old.txt".to_string(), 1, "modify"),
        ]
    );
}

/// The fastest of five Report diffs of `w` against `from`, each reporting nothing.
async fn fastest_quiet_diff(w: &Working, from: &Arc<State>) -> std::time::Duration {
    let mut fastest = std::time::Duration::MAX;
    for _ in 0..5 {
        let started = std::time::Instant::now();
        let changes = w.diff(from, FilesystemDiffIntent::Report).await;
        fastest = fastest.min(started.elapsed());
        assert!(changes.is_empty(), "{:?}", summary(&changes));
    }
    fastest
}

#[tokio::test]
async fn a_diff_over_a_staged_deleted_directory_walks_none_of_its_children() {
    const FILES: usize = 2000;
    let owned: Vec<(String, Vec<u8>)> = (0..FILES)
        .map(|i| (format!("dir/f{i:05}.txt"), format!("file {i}").into_bytes()))
        .chain(std::iter::once(("keep.txt".to_string(), b"keep".to_vec())))
        .collect();
    let files: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let untouched = Box::pin(working(&files)).await;
    let w = Box::pin(working(&files)).await;
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    for i in 0..FILES {
        w.fs.unlink(&req(), dir, format!("f{i:05}.txt").as_bytes())
            .await
            .expect("unlink");
    }
    w.fs.rmdir(&req(), ROOT_INO, b"dir").await.expect("rmdir");
    // A scan stages the missing directory's deletion whole. Staged through its dirty children
    // instead, each child is staged deleted and the directory only located, as for named paths
    // under a missing ancestor.
    stage_paths(&w, &[w.path.join("dir")], true).await;
    let staged = w.staged().await;
    let link = staged
        .find_node_link(w.repository.clone(), "dir")
        .await
        .expect("dir");
    assert!(
        staged
            .node(w.repository.clone(), link.node)
            .await
            .expect("dir")
            .is_staged_delete(),
        "dir is staged deleted"
    );
    in_context(async {
        let clean = fastest_quiet_diff(&untouched, &untouched.current).await;
        let deleted = fastest_quiet_diff(&w, &staged).await;
        assert!(
            deleted <= clean * 4 + std::time::Duration::from_millis(1),
            "a diff over the staged-deleted directory took {deleted:?}, one over the untouched tree {clean:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_written_file_whose_host_copy_is_gone_is_not_reported_while_the_state_holds_it() {
    in_context(async {
        let w = working(FIXTURE).await;
        let a = lookup_ino(&w.fs, ROOT_INO, "a.txt").await.expect("a.txt");
        write_all(&w.fs, a, 0, 0, b"ALPHA").await;
        std::fs::remove_file(w.write_dir().join("a.txt")).expect("the host copy goes");

        assert_eq!(
            summary(&w.diff(&w.current, FilesystemDiffIntent::Report).await),
            vec![]
        );
    })
    .await;
}

/// A delete in a directory whose chain did not change since the walk listed it is reported
/// without a find by name: the listing was exact.
#[tokio::test]
async fn a_delete_in_a_directory_unchanged_since_its_listing_takes_no_find() {
    in_context(async {
        let w = working(FIXTURE).await;
        w.fs.unlink(&req(), ROOT_INO, b"b.txt")
            .await
            .expect("unlink");
        // Left on: tests count at once, and one turning counting off would cut another's short.
        step_counting::set_chain_step_counting(true);
        let before = step_counting::thread_chain_steps();

        let changes = w.diff(&w.current, FilesystemDiffIntent::Report).await;
        let steps = step_counting::thread_chain_steps() - before;
        assert_eq!(
            summary(&changes),
            vec![("b.txt".to_string(), FileAction::Delete, false)]
        );
        assert_eq!(steps, 0, "the delete was confirmed by a find");
    })
    .await;
}

/// A read-only mount keeps no host files, so a local-content record measures as absent.
#[tokio::test]
async fn a_read_only_mount_measures_a_local_content_record_as_absent() {
    in_context(async {
        let m = mounted().await;
        let local = Node {
            flags: NodeFlags::File.bits(),
            ext_flags: NodeExtFlags::LocalContent.bits(),
            size: 9,
            ..Default::default()
        };
        let measured = m
            .operation
            .measure(&path("README.md"), &local)
            .await
            .expect("measure");
        assert!(matches!(measured, FileInfo::NotExist), "got {measured:?}");
    })
    .await;
}

/// The pairs of names each a task of its own renames one over the other in `out/`, so a diff's
/// walk of `out/` meets a rename often enough.
const RENAME_PAIRS: usize = 4;

/// `out/`, created through `w`'s provider with 60 files and `a0` to `a3`.
async fn created_directory(w: &Working) -> u64 {
    let out = mkdir(&w.fs, ROOT_INO, "out").await;
    for i in 0..60 {
        write_new(&w.fs, out, &format!("o{i:04}.o"), b"object").await;
    }
    for pair in 0..RENAME_PAIRS {
        write_new(&w.fs, out, &format!("a{pair}"), b"a").await;
    }
    out
}

/// Renames the older `out/a<pair>` over a newer `out/b<pair>` of `w` again and again until `stop`
/// is set: each round unlinks `b<pair>`, creates it, renames `a<pair>` over it and creates
/// `a<pair>`.
fn rename_older_over_newer(
    w: &Arc<Working>,
    out: u64,
    pair: usize,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<u64> {
    let (w, stop) = (w.clone(), stop.clone());
    let (a, b) = (format!("a{pair}"), format!("b{pair}"));
    tokio::spawn(
        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
            let mut rounds = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if rounds > 0 {
                    w.fs.unlink(&req(), out, b.as_bytes())
                        .await
                        .expect("unlink b");
                }
                write_new(&w.fs, out, &b, b"b").await;
                rename(&w.fs, out, &a, out, &b).await;
                write_new(&w.fs, out, &a, b"a").await;
                rounds += 1;
                tokio::task::yield_now().await;
            }
            rounds
        }),
    )
}

/// One renamer per pair of names in `out/`.
fn rename_every_pair(
    w: &Arc<Working>,
    out: u64,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> Vec<tokio::task::JoinHandle<u64>> {
    (0..RENAME_PAIRS)
        .map(|pair| rename_older_over_newer(w, out, pair, stop))
        .collect()
}

/// Every path `changes` reports more than once.
fn reported_twice(changes: &[NodeChange]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    changes
        .iter()
        .map(|change| change.path().to_string())
        .filter(|path| !seen.insert(path.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_marking_diff_of_a_new_directory_raced_by_renames_over_newer_files_leaves_no_duplicate() {
    const WINDOWS: usize = 50;
    const MAX_ROUNDS: usize = 8000;
    let w = Arc::new(working(FIXTURE).await);
    let out = created_directory(&w).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let renamers = rename_every_pair(&w, out, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS && failures.len() <= 4 {
            let from = State::deserialize(w.repository.clone(), w.current.revision())
                .await
                .expect("the revision loads");
            let (changes, raced) = w.raced_diff(&from, FilesystemDiffIntent::MarkDirty).await;
            rounds += 1;
            if !raced {
                continue;
            }
            windows += 1;
            for pair in 0..RENAME_PAIRS {
                for name in [format!("a{pair}"), format!("b{pair}")] {
                    if children_named(&w.repository, &from, "out", &name).await > 1 {
                        failures.push((rounds, name, reported_twice(&changes)));
                    }
                }
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for renamer in renamers {
        renamer.await.expect("a renamer");
    }
    assert!(
        windows >= WINDOWS || !failures.is_empty(),
        "a rename landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(failures.is_empty(), "{failures:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_report_diff_of_a_new_directory_raced_by_renames_over_newer_files_reports_each_path_once()
{
    const WINDOWS: usize = 50;
    const MAX_ROUNDS: usize = 8000;
    let w = Arc::new(working(FIXTURE).await);
    let out = created_directory(&w).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let renamers = rename_every_pair(&w, out, &stop);
    let mut failures = Vec::new();
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < WINDOWS && rounds < MAX_ROUNDS && failures.len() <= 4 {
            let (changes, raced) = w.raced_diff(&w.current, FilesystemDiffIntent::Report).await;
            rounds += 1;
            if !raced {
                continue;
            }
            windows += 1;
            let twice = reported_twice(&changes);
            if !twice.is_empty() {
                failures.push((rounds, twice));
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for renamer in renamers {
        renamer.await.expect("a renamer");
    }
    assert!(
        windows >= WINDOWS || !failures.is_empty(),
        "a rename landed under the walk in only {windows} of {rounds} rounds"
    );
    eprintln!("{windows} windows in {rounds} rounds");
    assert!(failures.is_empty(), "{failures:?}");
}

/// The bound leaves room for the load of the tests running beside this one, which has slowed a
/// diff under saves to 16 times its quiet time, and is still crossed by a diff that redoes work
/// for each file at every save.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_diff_of_a_new_directory_under_saves_stays_near_its_quiet_time() {
    const MAX_ROUNDS: usize = 200;
    let w = Arc::new(working(FIXTURE).await);
    let out = mkdir(&w.fs, ROOT_INO, "out").await;
    for i in 0..400 {
        write_new(&w.fs, out, &format!("o{i:04}.o"), b"object").await;
    }
    write_new(&w.fs, out, "target.txt", b"target").await;
    let mut quiet = std::time::Duration::MAX;
    in_context(async {
        for _ in 0..3 {
            let begun = std::time::Instant::now();
            w.diff(&w.current, FilesystemDiffIntent::Report).await;
            quiet = quiet.min(begun.elapsed());
        }
    })
    .await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saver = save_through_temporaries(&w, out, &stop);
    let mut churned = std::time::Duration::ZERO;
    let (mut windows, mut rounds) = (0, 0);
    in_context(async {
        while windows < 3 && rounds < MAX_ROUNDS {
            let begun = std::time::Instant::now();
            let (_, raced) = w.raced_diff(&w.current, FilesystemDiffIntent::Report).await;
            let took = begun.elapsed();
            rounds += 1;
            if raced {
                windows += 1;
                churned = churned.max(took);
            }
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    saver.await.expect("the saver");
    assert!(
        windows > 0,
        "a save landed under the walk in none of {rounds} rounds"
    );
    eprintln!(
        "{windows} windows in {rounds} rounds; slowest under saves {churned:?}, quiet {quiet:?}"
    );
    assert!(
        churned <= quiet * 20 + std::time::Duration::from_millis(500),
        "a Report diff under saves took {churned:?}, one without {quiet:?}"
    );
}

#[tokio::test]
async fn a_diff_rooted_at_another_spelling_of_a_directory_reports_and_marks_its_local_files() {
    in_context(async {
        let w = working(FIXTURE).await;
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        let c = lookup_ino(&w.fs, dir, "c.txt").await.expect("c.txt");
        write_all(&w.fs, c, 0, 0, b"SEA!").await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");

        assert_eq!(
            summary(
                &w.diff_at(&from, "DIR", FilesystemDiffIntent::MarkDirty)
                    .await
            ),
            vec![("dir/c.txt".to_string(), FileAction::Keep, true)]
        );
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from).await,
            BTreeMap::from([(
                "dir/c.txt".to_string(),
                (Action::Modify, NodeExtFlags::NoFlags)
            )])
        );
    })
    .await;
}

/// A diff rooted at another spelling of a directory the tree no longer holds spells what it
/// reports as `from` spells the directory.
#[tokio::test]
async fn a_diff_rooted_at_another_spelling_of_a_removed_directory_spells_it_as_from_does() {
    in_context(async {
        let w = working(FIXTURE).await;
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        w.fs.unlink(&req(), dir, b"c.txt").await.expect("unlink");
        w.fs.rmdir(&req(), ROOT_INO, b"dir").await.expect("rmdir");

        let changes = w
            .diff_at(&w.current, "DIR", FilesystemDiffIntent::Report)
            .await;
        assert!(!changes.is_empty(), "the removal is reported");
        assert!(
            changes
                .iter()
                .all(|change| change.path().as_str().starts_with("dir")),
            "{:?}",
            summary(&changes)
        );
    })
    .await;
}

/// A diff rooted at a directory `from` holds and the tree no longer does reports the directory
/// deleted, then what it held, as a walk over a working tree reports a missing path. A resolve
/// that takes a directory in place of a file removes the file, restores the directory from
/// the change at its root, and stages the directory over the file from it.
#[tokio::test]
async fn a_diff_rooted_at_a_removed_directory_reports_the_directory_deleted_with_its_files() {
    in_context(async {
        let w = working(FIXTURE).await;
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        w.fs.unlink(&req(), dir, b"c.txt").await.expect("unlink");
        w.fs.rmdir(&req(), ROOT_INO, b"dir").await.expect("rmdir");

        assert_eq!(
            summary(
                &w.diff_at(&w.current, "dir", FilesystemDiffIntent::Report)
                    .await
            ),
            vec![
                ("dir".to_string(), FileAction::Delete, false),
                ("dir/c.txt".to_string(), FileAction::Delete, false),
            ]
        );
    })
    .await;
}

/// [`a_diff_rooted_at_a_removed_directory_reports_the_directory_deleted_with_its_files`] marking:
/// the directory is marked deleted, as a marking walk over a working tree marks a missing path.
#[tokio::test]
async fn a_marking_diff_rooted_at_a_removed_directory_marks_the_directory_deleted() {
    in_context(async {
        let w = working(FIXTURE).await;
        let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
        w.fs.unlink(&req(), dir, b"c.txt").await.expect("unlink");
        w.fs.rmdir(&req(), ROOT_INO, b"dir").await.expect("rmdir");
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");

        let changes = w
            .diff_at(&from, "dir", FilesystemDiffIntent::MarkDirty)
            .await;
        assert_eq!(
            summary(&changes)
                .into_iter()
                .map(|(path, action, _)| (path, action))
                .collect::<Vec<_>>(),
            vec![
                ("dir".to_string(), FileAction::Delete),
                ("dir/c.txt".to_string(), FileAction::Delete),
            ]
        );
        assert_eq!(
            staged_state::dirty_actions(&w.repository, &from)
                .await
                .get("dir"),
            Some(&(Action::Delete, NodeExtFlags::NoFlags))
        );
    })
    .await;
}

/// A diff rooted at a removed directory lore's tree holds as an add never committed, which an
/// ignore rule now covers, reports nothing: the rules answer for a directory the tree does not
/// track, as the walk answers for one it lists. Without the rule the removal is reported.
#[tokio::test]
async fn a_diff_rooted_at_a_removed_uncommitted_directory_the_ignore_rules_cover_reports_nothing() {
    in_context(async {
        let w = working(FIXTURE).await;
        let out = mkdir(&w.fs, ROOT_INO, "out").await;
        write_new(&w.fs, out, "x.txt", b"x").await;
        let from = State::deserialize(w.repository.clone(), w.current.revision())
            .await
            .expect("the revision loads");
        w.diff(&from, FilesystemDiffIntent::MarkDirty).await;
        w.fs.unlink(&req(), out, b"x.txt").await.expect("unlink");
        w.fs.rmdir(&req(), ROOT_INO, b"out").await.expect("rmdir");
        let mut filter = lore_revision::filter::Filter::default();
        filter
            .ignore
            .add_exclusion("out")
            .expect("ignore exclusion");
        let ignoring = Arc::new(RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(w.immutable.clone(), w.mutable.clone())
                .with_path(&w.path)
                .with_id(w.repository.id)
                .with_instance_id(w.repository.instance_id)
                .with_filter(Arc::new(filter)),
        ));
        let mapped = |repository: &Arc<RepositoryContext>| {
            let (repository, from) = (repository.clone(), from.clone());
            async move {
                let node = from
                    .find_node_link(repository.clone(), "out")
                    .await
                    .expect("the add's node")
                    .node;
                NodeMapping {
                    repository,
                    state: from,
                    path: path("out"),
                    node,
                }
            }
        };

        let (reported, _) = w
            .diff_from(
                mapped(&w.repository).await,
                "out",
                FilesystemDiffIntent::Report,
            )
            .await;
        assert_eq!(
            summary(&reported)
                .into_iter()
                .map(|(path, action, _)| (path, action))
                .collect::<Vec<_>>(),
            vec![
                ("out".to_string(), FileAction::Delete),
                ("out/x.txt".to_string(), FileAction::Delete),
            ]
        );
        let (ignored, _) = w
            .diff_from(mapped(&ignoring).await, "out", FilesystemDiffIntent::Report)
            .await;
        assert_eq!(summary(&ignored), Vec::new());
    })
    .await;
}

#[tokio::test]
async fn file_info_of_a_local_file_below_another_spelling_of_its_directory_reads_the_host_file() {
    let w = working(FIXTURE).await;
    let dir = lookup_ino(&w.fs, ROOT_INO, "dir").await.expect("dir");
    let c = lookup_ino(&w.fs, dir, "c.txt").await.expect("c.txt");
    write_all(&w.fs, c, 0, 0, b"SEA!").await;

    let operation = FuseOperation::new(w.fs.clone());
    assert!(matches!(
        operation.file_info(&path("DIR/c.txt")).await.expect("info"),
        FileInfo::File { size: 4, .. }
    ));
}
