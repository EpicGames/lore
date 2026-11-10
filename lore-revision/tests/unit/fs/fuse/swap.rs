// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `swap`: record, apply in place, republish, invalidate. Every notification goes to a
//! recording sink.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_fuse::reply::Errno;
use lore_revision::change::NodeChange;
use lore_revision::filter::FilterMode;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::attr::RecordedTimes;
use lore_revision::fs::fuse::deletions::DELETION_LIST;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::fragment_cache::FragmentCacheStrategy;
use lore_revision::fs::fuse::fragment_cache::LeafPolicy;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::operation::FuseOperation;
use lore_revision::fs::fuse::swap::NotificationSink;
use lore_revision::fs::fuse::swap::swap;
use lore_revision::fs::hold::SwapHook;
use lore_revision::fs::hold::SwapPhase;
use lore_revision::lore::RepositoryId;
use lore_revision::node::NodeFileMode;
use lore_revision::node::NodeFlags;
use lore_revision::repository::RepositoryContext;
use lore_revision::state;
use lore_revision::state::Content;
use lore_revision::state::Ext;
use lore_revision::state::Mark;
use lore_revision::state::Rewrite;
use lore_revision::state::State;
use lore_revision::util::path::RelativePath;
use lore_storage::FlushOptions;
use lore_storage::ImmutableStore;
use lore_storage::KeyType;
use lore_storage::KeyValueStream;
use lore_storage::MutableStore;
use lore_storage::StoreError;
use lore_storage::StoreGetData;
use lore_storage::StoreMatchResult;
use lore_storage::StoreObliterateStats;
use lore_storage::hash::hash_string;
use parking_lot::Mutex;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_fixture::RepositoryContextCreationArgsExt as _;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::fuse_test_util::req;

const BASE_TIME: u64 = 1_700_000_000_000;
const BLKSIZE: u32 = 1 << 17;

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: BLKSIZE,
        base_time: BASE_TIME,
    }
}

fn path(value: &str) -> RelativePath {
    RelativePath::new_from_initial_path(value).expect("relative path")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Inode(u64),
    Entry(u64, Vec<u8>),
}

#[derive(Default)]
struct RecordingSink {
    calls: Mutex<Vec<Call>>,
}

impl RecordingSink {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }

    fn has(&self, call: &Call) -> bool {
        self.calls.lock().contains(call)
    }
}

impl NotificationSink for RecordingSink {
    fn inval_inode(&self, ino: u64) {
        self.calls.lock().push(Call::Inode(ino));
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        self.calls.lock().push(Call::Entry(parent, name.to_vec()));
    }
}

/// A repository whose working tree stays alive across commits, with the provider mounted on
/// its first commit.
struct Swapping {
    instance: fuse_fixture::TestRepository,
    first: Arc<State>,
    fs: Arc<FuseFilesystem>,
    operation: InstanceOperationImpl,
}

impl Swapping {
    fn repository(&self) -> Arc<RepositoryContext> {
        self.instance.repository.clone()
    }

    /// Writes (`Some`) or deletes (`None`) each path, then commits the whole tree.
    async fn commit(&self, edits: &[(&str, Option<&[u8]>)]) -> Arc<State> {
        for (name, contents) in edits {
            let full = self.instance.path.join(name);
            match contents {
                Some(contents) => {
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent).expect("parent directory");
                    }
                    fuse_fixture::test_file_write(&full, contents);
                }
                // A fixture edit, outside any repository write.
                #[allow(clippy::disallowed_methods)]
                None => std::fs::remove_file(&full).expect("remove fixture file"),
            }
        }
        fuse_fixture::test_commit_tree(&self.instance, "edit").await
    }

    async fn diff(&self, from: &Arc<State>, to: &Arc<State>) -> Arc<Vec<NodeChange>> {
        Arc::new(
            state::diff_collect(
                self.repository(),
                from.clone(),
                self.repository(),
                to.clone(),
                None,
                FilterMode::View,
            )
            .await
            .expect("diff"),
        )
    }

    async fn swap_to(&self, target: &Arc<State>, sink: Arc<dyn NotificationSink>) {
        swap(
            &self.fs,
            &self.operation,
            self.repository(),
            target.clone(),
            sink,
        )
        .await
        .expect("swap");
    }

    async fn lookup(&self, parent: u64, name: &str) -> Result<u64, Errno> {
        let served = self.fs.served();
        self.fs
            .lookup_at(&served, parent, name.as_bytes())
            .await
            .and_then(|entry| match entry.ino {
                // A cached negative entry.
                0 => Err(Errno::ENOENT),
                ino => Ok(ino),
            })
    }

    /// The repository under a view of `lines`.
    fn viewed(&self, lines: &[&str]) -> Arc<RepositoryContext> {
        let repository = self.repository();
        Arc::new(
            fuse_fixture::test_view_context(
                &self.instance,
                repository.immutable_store(),
                repository
                    .try_mutable_store_arc()
                    .expect("fixture holds a write token"),
                lines,
            )
            .without_command_locks()
            .with_write_token(self.instance.write_token.share()),
        )
    }

    async fn getattr(&self, ino: u64) -> lore_fuse::abi::Attr {
        self.fs.getattr(&req(), ino).await.expect("getattr").attr
    }

    async fn stored_time(&self, name: &str) -> u64 {
        state::file_modified_time(self.repository(), &path(name)).await
    }

    /// The time a swap of this read-only mount started now would record
    /// ([`state::time_before_next_write`] in the dot directory).
    async fn time_before_next_write(&self) -> u64 {
        let repository = self.repository();
        state::time_before_next_write(repository.dot_dir_path().ok()).await
    }
}

async fn swapping(files: &[(&str, &[u8])], work: impl AsyncFnOnce(Swapping)) {
    let (immutable_store, mutable_store, execution) =
        fuse_fixture::test_store_create().await.expect("test store");
    LORE_CONTEXT
        .scope(execution, async move {
            let instance = fuse_fixture::test_repository_create(
                immutable_store,
                mutable_store,
                RepositoryId::from(uuid::Uuid::now_v7()),
            )
            .await;
            for (name, contents) in files {
                let full = instance.path.join(name);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).expect("parent directory");
                }
                fuse_fixture::test_file_write(&full, contents);
            }
            let first = fuse_fixture::test_commit_tree(&instance, "first").await;
            let fs =
                FuseFilesystem::new(instance.repository.clone(), first.clone(), attr_context())
                    .await
                    .expect("provider construction");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            work(Swapping {
                instance,
                first,
                fs,
                operation,
            })
            .await;
        })
        .await;
}

#[tokio::test]
async fn swap_records_modified_time_for_each_added_or_modified_file() {
    swapping(
        &[("a.txt", b"one"), ("keep.txt", b"keep")],
        async |s: Swapping| {
            let second = s
                .commit(&[("a.txt", Some(b"two")), ("new.txt", Some(b"new"))])
                .await;
            // Read after this commit, which can record the time the first one left out for a
            // file written in its clock tick, so that only the swap can change it.
            let keep_before = s.stored_time("keep.txt").await;
            let before = s.time_before_next_write().await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let after = s.time_before_next_write().await;

            s.operation
                .take_modified_times()
                .store(s.repository())
                .await;
            for name in ["a.txt", "new.txt"] {
                let recorded = s.stored_time(name).await;
                assert!(
                    (before..=after).contains(&recorded),
                    "{name}: {recorded} not in {before}..={after}"
                );
            }
            assert_eq!(s.stored_time("keep.txt").await, keep_before);
        },
    )
    .await;
}

#[tokio::test]
async fn swap_republishes_the_state_at_the_target_revision() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let large = vec![7u8; 3 * BLKSIZE as usize];
        let second = s.commit(&[("large.bin", Some(&large))]).await;
        let old = s.fs.served();

        s.swap_to(&second, Arc::new(RecordingSink::default())).await;

        let published = s.fs.served();
        assert!(
            Arc::ptr_eq(&published.state, &old.state),
            "the State changes in place"
        );
        assert_eq!(published.revision(), second.revision());
        assert!(old.superseded());
        assert!(!published.superseded());

        let tree = second.tree(s.repository()).await.expect("tree");
        let statfs = s.fs.statfs(&req()).expect("statfs");
        assert_eq!(statfs.blocks, tree.size.div_ceil(u64::from(BLKSIZE)));
        assert!(statfs.blocks >= 3, "the published tree's size is reported");

        assert!(s.lookup(ROOT_INO, "large.bin").await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn swap_invalidates_inode_of_changed_file_and_entries_for_every_recorded_spelling() {
    swapping(
        &[("a.txt", b"one"), ("b.txt", b"bee")],
        async |s: Swapping| {
            let ino = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
            // Another spelling answers absent, which the kernel caches as a negative entry.
            assert_eq!(s.lookup(ROOT_INO, "A.TXT").await, Err(Errno::ENOENT));
            let untouched = s.lookup(ROOT_INO, "b.txt").await.expect("lookup");
            let second = s.commit(&[("a.txt", Some(b"two"))]).await;

            let sink = Arc::new(RecordingSink::default());
            s.swap_to(&second, sink.clone()).await;

            assert!(sink.has(&Call::Inode(ino)), "{:?}", sink.calls());
            assert!(sink.has(&Call::Entry(ROOT_INO, b"a.txt".to_vec())));
            assert!(sink.has(&Call::Entry(ROOT_INO, b"A.TXT".to_vec())));
            assert!(!sink.has(&Call::Inode(untouched)));
            assert!(!sink.has(&Call::Entry(ROOT_INO, b"b.txt".to_vec())));

            let fresh = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
            assert_eq!(fresh, ino, "the changed file keeps its inode");
            assert_eq!(s.getattr(fresh).await.size, 3);
            assert_eq!(
                s.lookup(ROOT_INO, "b.txt").await.expect("lookup"),
                untouched
            );
        },
    )
    .await;
}

#[tokio::test]
async fn swap_marks_removed_file_orphan_with_nlink_zero() {
    swapping(
        &[("gone.txt", b"bye"), ("stay.txt", b"hi")],
        async |s: Swapping| {
            let ino = s.lookup(ROOT_INO, "gone.txt").await.expect("lookup");
            assert_eq!(s.getattr(ino).await.nlink, 1);
            let second = s.commit(&[("gone.txt", None)]).await;

            let sink = Arc::new(RecordingSink::default());
            s.swap_to(&second, sink.clone()).await;

            let attr = s.getattr(ino).await;
            assert_eq!(attr.nlink, 0, "an orphan reports no links");
            assert_eq!(attr.size, 3, "an orphan still answers for its content");
            assert!(sink.has(&Call::Entry(ROOT_INO, b"gone.txt".to_vec())));
            assert_eq!(s.lookup(ROOT_INO, "gone.txt").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

/// A sync removing a directory the kernel holds orphans its inode: the State no longer holds the
/// directory, so the plan does not keep it as it keeps one holding local content.
#[tokio::test]
async fn swap_orphans_the_inode_of_a_removed_directory() {
    swapping(
        &[("gone/a.txt", b"a"), ("stay.txt", b"hi")],
        async |s: Swapping| {
            let gone = s.lookup(ROOT_INO, "gone").await.expect("lookup");
            s.lookup(gone, "a.txt").await.expect("lookup");
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_dir_all(s.instance.path.join("gone")).expect("remove gone/");
            let second = fuse_fixture::test_commit_tree(&s.instance, "drop gone").await;

            s.swap_to(&second, Arc::new(RecordingSink::default())).await;

            assert!(
                s.fs.inodes().get(gone).is_some_and(|entry| entry.orphaned),
                "the removed directory's inode is orphaned"
            );
            assert_eq!(s.lookup(ROOT_INO, "gone").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

#[tokio::test]
async fn noop_sync_sends_no_notifications() {
    swapping(
        &[("a.txt", b"one"), ("dir/b.txt", b"bee")],
        async |s: Swapping| {
            let dir = s.lookup(ROOT_INO, "dir").await.expect("lookup");
            s.lookup(dir, "b.txt").await.expect("lookup");
            let _ = s.lookup(ROOT_INO, "missing").await;

            let sink = Arc::new(RecordingSink::default());
            s.swap_to(&s.first.clone(), sink.clone()).await;

            assert_eq!(sink.calls(), Vec::new());
            assert_eq!(s.fs.served().revision(), s.first.revision());
            assert!(s.lookup(dir, "b.txt").await.is_ok());
        },
    )
    .await;
}

#[tokio::test]
async fn swap_with_stale_current_recomputes_diff_from_published() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let ino = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
        let second = s.commit(&[("a.txt", Some(b"two"))]).await;
        let third = s.commit(&[("c.txt", Some(b"sea"))]).await;

        // `current` names a revision the mount never published, and the changes are empty.
        let sink = Arc::new(RecordingSink::default());
        swap(
            &s.fs,
            &s.operation,
            s.repository(),
            third.clone(),
            sink.clone(),
        )
        .await
        .expect("swap");
        assert!(sink.has(&Call::Inode(ino)), "{:?}", sink.calls());
        assert_eq!(s.fs.served().revision(), third.revision());

        // No `current` at all, as a clone passes.
        let ino = s.lookup(ROOT_INO, "c.txt").await.expect("lookup");
        let sink = Arc::new(RecordingSink::default());
        swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            sink.clone(),
        )
        .await
        .expect("swap");
        assert!(sink.has(&Call::Entry(ROOT_INO, b"c.txt".to_vec())));
        let _ = ino;
        assert_eq!(s.lookup(ROOT_INO, "c.txt").await, Err(Errno::ENOENT));
    })
    .await;
}

/// Checks from another thread that the target revision is already published and that the
/// inode, spelling and served-state locks the notification concerns are free.
struct OrderingSink {
    fs: Arc<FuseFilesystem>,
    expected_revision: lore_base::types::Hash,
    calls: Mutex<Vec<Call>>,
    violations: Mutex<Vec<String>>,
}

impl OrderingSink {
    fn check(&self, what: String, ino: u64, spelling: Option<(u64, u64)>) {
        let fs = self.fs.clone();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let revision = fs.served().revision();
            drop(fs.inodes().get_mut(ino));
            if let Some((parent, name_hash)) = spelling {
                drop(fs.spellings().take(parent, name_hash));
            }
            let _ = done.send(revision);
        });
        match finished.recv_timeout(Duration::from_secs(5)) {
            Ok(revision) if revision == self.expected_revision => {}
            Ok(revision) => self
                .violations
                .lock()
                .push(format!("{what}: sent at revision {revision}")),
            Err(_) => self
                .violations
                .lock()
                .push(format!("{what}: a table lock was held while sending")),
        }
    }
}

impl NotificationSink for OrderingSink {
    fn inval_inode(&self, ino: u64) {
        self.calls.lock().push(Call::Inode(ino));
        self.check(format!("inval_inode({ino})"), ino, None);
    }

    fn inval_entry(&self, parent: u64, name: &[u8]) {
        self.calls.lock().push(Call::Entry(parent, name.to_vec()));
        let name_hash = hash_string(std::str::from_utf8(name).expect("utf-8 name"));
        self.check(
            format!("inval_entry({parent})"),
            parent,
            Some((parent, name_hash)),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_are_sent_after_publish_with_no_table_lock_held() {
    swapping(
        &[
            ("a.txt", b"one"),
            ("gone.txt", b"bye"),
            ("dir/b.txt", b"bee"),
        ],
        async |s: Swapping| {
            let a = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
            s.lookup(ROOT_INO, "gone.txt").await.expect("lookup");
            let dir = s.lookup(ROOT_INO, "dir").await.expect("lookup");
            s.lookup(dir, "b.txt").await.expect("lookup");
            let _ = s.lookup(dir, "later.txt").await;
            let second = s
                .commit(&[
                    ("a.txt", Some(b"two")),
                    ("gone.txt", None),
                    ("dir/later.txt", Some(b"now")),
                ])
                .await;

            let sink = Arc::new(OrderingSink {
                fs: s.fs.clone(),
                expected_revision: second.revision(),
                calls: Mutex::new(Vec::new()),
                violations: Mutex::new(Vec::new()),
            });
            s.swap_to(&second, sink.clone()).await;

            let calls = sink.calls.lock().clone();
            assert!(calls.contains(&Call::Inode(a)), "{calls:?}");
            assert!(calls.contains(&Call::Entry(dir, b"later.txt".to_vec())));
            assert!(
                calls.contains(&Call::Inode(dir)),
                "the directory's entries changed"
            );
            assert_eq!(*sink.violations.lock(), Vec::<String>::new());
        },
    )
    .await;
}

#[tokio::test]
async fn swap_records_modified_time_for_directories_whose_entry_set_changed() {
    swapping(
        &[
            ("dir/a.txt", b"a"),
            ("other/c.txt", b"c"),
            ("other/d.txt", b"d"),
            ("same/x.txt", b"x"),
        ],
        async |s: Swapping| {
            let dir = s.lookup(ROOT_INO, "dir").await.expect("lookup");
            let other = s.lookup(ROOT_INO, "other").await.expect("lookup");
            let same = s.lookup(ROOT_INO, "same").await.expect("lookup");
            let same_mtime = s.getattr(same).await.mtime;
            let second = s
                .commit(&[
                    ("dir/new.txt", Some(b"n")),
                    ("other/c.txt", None),
                    ("same/x.txt", Some(b"changed")),
                ])
                .await;

            let same_before = s.stored_time("same").await;
            let before = s.time_before_next_write().await;
            let sink = Arc::new(RecordingSink::default());
            s.swap_to(&second, sink.clone()).await;
            let after = s.time_before_next_write().await;

            for (ino, name) in [(dir, "dir"), (other, "other")] {
                let seconds = s.getattr(ino).await.mtime;
                assert!(
                    (before / 1000..=after / 1000).contains(&seconds),
                    "{name}: inode mtime {seconds}"
                );
                assert!(sink.has(&Call::Inode(ino)), "{name}: {:?}", sink.calls());
            }
            assert_eq!(s.getattr(same).await.mtime, same_mtime);
            assert!(!sink.has(&Call::Inode(same)));

            s.operation
                .take_modified_times()
                .store(s.repository())
                .await;
            for name in ["dir", "other"] {
                let recorded = s.stored_time(name).await;
                assert!(
                    (before..=after).contains(&recorded),
                    "{name}: {recorded} not in {before}..={after}"
                );
            }
            assert_eq!(
                s.stored_time("same").await,
                same_before,
                "a directory whose entries did not change keeps its time"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn view_change_swap_invalidates_every_kernel_known_entry_and_directory() {
    swapping(
        &[("dir/a.txt", b"a"), ("excluded/x.txt", b"x")],
        async |s: Swapping| {
            let dir = s.lookup(ROOT_INO, "dir").await.expect("lookup");
            let a = s.lookup(dir, "a.txt").await.expect("lookup");
            let excluded = s.lookup(ROOT_INO, "excluded").await.expect("lookup");
            let x = s.lookup(excluded, "x.txt").await.expect("lookup");
            assert_eq!(s.lookup(ROOT_INO, "nope").await, Err(Errno::ENOENT));

            let repository = s.repository();
            let viewed = fuse_fixture::test_view_context(
                &s.instance,
                repository.immutable_store(),
                repository
                    .try_mutable_store_arc()
                    .expect("fixture holds a write token"),
                &["excluded/"],
            );
            let sink = Arc::new(RecordingSink::default());
            swap(&s.fs, &s.operation, viewed, s.first.clone(), sink.clone())
                .await
                .expect("swap");

            for ino in [ROOT_INO, dir, excluded] {
                assert!(sink.has(&Call::Inode(ino)), "{ino}: {:?}", sink.calls());
            }
            for (parent, name) in [
                (ROOT_INO, &b"dir"[..]),
                (ROOT_INO, b"excluded"),
                (ROOT_INO, b"nope"),
                (dir, b"a.txt"),
                (excluded, b"x.txt"),
            ] {
                assert!(
                    sink.has(&Call::Entry(parent, name.to_vec())),
                    "{}: {:?}",
                    String::from_utf8_lossy(name),
                    sink.calls()
                );
            }
            assert_eq!(
                s.getattr(x).await.nlink,
                0,
                "a file that left the view is orphaned"
            );
            assert_eq!(s.getattr(a).await.nlink, 1);
            assert_eq!(s.lookup(ROOT_INO, "excluded").await, Err(Errno::ENOENT));
            assert!(s.lookup(dir, "a.txt").await.is_ok());
        },
    )
    .await;
}

/// A view change leaves alone the inodes of a local file and a local symlink below a path the new
/// view excludes, as a sync leaves local content: neither is orphaned nor invalidated, while the
/// tracked file beside them is orphaned.
#[tokio::test]
async fn a_view_change_leaves_the_inodes_of_local_files_and_symlinks_alone() {
    swapping(
        &[("excluded/x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-view-local-");
            let write = external.path().join("write");
            std::fs::create_dir_all(write.join("excluded")).expect("write/excluded");
            fuse_fixture::test_file_write(&write.join("excluded/local.txt"), b"local");
            std::os::unix::fs::symlink("x.txt", write.join("excluded/link")).expect("symlink");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let lookup = async |parent: u64, name: &str| {
                fs.lookup_at(&fs.served(), parent, name.as_bytes())
                    .await
                    .expect("lookup")
                    .ino
            };
            let excluded = lookup(ROOT_INO, "excluded").await;
            let x = lookup(excluded, "x.txt").await;
            let local = lookup(excluded, "local.txt").await;
            let link = lookup(excluded, "link").await;
            let sink = Arc::new(RecordingSink::default());

            swap(
                &fs,
                &operation,
                s.viewed(&["excluded/"]),
                s.first.clone(),
                sink.clone(),
            )
            .await
            .expect("swap");

            assert!(
                fs.inodes().get(x).is_some_and(|entry| entry.orphaned),
                "the tracked file that left the view is orphaned"
            );
            for (name, ino) in [("local.txt", local), ("link", link)] {
                assert!(
                    fs.inodes().get(ino).is_some_and(|entry| !entry.orphaned),
                    "{name} keeps its inode"
                );
                assert!(!sink.has(&Call::Inode(ino)), "{name}: no invalidation");
            }
        },
    )
    .await;
}

/// Content spanning many fragments, distinct per `seed`.
fn fragmented(seed: u64) -> Vec<u8> {
    fuse_test_util::incompressible(1_500_000, seed)
}

async fn read_at(fs: &FuseFilesystem, ino: u64, offset: usize, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let Ok(read) = fs
        .read(&req(), ino, 0, offset as u64, &mut out, false)
        .await
    else {
        panic!("read of {ino} at {offset} failed");
    };
    out.truncate(read);
    out
}

#[tokio::test]
async fn swap_serves_new_content_through_the_same_inode_under_every_fragment_cache() {
    for strategy in [
        FragmentCacheStrategy::PerInode(LeafPolicy::Load),
        FragmentCacheStrategy::PerInode(LeafPolicy::Decoded(4)),
        FragmentCacheStrategy::PerInode(LeafPolicy::Stored(1)),
        FragmentCacheStrategy::Global {
            budget: 1 << 20,
            decoded: true,
        },
    ] {
        let old = fragmented(1);
        Box::pin(swapping(&[("big.bin", &old)], async |s: Swapping| {
            s.fs.fragment_cache().set_strategy(strategy);
            let ino = s.lookup(ROOT_INO, "big.bin").await.expect("lookup");
            for offset in [0, 200_000, 1_400_000] {
                assert!(read_at(&s.fs, ino, offset, 65_536).await == old[offset..offset + 65_536]);
            }

            let new = fragmented(2);
            let second = s.commit(&[("big.bin", Some(&new))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let fresh = s.lookup(ROOT_INO, "big.bin").await.expect("lookup");
            assert_eq!(fresh, ino, "{strategy:?}: new content, same inode");
            for offset in [0, 200_000, 1_400_000] {
                assert!(
                    read_at(&s.fs, ino, offset, 65_536).await == new[offset..offset + 65_536],
                    "{strategy:?}: the inode serves the new content"
                );
            }

            if matches!(strategy, FragmentCacheStrategy::PerInode(_)) {
                assert!(s.fs.fragment_cache().inodes_held() >= 1, "{strategy:?}");
                s.fs.forget(ino, 2);
                assert_eq!(
                    s.fs.fragment_cache().inodes_held(),
                    0,
                    "{strategy:?}: FORGET frees the inode's state"
                );
                assert_eq!(
                    s.fs.fragment_cache().held_bytes(),
                    0,
                    "{strategy:?}: FORGET releases the inode's bytes"
                );
            }
        }))
        .await;
    }
}

/// The file info `read_directory` lists for `name` under `directory`.
async fn listed_info(
    operation: &InstanceOperationImpl,
    directory: &str,
    name: &str,
) -> lore_revision::fs::filesystem_provider::FileInfo {
    let mut listing = operation
        .read_directory(&path(directory))
        .await
        .expect("listing");
    while let Some(entry) = listing.next().await {
        let entry = entry.expect("entry");
        if entry.name == name {
            return entry.info;
        }
    }
    panic!("{directory} lists no {name}");
}

#[tokio::test]
async fn file_info_and_read_directory_agree_for_a_file_changed_by_a_sync() {
    swapping(
        &[("dir/a.txt", b"one"), ("dir/keep.txt", b"keep")],
        async |s: Swapping| {
            let second = s.commit(&[("dir/a.txt", Some(b"two"))]).await;
            let before = s.time_before_next_write().await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let after = s.time_before_next_write().await;

            let changed = s
                .operation
                .file_info(&path("dir/a.txt"))
                .await
                .expect("info");
            assert!(
                (before..=after).contains(&changed.mtime()),
                "{changed:?} not in {before}..={after}"
            );
            assert_eq!(listed_info(&s.operation, "dir", "a.txt").await, changed);
            let kept = s
                .operation
                .file_info(&path("dir/keep.txt"))
                .await
                .expect("info");
            assert_eq!(kept.mtime(), BASE_TIME, "a path no swap changed");
            assert_eq!(listed_info(&s.operation, "dir", "keep.txt").await, kept);
        },
    )
    .await;
}

#[tokio::test]
async fn a_remounted_provider_reports_the_times_an_earlier_swap_recorded() {
    swapping(
        &[("dir/a.txt", b"one"), ("dir/keep.txt", b"keep")],
        async |s: Swapping| {
            let second = s.commit(&[("dir/a.txt", Some(b"two"))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let swapped = s
                .operation
                .file_info(&path("dir/a.txt"))
                .await
                .expect("info")
                .mtime();
            s.operation
                .take_modified_times()
                .store(s.repository())
                .await;

            let fs = FuseFilesystem::new(s.repository(), second.clone(), attr_context())
                .await
                .expect("remounted provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let changed = operation.file_info(&path("dir/a.txt")).await.expect("info");
            assert_eq!(changed.mtime(), swapped, "the time the swap recorded");
            assert_eq!(listed_info(&operation, "dir", "a.txt").await, changed);
            let dir = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("lookup dir")
                .ino;
            let file = fs
                .lookup_at(&fs.served(), dir, b"a.txt")
                .await
                .expect("lookup file");
            assert_eq!(file.attr.mtime, changed.mtime() / 1000);
            // The fixture's commits stored scan times for every file; only a swap's count.
            let kept = operation
                .file_info(&path("dir/keep.txt"))
                .await
                .expect("info");
            assert_eq!(kept.mtime(), BASE_TIME, "a path no swap changed");
            assert_eq!(listed_info(&operation, "dir", "keep.txt").await, kept);
        },
    )
    .await;
}

/// A swap that cannot store the times it records changes nothing: the store is correct
/// whenever a revision is served.
#[tokio::test]
async fn a_failed_time_store_leaves_the_served_state_unchanged() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let second = s.commit(&[("a.txt", Some(b"two"))]).await;
        let served = s.fs.served();
        let read_only = Arc::new(s.repository().without_command_locks());

        let swapped = swap(
            &s.fs,
            &s.operation,
            read_only,
            second,
            Arc::new(RecordingSink::default()),
        )
        .await;

        assert!(
            swapped.is_err(),
            "a context without a write token stores nothing"
        );
        assert!(!served.superseded(), "nothing was published");
        assert_eq!(s.fs.served().revision(), s.first.revision());
        assert!(s.fs.recorded_times().none());
        let info = s.operation.file_info(&path("a.txt")).await.expect("info");
        assert_eq!(info.mtime(), BASE_TIME);
    })
    .await;
}

/// The marker that opens the store to lookups is written only by a swap that changed a file or a
/// directory's entries.
#[tokio::test]
async fn only_a_swap_that_changes_something_writes_the_marker() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.swap_to(&s.first.clone(), Arc::new(RecordingSink::default()))
            .await;
        assert!(s.fs.recorded_times().none());
        assert!(RecordedTimes::load(&s.repository()).await.none());

        let second = s.commit(&[("a.txt", Some(b"two"))]).await;
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        assert!(!s.fs.recorded_times().none());
        assert!(!RecordedTimes::load(&s.repository()).await.none());
    })
    .await;
}

/// The first publish onto a mount of the empty revision, as clone performs it, diffs and records
/// nothing: every path keeps `base_time`, and only what the kernel holds of the empty root is
/// invalidated.
#[tokio::test]
async fn first_publish_onto_an_empty_mount_records_nothing_and_invalidates_the_root() {
    swapping(
        &[("a.txt", b"one"), ("dir/b.txt", b"two")],
        async |s: Swapping| {
            let empty = FuseFilesystem::new(s.repository(), State::new(), attr_context())
                .await
                .expect("provider construction");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(empty.clone())),
                true,
            );
            let served = empty.served();
            let missing = empty.lookup_at(&served, ROOT_INO, b"a.txt").await;
            assert!(
                missing.is_ok_and(|entry| entry.ino == 0),
                "a negative entry"
            );
            drop(served);
            let sink = Arc::new(RecordingSink::default());

            swap(
                &empty,
                &operation,
                s.repository(),
                s.first.clone(),
                sink.clone(),
            )
            .await
            .expect("swap");

            assert_eq!(empty.served().revision(), s.first.revision());
            assert!(empty.recorded_times().none(), "no time is recorded");
            assert!(RecordedTimes::load(&s.repository()).await.none());
            assert!(sink.has(&Call::Inode(ROOT_INO)), "{:?}", sink.calls());
            assert!(
                sink.has(&Call::Entry(ROOT_INO, b"a.txt".to_vec())),
                "{:?}",
                sink.calls()
            );
            for name in ["a.txt", "dir/b.txt"] {
                let info = operation.file_info(&path(name)).await.expect("info");
                assert_eq!(info.mtime(), BASE_TIME, "{name}");
            }
        },
    )
    .await;
}

/// The bytes `name` reads back through LOOKUP and READ below `parent`.
async fn served_bytes(s: &Swapping, parent: u64, name: &str) -> Vec<u8> {
    let ino = s.lookup(parent, name).await.expect("lookup");
    read_at(&s.fs, ino, 0, 1 << 16).await
}

/// An empty change list is not a promise that nothing changed: a reset sync passes one, and the
/// target is still what is served afterwards.
#[tokio::test]
async fn a_sync_with_an_empty_change_list_serves_the_target() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let second = s
            .commit(&[("a.txt", Some(b"two")), ("b.txt", Some(b"bee"))])
            .await;
        swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");
        assert_eq!(s.fs.served().revision(), second.revision());
        assert_eq!(served_bytes(&s, ROOT_INO, "a.txt").await, b"two");
        assert_eq!(served_bytes(&s, ROOT_INO, "b.txt").await, b"bee");
    })
    .await;
}

/// A sync applies what the target changes and leaves every local change in the served State:
/// a file added and a file edited under `write/`, and a deleted tracked file.
#[tokio::test]
async fn a_sync_keeps_the_local_changes_the_target_leaves() {
    swapping(
        &[
            ("a.txt", b"one"),
            ("edited.txt", b"tracked"),
            ("gone.txt", b"g"),
        ],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-local-");
            let write = external.path().join("write");
            std::fs::create_dir_all(&write).expect("write/");
            fuse_fixture::test_file_write(&write.join("local.txt"), b"local");
            fuse_fixture::test_file_write(&write.join("edited.txt"), b"edited here");
            fuse_fixture::test_file_write(&external.path().join("deleted"), b"gone.txt\n");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let second = s.commit(&[("a.txt", Some(b"two"))]).await;
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");

            let state = fs.state();
            let node = |name: &'static str| {
                let state = state.clone();
                let repository = s.repository();
                async move {
                    match state
                        .find_subnode(
                            repository.clone(),
                            lore_revision::node::ROOT_NODE,
                            hash_string(name),
                        )
                        .await
                    {
                        Ok(node) => Some(state.node(repository, node).await.expect("node")),
                        Err(error) if error.is_node_not_found() => None,
                        Err(error) => panic!("{name}: {error}"),
                    }
                }
            };
            let local = lore_revision::node::NodeExtFlags::LocalContent;
            assert!(
                node("local.txt")
                    .await
                    .is_some_and(|n| n.ext_flags().contains(local)),
                "the local file stays in the State"
            );
            assert!(
                node("edited.txt")
                    .await
                    .is_some_and(|n| n.ext_flags().contains(local)),
                "the local edit stays in the State"
            );
            assert!(node("gone.txt").await.is_none(), "the local deletion stays");
            let a = node("a.txt").await.expect("a.txt");
            assert_eq!(a.size, 3, "the target's change is applied");
        },
    )
    .await;
}

/// A READDIRPLUS stream pinned before a sync goes on from its pin, but no inode takes content
/// from that pin: a file the sync changed keeps what the sync gave it.
#[tokio::test]
async fn a_readdirplus_stream_pinned_before_a_sync_leaves_the_inodes_content_alone() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let ino = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
        let fh = s.fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
        // Room for `.` and `..` only, so the stream is pinned before it reaches a.txt.
        let mut buffer = vec![0u8; 96];
        let mut writer = DirentWriter::new(&mut buffer);
        s.fs.readdirplus(&req(), ROOT_INO, fh, 0, &mut writer)
            .await
            .expect("first page");

        let second = s
            .commit(&[("a.txt", Some(b"longer bytes")), ("b.txt", Some(b"bee"))])
            .await;
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        assert_eq!(s.getattr(ino).await.size, 12);

        let mut buffer = vec![0u8; 64 * 1024];
        let mut writer = DirentWriter::new(&mut buffer);
        s.fs.readdirplus(&req(), ROOT_INO, fh, 2, &mut writer)
            .await
            .expect("the rest of the stream");
        let used = writer.finish();
        s.fs.releasedir(ROOT_INO, fh);
        let names: Vec<&[u8]> = fuse_test_util::dirents(&buffer[..used], true)
            .map(|dirent| dirent.name)
            .collect();
        assert_eq!(
            names,
            vec![&b"a.txt"[..]],
            "the stream goes on from its pin, which lacks the sync's b.txt"
        );
        assert_eq!(
            s.getattr(ino).await.size,
            12,
            "the pinned listing's old content is not written back"
        );
    })
    .await;
}

/// A swap back to the served revision after a failed apply that turned a file into a directory
/// and a directory into a file turns each back: the re-placement meets a node of the other kind
/// at each path.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_back_turns_back_what_a_failed_apply_made_another_kind() {
    swapping(
        &[("f", b"file"), ("d/inner.txt", b"in"), ("keep.txt", b"k")],
        async |s: Swapping| {
            s.fs.set_swap_hook(failing_once(SwapPhase::Placed));
            // Fixture edits, outside any repository write.
            #[allow(clippy::disallowed_methods)]
            {
                std::fs::remove_file(s.instance.path.join("f")).expect("remove f");
                std::fs::remove_dir_all(s.instance.path.join("d")).expect("remove d");
            }
            let turned = s
                .commit(&[
                    ("f/new.txt", Some(b"now a directory")),
                    ("d", Some(b"now a file")),
                ])
                .await;
            let failed = swap(
                &s.fs,
                &s.operation,
                s.repository(),
                turned.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed.is_err(), "the hook fails the apply once it placed");
            let f = s.lookup(ROOT_INO, "f").await.expect("f");
            assert_eq!(served_bytes(&s, f, "new.txt").await, b"now a directory");
            let d = s.lookup(ROOT_INO, "d").await.expect("d");
            assert_eq!(read_at(&s.fs, d, 0, 64).await, b"now a file");

            swap(
                &s.fs,
                &s.operation,
                s.repository(),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("the swap back");

            let f = s.lookup(ROOT_INO, "f").await.expect("f");
            assert_eq!(read_at(&s.fs, f, 0, 64).await, b"file");
            let d = s.lookup(ROOT_INO, "d").await.expect("d");
            assert_eq!(served_bytes(&s, d, "inner.txt").await, b"in");
        },
    )
    .await;
}

/// A directory the target adds over one the State already holds, as a failed sync leaves it,
/// takes the target's children it lacks, though the change list names the directory alone.
#[tokio::test]
async fn a_sync_over_a_partly_applied_directory_fills_in_its_children() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let second = s
            .commit(&[("new/x.txt", Some(b"x")), ("new/deeper/y.txt", Some(b"y"))])
            .await;
        let state = s.fs.state();
        let attached = state
            .node_attach(
                s.repository(),
                lore_revision::node::ROOT_NODE,
                lore_revision::node::Node::default(),
                "new",
            )
            .await
            .expect("attach");
        assert!(matches!(
            attached,
            lore_revision::state::AttachOutcome::Attached(_)
        ));
        let only_the_directory: Vec<NodeChange> = s
            .diff(&s.first, &second)
            .await
            .iter()
            .filter(|change| change.path().as_str() == "new")
            .cloned()
            .collect();
        assert_eq!(only_the_directory.len(), 1);
        swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let new = s.lookup(ROOT_INO, "new").await.expect("new");
        assert_eq!(served_bytes(&s, new, "x.txt").await, b"x");
        let deeper = s.lookup(new, "deeper").await.expect("deeper");
        assert_eq!(served_bytes(&s, deeper, "y.txt").await, b"y");
    })
    .await;
}

/// A sync applies what changes outside the view too, and records its time, so a wider view
/// afterwards serves the revision rather than what the view hid at the sync.
#[tokio::test]
async fn a_sync_out_of_view_then_a_wider_view_serves_the_revision() {
    swapping(
        &[
            ("seen.txt", b"s"),
            ("hidden/a.txt", b"one"),
            ("hidden/b.txt", b"bee"),
        ],
        async |s: Swapping| {
            let repository = s.repository();
            let viewed = Arc::new(
                fuse_fixture::test_view_context(
                    &s.instance,
                    repository.immutable_store(),
                    repository
                        .try_mutable_store_arc()
                        .expect("fixture holds a write token"),
                    &["hidden/"],
                )
                .without_command_locks()
                .with_write_token(s.instance.write_token.share()),
            );
            let second = s
                .commit(&[("hidden/a.txt", Some(b"changed")), ("hidden/b.txt", None)])
                .await;
            let before = s.time_before_next_write().await;
            swap(
                &s.fs,
                &s.operation,
                viewed,
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap under the view");
            let after = s.time_before_next_write().await;
            assert_eq!(s.lookup(ROOT_INO, "hidden").await, Err(Errno::ENOENT));

            swap(
                &s.fs,
                &s.operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap to a wider view");
            let hidden = s.lookup(ROOT_INO, "hidden").await.expect("hidden");
            assert_eq!(served_bytes(&s, hidden, "a.txt").await, b"changed");
            assert_eq!(s.lookup(hidden, "b.txt").await, Err(Errno::ENOENT));
            let changed = s
                .operation
                .file_info(&path("hidden/a.txt"))
                .await
                .expect("info")
                .mtime();
            assert!(
                (before..=after).contains(&changed),
                "{changed} not in {before}..={after}: the time of the sync under the view"
            );
        },
    )
    .await;
}

/// A sync with no change list to plan from, moving a file out of the view, plans the move at its
/// source: the inode the kernel held there is orphaned and invalidated, and the State no longer
/// holds the source.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_out_of_the_view_orphans_the_inode_at_its_source() {
    swapping(
        &[
            ("shown/a.txt", b"moved bytes"),
            ("shown/keep.txt", b"k"),
            ("hidden/h.txt", b"h"),
        ],
        async |s: Swapping| {
            let viewed = s.viewed(&["hidden/"]);
            let fs = FuseFilesystem::new(viewed.clone(), s.first.clone(), attr_context())
                .await
                .expect("provider construction");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let shown = fs
                .lookup(&req(), ROOT_INO, b"shown")
                .await
                .expect("shown")
                .ino;
            let a = fs.lookup(&req(), shown, b"a.txt").await.expect("a.txt").ino;
            // The target, held in memory alone: the served revision with a.txt moved, its node
            // kept, as a staged move leaves it.
            let repository = s.repository();
            let second = State::deserialize(repository.clone(), s.first.revision())
                .await
                .expect("served revision");
            let moved = second
                .find_node_link(repository.clone(), "shown/a.txt")
                .await
                .expect("shown/a.txt")
                .node;
            let hidden = second
                .find_node_link(repository.clone(), "hidden")
                .await
                .expect("hidden")
                .node;
            second
                .node_reattach(repository.clone(), moved, hidden, "a.txt")
                .await
                .expect("move a.txt");
            second.set_revision(Hash::from([7u8; 32]));
            let unfiltered = state::diff_collect(
                s.repository(),
                s.first.clone(),
                s.repository(),
                second.clone(),
                None,
                FilterMode::empty(),
            )
            .await
            .expect("diff");
            assert!(
                unfiltered
                    .iter()
                    .any(|change| change.move_source().is_some()),
                "the diff holds the move"
            );

            let sink = Arc::new(RecordingSink::default());
            swap(&fs, &operation, viewed, second.clone(), sink.clone())
                .await
                .expect("swap");

            assert_eq!(
                fs.inodes().find_child(shown, hash_string("a.txt"), "a.txt"),
                None,
                "the source's inode is orphaned"
            );
            assert!(sink.has(&Call::Inode(a)), "{:?}", sink.calls());
            let gone = fs.lookup(&req(), shown, b"a.txt").await;
            assert!(
                gone.is_err() || gone.is_ok_and(|entry| entry.ino == 0),
                "the State no longer holds the source"
            );
        },
    )
    .await;
}

/// A sync that changes, removes and adds most entries of a directory of several thousand
/// serves every change, and reads the State in proportion to the directory, not to its square.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_changing_most_of_a_large_directory_serves_every_change() {
    const ENTRIES: usize = 4000;
    let names: Vec<String> = (0..ENTRIES).map(|i| format!("big/f{i:05}.txt")).collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), &b"old"[..]))
        .collect();
    swapping(&files, async |s: Swapping| {
        let changed: Vec<String> = (0..ENTRIES).map(|i| format!("new {i}")).collect();
        let added: Vec<String> = (0..ENTRIES / 4)
            .map(|i| format!("big/added{i:05}.txt"))
            .collect();
        let mut edits: Vec<(&str, Option<&[u8]>)> = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match i % 4 {
                0 => {}
                1 => edits.push((name.as_str(), None)),
                _ => edits.push((name.as_str(), Some(changed[i].as_bytes()))),
            }
        }
        for name in &added {
            edits.push((name.as_str(), Some(&b"added"[..])));
        }
        let second = s.commit(&edits).await;
        let started = std::time::Instant::now();
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        println!("swap of {} changes: {:?}", edits.len(), started.elapsed());
        let reads = s.fs.last_swap_reads().await;
        let bound = 4 * (ENTRIES + added.len()) as u64;
        assert!(
            reads <= bound,
            "{reads} State reads for a directory of {ENTRIES}"
        );

        let big = s.lookup(ROOT_INO, "big").await.expect("big");
        for i in [0, 2, 3, 1998, 3999] {
            let expected: &[u8] = if i % 4 == 0 {
                b"old"
            } else {
                changed[i].as_bytes()
            };
            assert_eq!(
                served_bytes(&s, big, &format!("f{i:05}.txt")).await,
                expected,
                "f{i:05}.txt"
            );
        }
        for i in [1, 5, 3997] {
            assert_eq!(
                s.lookup(big, &format!("f{i:05}.txt")).await,
                Err(Errno::ENOENT)
            );
        }
        assert_eq!(served_bytes(&s, big, "added00999.txt").await, b"added");
    })
    .await;
}

/// Removing a large tree none of which holds local content reads nothing below its top.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_large_clean_tree_reads_nothing_below_its_top() {
    const ENTRIES: usize = 2000;
    let names: Vec<String> = (0..ENTRIES)
        .map(|i| format!("tree/d{}/f{i:05}.txt", i % 20))
        .collect();
    let mut files: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), &b"x"[..]))
        .collect();
    files.push(("keep.txt", b"k"));
    swapping(&files, async |s: Swapping| {
        // A fixture edit, outside any repository write.
        #[allow(clippy::disallowed_methods)]
        std::fs::remove_dir_all(s.instance.path.join("tree")).expect("remove the tree");
        let second = s.commit(&[]).await;
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        let reads = s.fs.last_swap_reads().await;
        assert!(reads < 64, "{reads} State reads to remove a clean tree");
        assert_eq!(s.lookup(ROOT_INO, "tree").await, Err(Errno::ENOENT));
    })
    .await;
}

/// A mutable store whose next load, once armed, waits to be released.
struct GatedStore {
    inner: Arc<dyn MutableStore>,
    armed: AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl MutableStore for GatedStore {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    async fn load(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.release.notified().await;
        }
        self.inner.clone().load(partition, key, key_type).await
    }

    async fn store(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .store(partition, key, value, key_type)
            .await
    }

    async fn compare_and_swap(
        self: Arc<Self>,
        partition: Partition,
        key: Hash,
        expected: Hash,
        value: Hash,
        key_type: KeyType,
    ) -> Result<Hash, StoreError> {
        self.inner
            .clone()
            .compare_and_swap(partition, key, expected, value, key_type)
            .await
    }

    async fn list(
        self: Arc<Self>,
        partition: Partition,
        key_type: KeyType,
    ) -> Result<KeyValueStream, StoreError> {
        self.inner.clone().list(partition, key_type).await
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }
}

/// A swap that runs between two entries of one READDIRPLUS page, while the page waits on the
/// store for a new inode's time: neither entry puts the content from before the swap in an
/// inode, and the page goes plain from the entry refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_between_two_entries_of_a_readdirplus_page_leaves_no_old_content() {
    swapping(
        &[("a.txt", b"one"), ("b.txt", b"bee")],
        async |s: Swapping| {
            let second = s.commit(&[("c.txt", Some(b"sea"))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let repository = s.repository();
            let gate = Arc::new(GatedStore {
                inner: repository.try_mutable_store_arc().expect("write token"),
                armed: AtomicBool::new(false),
                reached: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let gated = Arc::new(RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    repository.immutable_store(),
                    gate.clone(),
                )
                .with_path(&s.instance.path)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id),
            ));
            let fs = FuseFilesystem::new(gated, second.clone(), attr_context())
                .await
                .expect("provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let order: Vec<String> = {
                let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
                let mut buffer = vec![0u8; 64 * 1024];
                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), ROOT_INO, fh, 0, &mut writer)
                    .await
                    .expect("readdir");
                let used = writer.finish();
                fs.releasedir(ROOT_INO, fh);
                fuse_test_util::dirents(&buffer[..used], false)
                    .map(|dirent| String::from_utf8_lossy(dirent.name).into_owned())
                    .filter(|name| name != "." && name != "..")
                    .collect()
            };
            let (first, second_name) = (order[0].clone(), order[1].clone());
            let held = fs
                .lookup(&req(), ROOT_INO, second_name.as_bytes())
                .await
                .expect("lookup")
                .ino;

            gate.armed.store(true, Ordering::SeqCst);
            let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
            let page = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut writer = DirentWriter::new(&mut buffer);
                    let emitted = fs
                        .readdirplus(&req(), ROOT_INO, fh, 0, &mut writer)
                        .await
                        .expect("readdirplus");
                    let used = writer.finish();
                    let nodeids: Vec<u64> = fuse_test_util::dirents(&buffer[..used], true)
                        .filter(|dirent| dirent.name != b"." && dirent.name != b"..")
                        .map(|dirent| dirent.nodeid)
                        .collect();
                    (emitted, nodeids)
                })
            };
            gate.reached.notified().await;
            let third = s
                .commit(&[
                    (first.as_str(), Some(&b"first, rewritten"[..])),
                    (second_name.as_str(), Some(&b"second, rewritten"[..])),
                ])
                .await;
            swap(
                &fs,
                &operation,
                s.repository(),
                third.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");
            gate.release.notify_one();
            let (emitted, nodeids) = page.await.expect("the page completes");
            fs.releasedir(ROOT_INO, fh);
            assert_eq!(nodeids.len(), 3, "the page lists every entry: {nodeids:?}");
            assert_eq!(
                nodeids[0], 0,
                "the entry waiting on the store is refused: {nodeids:?}"
            );
            assert!(
                nodeids.iter().all(|&nodeid| nodeid == 0),
                "the rest of the page goes plain: {nodeids:?}"
            );
            assert_eq!(emitted, 0);

            assert_eq!(
                fs.getattr(&req(), held).await.expect("attr").attr.size,
                17,
                "the entry after the swap keeps the swap's content"
            );
            let first_ino = fs
                .lookup(&req(), ROOT_INO, first.as_bytes())
                .await
                .expect("lookup")
                .ino;
            assert_eq!(
                fs.getattr(&req(), first_ino).await.expect("attr").attr.size,
                16,
                "the entry waiting on the store takes no old content"
            );
        },
    )
    .await;
}

/// A READDIRPLUS interrupted while a row waits on the store for its new inode's time gives back
/// the lookups it counted for the rows before it: each inode's count is back to its value before
/// the request.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_readdirplus_gives_back_the_rows_it_counted() {
    use zerocopy::IntoBytes as _;

    swapping(
        &[("a.txt", b"one"), ("b.txt", b"bee")],
        async |s: Swapping| {
            let second = s.commit(&[("c.txt", Some(b"sea"))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let repository = s.repository();
            let gate = Arc::new(GatedStore {
                inner: repository.try_mutable_store_arc().expect("write token"),
                armed: AtomicBool::new(false),
                reached: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let gated = Arc::new(RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    repository.immutable_store(),
                    gate.clone(),
                )
                .with_path(&s.instance.path)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id),
            ));
            let fs = FuseFilesystem::new(gated, second.clone(), attr_context())
                .await
                .expect("provider");
            // A plain READDIR caches the root's listing, so the READDIRPLUS answers on a worker.
            let order: Vec<String> = {
                let mut buffer = vec![0u8; 64 * 1024];
                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), ROOT_INO, 0, 0, &mut writer)
                    .await
                    .expect("readdir");
                let used = writer.finish();
                fuse_test_util::dirents(&buffer[..used], false)
                    .map(|dirent| String::from_utf8_lossy(dirent.name).into_owned())
                    .filter(|name| name != "." && name != "..")
                    .collect()
            };
            let counted = fs
                .lookup(&req(), ROOT_INO, order[0].as_bytes())
                .await
                .expect("lookup")
                .ino;
            let lookups = |ino: u64| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
            let before = lookups(counted);

            let (session, transport) = fuse_test_util::mock_session(&fs, 2).await;
            let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
            gate.armed.store(true, Ordering::SeqCst);
            let page = lore_fuse::abi::ReadIn {
                fh,
                size: 64 * 1024,
                ..lore_fuse::abi::ReadIn::default()
            };
            transport.push_request(fuse_test_util::raw_request(
                lore_fuse::abi::Opcode::Readdirplus,
                2,
                ROOT_INO,
                page.as_bytes(),
            ));
            gate.reached.notified().await;
            assert_eq!(
                lookups(counted),
                before + 1,
                "the first row is counted before the page waits on the store"
            );
            transport.push_request(fuse_test_util::raw_request(
                lore_fuse::abi::Opcode::Interrupt,
                3,
                0,
                lore_fuse::abi::InterruptIn { unique: 2 }.as_bytes(),
            ));
            fuse_test_util::await_reply(&transport, 2).await;
            gate.release.notify_one();
            assert_eq!(session.deferred_requests(), 0, "answered on a worker");
            session.shutdown().await;
            fs.releasedir(ROOT_INO, fh);

            let reply = transport
                .replies
                .lock()
                .iter()
                .map(|(_, bytes)| bytes.clone())
                .find(|bytes| bytes[8..16] == 2u64.to_ne_bytes())
                .expect("the READDIRPLUS reply");
            assert_eq!(
                i32::from_ne_bytes(reply[4..8].try_into().expect("the error field")),
                -libc::EINTR,
                "the page answers EINTR"
            );
            assert_eq!(
                lookups(counted),
                before,
                "{}'s lookups after the interrupt",
                order[0]
            );
        },
    )
    .await;
}

/// One READDIRPLUS page of directory `dir` on stream `fh` from `offset`: how many lookups it
/// counted, and each row past `.` and `..` as its name, node id and offset.
async fn plus_rows(
    fs: &FuseFilesystem,
    dir: u64,
    fh: u64,
    offset: u64,
) -> Result<(usize, Vec<(String, u64, u64)>), Errno> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut writer = DirentWriter::new(&mut buffer);
    let emitted = fs.readdirplus(&req(), dir, fh, offset, &mut writer).await?;
    let used = writer.finish();
    let rows = fuse_test_util::dirents(&buffer[..used], true)
        .filter(|dirent| dirent.name != b"." && dirent.name != b"..")
        .map(|dirent| {
            let name = String::from_utf8_lossy(dirent.name).into_owned();
            (name, dirent.nodeid, dirent.offset)
        })
        .collect();
    Ok((emitted, rows))
}

/// A READDIRPLUS row whose own lookup fails goes plain, and the page answers every row: the rows
/// with inodes keep their attributes and are counted. Here the rows with no inode fail, as the
/// recorded time a new inode takes needs its directory's path, which an ancestor's inode gone
/// leaves unbuilt. A page whose first row fails answers too.
#[tokio::test(flavor = "multi_thread")]
async fn a_readdirplus_row_whose_lookup_fails_goes_plain_and_the_page_answers() {
    swapping(
        &[
            ("p/d/a.txt", b"a"),
            ("p/d/b.txt", b"b"),
            ("p/d/c.txt", b"c"),
            ("p/d/e.txt", b"e"),
        ],
        async |s: Swapping| {
            let second = s.commit(&[("q.txt", Some(b"q"))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let fs = FuseFilesystem::new(s.repository(), second.clone(), attr_context())
                .await
                .expect("provider");
            assert!(
                !fs.recorded_times().none(),
                "a new inode takes a recorded time"
            );
            let lookup = async |parent: u64, name: &str| {
                fs.lookup(&req(), parent, name.as_bytes())
                    .await
                    .expect("lookup")
                    .ino
            };
            let p = lookup(ROOT_INO, "p").await;
            let d = lookup(p, "d").await;
            let order: Vec<String> = {
                let mut buffer = vec![0u8; 64 * 1024];
                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), d, 0, 0, &mut writer)
                    .await
                    .expect("readdir");
                let used = writer.finish();
                fuse_test_util::dirents(&buffer[..used], false)
                    .map(|dirent| String::from_utf8_lossy(dirent.name).into_owned())
                    .filter(|name| name != "." && name != "..")
                    .collect()
            };
            assert_eq!(order.len(), 4);
            // Rows 0 and 2 have inodes; rows 1 and 3 have none.
            let held = [lookup(d, &order[0]).await, lookup(d, &order[2]).await];
            let lookups = |ino: u64| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
            fs.forget(p, lookups(p));
            assert!(fs.inodes().get(p).is_none(), "p's inode is gone");

            let fh = fs.opendir(&req(), d).expect("opendir").fh;
            let (emitted, rows) = plus_rows(&fs, d, fh, 0).await.expect("the page answers");
            let names: Vec<&str> = rows.iter().map(|(name, ..)| name.as_str()).collect();
            assert_eq!(names, order, "the page lists every row: {rows:?}");
            let nodeids: Vec<u64> = rows.iter().map(|&(_, nodeid, _)| nodeid).collect();
            assert_eq!(nodeids, [held[0], 0, held[1], 0], "{rows:?}");
            assert_eq!(emitted, 2);

            // From the first failing row.
            let (emitted, rows) = plus_rows(&fs, d, fh, rows[0].2)
                .await
                .expect("a page whose first row fails answers");
            let nodeids: Vec<u64> = rows.iter().map(|&(_, nodeid, _)| nodeid).collect();
            assert_eq!(nodeids, [0, held[1], 0], "{rows:?}");
            assert_eq!(emitted, 1);
            fs.releasedir(d, fh);
        },
    )
    .await;
}

/// A swap that lands mid-page, while a row waits on the store for its new inode's time, makes
/// every row after it plain, whatever the swap did to it: a row whose node it detached, one it
/// left alone and one it rewrote, each with an inode the kernel holds. None of them is counted,
/// and the rewritten one keeps the swap's content.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_landing_mid_page_makes_every_row_after_it_plain() {
    swapping(
        &[
            ("a.txt", b"one"),
            ("b.txt", b"bee"),
            ("c.txt", b"sea"),
            ("e.txt", b"eee"),
        ],
        async |s: Swapping| {
            let second = s.commit(&[("g.txt", Some(b"gee"))]).await;
            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            let repository = s.repository();
            let gate = Arc::new(GatedStore {
                inner: repository.try_mutable_store_arc().expect("write token"),
                armed: AtomicBool::new(false),
                reached: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let gated = Arc::new(RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    repository.immutable_store(),
                    gate.clone(),
                )
                .with_path(&s.instance.path)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id),
            ));
            let fs = FuseFilesystem::new(gated, second.clone(), attr_context())
                .await
                .expect("provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let order: Vec<String> = {
                let mut buffer = vec![0u8; 64 * 1024];
                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), ROOT_INO, 0, 0, &mut writer)
                    .await
                    .expect("readdir");
                let used = writer.finish();
                fuse_test_util::dirents(&buffer[..used], false)
                    .map(|dirent| String::from_utf8_lossy(dirent.name).into_owned())
                    .filter(|name| name != "." && name != "..")
                    .collect()
            };
            assert_eq!(order.len(), 5);
            // Every row after the first has an inode, so only the first waits on the store.
            let mut held = Vec::new();
            for name in &order[1..] {
                let ino = fs
                    .lookup(&req(), ROOT_INO, name.as_bytes())
                    .await
                    .expect("lookup")
                    .ino;
                held.push(ino);
            }
            let lookups = |ino: u64| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
            let before: Vec<u64> = held.iter().map(|&ino| lookups(ino)).collect();

            gate.armed.store(true, Ordering::SeqCst);
            let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
            let page = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move { plus_rows(&fs, ROOT_INO, fh, 0).await })
            };
            gate.reached.notified().await;
            let (detached, kept, rewritten) = (&order[1], &order[2], &order[3]);
            let third = s
                .commit(&[
                    (detached.as_str(), None),
                    (rewritten.as_str(), Some(&b"rewritten"[..])),
                ])
                .await;
            swap(
                &fs,
                &operation,
                s.repository(),
                third.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");
            gate.release.notify_one();
            let (emitted, rows) = page
                .await
                .expect("the page completes")
                .expect("the page answers");
            fs.releasedir(ROOT_INO, fh);

            let names: Vec<&str> = rows.iter().map(|(name, ..)| name.as_str()).collect();
            assert_eq!(names, order, "the page lists its pinned listing");
            assert!(
                rows.iter().all(|&(_, nodeid, _)| nodeid == 0),
                "{detached}, {kept} and {rewritten} are plain: {rows:?}"
            );
            assert_eq!(emitted, 0);
            assert_eq!(lookups(held[1]), before[1], "{kept} is not counted");
            assert_eq!(
                fs.getattr(&req(), held[2]).await.expect("attr").attr.size,
                9,
                "{rewritten} keeps the swap's content"
            );
        },
    )
    .await;
}

/// A swap that lands between two rows of a READDIRPLUS page with no row refused an inode for it,
/// here while a row whose lookup failed forgets the inode it made, still makes every row after it
/// plain: the page checks for a swap at each row, not only where one refuses an inode.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_landing_between_rows_with_none_refused_makes_the_rows_after_it_plain() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    swapping(
        &[
            ("a.txt", b"one"),
            ("b.txt", b"bee"),
            ("c.txt", b"sea"),
            ("e.txt", b"eee"),
        ],
        async |s: Swapping| {
            let third = s.commit(&[("a.txt", Some(&b"rewritten"[..]))]).await;
            let external = TempDir::new("lore-fuse-swap-between-rows-");
            let write = external.path().join("write");
            std::fs::create_dir_all(&write).expect("write");
            fuse_fixture::test_file_write(&write.join("gone.txt"), b"gone");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let order: Vec<String> = {
                let mut buffer = vec![0u8; 64 * 1024];
                let mut writer = DirentWriter::new(&mut buffer);
                fs.readdir(&req(), ROOT_INO, 0, 0, &mut writer)
                    .await
                    .expect("readdir");
                let used = writer.finish();
                fuse_test_util::dirents(&buffer[..used], false)
                    .map(|dirent| String::from_utf8_lossy(dirent.name).into_owned())
                    .filter(|name| name != "." && name != "..")
                    .collect()
            };
            let at = order
                .iter()
                .position(|name| name == "gone.txt")
                .expect("the local file is listed");
            assert!(at + 1 < order.len(), "rows follow gone.txt: {order:?}");
            let mut held = Vec::new();
            for name in &order[at + 1..] {
                let ino = fs
                    .lookup(&req(), ROOT_INO, name.as_bytes())
                    .await
                    .expect("lookup")
                    .ino;
                held.push(ino);
            }
            let lookups = |ino: u64| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
            let before: Vec<u64> = held.iter().map(|&ino| lookups(ino)).collect();
            // Its host file gone, the local file's new inode fails and is forgotten.
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_file(write.join("gone.txt")).expect("remove the host file");
            let swapped = Arc::new(AtomicBool::new(false));
            // The hook is synchronous, so the swap blocks the page's thread until it is done.
            #[allow(clippy::disallowed_methods)]
            let hook: lore_revision::fs::fuse::filesystem::WriteHook = Arc::new({
                let fs = Arc::downgrade(&fs);
                let swapped = swapped.clone();
                let repository = s.repository();
                move |step| {
                    if matches!(step, WriteStep::Forgotten { .. })
                        && !swapped.swap(true, Ordering::SeqCst)
                    {
                        let fs = fs.upgrade().expect("the provider");
                        let operation = InstanceOperationImpl::new(
                            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                            true,
                        );
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(swap(
                                &fs,
                                &operation,
                                repository.clone(),
                                third.clone(),
                                Arc::new(RecordingSink::default()),
                            ))
                        })
                        .expect("swap");
                    }
                    true
                }
            });
            fs.set_write_hook(hook);

            let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
            let (emitted, rows) = plus_rows(&fs, ROOT_INO, fh, 0)
                .await
                .expect("the page answers");
            fs.releasedir(ROOT_INO, fh);
            assert!(
                swapped.load(Ordering::SeqCst),
                "the swap ran inside the page"
            );
            let names: Vec<&str> = rows.iter().map(|(name, ..)| name.as_str()).collect();
            assert_eq!(names, order, "the page lists its pinned listing");
            assert!(
                rows[at..].iter().all(|&(_, nodeid, _)| nodeid == 0),
                "gone.txt and every row after it are plain: {rows:?}"
            );
            assert_eq!(emitted, at, "the rows before it are counted");
            let after: Vec<u64> = held.iter().map(|&ino| lookups(ino)).collect();
            assert_eq!(after, before, "no row after the swap is counted");
        },
    )
    .await;
}

/// A hook that, the first time it reaches `phase`, reports it was reached and then pends for
/// ever; otherwise it passes.
fn pending_once(reached: Arc<tokio::sync::Notify>) -> SwapHook {
    let armed = Arc::new(AtomicBool::new(true));
    Arc::new(move |phase| {
        let armed = armed.clone();
        let reached = reached.clone();
        Box::pin(async move {
            if phase == SwapPhase::Removed && armed.swap(false, Ordering::SeqCst) {
                reached.notify_one();
                std::future::pending::<()>().await;
            }
            true
        })
    })
}

/// A hook that fails the first apply at `phase` and passes otherwise.
fn failing_once(at: SwapPhase) -> SwapHook {
    let armed = Arc::new(AtomicBool::new(true));
    Arc::new(move |phase| {
        let armed = armed.clone();
        Box::pin(async move { phase != at || !armed.swap(false, Ordering::SeqCst) })
    })
}

/// A hook that runs `during` once, the first time an apply reaches `at`, and passes.
fn running_once(
    at: SwapPhase,
    during: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> SwapHook {
    let armed = Arc::new(AtomicBool::new(true));
    let during = Arc::new(during);
    Arc::new(move |phase| {
        let armed = armed.clone();
        let during = during.clone();
        Box::pin(async move {
            if phase == at && armed.swap(false, Ordering::SeqCst) {
                during().await;
            }
            true
        })
    })
}

/// A swap dropped mid-apply ends as one that failed: it is not left applying, the listings it
/// touched stay cached with what it changed in their logs, and the next swap completes what it
/// began.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_dropped_mid_apply_leaves_a_consistent_view() {
    swapping(
        &[("a.txt", b"one"), ("gone.txt", b"g")],
        async |s: Swapping| {
            let reached = Arc::new(tokio::sync::Notify::new());
            s.fs.set_swap_hook(pending_once(reached.clone()));
            s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
            let second = s
                .commit(&[("a.txt", Some(b"two")), ("gone.txt", None)])
                .await;
            let sink = Arc::new(RecordingSink::default());
            tokio::select! {
                () = s.swap_to(&second, sink.clone()) => panic!("the swap pends"),
                () = reached.notified() => {}
            }
            assert!(!s.fs.is_swapping(), "a dropped swap is not left applying");
            assert!(
                s.fs.served()
                    .listings
                    .get(lore_revision::node::ROOT_NODE)
                    .is_some(),
                "the listing of the directory it touched is dropped"
            );

            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            assert_eq!(served_bytes(&s, ROOT_INO, "a.txt").await, b"two");
            assert_eq!(s.lookup(ROOT_INO, "gone.txt").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

/// A listing built while a swap applies is cached: its log mark precedes its walk, so what the
/// swap changes there later is read from the log.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_built_while_a_swap_applies_is_cached() {
    Box::pin(swapping(
        &[("a.txt", b"one"), ("dir/b.txt", b"bee")],
        async |s: Swapping| {
            let reached = Arc::new(tokio::sync::Notify::new());
            s.fs.set_swap_hook(pending_once(reached.clone()));
            let dir = s.lookup(ROOT_INO, "dir").await.expect("dir");
            let second = s.commit(&[("a.txt", Some(b"two"))]).await;
            let fs = s.fs.clone();
            let during = async {
                reached.notified().await;
                fs.lookup(&req(), dir, b"b.txt").await.expect("lookup");
                let dir_node = fs
                    .state()
                    .find_subnode(
                        s.repository(),
                        lore_revision::node::ROOT_NODE,
                        hash_string("dir"),
                    )
                    .await
                    .expect("dir node");
                fs.served().listings.get(dir_node).is_some()
            };
            let sink = Arc::new(RecordingSink::default());
            let cached = tokio::select! {
                () = s.swap_to(&second, sink.clone()) => panic!("the swap pends"),
                cached = during => cached,
            };
            assert!(cached, "a listing built during the apply is not cached");
        },
    ))
    .await;
}

/// An apply that fails partway drops the listings it touched, tells the kernel what it
/// planned, and leaves the State unsettled: a swap back to the served revision undoes what it
/// did.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_apply_invalidates_and_the_next_swap_settles_the_state() {
    swapping(
        &[("a.txt", b"one"), ("gone.txt", b"g")],
        async |s: Swapping| {
            s.fs.set_swap_hook(failing_once(SwapPhase::Removed));
            s.lookup(ROOT_INO, "gone.txt").await.expect("lookup");
            let second = s.commit(&[("gone.txt", None)]).await;
            let sink = Arc::new(RecordingSink::default());
            let failed = swap(
                &s.fs,
                &s.operation,
                s.repository(),
                second.clone(),
                sink.clone(),
            )
            .await;
            assert!(failed.is_err(), "the hook fails the apply");
            assert!(!s.fs.is_swapping());
            assert!(
                sink.has(&Call::Entry(ROOT_INO, b"gone.txt".to_vec())),
                "{:?}",
                sink.calls()
            );
            assert!(sink.has(&Call::Inode(ROOT_INO)), "{:?}", sink.calls());
            assert_eq!(s.fs.served().revision(), s.first.revision());

            swap(
                &s.fs,
                &s.operation,
                s.repository(),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap back");
            assert_eq!(served_bytes(&s, ROOT_INO, "gone.txt").await, b"g");
        },
    )
    .await;
}

/// An apply that fails after removing a file below a directory leaves the directory clean: the
/// swap marks nothing, as its removal is no local change. The next swap to the target settles
/// the State, which then differs from the target in nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_removal_below_a_directory_marks_nothing_and_the_next_swap_settles_it() {
    swapping(
        &[("dir/kept.txt", b"k"), ("dir/gone.txt", b"g")],
        async |s: Swapping| {
            s.fs.set_swap_hook(failing_once(SwapPhase::Removed));
            let dir = s.lookup(ROOT_INO, "dir").await.expect("dir");
            s.lookup(dir, "gone.txt").await.expect("lookup");
            let second = s.commit(&[("dir/gone.txt", None)]).await;
            let failed = swap(
                &s.fs,
                &s.operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed.is_err(), "the hook fails the apply");
            let state = s.fs.state();
            let dir_node = state
                .find_subnode(
                    s.repository(),
                    lore_revision::node::ROOT_NODE,
                    hash_string("dir"),
                )
                .await
                .expect("dir node");
            let removed = state
                .find_subnode(s.repository(), dir_node, hash_string("gone.txt"))
                .await;
            assert!(
                removed
                    .as_ref()
                    .is_err_and(|error| error.is_node_not_found()),
                "the failed apply removed the file, got {removed:?}"
            );
            let record = state.node(s.repository(), dir_node).await.expect("dir");
            assert!(
                !record.is_dirty(),
                "the swap's removal marked its directory"
            );
            assert_eq!(s.fs.served().revision(), s.first.revision());

            s.swap_to(&second, Arc::new(RecordingSink::default())).await;
            assert_eq!(s.lookup(dir, "gone.txt").await, Err(Errno::ENOENT));
            assert_eq!(served_bytes(&s, dir, "kept.txt").await, b"k");
            let left = s.diff(&s.fs.state(), &second).await;
            assert!(left.is_empty(), "the settled State differs: {left:?}");
        },
    )
    .await;
}

/// A sync never loses a local change where the target touches the same path: a local edit of a
/// file the target changes or deletes, local content in a directory the target deletes, and a
/// local deletion of a file the target changes all stay, until verify and force decide them.
#[tokio::test]
async fn a_sync_never_loses_a_local_change_the_target_touches() {
    swapping(
        &[
            ("edited.txt", b"tracked"),
            ("edited-gone.txt", b"tracked"),
            ("dir/tracked.txt", b"t"),
            ("deleted.txt", b"d"),
            ("plain.txt", b"p"),
        ],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-guards-");
            let write = external.path().join("write");
            std::fs::create_dir_all(write.join("dir")).expect("write/");
            fuse_fixture::test_file_write(&write.join("edited.txt"), b"local edit");
            fuse_fixture::test_file_write(&write.join("edited-gone.txt"), b"local edit");
            fuse_fixture::test_file_write(&write.join("dir/local.txt"), b"local");
            fuse_fixture::test_file_write(&external.path().join("deleted"), b"deleted.txt\n");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            // The target deletes the directory holding local content whole.
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_dir_all(s.instance.path.join("dir")).expect("remove dir/");
            let second = s
                .commit(&[
                    ("edited.txt", Some(b"target")),
                    ("edited-gone.txt", None),
                    ("deleted.txt", Some(b"target")),
                    ("plain.txt", Some(b"target")),
                ])
                .await;
            assert!(
                s.diff(&s.first, &second)
                    .await
                    .iter()
                    .any(|change| change.path().as_str() == "dir"
                        && change.action == lore_revision::change::FileAction::Delete),
                "the target deletes dir/ itself"
            );
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");

            let state = fs.state();
            let repository = s.repository();
            let node = async |path: &str| {
                let mut node = lore_revision::node::ROOT_NODE;
                for name in path.split('/') {
                    match state
                        .find_subnode(repository.clone(), node, hash_string(name))
                        .await
                    {
                        Ok(child) => node = child,
                        Err(error) if error.is_node_not_found() => return None,
                        Err(error) => panic!("{path}: {error}"),
                    }
                }
                Some(state.node(repository.clone(), node).await.expect("node"))
            };
            let local = lore_revision::node::NodeExtFlags::LocalContent;
            for path in ["edited.txt", "edited-gone.txt", "dir/local.txt"] {
                assert!(
                    node(path)
                        .await
                        .is_some_and(|n| n.ext_flags().contains(local)),
                    "{path}: the local content stays"
                );
            }
            assert!(
                node("dir/tracked.txt").await.is_none(),
                "the target's deletion applies"
            );
            assert!(
                node("deleted.txt").await.is_none(),
                "the local deletion stays"
            );
            assert_eq!(node("plain.txt").await.expect("plain.txt").size, 6);
        },
    )
    .await;
}

/// The inode the kernel holds for a local file keeps serving the host bytes through a sync that
/// changes or deletes the file's path: the plan neither orphans nor invalidates it.
#[tokio::test]
async fn a_sync_leaves_the_inode_of_a_local_file_the_target_touches_alone() {
    swapping(
        &[("edited.txt", b"tracked"), ("edited-gone.txt", b"tracked")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-local-inode-");
            let write = external.path().join("write");
            std::fs::create_dir_all(&write).expect("write/");
            fuse_fixture::test_file_write(&write.join("edited.txt"), b"local edit");
            fuse_fixture::test_file_write(&write.join("edited-gone.txt"), b"local gone");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let lookup = async |name: &str| {
                fs.lookup_at(&fs.served(), ROOT_INO, name.as_bytes())
                    .await
                    .expect("lookup")
                    .ino
            };
            let edited = lookup("edited.txt").await;
            let gone = lookup("edited-gone.txt").await;
            let second = s
                .commit(&[("edited.txt", Some(b"target")), ("edited-gone.txt", None)])
                .await;
            let sink = Arc::new(RecordingSink::default());
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                sink.clone(),
            )
            .await
            .expect("swap");

            for (name, ino, bytes) in [
                ("edited.txt", edited, &b"local edit"[..]),
                ("edited-gone.txt", gone, b"local gone"),
            ] {
                assert!(!sink.has(&Call::Inode(ino)), "{name}: no invalidation");
                assert_eq!(
                    lookup(name).await,
                    ino,
                    "{name}: the inode stays on its path"
                );
                assert_eq!(
                    read_at(&fs, ino, 0, 64).await,
                    bytes,
                    "{name}: the host bytes"
                );
            }
        },
    )
    .await;
}

/// A sync whose target deletes a directory holding local content keeps the directory in the
/// State, and the kernel's inode for it keeps serving: CREATE and RENAME through it, and
/// GETATTR and READ of the local file below it.
#[tokio::test]
async fn a_sync_deleting_a_directory_holding_local_content_keeps_its_inode() {
    swapping(
        &[("dir/tracked.txt", b"tracked"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-kept-dir-");
            let write = external.path().join("write");
            std::fs::create_dir_all(write.join("dir")).expect("write/dir");
            fuse_fixture::test_file_write(&write.join("dir/local.txt"), b"local");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let lookup = async |parent: u64, name: &str| {
                fs.lookup_at(&fs.served(), parent, name.as_bytes())
                    .await
                    .map(|entry| entry.ino)
            };
            let dir = lookup(ROOT_INO, "dir").await.expect("dir");
            let local = lookup(dir, "local.txt").await.expect("local.txt");
            #[allow(clippy::disallowed_methods)]
            {
                std::fs::remove_file(s.instance.path.join("dir/tracked.txt")).expect("rm");
                std::fs::remove_dir(s.instance.path.join("dir")).expect("rmdir");
            }
            let second = fuse_fixture::test_commit_tree(&s.instance, "drop dir").await;
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");

            assert_eq!(lookup(ROOT_INO, "dir").await, Ok(dir), "the inode stays");
            let created = fs
                .create(
                    &req(),
                    dir,
                    b"new.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    (libc::O_CREAT | libc::O_RDWR) as u32,
                )
                .await;
            assert!(created.is_ok(), "CREATE through the kept directory");
            assert_eq!(
                fs.getattr(&req(), local).await.map(|reply| reply.attr.size),
                Ok(5)
            );
            assert_eq!(read_at(&fs, local, 0, 16).await, b"local");
            fs.rename(&req(), dir, b"local.txt", dir, b"moved.txt", 0)
                .await
                .expect("RENAME through the kept directory");
            assert_eq!(read_at(&fs, local, 0, 16).await, b"local");
        },
    )
    .await;
}

/// Whether a LOOKUP answered absent: `ENOENT` or a negative entry.
fn absent(reply: Result<lore_fuse::reply::EntryReply, Errno>) -> bool {
    match reply {
        Err(errno) => errno == Errno::ENOENT,
        Ok(entry) => entry.ino == 0,
    }
}

/// A swap hook that runs `work` once, the first time the apply reaches `at`.
fn once_at(
    at: SwapPhase,
    work: impl FnOnce() -> futures::future::BoxFuture<'static, ()> + Send + 'static,
) -> SwapHook {
    let work = Arc::new(Mutex::new(Some(work)));
    Arc::new(move |phase| {
        let work = (phase == at).then(|| work.lock().take()).flatten();
        Box::pin(async move {
            if let Some(work) = work {
                work().await;
            }
            true
        })
    })
}

/// A CREATE below a directory a sync has removed, into a subdirectory the kernel still holds,
/// fails rather than landing in the removed tree.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_below_a_directory_a_sync_removed_fails() {
    swapping(
        &[("dir/sub/tracked.txt", b"tracked"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-deep-create-");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let dir = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("dir")
                .ino;
            let sub = fs
                .lookup_at(&fs.served(), dir, b"sub")
                .await
                .expect("sub")
                .ino;
            let created = Arc::new(Mutex::new(None));
            let (creator, result) = (fs.clone(), created.clone());
            fs.set_swap_hook(once_at(SwapPhase::Removed, move || {
                Box::pin(async move {
                    let reply = creator
                        .create(
                            &req(),
                            sub,
                            b"new.txt",
                            libc::S_IFREG | 0o644,
                            0o022,
                            (libc::O_CREAT | libc::O_RDWR) as u32,
                        )
                        .await
                        .map(|reply| reply.entry.ino);
                    *result.lock() = Some(reply);
                })
            }));
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_dir_all(s.instance.path.join("dir")).expect("rm dir");
            let second = fuse_fixture::test_commit_tree(&s.instance, "drop dir").await;
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");
            assert_eq!(
                created.lock().take(),
                Some(Err(Errno::ENOENT)),
                "the CREATE fails"
            );
            assert!(
                !external.path().join("write/dir/sub/new.txt").exists(),
                "write/ keeps no file for it"
            );
        },
    )
    .await;
}

/// A CREATE landing in a directory a sync removes, between the removal's read of the directory
/// and its detach, is kept with the directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_while_a_sync_removes_its_directory_is_kept() {
    swapping(
        &[("dir/tracked.txt", b"tracked"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-create-race-");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let dir = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("dir")
                .ino;
            let creator = fs.clone();
            fs.set_swap_hook(once_at(SwapPhase::Detach, move || {
                Box::pin(async move {
                    creator
                        .create(
                            &req(),
                            dir,
                            b"new.txt",
                            libc::S_IFREG | 0o644,
                            0o022,
                            (libc::O_CREAT | libc::O_RDWR) as u32,
                        )
                        .await
                        .expect("CREATE during the removal");
                })
            }));
            #[allow(clippy::disallowed_methods)]
            {
                std::fs::remove_file(s.instance.path.join("dir/tracked.txt")).expect("rm");
                std::fs::remove_dir(s.instance.path.join("dir")).expect("rmdir");
            }
            let second = fuse_fixture::test_commit_tree(&s.instance, "drop dir").await;
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");
            let dir = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("dir stays")
                .ino;
            assert_ne!(dir, 0, "dir stays");
            let new = fs
                .lookup_at(&fs.served(), dir, b"new.txt")
                .await
                .expect("new.txt")
                .ino;
            assert_ne!(new, 0, "new.txt stays");
            let tracked = fs.lookup_at(&fs.served(), dir, b"tracked.txt").await;
            assert!(
                absent(tracked),
                "the target's deletion of tracked.txt applies"
            );
        },
    )
    .await;
}

/// A CREATE landing in a directory a sync empties child by child, after the removal read the
/// children, keeps the directory: the removal detaches it only while it has no child.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_as_a_sync_empties_its_directory_is_kept() {
    swapping(
        &[("dir/a.txt", b"a"), ("dir/b.txt", b"b"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-emptied-");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let dir = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("dir")
                .ino;
            let dir_node = fs
                .state()
                .find_subnode(
                    s.repository(),
                    lore_revision::node::ROOT_NODE,
                    hash_string("dir"),
                )
                .await
                .expect("dir's node");
            let detaches = Arc::new(AtomicUsize::new(0));
            let (counted, creator, repository) =
                (detaches.clone(), Arc::downgrade(&fs), s.repository());
            fs.set_swap_hook(Arc::new(move |phase| {
                let (counted, creator, repository) =
                    (counted.clone(), creator.clone(), repository.clone());
                Box::pin(async move {
                    if phase != SwapPhase::Detach {
                        return true;
                    }
                    let fs = creator.upgrade().expect("the provider");
                    match counted.fetch_add(1, Ordering::SeqCst) {
                        // The directory's own: dirty, it is removed child by child.
                        0 => fs
                            .state()
                            .node_mark(
                                &repository,
                                dir_node,
                                Mark::dirty(NodeFlags::Dirty, Ext::Keep, false),
                            )
                            .await
                            .expect("mark dir dirty"),
                        // Its first child's, once the removal has read the children.
                        1 => {
                            fs.create(
                                &req(),
                                dir,
                                b"new.txt",
                                libc::S_IFREG | 0o644,
                                0o022,
                                (libc::O_CREAT | libc::O_RDWR) as u32,
                            )
                            .await
                            .expect("CREATE during the removal");
                        }
                        _ => {}
                    }
                    true
                })
            }));
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_dir_all(s.instance.path.join("dir")).expect("remove dir/");
            let second = fuse_fixture::test_commit_tree(&s.instance, "drop dir").await;

            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");

            assert_eq!(
                detaches.load(Ordering::SeqCst),
                3,
                "the directory, then each of its two children"
            );
            let kept = fs
                .lookup_at(&fs.served(), ROOT_INO, b"dir")
                .await
                .expect("dir stays")
                .ino;
            assert_eq!(kept, dir);
            assert!(!absent(fs.lookup_at(&fs.served(), dir, b"new.txt").await));
            for gone in ["a.txt", "b.txt"] {
                assert!(
                    absent(fs.lookup_at(&fs.served(), dir, gone.as_bytes()).await),
                    "{gone} goes"
                );
            }
        },
    )
    .await;
}

/// An UNLINK landing between a sync's read of a file and its in-place change of the content
/// leaves the file deleted and the sync complete.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_landing_before_a_sync_changes_the_file_leaves_it_deleted() {
    swapping(
        &[("x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-unlink-race-");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let remover = fs.clone();
            fs.set_swap_hook(once_at(SwapPhase::Modify, move || {
                Box::pin(async move {
                    remover
                        .unlink(&req(), ROOT_INO, b"x.txt")
                        .await
                        .expect("UNLINK during the sync");
                })
            }));
            let second = s.commit(&[("x.txt", Some(b"target x"))]).await;
            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("the sync completes");
            let x = fs.lookup_at(&fs.served(), ROOT_INO, b"x.txt").await;
            assert!(absent(x), "x.txt stays deleted");
        },
    )
    .await;
}

/// A swap that makes a file executable along with its new content gives the file's node the
/// target's mode, and its inode an executable mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_making_a_file_executable_gives_it_the_targets_mode() {
    a_swap_making_x_executable(b"target x").await;
}

/// A swap that makes a file executable and leaves its content as it is gives the file's node the
/// target's mode, and its inode an executable mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_changing_only_a_files_mode_gives_it_the_targets_mode() {
    a_swap_making_x_executable(b"x").await;
}

/// A swap from `x.txt` holding `x` to a target where it is executable and holds `content`.
async fn a_swap_making_x_executable(content: &'static [u8]) {
    swapping(&[("x.txt", b"x")], async |s: Swapping| {
        let x = s.lookup(ROOT_INO, "x.txt").await.expect("x.txt");
        assert_eq!(
            s.getattr(x).await.mode & 0o111,
            0,
            "x.txt starts not executable"
        );
        let full = s.instance.path.join("x.txt");
        fuse_fixture::test_file_write(&full, content);
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let second = s.commit(&[]).await;
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        let node =
            s.fs.state()
                .find_node(s.repository(), "x.txt")
                .await
                .expect("x.txt");
        assert_eq!(
            node.mode & NodeFileMode::Executable.bits(),
            NodeFileMode::Executable.bits(),
            "x.txt's node takes the target's mode"
        );
        let x = s.lookup(ROOT_INO, "x.txt").await.expect("x.txt");
        assert_ne!(s.getattr(x).await.mode & 0o111, 0, "x.txt is executable");
    })
    .await;
}

/// A writable provider over `s`'s first revision with `x.txt` looked up, and a swap hook that
/// writes `WRITTEN` through `x.txt`'s inode the first time an apply reaches [`SwapPhase::Modify`],
/// then answers `at_placed` at [`SwapPhase::Placed`]: the provider, its operation, `x.txt`'s
/// inode and whether the write landed.
async fn writing_at_modify(
    s: &Swapping,
    external: &TempDir,
    at_placed: bool,
) -> (
    Arc<FuseFilesystem>,
    InstanceOperationImpl,
    u64,
    Arc<AtomicBool>,
) {
    let fs = FuseFilesystem::new_writable(
        s.repository(),
        s.first.clone(),
        attr_context(),
        external.path(),
    )
    .await
    .expect("writable provider");
    let operation = InstanceOperationImpl::new(
        StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
        true,
    );
    let x = fs
        .lookup_at(&fs.served(), ROOT_INO, b"x.txt")
        .await
        .expect("x.txt")
        .ino;
    let wrote = Arc::new(AtomicBool::new(false));
    let armed = Arc::new(AtomicBool::new(true));
    let (writer, landed) = (Arc::downgrade(&fs), wrote.clone());
    fs.set_swap_hook(Arc::new(move |phase| {
        let (writer, landed, armed) = (writer.clone(), landed.clone(), armed.clone());
        Box::pin(async move {
            match phase {
                SwapPhase::Modify if armed.swap(false, Ordering::SeqCst) => {
                    let writer = writer.upgrade().expect("the provider");
                    writer
                        .write(&req(), x, 0, 0, b"WRITTEN", 0)
                        .await
                        .expect("WRITE during the sync");
                    landed.store(true, Ordering::SeqCst);
                    true
                }
                SwapPhase::Placed => at_placed,
                _ => true,
            }
        })
    }));
    (fs, operation, x, wrote)
}

/// A WRITE landing between a sync's read of a tracked file and its in-place change of the
/// content copies the file up first, and the change refuses a node that is no longer clean: the
/// write's bytes and the local record stay, and the sync completes.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_landing_before_a_sync_changes_the_file_keeps_the_write() {
    swapping(
        &[("x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-write-race-");
            let (fs, operation, x, wrote) = writing_at_modify(&s, &external, true).await;
            let second = s.commit(&[("x.txt", Some(b"target x"))]).await;

            swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("the sync completes");

            assert!(wrote.load(Ordering::SeqCst), "the write landed in the sync");
            let node = fs
                .state()
                .find_subnode(
                    s.repository(),
                    lore_revision::node::ROOT_NODE,
                    hash_string("x.txt"),
                )
                .await
                .expect("x.txt");
            let record = fs
                .state()
                .node(s.repository(), node)
                .await
                .expect("x.txt's record");
            assert!(
                record
                    .ext_flags()
                    .contains(lore_revision::node::NodeExtFlags::LocalContent)
                    && record.address.hash.is_zero(),
                "x.txt's record stays local"
            );
            assert_eq!(read_at(&fs, x, 0, 64).await, b"WRITTEN");
            assert_eq!(
                std::fs::read(external.path().join("write/x.txt")).expect("write/x.txt"),
                b"WRITTEN"
            );
        },
    )
    .await;
}

/// An apply that fails after a WRITE copied up a file it was changing leaves that file's inode
/// alone: the paths it changed lose their tracked file inodes, but a local one keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_apply_keeps_the_inode_of_a_file_a_write_copied_up_in_it() {
    swapping(
        &[("x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-failed-write-race-");
            let (fs, operation, x, wrote) = writing_at_modify(&s, &external, false).await;
            let second = s.commit(&[("x.txt", Some(b"target x"))]).await;

            let failed = swap(
                &fs,
                &operation,
                s.repository(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;

            assert!(
                wrote.load(Ordering::SeqCst),
                "the write landed in the apply"
            );
            assert!(failed.is_err(), "the hook fails the apply once it placed");
            assert!(
                fs.inodes().get(x).is_some_and(|entry| !entry.orphaned),
                "x.txt's inode is not orphaned"
            );
            assert_eq!(
                fs.lookup_at(&fs.served(), ROOT_INO, b"x.txt")
                    .await
                    .expect("x.txt")
                    .ino,
                x
            );
            assert_eq!(read_at(&fs, x, 0, 64).await, b"WRITTEN");
        },
    )
    .await;
}

/// An UNLINK landing inside the re-placement of a path an earlier apply left unsettled, between
/// the removals and the placing, stays: a sync that restores deleted files the target changes
/// restores no file it read before the UNLINK.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_inside_a_re_placement_stays_deleted() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    swapping(
        &[("a.txt", b"one"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-review-guard-");
            std::fs::create_dir_all(external.path().join("write")).expect("write/");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            let fail = failing_once(SwapPhase::Placed);
            let armed = Arc::new(AtomicBool::new(false));
            let unlinked = Arc::new(Mutex::new(None));
            fs.set_swap_hook(Arc::new({
                let (remover, armed, unlinked) =
                    (Arc::downgrade(&fs), armed.clone(), unlinked.clone());
                move |phase| {
                    let failing = fail(phase);
                    let remover = (phase == SwapPhase::Removed
                        && armed.swap(false, Ordering::SeqCst))
                    .then(|| remover.upgrade())
                    .flatten();
                    let unlinked = unlinked.clone();
                    Box::pin(async move {
                        if let Some(remover) = remover {
                            let answer = remover.unlink(&req(), ROOT_INO, b"a.txt").await;
                            *unlinked.lock() = Some(answer);
                        }
                        failing.await
                    })
                }
            }));
            let failed_target = s.commit(&[("a.txt", Some(b"two"))]).await;
            let failed = swap(
                &fs,
                &operation,
                s.repository(),
                failed_target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");
            assert_eq!(fs.unsettled_paths().await, vec!["a.txt"]);

            armed.store(true, Ordering::SeqCst);
            let target = s.commit(&[("a.txt", Some(b"three"))]).await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the re-placement completes");
            assert_eq!(
                unlinked.lock().take(),
                Some(Ok(())),
                "the UNLINK landed inside the apply"
            );
            let a = fs.lookup_at(&fs.served(), ROOT_INO, b"a.txt").await;
            assert!(absent(a), "a.txt stays deleted");
        },
    )
    .await;
}

/// A LOOKUP answered while an apply runs, after it placed a file's new content, finds the inode
/// the kernel holds but leaves its content alone, and the plan still invalidates it.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_during_an_apply_leaves_the_invalidation_of_a_held_inode() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let ino = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
        let found = Arc::new(AtomicU64::new(0));
        let fs = s.fs.clone();
        let during = found.clone();
        s.fs.set_swap_hook(running_once(SwapPhase::Placed, move || {
            let fs = fs.clone();
            let during = during.clone();
            Box::pin(async move {
                let entry = fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("lookup");
                during.store(entry.ino, Ordering::SeqCst);
            })
        }));
        let second = s.commit(&[("a.txt", Some(b"two"))]).await;
        let sink = Arc::new(RecordingSink::default());
        s.swap_to(&second, sink.clone()).await;

        assert_eq!(
            found.load(Ordering::SeqCst),
            ino,
            "the lookup ran mid-apply"
        );
        assert!(sink.has(&Call::Inode(ino)), "{:?}", sink.calls());
        assert_eq!(read_at(&s.fs, ino, 0, 1 << 16).await, b"two");
    })
    .await;
}

/// After an apply that failed once it wrote a file, a valid sync to a target holding the served
/// revision's content plans from what it applies, so the inode created in between serves that
/// content.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_after_a_failed_apply_plans_from_what_it_applies() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Placed));
        let first_target = s.commit(&[("a.txt", Some(b"two"))]).await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            first_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");
        let ino = s.lookup(ROOT_INO, "a.txt").await.expect("lookup");
        assert_eq!(
            read_at(&s.fs, ino, 0, 1 << 16).await,
            b"two",
            "the failed apply wrote it"
        );

        let second = s
            .commit(&[("a.txt", Some(b"one")), ("b.txt", Some(b"bee"))])
            .await;
        let sink = Arc::new(RecordingSink::default());
        s.swap_to(&second, sink.clone()).await;

        assert!(sink.has(&Call::Inode(ino)), "{:?}", sink.calls());
        assert_eq!(read_at(&s.fs, ino, 0, 1 << 16).await, b"one");
    })
    .await;
}

/// A view change publishes a fresh listing cache and clears the one it replaces, so no listing
/// built under the old view is served under the new one.
#[tokio::test]
async fn a_view_change_publishes_a_fresh_listing_cache() {
    swapping(
        &[("seen.txt", b"s"), ("hidden/a.txt", b"one")],
        async |s: Swapping| {
            s.lookup(ROOT_INO, "hidden").await.expect("hidden");
            let before = s.fs.served();
            assert!(
                !before.listings.is_empty(),
                "the lookup cached the root listing"
            );
            swap(
                &s.fs,
                &s.operation,
                s.viewed(&["hidden/"]),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("view change");
            let after = s.fs.served();
            assert!(
                !Arc::ptr_eq(&before.listings, &after.listings),
                "a fresh cache"
            );
            assert!(before.listings.is_empty(), "the old cache is cleared");
            assert_eq!(s.lookup(ROOT_INO, "hidden").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

/// A root listing build under the old view, held after its log mark until a view change has
/// ended, caches into the listing cache of the State it was handed, which the view change
/// replaced: the served cache holds no listing with the old view's flags, and a lookup after
/// it answers by the new view.
#[tokio::test(flavor = "multi_thread")]
async fn a_build_held_across_a_view_change_caches_nothing_the_new_view_serves() {
    swapping(
        &[("seen.txt", b"s"), ("hidden/a.txt", b"one")],
        async |s: Swapping| {
            let walking = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let armed = Arc::new(AtomicBool::new(true));
            s.fs.set_walk_hook(Arc::new({
                let (walking, release, armed) = (walking.clone(), release.clone(), armed.clone());
                move |node| {
                    let (walking, release) = (walking.clone(), release.clone());
                    let held = node == lore_revision::node::ROOT_NODE
                        && armed.swap(false, Ordering::SeqCst);
                    Box::pin(async move {
                        if held {
                            walking.notify_one();
                            release.notified().await;
                        }
                    })
                }
            }));
            s.fs.served()
                .listings
                .remove(lore_revision::node::ROOT_NODE);
            let held = {
                let fs = s.fs.clone();
                lore_base::lore_spawn!(async move {
                    let served = fs.served();
                    let _ = fs.lookup_at(&served, ROOT_INO, b"hidden").await;
                })
            };
            walking.notified().await;
            tokio::time::timeout(
                Duration::from_secs(30),
                swap(
                    &s.fs,
                    &s.operation,
                    s.viewed(&["hidden/"]),
                    s.first.clone(),
                    Arc::new(RecordingSink::default()),
                ),
            )
            .await
            .expect("the view change waits on no build")
            .expect("view change");
            release.notify_one();
            held.await.expect("the held lookup");

            assert!(
                s.fs.served()
                    .listings
                    .get(lore_revision::node::ROOT_NODE)
                    .is_none(),
                "the build under the old view cached its listing for the new one"
            );
            assert_eq!(s.lookup(ROOT_INO, "hidden").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

/// A view change whose apply is dropped midway leaves the served repository in place, so the
/// next swap makes the view change whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_view_change_dropped_mid_apply_keeps_the_served_repository() {
    swapping(
        &[("seen.txt", b"s"), ("hidden/a.txt", b"one")],
        async |s: Swapping| {
            s.lookup(ROOT_INO, "hidden").await.expect("hidden");
            let reached = Arc::new(tokio::sync::Notify::new());
            s.fs.set_swap_hook(pending_once(reached.clone()));
            let second = s.commit(&[("seen.txt", Some(b"changed"))]).await;
            let viewed = s.viewed(&["hidden/"]);
            let before = s.fs.served().repository.clone();
            let dropped = Arc::new(RecordingSink::default());
            tokio::select! {
                _ = swap(
                    &s.fs,
                    &s.operation,
                    viewed.clone(),
                    second.clone(),
                    dropped.clone(),) => panic!("the swap pends"),
                () = reached.notified() => {}
            }
            assert!(!s.fs.is_swapping());
            assert!(
                Arc::ptr_eq(&s.fs.served().repository, &before),
                "the served repository stays"
            );

            let sink = Arc::new(RecordingSink::default());
            swap(&s.fs, &s.operation, viewed, second.clone(), sink.clone())
                .await
                .expect("the view change again");
            assert!(sink.has(&Call::Inode(ROOT_INO)), "{:?}", sink.calls());
            assert_eq!(s.lookup(ROOT_INO, "hidden").await, Err(Errno::ENOENT));
            assert_eq!(served_bytes(&s, ROOT_INO, "seen.txt").await, b"changed");
        },
    )
    .await;
}

/// A swap after an apply that failed re-places what that apply changed, never a path the user
/// deleted locally, which the failed target's own deletion did not touch.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_after_a_failed_apply_leaves_a_local_deletion_deleted() {
    swapping(
        &[("a.txt", b"one"), ("deleted.txt", b"d")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-unsettled-");
            std::fs::create_dir_all(external.path().join("write")).expect("write/");
            fuse_fixture::test_file_write(&external.path().join("deleted"), b"deleted.txt\n");
            let fs = FuseFilesystem::new_writable(
                s.repository(),
                s.first.clone(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            fs.set_swap_hook(failing_once(SwapPhase::Placed));
            let failed_target = s
                .commit(&[("a.txt", Some(b"two")), ("deleted.txt", None)])
                .await;
            let failed = swap(
                &fs,
                &operation,
                s.repository(),
                failed_target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");

            swap(
                &fs,
                &operation,
                s.repository(),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap back");
            let deleted = fs
                .state()
                .find_subnode(
                    s.repository(),
                    lore_revision::node::ROOT_NODE,
                    hash_string("deleted.txt"),
                )
                .await;
            assert!(
                deleted.is_err_and(|error| error.is_node_not_found()),
                "the local deletion stays"
            );
            let a = fs
                .lookup(&req(), ROOT_INO, b"a.txt")
                .await
                .expect("a.txt")
                .ino;
            assert_eq!(read_at(&fs, a, 0, 1 << 16).await, b"one");
        },
    )
    .await;
}

/// A WRITE landing while a re-placement's exact fill-in of a directory an earlier apply left
/// unsettled gives one of its files the target's content, between the fill-in's check that the
/// file is clean and its rewrite, keeps the written content.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_landing_while_a_re_placement_refills_a_file_keeps_the_write() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let external = TempDir::new("lore-fuse-swap-refill-race-");
        std::fs::create_dir_all(external.path().join("write")).expect("write/");
        let fs = FuseFilesystem::new_writable(
            s.repository(),
            s.first.clone(),
            attr_context(),
            external.path(),
        )
        .await
        .expect("writable provider");
        let operation = InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
            true,
        );
        // The first apply fails once it placed; the next one's fill-in meets a WRITE of x.txt.
        let x = Arc::new(AtomicU64::new(0));
        let wrote = Arc::new(Mutex::new(None));
        // x.txt's node as the WRITE left it: the content it was copied up from.
        let left = Arc::new(Mutex::new(None));
        let fail = failing_once(SwapPhase::Placed);
        let refill = {
            let (writer, x, result) = (Arc::downgrade(&fs), x.clone(), wrote.clone());
            let (repository, written_node) = (s.repository(), left.clone());
            once_at(SwapPhase::Refill, move || {
                Box::pin(async move {
                    let writer = writer.upgrade().expect("the provider");
                    let ino = x.load(Ordering::SeqCst);
                    let written = writer.write(&req(), ino, 0, 0, b"LOCAL", 0).await;
                    *result.lock() = Some(written);
                    let node = writer
                        .state()
                        .find_node(repository, "new/x.txt")
                        .await
                        .expect("new/x.txt");
                    *written_node.lock() = Some((node.mode, node.size, node.address));
                })
            })
        };
        fs.set_swap_hook(Arc::new(move |phase| {
            let (failing, refilling) = (fail(phase), refill(phase));
            Box::pin(async move { failing.await && refilling.await })
        }));
        let failed_target = s.commit(&[("new/x.txt", Some(b"x"))]).await;
        let failed = swap(
            &fs,
            &operation,
            s.repository(),
            failed_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");
        assert_eq!(fs.unsettled_paths().await, vec!["new"]);
        let new = fs
            .lookup_at(&fs.served(), ROOT_INO, b"new")
            .await
            .expect("new")
            .ino;
        let ino = fs
            .lookup_at(&fs.served(), new, b"x.txt")
            .await
            .expect("x.txt")
            .ino;
        assert_eq!(read_at(&fs, ino, 0, 64).await, b"x");
        x.store(ino, Ordering::SeqCst);

        let target = s.commit(&[("new/x.txt", Some(b"target x"))]).await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the re-placement completes");

        assert_eq!(
            wrote.lock().take(),
            Some(Ok(5)),
            "the WRITE landed in the fill-in's window before its rewrite of x.txt"
        );
        assert_eq!(read_at(&fs, ino, 0, 64).await, b"LOCAL");
        // The provider serves a local file from its host file whatever its node holds, so the
        // node itself shows whether the fill-in rewrote it.
        let node = fs
            .state()
            .find_node(s.repository(), "new/x.txt")
            .await
            .expect("new/x.txt");
        assert!(node.is_dirty(), "x.txt stays local");
        assert_eq!(
            Some((node.mode, node.size, node.address)),
            left.lock().take(),
            "the fill-in left x.txt's node as the WRITE left it"
        );
        let found = fs
            .lookup_at(&fs.served(), new, b"x.txt")
            .await
            .expect("x.txt")
            .ino;
        assert_eq!(read_at(&fs, found, 0, 64).await, b"LOCAL");
        assert_eq!(
            std::fs::read(external.path().join("write/new/x.txt")).expect("write/new/x.txt"),
            b"LOCAL"
        );
        assert!(fs.unsettled_paths().await.is_empty());
    }))
    .await;
}

/// An UNLINK landing while a re-placement's exact fill-in of a directory an earlier apply left
/// unsettled gives one of its files the target's content, between the fill-in's check of the
/// file and its rewrite, leaves the re-placement complete, and the file the target adds placed
/// as where nothing stood.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_landing_while_a_re_placement_refills_a_file_places_it_again() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let external = TempDir::new("lore-fuse-swap-refill-unlink-");
        std::fs::create_dir_all(external.path().join("write")).expect("write/");
        let fs = FuseFilesystem::new_writable(
            s.repository(),
            s.first.clone(),
            attr_context(),
            external.path(),
        )
        .await
        .expect("writable provider");
        let operation = InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
            true,
        );
        // The first apply fails once it placed; the next one's fill-in meets an UNLINK of x.txt.
        let new = Arc::new(AtomicU64::new(0));
        // Whether the UNLINK went through, and whether it listed x.txt's deletion.
        let unlinked = Arc::new(Mutex::new(None));
        let fail = failing_once(SwapPhase::Placed);
        let refill = {
            let (remover, new, result) = (Arc::downgrade(&fs), new.clone(), unlinked.clone());
            let deletions = external.path().join(DELETION_LIST);
            once_at(SwapPhase::Refill, move || {
                Box::pin(async move {
                    let remover = remover.upgrade().expect("the provider");
                    let removed = remover
                        .unlink(&req(), new.load(Ordering::SeqCst), b"x.txt")
                        .await;
                    let listed = std::fs::read_to_string(&deletions)
                        .is_ok_and(|text| text.lines().any(|line| line == "new/x.txt"));
                    *result.lock() = Some((removed.is_ok(), listed));
                })
            })
        };
        fs.set_swap_hook(Arc::new(move |phase| {
            let (failing, refilling) = (fail(phase), refill(phase));
            Box::pin(async move { failing.await && refilling.await })
        }));
        let failed_target = s.commit(&[("new/x.txt", Some(b"x"))]).await;
        let failed = swap(
            &fs,
            &operation,
            s.repository(),
            failed_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");
        assert_eq!(fs.unsettled_paths().await, vec!["new"]);
        let dir = fs
            .lookup_at(&fs.served(), ROOT_INO, b"new")
            .await
            .expect("new")
            .ino;
        new.store(dir, Ordering::SeqCst);

        let target = s.commit(&[("new/x.txt", Some(b"target x"))]).await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the re-placement completes");

        assert_eq!(
            unlinked.lock().take(),
            Some((true, true)),
            "the UNLINK landed in the fill-in's window before its rewrite of x.txt, and listed it"
        );
        let found = fs
            .lookup_at(&fs.served(), dir, b"x.txt")
            .await
            .expect("x.txt, which the target adds")
            .ino;
        assert_eq!(read_at(&fs, found, 0, 64).await, b"target x");
        let node = fs
            .state()
            .find_node(s.repository(), "new/x.txt")
            .await
            .expect("new/x.txt");
        assert!(
            !node.is_dirty(),
            "x.txt holds the target's content, not a local change"
        );
        assert!(fs.unsettled_paths().await.is_empty());
        assert!(
            !listed_deletions(&external).contains(&"new/x.txt".to_string()),
            "the UNLINK's deletion is dropped once the target's x.txt is served"
        );
    }))
    .await;
}

/// The lines of the deletion list in the instance's external directory `external`.
fn listed_deletions(external: &TempDir) -> Vec<String> {
    std::fs::read_to_string(external.path().join(DELETION_LIST))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// A hook that fails an apply the first `times` times it reaches `at`, and passes after.
fn failing_times(at: SwapPhase, times: usize) -> SwapHook {
    let left = Arc::new(AtomicUsize::new(times));
    Arc::new(move |phase| {
        let left = left.clone();
        Box::pin(async move {
            phase != at
                || left
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_err()
        })
    })
}

/// Two applies of `new/x.txt` that fail once they placed, the second refilling the `x.txt` the
/// first copied, so `new` is unsettled and `new/x.txt` is too as a path changed in place; then
/// `removal`, an UNLINK of `x.txt` run either before the third apply (`between`) or in that
/// apply's refill of it, against content the third apply changes again. The third apply
/// completes, and `x.txt`, which its replacement keeps, stays deleted.
async fn a_kept_path_deleted_around_a_third_apply_stays_deleted(s: &Swapping, between: bool) {
    let external = TempDir::new("lore-fuse-swap-kept-deletion-");
    std::fs::create_dir_all(external.path().join("write")).expect("write/");
    let fs = FuseFilesystem::new_writable(
        s.repository(),
        s.first.clone(),
        attr_context(),
        external.path(),
    )
    .await
    .expect("writable provider");
    let operation = InstanceOperationImpl::new(
        StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
        true,
    );
    let new = Arc::new(AtomicU64::new(0));
    let armed = Arc::new(AtomicBool::new(false));
    let unlinked = Arc::new(Mutex::new(None));
    let fail = failing_times(SwapPhase::Placed, 2);
    let refill: SwapHook = {
        let (remover, new, armed, result) = (
            Arc::downgrade(&fs),
            new.clone(),
            armed.clone(),
            unlinked.clone(),
        );
        Arc::new(move |phase| {
            let fire = phase == SwapPhase::Refill && armed.swap(false, Ordering::SeqCst);
            let (remover, new, result) = (remover.clone(), new.clone(), result.clone());
            Box::pin(async move {
                if fire {
                    let remover = remover.upgrade().expect("the provider");
                    let removed = remover
                        .unlink(&req(), new.load(Ordering::SeqCst), b"x.txt")
                        .await;
                    *result.lock() = Some(removed.is_ok());
                }
                true
            })
        })
    };
    fs.set_swap_hook(Arc::new(move |phase| {
        let (failing, refilling) = (fail(phase), refill(phase));
        Box::pin(async move { failing.await && refilling.await })
    }));
    for content in [b"x".as_slice(), b"target x"] {
        let target = s.commit(&[("new/x.txt", Some(content))]).await;
        let failed = swap(
            &fs,
            &operation,
            s.repository(),
            target,
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");
    }
    assert_eq!(fs.unsettled_paths().await, vec!["new", "new/x.txt"]);
    let dir = fs
        .lookup_at(&fs.served(), ROOT_INO, b"new")
        .await
        .expect("new")
        .ino;
    new.store(dir, Ordering::SeqCst);
    if between {
        fs.unlink(&req(), dir, b"x.txt")
            .await
            .expect("UNLINK between the applies");
    } else {
        armed.store(true, Ordering::SeqCst);
    }

    let target = s.commit(&[("new/x.txt", Some(b"third x"))]).await;
    swap(
        &fs,
        &operation,
        s.repository(),
        target,
        Arc::new(RecordingSink::default()),
    )
    .await
    .expect("the third apply completes");

    if !between {
        assert_eq!(
            unlinked.lock().take(),
            Some(true),
            "the UNLINK landed in the fill-in's window before its rewrite of x.txt"
        );
    }
    assert!(
        absent(fs.lookup_at(&fs.served(), dir, b"x.txt").await),
        "x.txt stays deleted"
    );
    assert!(fs.unsettled_paths().await.is_empty());
    assert!(
        listed_deletions(&external).contains(&"new/x.txt".to_string()),
        "x.txt's deletion stays listed: {:?}",
        listed_deletions(&external)
    );
}

/// A path an earlier apply changed in place, deleted locally before the next apply, stays
/// deleted when that apply's fill-in of its directory finds nothing there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the refill brings back a path its replacement keeps deleted; the fix must also invalidate entries below a directory a failed apply replaced whole (deferred)"]
async fn a_path_changed_in_place_and_deleted_before_the_next_apply_stays_deleted() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        a_kept_path_deleted_around_a_third_apply_stays_deleted(&s, true).await;
    }))
    .await;
}

/// A path an earlier apply changed in place, deleted locally while the next apply's fill-in
/// changes it again, stays deleted.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the refill brings back a path its replacement keeps deleted; the fix must also invalidate entries below a directory a failed apply replaced whole (deferred)"]
async fn a_path_changed_in_place_and_deleted_during_the_next_refill_stays_deleted() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        a_kept_path_deleted_around_a_third_apply_stays_deleted(&s, false).await;
    }))
    .await;
}

/// A swap back to the served revision after an apply that failed needs no State of the failed
/// target: it re-places what that apply changed from the served revision alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_back_after_a_failed_apply_needs_no_state_of_the_failed_target() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let repository = s.repository();
        let stored = s.commit(&[("a.txt", Some(b"two"))]).await;
        let written = stored
            .find_node(repository.clone(), "a.txt")
            .await
            .expect("a.txt");
        // The failed target: the served revision with a.txt rewritten, held in memory alone.
        let unstored = State::deserialize(repository.clone(), s.first.revision())
            .await
            .expect("served revision");
        let node = unstored
            .find_subnode(
                repository.clone(),
                lore_revision::node::ROOT_NODE,
                hash_string("a.txt"),
            )
            .await
            .expect("a.txt");
        unstored
            .node_rewrite(
                &repository,
                node,
                &Content {
                    mode: written.mode,
                    size: written.size,
                    address: written.address,
                },
                Rewrite::Plain,
            )
            .await
            .expect("rewrite a.txt");
        unstored.set_revision(Hash::from([7u8; 32]));

        s.fs.set_swap_hook(failing_once(SwapPhase::Placed));
        let failed = swap(
            &s.fs,
            &s.operation,
            repository.clone(),
            unstored.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");
        let ino = s.lookup(ROOT_INO, "a.txt").await.expect("a.txt");
        assert_eq!(read_at(&s.fs, ino, 0, 1 << 16).await, b"two");

        swap(
            &s.fs,
            &s.operation,
            repository,
            s.first.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the swap back needs only the served revision");
        assert_eq!(served_bytes(&s, ROOT_INO, "a.txt").await, b"one");
    })
    .await;
}

/// A copy the target adds that fails before it attaches is added by the next swap to the target.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_failed_before_attaching_is_added_by_the_next_swap() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Copy));
        let second = s.commit(&[("new.txt", Some(b"new"))]).await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the copy");
        assert_eq!(s.lookup(ROOT_INO, "new.txt").await, Err(Errno::ENOENT));

        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        assert_eq!(served_bytes(&s, ROOT_INO, "new.txt").await, b"new");
    })
    .await;
}

/// A file a dropped apply detached is orphaned when the apply ends, and the next swap to a
/// target without it tells the kernel it went.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_dropped_apply_detached_is_orphaned_and_invalidated_by_the_next_swap() {
    swapping(
        &[("a.txt", b"one"), ("gone.txt", b"g")],
        async |s: Swapping| {
            s.lookup(ROOT_INO, "gone.txt")
                .await
                .expect("lookup gone.txt");
            let indexed = || {
                s.fs.inodes()
                    .find_child(ROOT_INO, hash_string("gone.txt"), "gone.txt")
            };
            let reached = Arc::new(tokio::sync::Notify::new());
            s.fs.set_swap_hook(pending_once(reached.clone()));
            let second = s.commit(&[("gone.txt", None)]).await;
            let dropped = Arc::new(RecordingSink::default());
            tokio::select! {
                () = s.swap_to(&second, dropped.clone()) => panic!("the swap pends"),
                () = reached.notified() => {}
            }
            assert_eq!(indexed(), None, "the dropped apply's end orphans gone.txt");

            let sink = Arc::new(RecordingSink::default());
            s.swap_to(&second, sink.clone()).await;
            assert!(
                sink.has(&Call::Entry(ROOT_INO, b"gone.txt".to_vec())),
                "{:?}",
                sink.calls()
            );
            assert_eq!(indexed(), None);
            assert_eq!(s.lookup(ROOT_INO, "gone.txt").await, Err(Errno::ENOENT));
        },
    )
    .await;
}

/// A file a failed apply rewrote is re-placed from what the served State holds, so its inode
/// is invalidated even where it took the target's content before the plan ran.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_failed_apply_rewrote_is_invalidated_whatever_its_inode_holds() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let served_a = s
            .first
            .find_node(s.repository(), "a.txt")
            .await
            .expect("a.txt");
        let ino = Arc::new(AtomicU64::new(0));
        let applies = Arc::new(AtomicU64::new(0));
        let rewrote = Arc::new(AtomicBool::new(false));
        let fs = s.fs.clone();
        let (hook_ino, hook_applies, hook_rewrote) =
            (ino.clone(), applies.clone(), rewrote.clone());
        s.fs.set_swap_hook(Arc::new(move |phase| {
            let fs = fs.clone();
            let (ino, applies, rewrote) =
                (hook_ino.clone(), hook_applies.clone(), hook_rewrote.clone());
            Box::pin(async move {
                if phase != SwapPhase::Placed {
                    return true;
                }
                if applies.fetch_add(1, Ordering::SeqCst) == 0 {
                    return false;
                }
                // What a lookup answered between the apply's end and the plan would leave.
                if let Some(mut entry) = fs.inodes().get_mut(ino.load(Ordering::SeqCst))
                    && let lore_revision::fs::fuse::inode::InodeKind::File { address, size, .. } =
                        &mut entry.kind
                {
                    *address = served_a.address;
                    *size = served_a.size;
                    rewrote.store(true, Ordering::SeqCst);
                }
                true
            })
        }));
        let first_target = s.commit(&[("a.txt", Some(b"two"))]).await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            first_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err());
        let a = s.lookup(ROOT_INO, "a.txt").await.expect("a.txt");
        ino.store(a, Ordering::SeqCst);
        assert_eq!(
            read_at(&s.fs, a, 0, 1 << 16).await,
            b"two",
            "the kernel caches two"
        );

        let second = s.commit(&[("a.txt", Some(b"one"))]).await;
        let sink = Arc::new(RecordingSink::default());
        s.swap_to(&second, sink.clone()).await;
        assert!(
            rewrote.load(Ordering::SeqCst),
            "the inode held the target's content when the plan ran"
        );
        assert!(sink.has(&Call::Inode(a)), "{:?}", sink.calls());
    })
    .await;
}

/// A lookup that read a file, then waited on the store while a swap replaced its content, does
/// not revive the orphan holding what it read.
#[tokio::test(flavor = "multi_thread")]
async fn an_orphan_is_not_revived_across_a_swap_that_replaced_its_content() {
    swapping(
        &[("a.txt", b"one"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let repository = s.repository();
            let gate = Arc::new(GatedStore {
                inner: repository.try_mutable_store_arc().expect("write token"),
                armed: AtomicBool::new(false),
                reached: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let gated = Arc::new(
                RepositoryContext::new(
                    fuse_fixture::default_repository_creation_args(
                        repository.immutable_store(),
                        gate.clone(),
                    )
                    .with_path(&s.instance.path)
                    .with_id(repository.id)
                    .with_instance_id(repository.instance_id),
                )
                .without_command_locks()
                .with_write_token(s.instance.write_token.share()),
            );
            let fs = FuseFilesystem::new(gated.clone(), s.first.clone(), attr_context())
                .await
                .expect("provider");
            fs.set_swap_hook(failing_once(SwapPhase::Placed));
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            fs.lookup(&req(), ROOT_INO, b"a.txt").await.expect("a.txt");
            let rewritten = s.commit(&[("a.txt", Some(b"zzz"))]).await;
            let failed = swap(
                &fs,
                &operation,
                gated.clone(),
                rewritten.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed.is_err(), "the failed apply orphans a.txt's inode");
            swap(
                &fs,
                &operation,
                gated.clone(),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap back");
            fs.lookup(&req(), ROOT_INO, b"keep.txt")
                .await
                .expect("keep.txt");

            gate.armed.store(true, Ordering::SeqCst);
            let lookup = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    fs.lookup(&req(), ROOT_INO, b"a.txt")
                        .await
                        .expect("lookup")
                        .ino
                })
            };
            gate.reached.notified().await;
            let changed = s.commit(&[("a.txt", Some(b"changed"))]).await;
            swap(
                &fs,
                &operation,
                gated.clone(),
                changed.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("change");
            gate.release.notify_one();
            let ino = lookup.await.expect("the lookup completes");
            assert_eq!(read_at(&fs, ino, 0, 1 << 16).await, b"changed");
        },
    )
    .await;
}

/// The State reads of a swap that adds a chain of directories, over the reads for the diff's
/// changes.
async fn reads_to_add_a_chain(depth: usize) -> u64 {
    let reads = Arc::new(AtomicU64::new(0));
    let out = reads.clone();
    swapping(&[("a.txt", b"one")], async move |s: Swapping| {
        let mut path = String::from("deep");
        let mut edits: Vec<(String, Vec<u8>)> = Vec::new();
        for level in 0..depth {
            edits.push((format!("{path}/f{level}.txt"), vec![b'x'; 3]));
            path = format!("{path}/d{level}");
        }
        let borrowed: Vec<(&str, Option<&[u8]>)> = edits
            .iter()
            .map(|(path, contents)| (path.as_str(), Some(contents.as_slice())))
            .collect();
        let second = s.commit(&borrowed).await;
        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        assert_eq!(
            served_bytes(
                &s,
                s.lookup(ROOT_INO, "deep").await.expect("deep"),
                "f0.txt"
            )
            .await,
            b"xxx"
        );
        out.store(s.fs.last_swap_reads().await, Ordering::SeqCst);
    })
    .await;
    reads.load(Ordering::SeqCst)
}

/// Adding a tree reads the State in proportion to the tree: nothing below a directory the swap
/// copied is placed again.
#[tokio::test(flavor = "multi_thread")]
async fn adding_a_deep_tree_reads_in_proportion_to_it() {
    let shallow = reads_to_add_a_chain(8).await;
    let deep = reads_to_add_a_chain(32).await;
    assert!(
        deep <= shallow * 5,
        "{deep} State reads for four times the {shallow} of depth 8"
    );
}

/// An immutable store whose reads, once `stall` is armed, wait for `release` the first time, and
/// fail while `failing` is set.
struct GatedImmutable {
    inner: Arc<dyn ImmutableStore>,
    stall: AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
    failing: AtomicBool,
}

impl GatedImmutable {
    fn new(inner: Arc<dyn ImmutableStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            stall: AtomicBool::new(false),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            failing: AtomicBool::new(false),
        })
    }

    async fn gate(&self) -> Result<(), StoreError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(lore_base::error::Disconnected.into());
        }
        if self.stall.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

impl std::fmt::Debug for GatedImmutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GatedImmutable")
    }
}

#[async_trait]
impl ImmutableStore for GatedImmutable {
    /// Joins the claim of the store this one wraps.
    async fn hold_for_command(
        self: Arc<Self>,
    ) -> Result<Option<lore_storage::local::store_lock::StoreGuard>, StoreError> {
        self.inner.clone().hold_for_command().await
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.gate().await?;
        self.inner.clone().get(partition, address).await
    }

    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError> {
        self.gate().await?;
        self.inner.clone().get_metadata(partition, address).await
    }

    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError> {
        self.gate().await?;
        self.inner
            .clone()
            .query(partition, addresses, results)
            .await
    }

    async fn put(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        fragment: Fragment,
        payload: Option<Bytes>,
        force: bool,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .put(partition, address, fragment, payload, force)
            .await
    }

    async fn obliterate(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
        stats: Arc<StoreObliterateStats>,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .obliterate(partition, address, stats)
            .await
    }

    async fn evict(
        self: Arc<Self>,
        max_capacity: usize,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<usize, StoreError> {
        self.inner
            .clone()
            .evict(max_capacity, sync_data, sink)
            .await
    }

    async fn compact(
        self: Arc<Self>,
        max_size: usize,
        at: Option<usize>,
        sync_data: bool,
        sink: Option<lore_storage::gc_event::GcEventSinkRef>,
    ) -> Result<Option<usize>, StoreError> {
        self.inner
            .clone()
            .compact(max_size, at, sync_data, sink)
            .await
    }

    async fn compact_resume_at(self: Arc<Self>) -> Option<usize> {
        self.inner.clone().compact_resume_at().await
    }

    fn max_query_batch(&self) -> Option<usize> {
        self.inner.max_query_batch()
    }

    async fn flush(self: Arc<Self>, options: FlushOptions) -> Result<(), StoreError> {
        self.inner.clone().flush(options).await
    }

    async fn verify(self: Arc<Self>, heal: bool) -> Result<(), StoreError> {
        self.inner.clone().verify(heal).await
    }

    async fn copy(
        self: Arc<Self>,
        source_partition: Partition,
        source_address: Address,
        destination_partition: Partition,
        destination_context: Context,
        behavior: lore_storage::immutable_store::CopyBehavior,
    ) -> Result<(), StoreError> {
        self.inner
            .clone()
            .copy(
                source_partition,
                source_address,
                destination_partition,
                destination_context,
                behavior,
            )
            .await
    }
}

impl Swapping {
    /// The repository, reading its immutable data through `store`.
    fn through(&self, store: Arc<GatedImmutable>) -> Arc<RepositoryContext> {
        let repository = self.repository();
        Arc::new(
            RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    store,
                    repository
                        .try_mutable_store_arc()
                        .expect("fixture holds a write token"),
                )
                .with_path(&self.instance.path)
                .with_id(repository.id)
                .with_instance_id(repository.instance_id),
            )
            .without_command_locks()
            .with_write_token(self.instance.write_token.share()),
        )
    }
}

/// The node id and name of each entry of one READDIRPLUS page of the root, dots left out.
async fn root_page(fs: &Arc<FuseFilesystem>) -> Vec<(u64, Vec<u8>)> {
    let fh = fs.opendir(&req(), ROOT_INO).expect("opendir").fh;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdirplus(&req(), ROOT_INO, fh, 0, &mut writer)
        .await
        .expect("readdirplus");
    let used = writer.finish();
    fs.releasedir(ROOT_INO, fh);
    fuse_test_util::dirents(&buffer[..used], true)
        .filter(|dirent| dirent.name != b"." && dirent.name != b"..")
        .map(|dirent| (dirent.nodeid, dirent.name.to_vec()))
        .collect()
}

/// A listing whose build waits on the store while a whole swap runs is cached, and the next
/// READDIRPLUS serves the swap's content through it.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_built_across_a_whole_swap_is_cached() {
    swapping(
        &[("a.txt", b"one"), ("b.txt", b"bee")],
        async |s: Swapping| {
            let store = GatedImmutable::new(s.repository().immutable_store());
            let gated = s.through(store.clone());
            let fs = FuseFilesystem::new(gated.clone(), s.first.clone(), attr_context())
                .await
                .expect("provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
                true,
            );
            // Block 0 loads now; the build loads its name table, and waits there.
            fs.state()
                .node(gated.clone(), lore_revision::node::ROOT_NODE)
                .await
                .expect("root");
            store.stall.store(true, Ordering::SeqCst);
            let page = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move { root_page(&fs).await })
            };
            store.reached.notified().await;
            let second = s.commit(&[("a.txt", Some(b"two!"))]).await;
            swap(
                &fs,
                &operation,
                gated.clone(),
                second.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");
            store.release.notify_one();
            page.await.expect("the page completes");

            assert!(
                fs.served()
                    .listings
                    .get(lore_revision::node::ROOT_NODE)
                    .is_some(),
                "a listing built across the swap is not cached"
            );
            let entries = root_page(&fs).await;
            let (a, _) = entries
                .iter()
                .find(|(_, name)| name == b"a.txt")
                .expect("a.txt listed");
            assert_ne!(*a, 0, "{entries:?}");
            assert_eq!(fs.getattr(&req(), *a).await.expect("attr").attr.size, 4);
            assert_eq!(read_at(&fs, *a, 0, 1 << 16).await, b"two!");
        },
    )
    .await;
}

/// A read error while resolving a path an earlier failed apply changed fails the swap, and the
/// retry serves the target.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_error_resolving_a_replaced_path_fails_the_swap() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Placed));
        let rewritten = s.commit(&[("a.txt", Some(b"two"))]).await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            rewritten.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err(), "the hook fails the apply after it placed");

        let store = GatedImmutable::new(s.repository().immutable_store());
        let failing = s.through(store.clone());
        let cold = State::deserialize(failing.clone(), s.first.revision())
            .await
            .expect("the served revision");
        store.failing.store(true, Ordering::SeqCst);
        let sink = Arc::new(RecordingSink::default());
        let swap_back = || {
            swap(
                &s.fs,
                &s.operation,
                failing.clone(),
                cold.clone(),
                sink.clone(),
            )
        };
        assert!(swap_back().await.is_err(), "the read error fails the swap");
        assert_eq!(served_bytes(&s, ROOT_INO, "a.txt").await, b"two");

        store.failing.store(false, Ordering::SeqCst);
        swap_back().await.expect("the retry");
        assert_eq!(served_bytes(&s, ROOT_INO, "a.txt").await, b"one");
    })
    .await;
}

/// A directory a failed apply copied whole is made to match the next target: a child neither
/// the served revision nor the target holds goes, and the kernel is told.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_a_failed_apply_copied_keeps_no_child_neither_revision_holds() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Placed));
        let first_target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            first_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed.is_err());
        let d = s
            .lookup(ROOT_INO, "d")
            .await
            .expect("the failed apply copied d");
        s.lookup(d, "b.txt").await.expect("and d/b.txt");

        let second = s
            .commit(&[
                ("d/b.txt", None),
                ("d/c.txt", None),
                ("d/x.txt", Some(b"ex")),
            ])
            .await;
        let sink = Arc::new(RecordingSink::default());
        s.swap_to(&second, sink.clone()).await;
        let d = s.lookup(ROOT_INO, "d").await.expect("d");
        assert_eq!(served_bytes(&s, d, "x.txt").await, b"ex");
        assert_eq!(s.lookup(d, "b.txt").await, Err(Errno::ENOENT));
        assert_eq!(s.lookup(d, "c.txt").await, Err(Errno::ENOENT));
        assert!(
            sink.has(&Call::Entry(d, b"b.txt".to_vec())),
            "{:?}",
            sink.calls()
        );
    })
    .await;
}

/// A swap back to the served revision makes a directory that failed applies left behind match
/// it, though no change names what is below: a file holding a failed target's content takes the
/// served content, and a file where the served revision holds a directory becomes that
/// directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_back_makes_a_directory_failed_applies_left_match_the_served_revision() {
    swapping(
        &[
            ("a.txt", b"one"),
            ("d/b.txt", b"bee"),
            ("d/k/z.txt", b"zed"),
        ],
        async |s: Swapping| {
            let failures = Arc::new(AtomicU64::new(0));
            let failing = failures.clone();
            s.fs.set_swap_hook(Arc::new(move |phase| {
                let failing = failing.clone();
                Box::pin(async move {
                    phase != SwapPhase::Placed || failing.fetch_add(1, Ordering::SeqCst) >= 2
                })
            }));
            let failed_swap = async |target: &Arc<State>| {
                swap(
                    &s.fs,
                    &s.operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                )
                .await
            };
            // A fixture edit, outside any repository write.
            #[allow(clippy::disallowed_methods)]
            std::fs::remove_dir_all(s.instance.path.join("d")).expect("remove d");
            let without_d = s.commit(&[]).await;
            assert!(failed_swap(&without_d).await.is_err(), "the hook fails it");
            assert_eq!(s.lookup(ROOT_INO, "d").await, Err(Errno::ENOENT));
            let other_d = s
                .commit(&[("d/b.txt", Some(b"other")), ("d/k", Some(b"a file"))])
                .await;
            assert!(failed_swap(&other_d).await.is_err(), "the hook fails it");
            assert_eq!(s.fs.unsettled_paths().await, vec!["d"]);
            let d = s
                .lookup(ROOT_INO, "d")
                .await
                .expect("the failed apply copied d");
            let b = s.lookup(d, "b.txt").await.expect("d/b.txt");
            assert_eq!(read_at(&s.fs, b, 0, 64).await, b"other");

            let sink = Arc::new(RecordingSink::default());
            swap(
                &s.fs,
                &s.operation,
                s.repository(),
                s.first.clone(),
                sink.clone(),
            )
            .await
            .expect("the swap back");

            let d = s.lookup(ROOT_INO, "d").await.expect("d");
            assert_eq!(served_bytes(&s, d, "b.txt").await, b"bee");
            assert!(sink.has(&Call::Inode(b)), "{:?}", sink.calls());
            let k = s.lookup(d, "k").await.expect("d/k");
            assert_eq!(served_bytes(&s, k, "z.txt").await, b"zed");
        },
    )
    .await;
}

/// Holds `after`'s root listing in `before`'s cache too, then lets `before` release it last:
/// `after`'s pinned charge comes back to zero only where the two caches share it.
fn charge_left_after_the_old_cache_releases_last(
    before: &lore_revision::fs::fuse::served::Served,
    after: &lore_revision::fs::fuse::served::Served,
) -> usize {
    let root = after
        .listings
        .get(lore_revision::node::ROOT_NODE)
        .expect("the root listing is cached");
    before.listings.insert(lore_revision::node::ROOT_NODE, root);
    after.listings.remove(lore_revision::node::ROOT_NODE);
    before.listings.clear();
    after.listings.pinned_bytes()
}

/// The listing cache a view change publishes shares the pinned-block charge of the one it
/// replaces.
#[tokio::test]
async fn a_view_change_shares_the_pinned_charge_with_the_cache_it_replaces() {
    swapping(
        &[("a.txt", b"one"), ("hidden/b.txt", b"bee")],
        async |s: Swapping| {
            s.lookup(ROOT_INO, "a.txt").await.expect("a.txt");
            let before = s.fs.served();
            swap(
                &s.fs,
                &s.operation,
                s.viewed(&["hidden/"]),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("view change");
            s.lookup(ROOT_INO, "a.txt").await.expect("a.txt");
            let after = s.fs.served();
            assert!(!Arc::ptr_eq(&before.listings, &after.listings));
            assert_eq!(
                charge_left_after_the_old_cache_releases_last(&before, &after),
                0
            );
        },
    )
    .await;
}

/// The listing cache the first publish onto an empty mount installs shares the pinned-block
/// charge of the empty mount's.
#[tokio::test]
async fn a_first_publish_shares_the_pinned_charge_with_the_empty_mount_cache() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let empty = FuseFilesystem::new(s.repository(), State::new(), attr_context())
            .await
            .expect("provider construction");
        let operation = InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(empty.clone())),
            true,
        );
        let before = empty.served();
        swap(
            &empty,
            &operation,
            s.repository(),
            s.first.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("first publish");
        empty
            .lookup(&req(), ROOT_INO, b"a.txt")
            .await
            .expect("a.txt");
        let after = empty.served();
        assert!(!Arc::ptr_eq(&before.listings, &after.listings));
        assert_eq!(
            charge_left_after_the_old_cache_releases_last(&before, &after),
            0
        );
    })
    .await;
}

/// A copy that fails once its top node attached published it whole: the nodes below went in
/// before it was published (H22). The next swap to the target leaves it so.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_failed_after_its_attach_published_its_directory_whole() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Attached));
        let second = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed_at(&failed, SwapPhase::Attached), "{failed:?}");
        let d = s
            .lookup(ROOT_INO, "d")
            .await
            .expect("the attach of d landed");
        assert_eq!(served_bytes(&s, d, "b.txt").await, b"bee");
        assert_eq!(served_bytes(&s, d, "c.txt").await, b"sea");

        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        let d = s.lookup(ROOT_INO, "d").await.expect("d");
        assert_eq!(served_bytes(&s, d, "b.txt").await, b"bee");
        assert_eq!(served_bytes(&s, d, "c.txt").await, b"sea");
    })
    .await;
}

/// A writable provider over `s`'s first revision, with its operation; `name` names its external
/// directory.
async fn writable(
    s: &Swapping,
    name: &str,
) -> (TempDir, Arc<FuseFilesystem>, InstanceOperationImpl) {
    let external = TempDir::new(name);
    std::fs::create_dir_all(external.path().join("write")).expect("write/");
    let fs = FuseFilesystem::new_writable(
        s.repository(),
        s.first.clone(),
        attr_context(),
        external.path(),
    )
    .await
    .expect("writable provider");
    let operation = InstanceOperationImpl::new(
        StaticDispatchInstanceOperation::Fuse(FuseOperation::new(fs.clone())),
        true,
    );
    (external, fs, operation)
}

/// A MKDIR of `d` landing as a swap copies the target's `d`, before the copy looks its name up,
/// keeps the user's directory, and the target's files go into it: the copy finds the name taken
/// and fills the directory in, so no file of the target's `d` is left out of the State that now
/// stands at the target (H23).
#[tokio::test(flavor = "multi_thread")]
async fn a_mkdir_landing_in_a_copy_of_its_name_takes_the_targets_files() {
    a_mkdir_landing_in_a_copy_at(SwapPhase::Copy).await;
}

/// [`a_mkdir_landing_in_a_copy_of_its_name_takes_the_targets_files`], the MKDIR landing while the
/// copy fills, after its lookup: its publish finds the name taken, it frees what it built, and
/// fills the user's directory in.
#[tokio::test(flavor = "multi_thread")]
async fn a_mkdir_landing_while_a_copy_of_its_name_fills_takes_the_targets_files() {
    a_mkdir_landing_in_a_copy_at(SwapPhase::Fill).await;
}

async fn a_mkdir_landing_in_a_copy_at(at: SwapPhase) {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-mkdir-race-").await;
        let (maker, made) = (Arc::downgrade(&fs), Arc::new(AtomicU64::new(0)));
        let mkdir = once_at(at, {
            let made = made.clone();
            move || {
                Box::pin(async move {
                    let entry = maker
                        .upgrade()
                        .expect("the provider")
                        .mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("MKDIR during the copy");
                    made.store(entry.ino, Ordering::SeqCst);
                })
            }
        });
        // The changes the passes place below `d` after the copy: each finds its file there, as
        // the fill-in left it, and gives it the target's content in place.
        let modified = Arc::new(AtomicUsize::new(0));
        let counted = modified.clone();
        fs.set_swap_hook(Arc::new(move |phase| {
            if phase == SwapPhase::Modify {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            mkdir(phase)
        }));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        for (name, content) in [("b.txt", b"bee"), ("c.txt", b"sea")] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_ne!(ino, 0, "{name} stands in the user's d");
            assert_eq!(read_at(&fs, ino, 0, 64).await, content);
            let node = fs
                .state()
                .find_node(s.repository(), &format!("d/{name}"))
                .await
                .expect("the file's node");
            assert!(!node.is_dirty(), "{name} holds the target's content");
        }
        assert_eq!(d, made.load(Ordering::SeqCst), "d is the user's directory");
        let node = fs
            .state()
            .find_node(s.repository(), "d")
            .await
            .expect("d's node");
        assert!(
            !node.is_dirty(),
            "d, holding the target's files alone, settles as the target's"
        );
        assert_eq!(
            modified.load(Ordering::SeqCst),
            2,
            "the passes found both files the fill-in placed"
        );
    })
    .await;
}

/// A RENAME of the user's directory onto `d` landing before a swap's copy of `d` looks its name
/// up: the copy merges the target's files into the user's directory, whose own file stays, and
/// the directory, holding it, stays dirty.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_landing_in_a_copy_of_its_name_keeps_the_users_files() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-rename-race-").await;
        let u = fs
            .mkdir(&req(), ROOT_INO, b"u", libc::S_IFDIR | 0o755, 0o022)
            .await
            .expect("mkdir")
            .ino;
        let x = fs
            .create(
                &req(),
                u,
                b"x.txt",
                libc::S_IFREG | 0o644,
                0o022,
                (libc::O_CREAT | libc::O_RDWR) as u32,
            )
            .await
            .expect("create")
            .entry
            .ino;
        fs.write(&req(), x, 0, 0, b"x", 0).await.expect("write");
        let renamer = Arc::downgrade(&fs);
        fs.set_swap_hook(once_at(SwapPhase::Copy, move || {
            Box::pin(async move {
                renamer
                    .upgrade()
                    .expect("the provider")
                    .rename(&req(), ROOT_INO, b"u", ROOT_INO, b"d", 0)
                    .await
                    .expect("RENAME before the copy");
            })
        }));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        assert_eq!(d, u, "d is the user's directory");
        for (name, content) in [
            ("x.txt", &b"x"[..]),
            ("b.txt", &b"bee"[..]),
            ("c.txt", &b"sea"[..]),
        ] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_eq!(read_at(&fs, ino, 0, 64).await, content, "{name}");
        }
        let node = fs
            .state()
            .find_node(s.repository(), "d")
            .await
            .expect("d's node");
        assert!(node.is_dirty(), "d holds the user's file");
    })
    .await;
}

/// A hook that runs `work` once, the `n`th time an apply reaches `at`, counting from 1, and
/// passes.
fn nth_at(
    at: SwapPhase,
    n: usize,
    work: impl FnOnce() -> futures::future::BoxFuture<'static, ()> + Send + 'static,
) -> SwapHook {
    let reached = Arc::new(AtomicUsize::new(0));
    let work = Arc::new(Mutex::new(Some(work)));
    Arc::new(move |phase| {
        let work = (phase == at && reached.fetch_add(1, Ordering::SeqCst) + 1 == n)
            .then(|| work.lock().take())
            .flatten();
        Box::pin(async move {
            if let Some(work) = work {
                work().await;
            }
            true
        })
    })
}

/// A hook running `first`, then `second`, at each phase, passing where both pass.
fn both(first: SwapHook, second: SwapHook) -> SwapHook {
    Arc::new(move |phase| {
        let (first, second) = (first(phase), second(phase));
        Box::pin(async move { first.await && second.await })
    })
}

/// A MKDIR of `d` landing before a swap's copy of `d` looks its name up, and a RENAME of it to
/// `e` before the copy reads the directory it found: the name is free again, and the copy tries
/// it again and lands whole, the user's directory left at `e` as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_renamed_before_a_copy_reads_it_leaves_the_name_to_the_copy() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-held-rename-").await;
        let made = Arc::new(AtomicU64::new(0));
        let mkdir = {
            let (maker, made) = (Arc::downgrade(&fs), made.clone());
            once_at(SwapPhase::Copy, move || {
                Box::pin(async move {
                    let entry = maker
                        .upgrade()
                        .expect("the provider")
                        .mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("MKDIR before the copy");
                    made.store(entry.ino, Ordering::SeqCst);
                })
            })
        };
        let rename = {
            let renamer = Arc::downgrade(&fs);
            once_at(SwapPhase::Held, move || {
                Box::pin(async move {
                    renamer
                        .upgrade()
                        .expect("the provider")
                        .rename(&req(), ROOT_INO, b"d", ROOT_INO, b"e", 0)
                        .await
                        .expect("RENAME before the copy reads the holder");
                })
            })
        };
        fs.set_swap_hook(both(mkdir, rename));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the swap completes");

        let made = made.load(Ordering::SeqCst);
        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        assert_ne!(d, made, "d is the copy, not the user's directory");
        for (name, content) in [("b.txt", b"bee"), ("c.txt", b"sea")] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_eq!(read_at(&fs, ino, 0, 64).await, content);
        }
        let e = fs
            .lookup_at(&fs.served(), ROOT_INO, b"e")
            .await
            .expect("e")
            .ino;
        assert_eq!(e, made, "e is the user's directory");
        assert!(
            absent(fs.lookup_at(&fs.served(), e, b"b.txt").await),
            "nothing of the target's lands in e"
        );
    })
    .await;
}

/// A RENAME of the user's directory onto `d` landing while a swap's copy of `d` fills: the copy's
/// publish finds the name held and merges the target's files into the user's directory, whose
/// own file stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_landing_while_a_copy_of_its_name_fills_keeps_the_users_files() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-fill-rename-").await;
        let u = fs
            .mkdir(&req(), ROOT_INO, b"u", libc::S_IFDIR | 0o755, 0o022)
            .await
            .expect("mkdir")
            .ino;
        let x = fs
            .create(
                &req(),
                u,
                b"x.txt",
                libc::S_IFREG | 0o644,
                0o022,
                (libc::O_CREAT | libc::O_RDWR) as u32,
            )
            .await
            .expect("create")
            .entry
            .ino;
        fs.write(&req(), x, 0, 0, b"x", 0).await.expect("write");
        let renamer = Arc::downgrade(&fs);
        fs.set_swap_hook(once_at(SwapPhase::Fill, move || {
            Box::pin(async move {
                renamer
                    .upgrade()
                    .expect("the provider")
                    .rename(&req(), ROOT_INO, b"u", ROOT_INO, b"d", 0)
                    .await
                    .expect("RENAME while the copy fills");
            })
        }));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        assert_eq!(d, u, "d is the user's directory");
        for (name, content) in [
            ("x.txt", &b"x"[..]),
            ("b.txt", &b"bee"[..]),
            ("c.txt", &b"sea"[..]),
        ] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_eq!(read_at(&fs, ino, 0, 64).await, content, "{name}");
        }
    })
    .await;
}

/// [`an_rmdir_landing_between_two_restores_below_one_directory_leaves_it_deleted`]'s sync, the
/// RMDIR of `a` too where `whole`, failing after it placed; then, after the user makes `a` again
/// where `remade`, the next sync to the target. Answers the provider, with the directory it
/// serves from.
async fn restores_raced_by_rmdirs_then_replayed(
    s: &Swapping,
    (whole, remade): (bool, bool),
) -> (TempDir, Arc<FuseFilesystem>) {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    let (external, fs, operation) = writable(s, "lore-fuse-swap-restore-replay-").await;
    let served = fs.served();
    let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
    for (dir, file) in [("b", "x.txt"), ("c", "y.txt")] {
        let ino = fs
            .lookup_at(&served, a, dir.as_bytes())
            .await
            .expect("dir")
            .ino;
        fs.unlink(&req(), ino, file.as_bytes())
            .await
            .expect("unlink");
        fs.rmdir(&req(), a, dir.as_bytes()).await.expect("rmdir");
    }
    drop(served);
    let remove = {
        let remover = Arc::downgrade(&fs);
        once_at(SwapPhase::Copy, move || {
            Box::pin(async move {
                let remover = remover.upgrade().expect("the provider");
                remover.rmdir(&req(), a, b"b").await.expect("RMDIR of a/b");
                if whole {
                    remover
                        .rmdir(&req(), ROOT_INO, b"a")
                        .await
                        .expect("RMDIR of a");
                }
            })
        })
    };
    fs.set_swap_hook(both(remove, failing_once(SwapPhase::Placed)));
    let target = s
        .commit(&[("a/b/x.txt", Some(b"x2")), ("a/c/y.txt", Some(b"y2"))])
        .await;
    for completes in [false, true] {
        if completes && remade {
            fs.mkdir(&req(), ROOT_INO, b"a", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("the user makes a again");
        }
        let synced = swap_with(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
            HoldOptions::default(),
        )
        .await;
        assert!(
            if completes {
                synced.is_ok()
            } else {
                failed_at(&synced, SwapPhase::Placed)
            },
            "{synced:?}"
        );
    }
    (external, fs)
}

/// The RMDIRs of a restored directory and its parent landing before the copy below them, in a
/// sync that then fails: the next sync restores neither, nor the sibling the failed sync found
/// its parent gone for.
#[tokio::test(flavor = "multi_thread")]
async fn rmdirs_of_restored_directories_in_a_failed_sync_are_not_replayed() {
    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/c/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs) = restores_raced_by_rmdirs_then_replayed(&s, (true, false)).await;
            assert!(
                absent(fs.lookup_at(&fs.served(), ROOT_INO, b"a").await),
                "a stays deleted"
            );
        },
    ))
    .await;
}

/// The RMDIR of a restored directory landing before the copy below it, in a sync that then
/// fails: the next sync leaves it deleted, and restores the sibling the user did not remove.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_restored_directory_in_a_failed_sync_is_not_replayed() {
    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/c/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs) = restores_raced_by_rmdirs_then_replayed(&s, (false, false)).await;
            let a = fs
                .lookup_at(&fs.served(), ROOT_INO, b"a")
                .await
                .expect("a")
                .ino;
            assert!(
                absent(fs.lookup_at(&fs.served(), a, b"b").await),
                "a/b stays deleted"
            );
            let c = fs.lookup_at(&fs.served(), a, b"c").await.expect("c").ino;
            let y = fs
                .lookup_at(&fs.served(), c, b"y.txt")
                .await
                .expect("y")
                .ino;
            assert_eq!(read_at(&fs, y, 0, 64).await, b"y2");
        },
    ))
    .await;
}

/// [`rmdirs_of_restored_directories_in_a_failed_sync_are_not_replayed`], the user making `a`
/// again before the next sync: that sync restores neither `a/b` nor `a/c` into it.
#[tokio::test(flavor = "multi_thread")]
async fn restored_directories_a_failed_sync_saw_removed_stay_removed_below_a_remade_parent() {
    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/c/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs) = restores_raced_by_rmdirs_then_replayed(&s, (true, true)).await;
            let a = fs
                .lookup_at(&fs.served(), ROOT_INO, b"a")
                .await
                .expect("the user's a")
                .ino;
            for name in ["b", "c"] {
                assert!(
                    absent(fs.lookup_at(&fs.served(), a, name.as_bytes()).await),
                    "a/{name} stays removed"
                );
            }
        },
    ))
    .await;
}

/// RMDIRs of both directories a restore attached, landing before the copy below them, in a sync
/// that then fails: the next sync restores neither.
#[tokio::test(flavor = "multi_thread")]
async fn rmdirs_of_a_restores_levels_before_its_copy_in_a_failed_sync_are_not_replayed() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-copy-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir b");
            fs.rmdir(&req(), ROOT_INO, b"a").await.expect("rmdir a");
            drop(served);
            let remove = {
                let remover = Arc::downgrade(&fs);
                once_at(SwapPhase::Copy, move || {
                    Box::pin(async move {
                        let remover = remover.upgrade().expect("the provider");
                        let a = remover
                            .lookup_at(&remover.served(), ROOT_INO, b"a")
                            .await
                            .expect("the restored a")
                            .ino;
                        remover.rmdir(&req(), a, b"b").await.expect("RMDIR of a/b");
                        remover
                            .rmdir(&req(), ROOT_INO, b"a")
                            .await
                            .expect("RMDIR of a");
                    })
                })
            };
            fs.set_swap_hook(both(remove, failing_once(SwapPhase::Placed)));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            for completes in [false, true] {
                let synced = swap_with(
                    &fs,
                    &operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                    HoldOptions::default(),
                )
                .await;
                assert!(
                    if completes {
                        synced.is_ok()
                    } else {
                        failed_at(&synced, SwapPhase::Placed)
                    },
                    "{synced:?}"
                );
            }
            assert!(
                absent(fs.lookup_at(&fs.served(), ROOT_INO, b"a").await),
                "a stays deleted"
            );
        },
    ))
    .await;
}

/// A copy of `d` that published but whose apply failed, so the next apply adds `d` back; the
/// user empties `d`, and an RMDIR of it lands as that apply refills it, which fails too: the
/// apply after leaves `d` removed, its record from the first apply flipped to kept.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_directory_an_earlier_failed_apply_added_is_not_replayed() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-unsettled-rmdir-").await;
        // The first apply fails at its copy's `Attached`, before it places; the second reaches
        // `Copy` again only in its refill of `d`, where the RMDIR lands, and fails as it placed.
        let remove = {
            let remover = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 2, move || {
                Box::pin(async move {
                    remover
                        .upgrade()
                        .expect("the provider")
                        .rmdir(&req(), ROOT_INO, b"d")
                        .await
                        .expect("RMDIR as the refill copies into d");
                })
            })
        };
        fs.set_swap_hook(both(
            failing_once(SwapPhase::Attached),
            both(remove, failing_once(SwapPhase::Placed)),
        ));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let run = async || {
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
        };
        let first = run().await;
        assert!(failed_at(&first, SwapPhase::Attached), "{first:?}");
        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        for name in ["b.txt", "c.txt"] {
            fs.unlink(&req(), d, name.as_bytes())
                .await
                .expect("the user empties d");
        }
        let second = run().await;
        assert!(failed_at(&second, SwapPhase::Placed), "{second:?}");
        run().await.expect("the third apply completes");
        assert!(
            absent(fs.lookup_at(&fs.served(), ROOT_INO, b"d").await),
            "d stays removed"
        );
    }))
    .await;
}

/// A hook that fails the apply the `n`th time it reaches `at`, counting from 1, and passes
/// otherwise.
fn failing_nth(at: SwapPhase, n: usize) -> SwapHook {
    let reached = Arc::new(AtomicUsize::new(0));
    Arc::new(move |phase| {
        let failing = phase == at && reached.fetch_add(1, Ordering::SeqCst) + 1 == n;
        Box::pin(async move { !failing })
    })
}

/// Whether `result` is the failure a hook made at `phase`.
fn failed_at<T, E: std::fmt::Debug>(result: &Result<T, E>, phase: SwapPhase) -> bool {
    let failure = format!("the swap hook failed the apply at {phase:?}");
    result
        .as_ref()
        .is_err_and(|error| format!("{error:?}").contains(&failure))
}

/// A copy of `d` holding `e/f.txt` that published but whose apply failed, so the next apply adds
/// `d` back; the user empties `e`, and RMDIRs of `e` and then `d` land as that apply refills `e`,
/// which fails too: the apply after leaves `d` removed, its record a level above the removed `e`
/// flipped to kept.
#[tokio::test(flavor = "multi_thread")]
async fn rmdirs_below_a_directory_an_earlier_failed_apply_added_are_not_replayed() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-unsettled-climb-").await;
        // The first apply fails at its copy's `Attached`, before it places; the second reaches
        // `Copy` again only in its refill of `e`, where the RMDIRs land, and fails as it placed.
        let remove = {
            let remover = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 2, move || {
                Box::pin(async move {
                    let remover = remover.upgrade().expect("the provider");
                    let d = remover
                        .lookup_at(&remover.served(), ROOT_INO, b"d")
                        .await
                        .expect("d")
                        .ino;
                    remover.rmdir(&req(), d, b"e").await.expect("RMDIR of d/e");
                    remover
                        .rmdir(&req(), ROOT_INO, b"d")
                        .await
                        .expect("RMDIR of d");
                })
            })
        };
        fs.set_swap_hook(both(
            failing_once(SwapPhase::Attached),
            both(remove, failing_once(SwapPhase::Placed)),
        ));
        let target = s.commit(&[("d/e/f.txt", Some(b"f"))]).await;
        let run = async || {
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
        };
        let first = run().await;
        assert!(failed_at(&first, SwapPhase::Attached), "{first:?}");
        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        let e = fs.lookup_at(&fs.served(), d, b"e").await.expect("e").ino;
        fs.unlink(&req(), e, b"f.txt")
            .await
            .expect("the user empties e");
        let second = run().await;
        assert!(failed_at(&second, SwapPhase::Placed), "{second:?}");
        run().await.expect("the third apply completes");
        assert!(
            absent(fs.lookup_at(&fs.served(), ROOT_INO, b"d").await),
            "d stays removed"
        );
    }))
    .await;
}

/// A MKDIR of `d` landing before a swap's copy of `d` looks its name up, an RMDIR of it before
/// the merge's first copy into it, and the apply failing at the merge's next copy: the next apply
/// leaves `d` removed, as the copy recorded `d` kept before it merged into the user's `d`.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_removed_while_a_copy_merges_into_it_in_a_failed_apply_is_not_replayed() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-merge-rmdir-").await;
        let mkdir = {
            let maker = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 1, move || {
                Box::pin(async move {
                    maker
                        .upgrade()
                        .expect("the provider")
                        .mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("MKDIR before the copy");
                })
            })
        };
        let rmdir = {
            let remover = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 2, move || {
                Box::pin(async move {
                    remover
                        .upgrade()
                        .expect("the provider")
                        .rmdir(&req(), ROOT_INO, b"d")
                        .await
                        .expect("RMDIR before the merge's first copy");
                })
            })
        };
        fs.set_swap_hook(both(mkdir, both(rmdir, failing_nth(SwapPhase::Copy, 3))));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let run = async || {
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
        };
        let first = run().await;
        assert!(failed_at(&first, SwapPhase::Copy), "{first:?}");
        run().await.expect("the next apply completes");
        assert!(
            absent(fs.lookup_at(&fs.served(), ROOT_INO, b"d").await),
            "d stays removed"
        );
    }))
    .await;
}

/// A MKDIR or a CREATE of `d` landing before a swap's copy of `d` looks its name up, in an apply
/// that then fails at `fail`: once the user removes their `d`, the next apply leaves it removed,
/// as a completed apply would have: the user's node held the name.
async fn a_users_holder_in_a_failed_apply_removed_before_the_replay(
    directory: bool,
    fail: (SwapPhase, usize),
) {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-holder-replay-").await;
        let make = {
            let maker = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 1, move || {
                Box::pin(async move {
                    let maker = maker.upgrade().expect("the provider");
                    if directory {
                        maker
                            .mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("MKDIR before the copy");
                    } else {
                        maker
                            .create(
                                &req(),
                                ROOT_INO,
                                b"d",
                                libc::S_IFREG | 0o644,
                                0o022,
                                (libc::O_CREAT | libc::O_RDWR) as u32,
                            )
                            .await
                            .expect("CREATE before the copy");
                    }
                })
            })
        };
        fs.set_swap_hook(both(make, failing_nth(fail.0, fail.1)));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let run = async || {
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
        };
        let first = run().await;
        assert!(failed_at(&first, fail.0), "{first:?}");
        if directory {
            let d = fs
                .lookup_at(&fs.served(), ROOT_INO, b"d")
                .await
                .expect("the user's d")
                .ino;
            for name in ["b.txt", "c.txt"] {
                if !absent(fs.lookup_at(&fs.served(), d, name.as_bytes()).await) {
                    fs.unlink(&req(), d, name.as_bytes()).await.expect("unlink");
                }
            }
            fs.rmdir(&req(), ROOT_INO, b"d")
                .await
                .expect("the user removes d");
        } else {
            fs.unlink(&req(), ROOT_INO, b"d")
                .await
                .expect("the user removes d");
        }
        run().await.expect("the next apply completes");
        assert!(
            absent(fs.lookup_at(&fs.served(), ROOT_INO, b"d").await),
            "d stays removed"
        );
    }))
    .await;
}

/// [`a_users_holder_in_a_failed_apply_removed_before_the_replay`]: a directory the copy merged
/// into, the apply failing as it placed.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_directory_a_copy_merged_into_is_not_replayed_once_removed() {
    a_users_holder_in_a_failed_apply_removed_before_the_replay(true, (SwapPhase::Placed, 1)).await;
}

/// [`a_users_holder_in_a_failed_apply_removed_before_the_replay`]: a directory the copy was
/// merging into, the apply failing at the merge's first copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_directory_a_copy_failed_to_merge_into_is_not_replayed_once_removed() {
    a_users_holder_in_a_failed_apply_removed_before_the_replay(true, (SwapPhase::Copy, 2)).await;
}

/// [`a_users_holder_in_a_failed_apply_removed_before_the_replay`]: a file, which wins over the
/// target's directory, the apply failing as it placed.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_file_holding_a_copys_name_is_not_replayed_once_removed() {
    a_users_holder_in_a_failed_apply_removed_before_the_replay(false, (SwapPhase::Placed, 1)).await;
}

/// A copy of `d` that failed while it filled, so the next apply adds `d` back; the user makes `d`
/// again, and an RMDIR of it lands as that apply refills it, which fails too: the apply after
/// leaves `d` removed, the first apply's record of `d` flipped to kept.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_directory_remade_over_a_failed_copy_is_not_replayed() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-remade-rmdir-").await;
        // The first apply fails at its copy's first `Fill`, before it places; the second reaches
        // `Copy` again only in its refill of the user's `d`, where the RMDIR lands, and fails as
        // it placed.
        let remove = {
            let remover = Arc::downgrade(&fs);
            nth_at(SwapPhase::Copy, 2, move || {
                Box::pin(async move {
                    remover
                        .upgrade()
                        .expect("the provider")
                        .rmdir(&req(), ROOT_INO, b"d")
                        .await
                        .expect("RMDIR as the refill copies into d");
                })
            })
        };
        fs.set_swap_hook(both(
            failing_once(SwapPhase::Fill),
            both(remove, failing_once(SwapPhase::Placed)),
        ));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let run = async || {
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
        };
        let first = run().await;
        assert!(failed_at(&first, SwapPhase::Fill), "{first:?}");
        fs.mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
            .await
            .expect("the user makes d");
        let second = run().await;
        assert!(failed_at(&second, SwapPhase::Placed), "{second:?}");
        run().await.expect("the third apply completes");
        assert!(
            absent(fs.lookup_at(&fs.served(), ROOT_INO, b"d").await),
            "d stays removed"
        );
    }))
    .await;
}

/// A CREATE of a file at the name of a directory a sync restores, landing before the restore
/// attaches it, in a sync that then fails: the user's file stays, and once the user removes it,
/// the next sync restores neither the directory nor the file the target changes below it.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_file_a_failed_restore_found_is_not_replayed_once_removed() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/k.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-file-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let create = {
                let creator = Arc::downgrade(&fs);
                nth_at(SwapPhase::Restore, 1, move || {
                    Box::pin(async move {
                        creator
                            .upgrade()
                            .expect("the provider")
                            .create(
                                &req(),
                                a,
                                b"b",
                                libc::S_IFREG | 0o644,
                                0o022,
                                (libc::O_CREAT | libc::O_RDWR) as u32,
                            )
                            .await
                            .expect("CREATE before the restore");
                    })
                })
            };
            fs.set_swap_hook(both(create, failing_once(SwapPhase::Placed)));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            let sync = async || {
                swap_with(
                    &fs,
                    &operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                    HoldOptions::default(),
                )
                .await
            };
            let failed = sync().await;
            assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");
            let node = fs
                .state()
                .find_node(s.repository(), "a/b")
                .await
                .expect("a/b");
            assert!(node.is_file(), "a/b is the user's file");
            fs.unlink(&req(), a, b"b")
                .await
                .expect("the user removes b");
            sync().await.expect("the next sync completes");

            assert!(
                absent(fs.lookup_at(&fs.served(), a, b"b").await),
                "b stays removed"
            );
        },
    ))
    .await;
}

/// A MKDIR of a directory a sync restores, landing before the restore attaches it, and an RMDIR
/// of it before the restore reads the directory it found: the name is free again, and the
/// restore attaches it again and restores the file below it.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_removed_before_a_restore_reads_it_leaves_the_name_to_the_restore() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/k.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-gone-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let made = Arc::new(AtomicU64::new(0));
            let mkdir = {
                let (maker, made) = (Arc::downgrade(&fs), made.clone());
                nth_at(SwapPhase::Restore, 1, move || {
                    Box::pin(async move {
                        let entry = maker
                            .upgrade()
                            .expect("the provider")
                            .mkdir(&req(), a, b"b", libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("MKDIR before the restore");
                        made.store(entry.ino, Ordering::SeqCst);
                    })
                })
            };
            let rmdir = {
                let remover = Arc::downgrade(&fs);
                once_at(SwapPhase::Held, move || {
                    Box::pin(async move {
                        remover
                            .upgrade()
                            .expect("the provider")
                            .rmdir(&req(), a, b"b")
                            .await
                            .expect("RMDIR before the restore reads the holder");
                    })
                })
            };
            fs.set_swap_hook(both(mkdir, rmdir));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the sync completes");

            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            assert_ne!(b, made.load(Ordering::SeqCst), "b is the restore's");
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2");
        },
    ))
    .await;
}

/// A MKDIR of `d` and of `u` landing before a swap's copy of `d` looks its name up, and a RENAME
/// of `d` to `u/d`, its name kept, before the copy reads the directory it found: the copy tries
/// the name again and lands whole, the user's directory left at `u/d`.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_moved_under_its_name_before_a_copy_reads_it_leaves_the_name_to_the_copy() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-held-move-").await;
        let mkdirs = {
            let maker = Arc::downgrade(&fs);
            once_at(SwapPhase::Copy, move || {
                Box::pin(async move {
                    let maker = maker.upgrade().expect("the provider");
                    for name in [&b"d"[..], b"u"] {
                        maker
                            .mkdir(&req(), ROOT_INO, name, libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("MKDIR before the copy");
                    }
                })
            })
        };
        let rename = {
            let renamer = Arc::downgrade(&fs);
            once_at(SwapPhase::Held, move || {
                Box::pin(async move {
                    let renamer = renamer.upgrade().expect("the provider");
                    let u = renamer
                        .lookup_at(&renamer.served(), ROOT_INO, b"u")
                        .await
                        .expect("u")
                        .ino;
                    renamer
                        .rename(&req(), ROOT_INO, b"d", u, b"d", 0)
                        .await
                        .expect("RENAME before the copy reads the holder");
                })
            })
        };
        fs.set_swap_hook(both(mkdirs, rename));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the swap completes");

        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        for (name, content) in [("b.txt", b"bee"), ("c.txt", b"sea")] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_eq!(read_at(&fs, ino, 0, 64).await, content);
        }
        let u = fs
            .lookup_at(&fs.served(), ROOT_INO, b"u")
            .await
            .expect("u")
            .ino;
        let moved = fs.lookup_at(&fs.served(), u, b"d").await.expect("u/d").ino;
        assert!(
            absent(fs.lookup_at(&fs.served(), moved, b"b.txt").await),
            "nothing of the target's lands in u/d"
        );
    })
    .await;
}

/// A MKDIR of a directory a sync restores, and of a sibling `u`, landing before the restore
/// attaches it, and a RENAME of it into `u`, its name kept, before the restore reads the
/// directory it found: the restore attaches it again and restores the file below it there.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_moved_under_its_name_before_a_restore_reads_it_leaves_the_name_to_the_restore() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/k.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-move-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let mkdirs = {
                let maker = Arc::downgrade(&fs);
                nth_at(SwapPhase::Restore, 1, move || {
                    Box::pin(async move {
                        let maker = maker.upgrade().expect("the provider");
                        for name in [&b"b"[..], b"u"] {
                            maker
                                .mkdir(&req(), a, name, libc::S_IFDIR | 0o755, 0o022)
                                .await
                                .expect("MKDIR before the restore");
                        }
                    })
                })
            };
            let rename = {
                let renamer = Arc::downgrade(&fs);
                once_at(SwapPhase::Held, move || {
                    Box::pin(async move {
                        let renamer = renamer.upgrade().expect("the provider");
                        let u = renamer
                            .lookup_at(&renamer.served(), a, b"u")
                            .await
                            .expect("u")
                            .ino;
                        renamer
                            .rename(&req(), a, b"b", u, b"b", 0)
                            .await
                            .expect("RENAME before the restore reads the holder");
                    })
                })
            };
            fs.set_swap_hook(both(mkdirs, rename));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the sync completes");

            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2");
            let u = fs.lookup_at(&fs.served(), a, b"u").await.expect("u").ino;
            let moved = fs.lookup_at(&fs.served(), u, b"b").await.expect("u/b").ino;
            assert!(
                absent(fs.lookup_at(&fs.served(), moved, b"x.txt").await),
                "nothing restored lands in u/b"
            );
        },
    ))
    .await;
}

/// RMDIRs of the two directories a restore attached above a third, landing before it attaches
/// the third, in a sync that then fails: the next sync restores none of the three.
#[tokio::test(flavor = "multi_thread")]
async fn rmdirs_above_a_restore_in_a_failed_sync_are_not_replayed() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/c/x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-levels-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            let c = fs.lookup_at(&served, b, b"c").await.expect("c").ino;
            fs.unlink(&req(), c, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), b, b"c").await.expect("rmdir c");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir b");
            fs.rmdir(&req(), ROOT_INO, b"a").await.expect("rmdir a");
            drop(served);
            let remove = {
                let remover = Arc::downgrade(&fs);
                // Before the attach of a/b/c, the third the restore attaches.
                nth_at(SwapPhase::Restore, 3, move || {
                    Box::pin(async move {
                        let remover = remover.upgrade().expect("the provider");
                        let a = remover
                            .lookup_at(&remover.served(), ROOT_INO, b"a")
                            .await
                            .expect("the restored a")
                            .ino;
                        remover.rmdir(&req(), a, b"b").await.expect("RMDIR of a/b");
                        remover
                            .rmdir(&req(), ROOT_INO, b"a")
                            .await
                            .expect("RMDIR of a");
                    })
                })
            };
            fs.set_swap_hook(both(remove, failing_once(SwapPhase::Placed)));
            let target = s.commit(&[("a/b/c/x.txt", Some(b"x2"))]).await;
            for completes in [false, true] {
                let synced = swap_with(
                    &fs,
                    &operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                    HoldOptions::default(),
                )
                .await;
                assert!(
                    if completes {
                        synced.is_ok()
                    } else {
                        failed_at(&synced, SwapPhase::Placed)
                    },
                    "{synced:?}"
                );
            }
            assert!(
                absent(fs.lookup_at(&fs.served(), ROOT_INO, b"a").await),
                "a stays deleted"
            );
        },
    ))
    .await;
}

/// A MKDIR of a directory a sync restores, landing before the restore attaches it, and a RENAME
/// of it before the restore reads the directory it found: the name is free again, and the
/// restore attaches it again and restores the file below it there, the user's directory left
/// where it went.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_renamed_before_a_restore_reads_it_leaves_the_name_to_the_restore() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/k.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-held-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let made = Arc::new(AtomicU64::new(0));
            let mkdir = {
                let (maker, made) = (Arc::downgrade(&fs), made.clone());
                nth_at(SwapPhase::Restore, 1, move || {
                    Box::pin(async move {
                        let entry = maker
                            .upgrade()
                            .expect("the provider")
                            .mkdir(&req(), a, b"b", libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("MKDIR before the restore");
                        made.store(entry.ino, Ordering::SeqCst);
                    })
                })
            };
            let rename = {
                let renamer = Arc::downgrade(&fs);
                once_at(SwapPhase::Held, move || {
                    Box::pin(async move {
                        renamer
                            .upgrade()
                            .expect("the provider")
                            .rename(&req(), a, b"b", a, b"e", 0)
                            .await
                            .expect("RENAME before the restore reads the holder");
                    })
                })
            };
            fs.set_swap_hook(both(mkdir, rename));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the sync completes");

            let made = made.load(Ordering::SeqCst);
            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            assert_ne!(b, made, "b is the restore's, not the user's directory");
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2");
            let e = fs.lookup_at(&fs.served(), a, b"e").await.expect("e").ino;
            assert_eq!(e, made, "e is the user's directory");
            assert!(
                absent(fs.lookup_at(&fs.served(), e, b"x.txt").await),
                "nothing restored lands in e"
            );
        },
    ))
    .await;
}

/// A MKDIR of a directory a sync restores, landing before the restore attaches it, in a sync that
/// then fails: the restore takes the user's directory for the level, and once the user removes
/// it, the next sync leaves it removed, as a completed sync would have restored nothing there.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_directory_a_failed_restore_took_is_not_replayed_once_removed() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/k.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-holder-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let made = Arc::new(AtomicU64::new(0));
            let mkdir = {
                let (maker, made) = (Arc::downgrade(&fs), made.clone());
                nth_at(SwapPhase::Restore, 1, move || {
                    Box::pin(async move {
                        let entry = maker
                            .upgrade()
                            .expect("the provider")
                            .mkdir(&req(), a, b"b", libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("MKDIR before the restore");
                        made.store(entry.ino, Ordering::SeqCst);
                    })
                })
            };
            fs.set_swap_hook(both(mkdir, failing_once(SwapPhase::Placed)));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            let sync = async || {
                swap_with(
                    &fs,
                    &operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                    HoldOptions::default(),
                )
                .await
            };
            let failed = sync().await;
            assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");
            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            assert_eq!(
                b,
                made.load(Ordering::SeqCst),
                "the restore took the user's b"
            );
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2", "x is restored into it");
            fs.unlink(&req(), b, b"x.txt").await.expect("unlink");
            fs.rmdir(&req(), a, b"b").await.expect("the user removes b");
            sync().await.expect("the next sync completes");

            assert!(
                absent(fs.lookup_at(&fs.served(), a, b"b").await),
                "b stays removed"
            );
        },
    ))
    .await;
}

/// The Discarded slots of every block of `state` in memory that are not free: detached nodes,
/// and the slots of attaches and copies dropped before they published.
fn detached_slots(state: &State) -> usize {
    (0..state.block_count())
        .filter_map(|index| state.resident_block(index))
        .map(|block| {
            let reader = block.read();
            let data = reader.node_block();
            let discarded = data.node[..data.node_count as usize]
                .iter()
                .filter(|node| node.is_discarded())
                .count();
            discarded - data.node_unused_count as usize
        })
        .sum()
}

/// A copy failing while it fills, before it publishes, leaves nothing at its name: what it built
/// is freed. The next swap to the target copies it whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_failed_while_it_filled_is_added_by_the_next_swap() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        s.fs.set_swap_hook(failing_once(SwapPhase::Fill));
        let second = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        let detached = detached_slots(&s.fs.state());
        let failed = swap(
            &s.fs,
            &s.operation,
            s.repository(),
            second.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed_at(&failed, SwapPhase::Fill), "{failed:?}");
        assert_eq!(s.lookup(ROOT_INO, "d").await, Err(Errno::ENOENT));
        assert_eq!(
            detached_slots(&s.fs.state()),
            detached,
            "what the copy built is free, none of it left detached"
        );

        s.swap_to(&second, Arc::new(RecordingSink::default())).await;
        let d = s.lookup(ROOT_INO, "d").await.expect("d");
        assert_eq!(served_bytes(&s, d, "b.txt").await, b"bee");
        assert_eq!(served_bytes(&s, d, "c.txt").await, b"sea");
    })
    .await;
}

/// A CREATE of a file named `d` landing as a swap copies the target's directory `d` keeps the
/// user's file, and the swap completes with nothing placed below it.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_in_a_copy_of_its_name_keeps_the_users_file() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-create-race-").await;
        let creator = Arc::downgrade(&fs);
        fs.set_swap_hook(once_at(SwapPhase::Copy, move || {
            Box::pin(async move {
                creator
                    .upgrade()
                    .expect("the provider")
                    .create(
                        &req(),
                        ROOT_INO,
                        b"d",
                        libc::S_IFREG | 0o644,
                        0o022,
                        (libc::O_CREAT | libc::O_RDWR) as u32,
                    )
                    .await
                    .expect("CREATE during the copy");
            })
        }));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        assert_ne!(d, 0, "the user's d stands");
        let node = fs
            .state()
            .find_node(s.repository(), "d")
            .await
            .expect("d's node");
        assert!(node.is_file(), "d is the user's file");
    })
    .await;
}

/// A CREATE of a file `n.txt` landing as a swap copies the target's file `n.txt` keeps the
/// user's file and its content, and the swap completes.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_in_a_copy_of_its_file_keeps_the_users_file() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-create-file-race-").await;
        let creator = Arc::downgrade(&fs);
        fs.set_swap_hook(once_at(SwapPhase::Copy, move || {
            Box::pin(async move {
                let creator = creator.upgrade().expect("the provider");
                let n = creator
                    .create(
                        &req(),
                        ROOT_INO,
                        b"n.txt",
                        libc::S_IFREG | 0o644,
                        0o022,
                        (libc::O_CREAT | libc::O_RDWR) as u32,
                    )
                    .await
                    .expect("CREATE during the copy")
                    .entry
                    .ino;
                creator
                    .write(&req(), n, 0, 0, b"mine", 0)
                    .await
                    .expect("write");
            })
        }));
        let target = s.commit(&[("n.txt", Some(b"theirs"))]).await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let n = fs
            .lookup_at(&fs.served(), ROOT_INO, b"n.txt")
            .await
            .expect("n.txt")
            .ino;
        assert_eq!(
            read_at(&fs, n, 0, 64).await,
            b"mine",
            "the user's content stays"
        );
    })
    .await;
}

/// An UNLINK of the last file of a directory an earlier apply left unsettled, and an RMDIR of the
/// directory, landing as a re-placement refills the file: the refill finds the file gone and the
/// directory with it, the user's deletion, and the swap completes with the directory deleted
/// (H11).
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_landing_while_a_re_placement_refills_its_directory_leaves_it_deleted() {
    Box::pin(swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-refill-rmdir-").await;
        let new = Arc::new(AtomicU64::new(0));
        let removed = Arc::new(Mutex::new(None));
        let fail = failing_once(SwapPhase::Placed);
        let refill = {
            let (remover, new, removed) = (Arc::downgrade(&fs), new.clone(), removed.clone());
            once_at(SwapPhase::Refill, move || {
                Box::pin(async move {
                    let remover = remover.upgrade().expect("the provider");
                    let unlinked = remover
                        .unlink(&req(), new.load(Ordering::SeqCst), b"x.txt")
                        .await;
                    let rmdir = remover.rmdir(&req(), ROOT_INO, b"new").await;
                    *removed.lock() = Some((unlinked, rmdir));
                })
            })
        };
        fs.set_swap_hook(Arc::new(move |phase| {
            let (failing, refilling) = (fail(phase), refill(phase));
            Box::pin(async move { failing.await && refilling.await })
        }));
        let failed_target = s.commit(&[("new/x.txt", Some(b"x"))]).await;
        let failed = swap(
            &fs,
            &operation,
            s.repository(),
            failed_target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await;
        assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");
        assert_eq!(fs.unsettled_paths().await, vec!["new"]);
        let dir = fs
            .lookup_at(&fs.served(), ROOT_INO, b"new")
            .await
            .expect("new")
            .ino;
        new.store(dir, Ordering::SeqCst);

        let target = s.commit(&[("new/x.txt", Some(b"target x"))]).await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the re-placement completes");

        assert_eq!(
            removed.lock().take(),
            Some((Ok(()), Ok(()))),
            "the UNLINK and the RMDIR landed in the refill's window"
        );
        let found = fs.lookup_at(&fs.served(), ROOT_INO, b"new").await;
        assert!(absent(found), "new stays deleted");
        assert!(fs.unsettled_paths().await.is_empty());
    }))
    .await;
}

/// An RMDIR of a directory a sync restored, landing before the sync restores a sibling below
/// the directory's parent, which it removes too: the restore of the sibling finds its parent
/// gone, the user's deletion, and the sync completes with both deleted (H11). The sync keeps no
/// local change, so it restores the files the target changes that the user deleted.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_landing_between_two_restores_below_one_directory_leaves_it_deleted() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/c/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-rmdir-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            for (dir, file) in [("b", "x.txt"), ("c", "y.txt")] {
                let ino = fs
                    .lookup_at(&served, a, dir.as_bytes())
                    .await
                    .expect("dir")
                    .ino;
                fs.unlink(&req(), ino, file.as_bytes())
                    .await
                    .expect("unlink");
                fs.rmdir(&req(), a, dir.as_bytes()).await.expect("rmdir");
            }
            drop(served);
            let removed = Arc::new(Mutex::new(None));
            let remover = (Arc::downgrade(&fs), removed.clone());
            fs.set_swap_hook(once_at(SwapPhase::Copy, move || {
                Box::pin(async move {
                    let (remover, removed) = remover;
                    let remover = remover.upgrade().expect("the provider");
                    let b = remover.rmdir(&req(), a, b"b").await;
                    let a = remover.rmdir(&req(), ROOT_INO, b"a").await;
                    *removed.lock() = Some((b, a));
                })
            }));
            let target = s
                .commit(&[("a/b/x.txt", Some(b"x2")), ("a/c/y.txt", Some(b"y2"))])
                .await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the sync completes");

            assert_eq!(
                removed.lock().take(),
                Some((Ok(()), Ok(()))),
                "the RMDIRs landed before the copy of a/b/x.txt"
            );
            let found = fs.lookup_at(&fs.served(), ROOT_INO, b"a").await;
            assert!(absent(found), "a stays deleted");
        },
    ))
    .await;
}

/// An RMDIR of the directory a swap copies a directory into, landing while the copy fills, before
/// it publishes: the publish finds its parent gone, the user's deletion, the copy is freed, and
/// the swap completes with the directory deleted (H11).
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_copys_parent_landing_while_it_fills_leaves_the_parent_deleted() {
    Box::pin(swapping(
        &[("p/x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-fill-rmdir-").await;
            let p = fs
                .lookup_at(&fs.served(), ROOT_INO, b"p")
                .await
                .expect("p")
                .ino;
            fs.unlink(&req(), p, b"x.txt").await.expect("unlink");
            let removed = Arc::new(Mutex::new(None));
            let remover = (Arc::downgrade(&fs), removed.clone());
            fs.set_swap_hook(once_at(SwapPhase::Fill, move || {
                Box::pin(async move {
                    let (remover, removed) = remover;
                    let remover = remover.upgrade().expect("the provider");
                    *removed.lock() = Some(remover.rmdir(&req(), ROOT_INO, b"p").await);
                })
            }));
            let target = s
                .commit(&[("p/d/b.txt", Some(b"bee")), ("p/d/c.txt", Some(b"sea"))])
                .await;
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("the swap completes");

            assert_eq!(
                removed.lock().take(),
                Some(Ok(())),
                "the RMDIR landed in the copy's fill"
            );
            let found = fs.lookup_at(&fs.served(), ROOT_INO, b"p").await;
            assert!(absent(found), "p stays deleted");
        },
    ))
    .await;
}

/// A MKDIR of `d` landing before a swap's copy of `d` looks its name up, and an RMDIR of it
/// before the copy reads the directory it found: the name is free again, and the copy tries it
/// again and lands whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_holder_removed_before_a_copy_reads_it_leaves_the_name_to_the_copy() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let (_external, fs, operation) = writable(&s, "lore-fuse-swap-held-rmdir-").await;
        let mkdir = {
            let maker = Arc::downgrade(&fs);
            once_at(SwapPhase::Copy, move || {
                Box::pin(async move {
                    maker
                        .upgrade()
                        .expect("the provider")
                        .mkdir(&req(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("MKDIR before the copy");
                })
            })
        };
        let removed = Arc::new(Mutex::new(None));
        let rmdir = {
            let (remover, removed) = (Arc::downgrade(&fs), removed.clone());
            once_at(SwapPhase::Held, move || {
                Box::pin(async move {
                    let remover = remover.upgrade().expect("the provider");
                    *removed.lock() = Some(remover.rmdir(&req(), ROOT_INO, b"d").await);
                })
            })
        };
        fs.set_swap_hook(Arc::new(move |phase| {
            let (made, removing) = (mkdir(phase), rmdir(phase));
            Box::pin(async move { made.await && removing.await })
        }));
        let target = s
            .commit(&[("d/b.txt", Some(b"bee")), ("d/c.txt", Some(b"sea"))])
            .await;
        swap(
            &fs,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("the swap completes");

        assert_eq!(
            removed.lock().take(),
            Some(Ok(())),
            "the RMDIR landed before the copy read the holder"
        );
        let d = fs
            .lookup_at(&fs.served(), ROOT_INO, b"d")
            .await
            .expect("d")
            .ino;
        for (name, content) in [("b.txt", b"bee"), ("c.txt", b"sea")] {
            let ino = fs
                .lookup_at(&fs.served(), d, name.as_bytes())
                .await
                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"))
                .ino;
            assert_eq!(read_at(&fs, ino, 0, 64).await, content);
        }
    })
    .await;
}

/// A sync that keeps no local change restores a directory the user deleted to hold a file the
/// target changes in it, and then fails: the next sync keeps that file, with the target's content,
/// and brings back nothing else the user deleted below the directory, which the target does not
/// change.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sync_that_restored_a_directory_brings_back_only_what_the_target_changes() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/b/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-replay-y-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            for file in ["x.txt", "y.txt"] {
                fs.unlink(&req(), b, file.as_bytes()).await.expect("unlink");
            }
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            fs.set_swap_hook(failing_once(SwapPhase::Placed));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            for completes in [false, true] {
                let synced = swap_with(
                    &fs,
                    &operation,
                    s.repository(),
                    target.clone(),
                    Arc::new(RecordingSink::default()),
                    HoldOptions::default(),
                )
                .await;
                assert!(
                    if completes {
                        synced.is_ok()
                    } else {
                        failed_at(&synced, SwapPhase::Placed)
                    },
                    "{synced:?}"
                );
            }
            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2");
            assert!(
                absent(fs.lookup_at(&fs.served(), b, b"y.txt").await),
                "y.txt, which the target does not change, stays deleted"
            );
        },
    ))
    .await;
}

/// A sync that keeps no local change restores a directory the user deleted to hold a file the
/// target changes in it, and the directory stands dirty when the sync copies the file into it:
/// it holds only what the sync restores into it, which no settle may take for the target's
/// directory. That no moment leaves it published clean, which no phase of the sync can observe,
/// rests on the served attach publishing it dirty.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_a_sync_restores_is_published_dirty() {
    use lore_revision::fs::filesystem_provider::HoldOptions;
    use lore_revision::fs::fuse::swap::swap_with;

    Box::pin(swapping(
        &[("a/b/x.txt", b"x"), ("a/b/y.txt", b"y"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-restore-dirty-").await;
            let served = fs.served();
            let a = fs.lookup_at(&served, ROOT_INO, b"a").await.expect("a").ino;
            let b = fs.lookup_at(&served, a, b"b").await.expect("b").ino;
            for file in ["x.txt", "y.txt"] {
                fs.unlink(&req(), b, file.as_bytes()).await.expect("unlink");
            }
            fs.rmdir(&req(), a, b"b").await.expect("rmdir");
            drop(served);
            let restored = Arc::new(Mutex::new(None));
            let (reader, seen, repository) =
                (Arc::downgrade(&fs), restored.clone(), s.repository());
            fs.set_swap_hook(once_at(SwapPhase::Copy, move || {
                Box::pin(async move {
                    let state = reader.upgrade().expect("the provider").state();
                    let node = state.find_node(repository, "a/b").await.ok();
                    *seen.lock() = Some(node.map(|node| node.is_dirty()));
                })
            }));
            let target = s.commit(&[("a/b/x.txt", Some(b"x2"))]).await;
            swap_with(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
                HoldOptions::default(),
            )
            .await
            .expect("the sync completes");

            assert_eq!(
                restored.lock().take(),
                Some(Some(true)),
                "a/b stands dirty when the sync copies x.txt into it"
            );
            let b = fs.lookup_at(&fs.served(), a, b"b").await.expect("b").ino;
            let x = fs
                .lookup_at(&fs.served(), b, b"x.txt")
                .await
                .expect("x")
                .ino;
            assert_eq!(read_at(&fs, x, 0, 64).await, b"x2");
            assert!(
                absent(fs.lookup_at(&fs.served(), b, b"y.txt").await),
                "y.txt, which the target does not change, stays deleted"
            );
        },
    ))
    .await;
}

/// An RMDIR of the directory a copy fills, in an apply that then fails: the next swap to the
/// target, after the user made the directory again, adds nothing back below it, as the apply
/// that completed past the RMDIR would have left it.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_whose_parent_a_failed_apply_saw_removed_is_not_added_back() {
    Box::pin(swapping(
        &[("p/x.txt", b"x"), ("keep.txt", b"k")],
        async |s: Swapping| {
            let (_external, fs, operation) = writable(&s, "lore-fuse-swap-gone-replay-").await;
            let p = fs
                .lookup_at(&fs.served(), ROOT_INO, b"p")
                .await
                .expect("p")
                .ino;
            fs.unlink(&req(), p, b"x.txt").await.expect("unlink");
            let rmdir = {
                let remover = Arc::downgrade(&fs);
                once_at(SwapPhase::Fill, move || {
                    Box::pin(async move {
                        remover
                            .upgrade()
                            .expect("the provider")
                            .rmdir(&req(), ROOT_INO, b"p")
                            .await
                            .expect("RMDIR while the copy fills");
                    })
                })
            };
            let fail = failing_once(SwapPhase::Placed);
            fs.set_swap_hook(Arc::new(move |phase| {
                let (removing, failing) = (rmdir(phase), fail(phase));
                Box::pin(async move { removing.await && failing.await })
            }));
            let target = s
                .commit(&[("p/d/b.txt", Some(b"bee")), ("p/d/c.txt", Some(b"sea"))])
                .await;
            let failed = swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await;
            assert!(failed_at(&failed, SwapPhase::Placed), "{failed:?}");

            let p = fs
                .mkdir(&req(), ROOT_INO, b"p", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("the user makes p again")
                .ino;
            swap(
                &fs,
                &operation,
                s.repository(),
                target.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("the next swap completes");
            assert!(
                absent(fs.lookup_at(&fs.served(), p, b"d").await),
                "p/d, which the user's removal of p took, is not added back"
            );
        },
    ))
    .await;
}

/// The first publish of the zero revision onto a writable provider with no local changes serves
/// a State of the provider's own, taking new nodes from fresh blocks: a CREATE on the worker
/// attaches there in a fresh block, and the State the swap was given gains nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_publish_of_the_zero_revision_onto_a_writable_provider_serves_its_own_state() {
    swapping(&[("a.txt", b"one")], async |s: Swapping| {
        let external = TempDir::new("lore-fuse-swap-zero-publish-");
        let empty = FuseFilesystem::new_writable(
            s.repository(),
            State::new(),
            attr_context(),
            external.path(),
        )
        .await
        .expect("writable provider");
        let operation = InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(empty.clone())),
            true,
        );
        let before = empty.served().state.clone();
        let target = State::new();
        swap(
            &empty,
            &operation,
            s.repository(),
            target.clone(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .expect("swap");

        let served = empty.served().state.clone();
        assert!(
            !Arc::ptr_eq(&served, &before),
            "the swap published no State"
        );
        assert!(
            !Arc::ptr_eq(&served, &target),
            "the provider serves the State the swap was given"
        );
        assert!(
            served.attach_slot_resident(),
            "the served State takes no fresh blocks"
        );
        let blocks = served.block_count();
        let missing = empty.lookup(&req(), ROOT_INO, b"new.o").await;
        assert!(missing.map_or(true, |reply| reply.ino == 0), "new.o exists");
        let created = match empty
            .create_local(
                &req(),
                ROOT_INO,
                b"new.o",
                libc::S_IFREG | 0o644,
                0o022,
                (libc::O_CREAT | libc::O_RDWR) as u32,
            )
            .await
        {
            Ok(created) => created.entry.ino,
            Err(lore_fuse::reply::ReadFault::WouldFetch) => panic!("the CREATE deferred"),
            Err(lore_fuse::reply::ReadFault::Errno(errno)) => panic!("CREATE: {errno:?}"),
        };
        assert_ne!(created, 0);
        let new = hash_string("new.o");
        let node = served
            .find_subnode(s.repository(), lore_revision::node::ROOT_NODE, new)
            .await
            .expect("new.o in the served State");
        assert!(
            lore_revision::node::NodeBlock::index(node) >= blocks,
            "the node took a slot in block {} of the State's {blocks}",
            lore_revision::node::NodeBlock::index(node)
        );
        assert!(
            target
                .find_subnode(s.repository(), lore_revision::node::ROOT_NODE, new)
                .await
                .is_err(),
            "the State the swap was given gained new.o"
        );
    })
    .await;
}

/// The first publish onto a writable provider with local changes serves the revision with them
/// applied, the files under `write/` and the deletion list, and orphans the inodes the empty
/// mount made, whose nodes the published State does not hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_publish_onto_a_writable_provider_applies_its_local_changes_and_orphans_its_inodes()
{
    use lore_revision::fs::fuse::deletions::DeletionList;
    swapping(
        &[("a.txt", b"one"), ("gone.txt", b"g")],
        async |s: Swapping| {
            let external = TempDir::new("lore-fuse-swap-first-publish-local-");
            std::fs::create_dir_all(external.path().join("write")).expect("write/");
            fuse_fixture::test_file_write(&external.path().join("write/local.txt"), b"LOCAL");
            DeletionList::open(&external.path().join(DELETION_LIST))
                .expect("deletion list")
                .append(&path("gone.txt"))
                .expect("a deletion");
            let empty = FuseFilesystem::new_writable(
                s.repository(),
                State::new(),
                attr_context(),
                external.path(),
            )
            .await
            .expect("writable provider");
            let operation = InstanceOperationImpl::new(
                StaticDispatchInstanceOperation::Fuse(FuseOperation::new(empty.clone())),
                true,
            );
            let before = empty
                .lookup(&req(), ROOT_INO, b"local.txt")
                .await
                .expect("local.txt on the empty mount")
                .ino;
            assert_ne!(before, 0);

            swap(
                &empty,
                &operation,
                s.repository(),
                s.first.clone(),
                Arc::new(RecordingSink::default()),
            )
            .await
            .expect("swap");

            assert_eq!(
                empty
                    .inodes()
                    .find_child(ROOT_INO, hash_string("local.txt"), "local.txt"),
                None,
                "the empty mount's inode {before} is orphaned"
            );
            let local = empty
                .lookup(&req(), ROOT_INO, b"local.txt")
                .await
                .expect("local.txt")
                .ino;
            assert_ne!(local, 0, "write/ is applied");
            assert_eq!(read_at(&empty, local, 0, 64).await, b"LOCAL");
            let a = empty
                .lookup(&req(), ROOT_INO, b"a.txt")
                .await
                .expect("a.txt")
                .ino;
            assert_eq!(read_at(&empty, a, 0, 64).await, b"one");
            let gone = empty.lookup(&req(), ROOT_INO, b"gone.txt").await;
            assert!(
                gone.is_err() || gone.is_ok_and(|entry| entry.ino == 0),
                "the deletion list is applied"
            );
        },
    )
    .await;
}
