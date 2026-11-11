// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Writes through a real read-write mount and local content served from `write/`: what a
//! build does to its tree, done with ordinary system calls, then checked through the mount and
//! under `write/`.
// The tests write through the mount as a build does, outside any repository write.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Hash;
use lore_fuse::filesystem::EntryRequest;
use lore_fuse::filesystem::Fetch;
use lore_fuse::filesystem::Filesystem as _;
use lore_fuse::filesystem::RequestInfo;
use lore_fuse::init::InitConfig;
use lore_fuse::mount::MountAccess;
use lore_fuse::mount::MountOptions;
use lore_fuse::mount::unmount_path;
use lore_fuse::reply::Errno;
use lore_fuse::reply::ReadFault;
use lore_fuse::session::Session;
use lore_fuse::session::SessionConfig;
use lore_fuse::skip_without_fuse;
use lore_fuse::transport::device::DeviceTransport;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::node::Node;
use lore_revision::node::NodeFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::AttachOutcome;
use lore_revision::state::Content;
use lore_revision::state::Detach;
use lore_revision::state::Rewrite;
use lore_storage::hash::hash_string;

use crate::fs::fuse::bench_stats;
use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_test_util;
use crate::fs::fuse::holding_store;
use crate::fs::fuse::staged_state;

const MAX_READ: u32 = 1 << 20;
const BASE_TIME: u64 = 1_700_000_000_000;
const WORKERS: usize = 4;

/// Unmounts on drop, lazily if a plain unmount is refused, so a failing test leaves no mount.
struct UnmountGuard(PathBuf);

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if unmount_path(&self.0, false).is_err() {
            let _ = unmount_path(&self.0, true);
        }
    }
}

struct Mounted {
    // Field order is drop order: the session stops before the unmount.
    session: Arc<Session<FuseFilesystem, DeviceTransport>>,
    _guard: UnmountGuard,
    fs: Arc<FuseFilesystem>,
    root: PathBuf,
    _dir: TempDir,
}

impl Mounted {
    /// Unmounts and stops serving, as a service stop does.
    async fn unmount(self) {
        let root = self.root.clone();
        blocking(move || unmount_path(&root, false).expect("an idle mount unmounts")).await;
        tokio::time::timeout(Duration::from_secs(5), self.session.shutdown())
            .await
            .expect("shutdown completes");
    }
}

/// A writable fixture holding no write token: the committed revision and the instance's
/// external directory, which holds `write/` and the deletion list.
struct Instance {
    fixture: fuse_fixture::ReleasedFixture,
    external: TempDir,
}

impl Instance {
    async fn new(files: &[(&str, &[u8])]) -> Self {
        Self::with_revisions(&[files]).await
    }

    /// An instance at the last of `revisions`, one commit each: a tree of two or more records
    /// file metadata.
    async fn with_revisions(revisions: &[&[(&str, &[u8])]]) -> Self {
        Self {
            fixture: fuse_fixture::released_revisions(revisions).await,
            external: TempDir::new("lore-fuse-write-mount-instance-"),
        }
    }

    /// An instance whose tree records file metadata and whose unused chain is many blocks with a
    /// few free slots each, none of them held by a listing: `a/` and `b/` hold two files each,
    /// and `c/` fills [`HOLED_BLOCKS`] blocks, less every [`HOLE_SPACING`]th file.
    async fn with_holed_blocks() -> Self {
        let tracked: Vec<String> = ["a/x0", "a/x1", "b/x0", "b/x1"]
            .into_iter()
            .map(String::from)
            .chain((0..HOLED_BLOCKS * BLOCK_SLOTS).map(|i| format!("c/f{i:05}")))
            .collect();
        let files: Vec<(&str, &[u8])> = tracked
            .iter()
            .map(|name| (name.as_str(), &b"t"[..]))
            .collect();
        let deleted: Vec<String> = (HOLE_SPACING / 2..HOLED_BLOCKS * BLOCK_SLOTS)
            .step_by(HOLE_SPACING)
            .map(|i| format!("c/f{i:05}"))
            .collect();
        let deleted: Vec<&str> = deleted.iter().map(String::as_str).collect();
        Self {
            fixture: fuse_fixture::released_deleting(&[&files, &[]], &deleted).await,
            external: TempDir::new("lore-fuse-write-mount-instance-"),
        }
    }

    fn write_dir(&self) -> PathBuf {
        self.external.path().join("write")
    }

    /// The staged State the instance's anchor names.
    async fn staged(&self) -> Arc<lore_revision::state::State> {
        let staged = lore_revision::instance::load_staged_revision(&self.fixture.repository)
            .await
            .expect("the staged anchor reads")
            .expect("a staged anchor is stored");
        lore_revision::state::State::deserialize(self.fixture.repository.clone(), staged)
            .await
            .expect("the staged State loads")
    }

    /// The instance's context holding `token`, with no filesystem of its own: for a store write
    /// another command makes while one holds the token.
    fn writing(
        &self,
        token: &lore_revision::repository::RepositoryWriteToken,
    ) -> Arc<lore_revision::repository::RepositoryContext> {
        use fuse_fixture::RepositoryContextCreationArgsExt as _;

        let fixture = &self.fixture;
        Arc::new(
            lore_revision::repository::RepositoryContext::new(
                fuse_fixture::default_repository_creation_args(
                    fixture.immutable.clone(),
                    fixture.mutable.clone(),
                )
                .with_path(&fixture.path)
                .with_id(fixture.repository.id)
                .with_instance_id(fixture.repository.instance_id),
            )
            .with_write_token(token.share()),
        )
    }

    /// The instance's context with `fs` as its filesystem, as a command relayed to the service
    /// opens it, holding `token` if given.
    fn command_context(
        &self,
        fs: &Arc<FuseFilesystem>,
        token: Option<lore_revision::repository::RepositoryWriteToken>,
    ) -> Arc<lore_revision::repository::RepositoryContext> {
        use fuse_fixture::RepositoryContextCreationArgsExt as _;

        let fixture = &self.fixture;
        let context = lore_revision::repository::RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(
                fixture.immutable.clone(),
                fixture.mutable.clone(),
            )
            .with_path(&fixture.path)
            .with_id(fixture.repository.id)
            .with_instance_id(fixture.repository.instance_id)
            .with_filesystem_provider(Arc::new(FuseProvider(fs.clone()))),
        );
        Arc::new(match token {
            Some(token) => context.with_write_token(token),
            None => context,
        })
    }

    /// Mounts a fresh provider over the instance read-write, as the service does.
    async fn mount(&self) -> Mounted {
        self.mount_at(self.fixture.state.clone()).await
    }

    /// Mounts a fresh provider over `state` with the instance's local changes, as the service
    /// remounts an instance standing on that revision.
    async fn mount_at(&self, state: Arc<lore_revision::state::State>) -> Mounted {
        let fs = LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                FuseFilesystem::new_writable(
                    self.fixture.repository.clone(),
                    state,
                    AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
                    self.external.path(),
                ),
            )
            .await
            .expect("writable provider");
        let dir = TempDir::new("lore-fuse-write-mount-");
        let options = MountOptions {
            fsname: "lore:write-mount".to_string(),
            max_read: MAX_READ,
            fusermount: None,
        };
        let mount = Arc::new(
            lore_fuse::mount::mount_with_access(dir.path(), &options, MountAccess::ReadWrite)
                .expect("read-write mount"),
        );
        let guard = UnmountGuard(dir.path().to_path_buf());
        let session = Session::start(
            fs.clone(),
            Arc::new(DeviceTransport::new(mount)),
            SessionConfig {
                workers: WORKERS,
                init: InitConfig {
                    zero_message_open: true,
                    ..InitConfig::read_write()
                },
            },
        )
        .await
        .expect("session negotiates");
        fs.set_notifier(session.notifier().expect("notifier"))
            .expect("the notification thread");
        Mounted {
            session,
            _guard: guard,
            fs,
            root: dir.path().to_path_buf(),
            _dir: dir,
        }
    }
}

/// The `FilesystemProvider` of a mounted instance, as the service hands a relayed command.
struct FuseProvider(Arc<FuseFilesystem>);

#[async_trait::async_trait]
impl lore_revision::fs::filesystem_provider::FilesystemProvider for FuseProvider {
    async fn begin_operation(
        &self,
    ) -> Result<
        Arc<lore_revision::fs::filesystem_provider::InstanceOperationImpl>,
        lore_revision::fs::filesystem_provider::FsError,
    > {
        use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
        use lore_revision::fs::filesystem_provider::StaticDispatchInstanceOperation;
        use lore_revision::fs::fuse::operation::FuseOperation;

        Ok(Arc::new(InstanceOperationImpl::new(
            StaticDispatchInstanceOperation::Fuse(FuseOperation::new(self.0.clone())),
            self.holds_state(),
        )))
    }

    fn holds_state(&self) -> bool {
        true
    }
}

/// The unified diffs `lore diff` with no revision emits for `repository`, by path: the action
/// and the patch.
async fn working_diffs(
    repository: Arc<lore_revision::repository::RepositoryContext>,
) -> Vec<(String, lore_revision::interface::LoreFileAction, String)> {
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::relay::EventDispatcher;

    let found = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = found.clone();
    let callback: LoreEventCallback = Some(Box::new(move |event| {
        if let LoreEvent::FileDiff(data) = event {
            captured.lock().expect("diffs").push((
                data.path.as_str().to_string(),
                data.action,
                data.patch.as_str().to_string(),
            ));
        }
    }));
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs::default(),
        EventDispatcher::new(callback),
        "test-user".to_string(),
    ));
    LORE_CONTEXT
        .scope(
            execution.clone(),
            Box::pin(lore_revision::file::diff::diff(
                repository,
                None,
                None,
                Vec::new(),
                false,
                lore_revision::file::diff::DiffOptions {
                    context_lines: 3,
                    ignore_whitespace_eol: false,
                    ignore_whitespace_inline: false,
                },
            )),
        )
        .await
        .expect("diff");
    execution.dispatcher.drain().await;
    let mut diffs = found.lock().expect("diffs").clone();
    diffs.sort_by(|a, b| a.0.cmp(&b.0));
    diffs
}

/// Runs `work` on the blocking pool: a system call on the mount waits on the session this
/// runtime serves.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    lore_base::lore_spawn_blocking!(work)
        .await
        .expect("system call task must not panic")
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("dirent")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn files_written_through_the_mount_read_back() {
    skip_without_fuse!();
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    let big = pattern(3 << 20, 5);
    let expected = big.clone();

    blocking(move || {
        std::fs::write(root.join("small.txt"), b"hello mount").expect("write small");
        let mut file = std::fs::File::create(root.join("big.bin")).expect("create big");
        for chunk in big.chunks(1 << 20) {
            file.write_all(chunk).expect("write chunk");
        }
        drop(file);
        assert_eq!(
            std::fs::read(root.join("small.txt")).expect("read small"),
            b"hello mount"
        );
        assert!(std::fs::read(root.join("big.bin")).expect("read big") == expected);
        assert_eq!(
            std::fs::metadata(root.join("big.bin")).expect("stat").len(),
            3 << 20
        );
        assert_eq!(names(&root), vec!["big.bin", "keep.txt", "small.txt"]);
    })
    .await;
    assert_eq!(
        std::fs::metadata(instance.write_dir().join("big.bin"))
            .expect("the host file")
            .len(),
        3 << 20
    );
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn writing_a_temporary_then_renaming_it_over_the_output_replaces_it() {
    skip_without_fuse!();
    let instance = Instance::new(&[("Binaries/app", b"tracked build")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();

    blocking(move || {
        let bin = root.join("Binaries");
        for build in [&b"first build"[..], b"second build"] {
            std::fs::write(bin.join("app.tmp"), build).expect("write temporary");
            std::fs::rename(bin.join("app.tmp"), bin.join("app")).expect("rename over");
            assert_eq!(std::fs::read(bin.join("app")).expect("read"), build);
        }
        assert_eq!(names(&bin), vec!["app"]);
    })
    .await;
    assert_eq!(
        std::fs::read(instance.write_dir().join("Binaries/app")).expect("the host file"),
        b"second build"
    );
    m.unmount().await;
}

/// A directory listed around each save through a temporary never lists the temporary once its
/// rename returned, though the kernel caches the listing (`CACHE_DIR`) and the provider patches
/// its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_after_a_save_returns_omits_the_temporary() {
    skip_without_fuse!();
    let instance = Instance::new(&[("Binaries/app", b"tracked build")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();

    // A name the provider makes without telling the kernel, the directory's time kept, stays out
    // of the listing the kernel cached: the listings below can come from that cache. Memory
    // pressure may drop the cache between two listings, so one served from it of three is enough.
    let bin = root.join("Binaries");
    let listed = |bin: PathBuf| blocking(move || names(&bin));
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let dir = lookup(&m.fs, ROOT_INO, "Binaries").await.expect("Binaries");
            let mut cached = Vec::new();
            for _ in 0..3 {
                assert_eq!(listed(bin.clone()).await, vec!["app"]);
                let mtime = m.fs.inodes().get(dir).expect("Binaries").mtime;
                create_with(&m.fs, dir, "unseen.tmp", b"unseen").await;
                m.fs.inodes().get_mut(dir).expect("Binaries").mtime = mtime;
                cached = listed(bin.clone()).await;
                m.fs.unlink(&request(), dir, b"unseen.tmp")
                    .await
                    .expect("unlink");
                if cached == ["app"] {
                    break;
                }
            }
            assert_eq!(
                cached,
                vec!["app"],
                "the kernel serves the listing it cached"
            );
        })
        .await;

    blocking(move || {
        let bin = root.join("Binaries");
        for save in 0..200u32 {
            assert_eq!(names(&bin), vec!["app"], "before save {save}");
            let temporary = format!("app-{save}.tmp");
            std::fs::write(bin.join(&temporary), save.to_le_bytes()).expect("write temporary");
            assert_eq!(
                names(&bin),
                vec!["app".to_string(), temporary.clone()],
                "during save {save}"
            );
            std::fs::rename(bin.join(&temporary), bin.join("app")).expect("rename over");
        }
        assert_eq!(names(&bin), vec!["app"]);
        assert_eq!(
            std::fs::read(bin.join("app")).expect("read"),
            199u32.to_le_bytes()
        );
    })
    .await;
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn truncating_a_tracked_file_keeps_its_inode_and_advances_its_mtime() {
    skip_without_fuse!();
    let instance = Instance::new(&[("Source/a.cpp", b"int a;")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();

    blocking(move || {
        let path = root.join("Source/a.cpp");
        let before = std::fs::metadata(&path).expect("stat");
        std::thread::sleep(Duration::from_millis(20));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("open for rewrite");
        file.write_all(b"int b = 2;").expect("rewrite");
        drop(file);
        let after = std::fs::metadata(&path).expect("stat");
        assert_eq!(after.ino(), before.ino(), "a rewrite keeps the inode");
        assert!(
            (after.mtime(), after.mtime_nsec()) > (before.mtime(), before.mtime_nsec()),
            "a rewrite advances the mtime"
        );
        assert_eq!(after.len(), 10);
        assert_eq!(std::fs::read(&path).expect("read"), b"int b = 2;");
    })
    .await;
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unlink_mkdir_rmdir_and_listings_through_the_mount() {
    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"a"), ("dir/b.txt", b"b"), ("c.txt", b"c")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();

    blocking(move || {
        std::fs::remove_file(root.join("a.txt")).expect("unlink tracked");
        std::fs::write(root.join("new.txt"), b"n").expect("write");
        std::fs::remove_file(root.join("new.txt")).expect("unlink new");
        std::fs::create_dir(root.join("made")).expect("mkdir");
        std::fs::write(root.join("made/f"), b"f").expect("write");
        let refused = std::fs::remove_dir(root.join("made")).expect_err("not empty");
        assert_eq!(refused.raw_os_error(), Some(libc::ENOTEMPTY));
        std::fs::remove_file(root.join("made/f")).expect("unlink");
        std::fs::remove_dir(root.join("made")).expect("rmdir");
        std::fs::create_dir(root.join("Saved")).expect("mkdir");

        assert_eq!(names(&root), vec!["Saved", "c.txt", "dir"]);
        assert!(!root.join("a.txt").exists());
        std::fs::write(root.join("dir/c.o"), b"o").expect("write in tracked dir");
        assert_eq!(names(&root.join("dir")), vec!["b.txt", "c.o"]);
        std::os::unix::fs::symlink("dir/b.txt", root.join("link")).expect("symlink");
        assert_eq!(
            std::fs::read_link(root.join("link")).expect("readlink"),
            Path::new("dir/b.txt")
        );
        assert_eq!(
            std::fs::read(root.join("link")).expect("read through"),
            b"b"
        );
    })
    .await;
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_writable_mapping_writes_through_to_the_host_file() {
    skip_without_fuse!();
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    let len = (256 << 10) + 123;
    let data = pattern(len, 11);
    let expected = data.clone();

    blocking(move || {
        let path = root.join("linked.elf");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create output");
        file.set_len(len as u64).expect("size output");
        // SAFETY: a fresh shared mapping of `len` bytes of an open file.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            mapped,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the mapping is `len` bytes, readable and writable, until the munmap below.
        let view = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), len) };
        view.copy_from_slice(&data);
        // SAFETY: `mapped` is the live mapping of `len` bytes.
        assert_eq!(unsafe { libc::msync(mapped, len, libc::MS_SYNC) }, 0);
        // SAFETY: as above; the view is not used past this point.
        assert_eq!(unsafe { libc::munmap(mapped, len) }, 0);
        drop(file);
        assert!(std::fs::read(&path).expect("read back") == data);
    })
    .await;
    assert!(
        std::fs::read(instance.write_dir().join("linked.elf")).expect("the host file") == expected,
        "the mapping's pages reached the host file"
    );
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_shaped_burst_creates_and_renames_two_thousand_files() {
    skip_without_fuse!();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 250;
    let instance = Instance::new(&[("Engine/Build/Build.version", b"{}")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();

    blocking(move || {
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let root = &root;
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        let dir = root.join(format!(
                            "Engine/Intermediate/Build/Linux/Module{}/Sub{}",
                            thread % 4,
                            i % 10
                        ));
                        std::fs::create_dir_all(&dir).expect("create_dir_all");
                        let temp = dir.join(format!("t{thread}-{i}.o.tmp"));
                        std::fs::write(&temp, format!("object {thread} {i}")).expect("write");
                        std::fs::rename(&temp, dir.join(format!("t{thread}-{i}.o")))
                            .expect("rename");
                    }
                });
            }
        });
        let mut count = 0;
        let mut pending = vec![root.join("Engine/Intermediate")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let entry = entry.expect("dirent");
                let file_type = entry.file_type().expect("file type");
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    assert!(name.ends_with(".o"), "{name}");
                    count += 1;
                }
            }
        }
        assert_eq!(count, THREADS * PER_THREAD);
        assert_eq!(
            std::fs::read_to_string(
                root.join("Engine/Intermediate/Build/Linux/Module3/Sub9/t7-249.o")
            )
            .expect("read"),
            "object 7 249"
        );
        assert_eq!(names(&root.join("Engine")), vec!["Build", "Intermediate"]);
    })
    .await;
    m.unmount().await;
}

/// An ordinary clone versions no symlink: its status walk skips one (`file_list_item`), so a new
/// symlink is no change, and a tracked file a symlink replaced reads as deleted. A virtual clone
/// matches this (the tests below).
#[tokio::test(flavor = "multi_thread")]
async fn an_ordinary_clone_sees_no_symlink_and_a_file_a_symlink_replaced_as_deleted() {
    use fuse_fixture::RepositoryContextCreationArgsExt as _;
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    let root = instance.fixture.path.clone();
    #[allow(clippy::disallowed_methods)]
    std::fs::remove_file(root.join("a.txt")).expect("remove a.txt");
    std::os::unix::fs::symlink("b.txt", root.join("a.txt")).expect("symlink at a.txt");
    std::os::unix::fs::symlink("b.txt", root.join("link")).expect("symlink link");
    let token =
        lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let fixture = &instance.fixture;
    let ordinary = Arc::new(
        lore_revision::repository::RepositoryContext::new(
            fuse_fixture::default_repository_creation_args(
                fixture.immutable.clone(),
                fixture.mutable.clone(),
            )
            .with_path(&fixture.path)
            .with_id(fixture.repository.id)
            .with_instance_id(fixture.repository.instance_id),
        )
        .with_write_token(token.share()),
    );

    let reported = staged_state::status_files(ordinary, true).await;

    assert_eq!(
        crate::helper::test_status_actions(reported),
        vec![("a.txt".to_string(), LoreFileAction::Delete)]
    );
}

/// A symlink made through a virtual clone is no change, as on an ordinary clone: status lists only
/// the file it names, a status of the symlink's own path lists nothing, and the commit takes the
/// file alone. The symlink stays local, served from `write/`.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_made_through_the_mount_is_no_change() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            // Naming a file under `write/`, so the symlink resolves on the host.
            let link = fs
                .symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink")
                .ino;

            assert_eq!(
                changed_paths(&instance, &fs).await,
                vec!["dir/local.txt".to_string()]
            );
            let at = lore_revision::util::path::RelativePath::new_from_initial_path("dir/link")
                .expect("path");
            assert_eq!(
                crate::helper::test_status_actions(
                    crate::helper::test_status_files_at(
                        instance.command_context(&fs, None),
                        Some(vec![at]),
                        false
                    )
                    .await
                ),
                Vec::new(),
                "a status of the symlink's path"
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the file");
            assert_eq!(
                revision_content(&instance, revision, "dir/local.txt").await,
                Some(b"local".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "dir/link").await,
                None,
                "the revision holds no symlink"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
            let mut target = [0u8; 16];
            let len = fs
                .readlink(&request(), link, &mut target)
                .await
                .expect("readlink");
            assert_eq!(&target[..len], b"local.txt");
        })
        .await;
}

/// A tracked file a symlink replaced through a virtual clone reads as deleted, as on an ordinary
/// clone: a commit removes it from the revision, status is clean after, and the symlink stays
/// local.
#[tokio::test(flavor = "multi_thread")]
async fn a_tracked_file_a_symlink_replaced_reads_as_deleted() {
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.unlink(&request(), ROOT_INO, b"a.txt")
                .await
                .expect("unlink");
            let link = fs
                .symlink(&request(), ROOT_INO, b"a.txt", b"b.txt")
                .await
                .expect("symlink")
                .ino;

            assert_eq!(
                crate::helper::test_status_actions(status_of(&instance, &fs).await),
                vec![("a.txt".to_string(), LoreFileAction::Delete)]
            );
            let at = lore_revision::util::path::RelativePath::new_from_initial_path("a.txt")
                .expect("path");
            assert_eq!(
                crate::helper::test_status_actions(
                    crate::helper::test_status_files_at(
                        instance.command_context(&fs, None),
                        Some(vec![at]),
                        false
                    )
                    .await
                ),
                vec![("a.txt".to_string(), LoreFileAction::Delete)],
                "a status of the path"
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the deletion");
            assert!(
                state_at(&instance, revision)
                    .await
                    .find_subnode(
                        instance.fixture.repository.clone(),
                        lore_revision::node::ROOT_NODE,
                        hash_string("a.txt"),
                    )
                    .await
                    .is_err(),
                "the revision holds no a.txt"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
            let mut target = [0u8; 16];
            let len = fs
                .readlink(&request(), link, &mut target)
                .await
                .expect("readlink");
            assert_eq!(&target[..len], b"b.txt");
        })
        .await;
}

/// `lore dirty` of a directory through the mount marks no symlink in it, as on an ordinary clone,
/// whose listing skips one: the listing the instance operation reads leaves it out, whatever the
/// symlink names.
#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_mark_of_a_directory_through_the_mount_skips_a_symlink() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;

    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            // Naming a file under `write/`, which a listing following it would report.
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");
            let token =
                lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path)
                    .await;
            lore_revision::file::dirty::dirty(
                instance.command_context(&fs, Some(token.share())),
                LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path.join("dir"))]),
            )
            .await
            .expect("lore dirty");

            let staged = instance.staged().await;
            let marked = staged_state::dirty_actions(&instance.fixture.repository, &staged).await;
            assert!(
                marked.contains_key("dir/local.txt"),
                "the walk marked the directory's file: {marked:?}"
            );
            assert!(
                !marked.contains_key("dir/link"),
                "the walk marked the symlink: {marked:?}"
            );
        })
        .await;
}

/// A symlink in a directory made through the mount is no change, as on an ordinary clone: status
/// lists the directory and its file, the commit takes both, and status is clean after.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_in_a_directory_made_through_the_mount_is_no_change() {
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, made, "f.txt", b"eff").await;
            fs.symlink(&request(), made, b"link", b"f.txt")
                .await
                .expect("symlink");

            assert_eq!(
                crate::helper::test_status_actions(status_of(&instance, &fs).await),
                vec![
                    ("made".to_string(), LoreFileAction::Add),
                    ("made/f.txt".to_string(), LoreFileAction::Add)
                ]
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the directory");
            assert_eq!(
                revision_content(&instance, revision, "made/f.txt").await,
                Some(b"eff".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "made/link").await,
                None,
                "the revision holds no symlink"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A symlink two levels below a directory made through the mount is no change either: the walk of
/// the added directory leaves it out of its child level, which runs in a task of its own while the
/// walk's budget allows, as it leaves one out of the top level.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_below_a_directory_made_through_the_mount_is_no_change() {
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let sub = fs
                .mkdir(&request(), made, b"sub", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir sub")
                .ino;
            create_with(&fs, sub, "g.txt", b"gee").await;
            fs.symlink(&request(), sub, b"link", b"g.txt")
                .await
                .expect("symlink");

            assert_eq!(
                crate::helper::test_status_actions(status_of(&instance, &fs).await),
                vec![
                    ("made".to_string(), LoreFileAction::Add),
                    ("made/sub".to_string(), LoreFileAction::Add),
                    ("made/sub/g.txt".to_string(), LoreFileAction::Add)
                ]
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the directories");
            assert_eq!(
                revision_content(&instance, revision, "made/sub/g.txt").await,
                Some(b"gee".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "made/sub/link").await,
                None,
                "the revision holds no symlink"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A symlink in a directory that replaced a tracked file through the mount is no change: status
/// lists the directory and its file alone, the commit replaces the file with both, and status is
/// clean after.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_in_a_directory_replacing_a_tracked_file_is_no_change() {
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("x", b"ex")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup(&fs, ROOT_INO, "x").await.expect("x");
            fs.unlink(&request(), ROOT_INO, b"x").await.expect("unlink");
            let x = fs
                .mkdir(&request(), ROOT_INO, b"x", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, x, "f.txt", b"eff").await;
            fs.symlink(&request(), x, b"link", b"f.txt")
                .await
                .expect("symlink");

            assert_eq!(
                crate::helper::test_status_actions(status_of(&instance, &fs).await),
                vec![
                    ("x".to_string(), LoreFileAction::Add),
                    ("x/f.txt".to_string(), LoreFileAction::Add)
                ]
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the replacement");
            assert_eq!(
                revision_content(&instance, revision, "x/f.txt").await,
                Some(b"eff".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "x/link").await,
                None,
                "the revision holds no symlink"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// `lore stage` naming a symlink through the mount skips it, as a path the filter excludes is
/// skipped: it stages nothing, the commit has nothing to commit, and status lists only the file
/// the symlink names.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_naming_a_symlink_through_the_mount_stages_nothing() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");

            let staged = stage_paths_through(&instance, &fs, &["dir/link"]).await;
            assert!(staged.is_ok(), "the stage: {staged:?}");
            let committed = commit_staged(&instance, &fs).await;
            assert!(
                committed
                    .as_ref()
                    .is_err_and(|error| error.contains("Nothing staged")),
                "the commit: {committed:?}"
            );
            assert_eq!(
                changed_paths(&instance, &fs).await,
                vec!["dir/local.txt".to_string()]
            );
        })
        .await;
}

/// `lore stage dir/link dir/local.txt` through the mount stages the file and skips the symlink:
/// the commit stores the file alone, and status is clean after.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_naming_a_symlink_and_a_file_stages_the_file() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");

            let staged = stage_paths_through(&instance, &fs, &["dir/link", "dir/local.txt"]).await;
            assert!(staged.is_ok(), "the stage: {staged:?}");
            let revision = commit_staged(&instance, &fs)
                .await
                .expect("commit the file");
            assert_eq!(
                revision_content(&instance, revision, "dir/local.txt").await,
                Some(b"local".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "dir/link").await,
                None,
                "the revision holds no symlink"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// `lore stage` of every name in a directory, as a shell glob expands it, stages the files and
/// skips each symlink, whatever it names: the commit stores the edit and the new file, and status
/// is clean after.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_of_a_glob_mixing_symlinks_and_files_stages_the_files() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let a = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("edit");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");
            fs.symlink(&request(), dir, b"zlink", b"missing.txt")
                .await
                .expect("dangling symlink");

            let staged = stage_paths_through(
                &instance,
                &fs,
                &["dir/a.txt", "dir/link", "dir/local.txt", "dir/zlink"],
            )
            .await;
            assert!(staged.is_ok(), "the stage: {staged:?}");
            let revision = commit_staged(&instance, &fs)
                .await
                .expect("commit the files");
            assert_eq!(
                revision_content(&instance, revision, "dir/a.txt").await,
                Some(b"ALPHA".to_vec())
            );
            assert_eq!(
                revision_content(&instance, revision, "dir/local.txt").await,
                Some(b"local".to_vec())
            );
            for link in ["dir/link", "dir/zlink"] {
                assert_eq!(
                    revision_content(&instance, revision, link).await,
                    None,
                    "the revision holds no {link}"
                );
            }
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// `lore stage` naming a path below a symlink fails as one below nothing does: only a symlink
/// named itself is skipped.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_naming_a_path_below_a_symlink_fails_as_one_below_nothing() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");

            let below_nothing = stage_paths_through(&instance, &fs, &["dir/none/typo.txt"]).await;
            let below_link = stage_paths_through(&instance, &fs, &["dir/link/typo.txt"]).await;

            assert!(
                below_nothing
                    .as_ref()
                    .is_err_and(|error| error.contains("Invalid path dir/none/typo.txt")),
                "below nothing: {below_nothing:?}"
            );
            assert!(
                below_link
                    .as_ref()
                    .is_err_and(|error| error.contains("Invalid path dir/link/typo.txt")),
                "below the symlink: {below_link:?}"
            );
        })
        .await;
}

/// `lore stage` naming a symlink reports the path ignored, as status reports a path holding
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_skipping_a_symlink_reports_the_path_ignored() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreString;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryWriteToken;

    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    let ignored = Arc::new(AtomicUsize::new(0));
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");
            let counted = ignored.clone();
            let execution = Arc::new(ExecutionContext::new_client_with_user_id(
                lore_revision::interface::LoreGlobalArgs::default(),
                EventDispatcher::new(Some(Box::new(move |event: &LoreEvent| {
                    if let LoreEvent::PathIgnore(data) = event
                        && data.path.as_str() == "dir/link"
                    {
                        counted.fetch_add(1, Ordering::Relaxed);
                    }
                }))),
                "test-user".to_string(),
            ));
            let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;

            let staged = LORE_CONTEXT
                .scope(execution.clone(), async {
                    let staged = lore_revision::file::stage::stage(
                        instance.command_context(&fs, Some(token.share())),
                        &token,
                        LoreArray::from_vec(vec![LoreString::from(
                            &instance.fixture.path.join("dir/link"),
                        )]),
                        lore_revision::stage::StageOptions::default(),
                    )
                    .await;
                    execution.dispatcher.drain().await;
                    staged
                })
                .await;

            assert!(staged.is_ok(), "the stage: {staged:?}");
        })
        .await;
    assert_eq!(
        ignored.load(Ordering::Relaxed),
        1,
        "PathIgnore events for dir/link"
    );
}

/// `lore stage` naming a tracked file a symlink replaced stages its deletion, as status reads it:
/// the commit removes the file, and status is clean after.
#[tokio::test(flavor = "multi_thread")]
async fn a_stage_naming_a_tracked_file_a_symlink_replaced_stages_its_deletion() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            replace_by_symlink(&fs, dir, "a.txt", SymlinkTarget::Local).await;

            let staged = stage_paths_through(&instance, &fs, &["dir/a.txt", "dir/local.txt"]).await;
            assert!(staged.is_ok(), "the stage: {staged:?}");
            let revision = commit_staged(&instance, &fs)
                .await
                .expect("commit the deletion");
            assert_eq!(
                revision_content(&instance, revision, "dir/a.txt").await,
                None,
                "the revision holds no dir/a.txt"
            );
            assert_eq!(
                revision_content(&instance, revision, "dir/local.txt").await,
                Some(b"local".to_vec())
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// The operation a command reads the working tree through answers a symlink as nothing there, as
/// its listing leaves one out, rather than following it to the file it names.
#[tokio::test(flavor = "multi_thread")]
async fn the_mount_operation_answers_a_symlink_as_nothing_there() {
    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;

    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            create_with(&fs, dir, "local.txt", b"local").await;
            fs.symlink(&request(), dir, b"link", b"local.txt")
                .await
                .expect("symlink");
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            let link = relative("dir/link");

            assert_eq!(
                operation.file_info(&link).await.expect("info"),
                FileInfo::NotExist
            );
            assert_eq!(
                operation
                    .untracked_file_info(&link)
                    .await
                    .expect("untracked info"),
                FileInfo::NotExist
            );
            assert!(
                operation.content_source(&link).await.is_err(),
                "the symlink's content"
            );
        })
        .await;
}

/// What a symlink made through the mount names: a local file it resolves to on the host, or a
/// name the host holds nothing at, as every symlink naming a tracked file is.
#[derive(Clone, Copy, Debug)]
enum SymlinkTarget {
    Local,
    Dangling,
}

impl SymlinkTarget {
    fn names(self) -> &'static [u8] {
        match self {
            SymlinkTarget::Local => b"local.txt",
            SymlinkTarget::Dangling => b"missing.txt",
        }
    }
}

/// Replaces the tracked file `name` in the directory `parent` by a symlink naming `target`,
/// beside the local file `local.txt`; answers the symlink's inode.
async fn replace_by_symlink(
    fs: &FuseFilesystem,
    parent: u64,
    name: &str,
    target: SymlinkTarget,
) -> u64 {
    lookup(fs, parent, name).await.expect("the tracked file");
    fs.unlink(&request(), parent, name.as_bytes())
        .await
        .expect("unlink");
    create_with(fs, parent, "local.txt", b"local").await;
    fs.symlink(&request(), parent, name.as_bytes(), target.names())
        .await
        .expect("symlink")
        .ino
}

/// What the symlink at `ino` names.
async fn link_target(fs: &FuseFilesystem, ino: u64) -> Vec<u8> {
    let mut target = [0u8; 64];
    let len = fs
        .readlink(&request(), ino, &mut target)
        .await
        .expect("readlink");
    target[..len].to_vec()
}

/// A sync whose target changes a tracked file a symlink naming `target` replaced refuses with
/// `LocalModifications`: the symlink is the user's, which no sync writes over. Nothing is applied,
/// the symlink stays, and status is as it was.
async fn a_sync_onto_a_symlink_refuses(target: SymlinkTarget) {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let link = replace_by_symlink(&fs, dir, "f.txt", target).await;
            let before = status_of(&instance, &fs).await;

            let synced = sync_through(&instance, &fs, first, Default::default()).await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(
                served_at(&fs, "dir/g.txt").await,
                b"g-two",
                "the sync applied a change"
            );
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
            assert_eq!(link_target(&fs, link).await, target.names());
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_changing_a_tracked_file_a_symlink_to_a_local_file_replaced_refuses() {
    a_sync_onto_a_symlink_refuses(SymlinkTarget::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_changing_a_tracked_file_a_dangling_symlink_replaced_refuses() {
    a_sync_onto_a_symlink_refuses(SymlinkTarget::Dangling).await;
}

/// A merge whose incoming branch changes a tracked file a symlink naming `target` replaced refuses
/// with `LocalModifications` before writing anything: no incoming change is applied, the symlink
/// stays, and status is as it was.
async fn a_merge_onto_a_symlink_refuses(target: SymlinkTarget) {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = feature_branch(&instance, &fs).await;
            let link = replace_by_symlink(&fs, ROOT_INO, "a.txt", target).await;
            let before = status_of(&instance, &fs).await;

            let merged = merge_through(&instance, &fs, feature).await;

            assert!(
                merged
                    .as_ref()
                    .is_err_and(|error| error.contains("LocalModifications")),
                "{merged:?}"
            );
            assert_eq!(
                served_at(&fs, "b.txt").await,
                b"bee",
                "the merge deleted b.txt"
            );
            assert_eq!(
                lookup(&fs, ROOT_INO, "new.txt").await,
                Err(Errno::ENOENT),
                "the merge added new.txt"
            );
            assert_eq!(link_target(&fs, link).await, target.names());
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_changing_a_tracked_file_a_symlink_to_a_local_file_replaced_refuses() {
    a_merge_onto_a_symlink_refuses(SymlinkTarget::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_changing_a_tracked_file_a_dangling_symlink_replaced_refuses() {
    a_merge_onto_a_symlink_refuses(SymlinkTarget::Dangling).await;
}

/// A sync whose target adds a file where an untracked symlink stands refuses with
/// `LocalModifications`: the symlink is the user's, which no sync writes over. Nothing is applied,
/// the symlink stays, and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_adding_a_file_where_a_symlink_stands_refuses() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let new = fs
                .mkdir(&request(), ROOT_INO, b"new", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir new")
                .ino;
            let a = fs
                .mkdir(&request(), new, b"a", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir new/a")
                .ino;
            create_with(&fs, a, "local.txt", b"local").await;
            let link = fs
                .symlink(&request(), a, b"b.txt", b"local.txt")
                .await
                .expect("symlink")
                .ino;
            let before = status_of(&instance, &fs).await;

            let synced = sync_through(&instance, &fs, first, Default::default()).await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(
                served_at(&fs, "dir/g.txt").await,
                b"g-two",
                "the sync applied a change"
            );
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A merge whose incoming branch adds a file where an untracked symlink stands refuses with
/// `LocalModifications` before writing anything: no incoming change is applied, the symlink stays,
/// and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_adding_a_file_where_a_symlink_stands_refuses() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = feature_branch(&instance, &fs).await;
            create_with(&fs, ROOT_INO, "local.txt", b"local").await;
            let link = fs
                .symlink(&request(), ROOT_INO, b"new.txt", b"local.txt")
                .await
                .expect("symlink")
                .ino;
            let before = status_of(&instance, &fs).await;

            let merged = merge_through(&instance, &fs, feature).await;

            assert!(
                merged
                    .as_ref()
                    .is_err_and(|error| error.contains("LocalModifications")),
                "{merged:?}"
            );
            assert_eq!(
                served_at(&fs, "a.txt").await,
                b"alpha",
                "the merge wrote a.txt"
            );
            assert_eq!(
                served_at(&fs, "b.txt").await,
                b"bee",
                "the merge deleted b.txt"
            );
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// Replaces the tracked directory `dir` below the root, emptied of `files` first, by a symlink
/// naming the local file `local.txt`; answers the symlink's inode.
async fn replace_directory_by_symlink(fs: &FuseFilesystem, files: &[&str]) -> u64 {
    let dir = lookup(fs, ROOT_INO, "dir").await.expect("dir");
    for name in files {
        lookup(fs, dir, name).await.expect("a tracked file");
        fs.unlink(&request(), dir, name.as_bytes())
            .await
            .expect("unlink");
    }
    fs.rmdir(&request(), ROOT_INO, b"dir").await.expect("rmdir");
    create_with(fs, ROOT_INO, "local.txt", b"local").await;
    fs.symlink(&request(), ROOT_INO, b"dir", b"local.txt")
        .await
        .expect("symlink")
        .ino
}

/// A sync whose target changes files in a tracked directory a symlink replaced refuses with
/// `LocalModifications`: writing them would go through the user's symlink. Nothing is applied,
/// the symlink stays, and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_changing_files_in_a_directory_a_symlink_replaced_refuses() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let link = replace_directory_by_symlink(&fs, &["f.txt", "g.txt"]).await;
            let before = status_of(&instance, &fs).await;

            let synced = sync_through(&instance, &fs, first, Default::default()).await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(
                served_at(&fs, "gone/x.txt").await,
                b"x",
                "the sync applied a change"
            );
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A merge whose incoming branch changes a file in a tracked directory a symlink replaced refuses
/// with `LocalModifications` before writing anything: no incoming change is applied, the symlink
/// stays, and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_changing_a_file_in_a_directory_a_symlink_replaced_refuses() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = create_branch_id_through(&instance, &fs, "feature").await;
            switch_through(&instance, &fs, "feature")
                .await
                .expect("switch to feature");
            let a = lookup_path(&fs, "dir/a.txt").await.expect("dir/a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("write");
            lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            fs.unlink(&request(), ROOT_INO, b"b.txt")
                .await
                .expect("unlink");
            commit_through(&instance, &fs, false)
                .await
                .expect("commit on feature");
            switch_through(&instance, &fs, lore_revision::branch::DEFAULT_DEFAULT_NAME)
                .await
                .expect("switch back");
            let link = replace_directory_by_symlink(&fs, &["a.txt"]).await;
            let before = status_of(&instance, &fs).await;

            let merged = merge_through(&instance, &fs, feature).await;

            assert!(
                merged
                    .as_ref()
                    .is_err_and(|error| error.contains("LocalModifications")),
                "{merged:?}"
            );
            assert_eq!(
                served_at(&fs, "b.txt").await,
                b"bee",
                "the merge deleted b.txt"
            );
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A cherry-pick whose picked revision changes a tracked file a symlink replaced refuses with
/// `LocalModifications` before writing anything, as a merge does: no picked change is applied, the
/// symlink stays, and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_cherry_pick_changing_a_tracked_file_a_symlink_replaced_refuses() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (_, picked) = feature_branch_and_revision(&instance, &fs).await;
            let link = replace_by_symlink(&fs, ROOT_INO, "a.txt", SymlinkTarget::Local).await;
            let before = status_of(&instance, &fs).await;

            let result = cherry_pick_through(&instance, &fs, picked).await;

            assert!(
                matches!(
                    result,
                    Err(lore_revision::branch::merge::MergeError::LocalModifications(_))
                ),
                "{result:?}"
            );
            assert_eq!(
                served_at(&fs, "b.txt").await,
                b"bee",
                "the cherry-pick deleted b.txt"
            );
            assert_eq!(
                lookup(&fs, ROOT_INO, "new.txt").await,
                Err(Errno::ENOENT),
                "the cherry-pick added new.txt"
            );
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A cherry-pick whose picked revision adds a file where an untracked symlink stands refuses with
/// `LocalModifications` before writing anything: no picked change is applied, the symlink stays,
/// and status is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_cherry_pick_adding_a_file_where_a_symlink_stands_refuses() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (_, picked) = feature_branch_and_revision(&instance, &fs).await;
            create_with(&fs, ROOT_INO, "local.txt", b"local").await;
            let link = fs
                .symlink(&request(), ROOT_INO, b"new.txt", b"local.txt")
                .await
                .expect("symlink")
                .ino;
            let before = status_of(&instance, &fs).await;

            let result = cherry_pick_through(&instance, &fs, picked).await;

            assert!(
                matches!(
                    result,
                    Err(lore_revision::branch::merge::MergeError::LocalModifications(_))
                ),
                "{result:?}"
            );
            assert_eq!(
                served_at(&fs, "a.txt").await,
                b"alpha",
                "the cherry-pick wrote a.txt"
            );
            assert_eq!(
                served_at(&fs, "b.txt").await,
                b"bee",
                "the cherry-pick deleted b.txt"
            );
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A forced sync skips the check and keeps a symlink standing where the target changes a tracked
/// file, as the swap keeps every local symlink: the target's change at that path is not applied,
/// the rest is, and status then lists the tracked file deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_sync_keeps_a_symlink_where_the_target_changes_a_tracked_file() {
    use lore_revision::interface::LoreFileAction;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            replace_by_symlink(&fs, dir, "f.txt", SymlinkTarget::Local).await;

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            assert_eq!(fs.served().revision(), first);
            assert_eq!(
                served_at(&fs, "dir/g.txt").await,
                b"g-one",
                "the rest of the target applied"
            );
            let link = lookup_path(&fs, "dir/f.txt").await.expect("dir/f.txt");
            assert_eq!(link_target(&fs, link).await, b"local.txt");
            assert_eq!(
                crate::helper::test_status_actions(status_of(&instance, &fs).await),
                vec![
                    ("dir/f.txt".to_string(), LoreFileAction::Delete),
                    ("dir/local.txt".to_string(), LoreFileAction::Add)
                ]
            );
        })
        .await;
}

/// `lore dirty` through the mount marks the staged State with each path's action alone, an edit
/// a Modify and a new file an Add, never `LocalContent`: the provider's own State records where
/// content lives.
#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_mark_through_the_mount_records_the_action_alone() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::node::NodeExtFlags;
    use staged_state::Action;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("edit");
            create_with(&fs, ROOT_INO, "new.txt", b"new").await;
            let token =
                lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path)
                    .await;
            let paths = ["a.txt", "new.txt"]
                .iter()
                .map(|path| LoreString::from(&instance.fixture.path.join(path)))
                .collect();
            lore_revision::file::dirty::dirty(
                instance.command_context(&fs, Some(token.share())),
                LoreArray::from_vec(paths),
            )
            .await
            .expect("lore dirty");

            let none = NodeExtFlags::NoFlags;
            let staged = instance.staged().await;
            assert_eq!(
                staged_state::dirty_actions(&instance.fixture.repository, &staged).await,
                [
                    ("a.txt", (Action::Modify, none)),
                    ("new.txt", (Action::Add, none)),
                ]
                .into_iter()
                .map(|(path, action)| (path.to_string(), action))
                .collect()
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_records_the_mount_writes_in_the_staged_state_and_they_survive_a_remount() {
    use lore_revision::interface::LoreFileAction;
    use lore_revision::node::NodeExtFlags;
    use staged_state::Action;

    skip_without_fuse!();
    let instance = Instance::new(&[
        ("a.txt", b"alpha"),
        ("gone.txt", b"gone"),
        ("old.txt", b"old"),
    ])
    .await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || {
        std::fs::create_dir_all(root.join("Saved/Logs")).expect("mkdir");
        std::fs::write(root.join("Saved/Logs/build.log"), b"log").expect("write");
        std::fs::remove_file(root.join("gone.txt")).expect("unlink");
        std::fs::write(root.join("a.txt"), b"ALPHA").expect("rewrite tracked");
        std::fs::rename(root.join("old.txt"), root.join("new.txt")).expect("rename tracked");
    })
    .await;
    let token =
        lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let reported =
        staged_state::status_files(instance.command_context(&m.fs, Some(token)), false).await;

    let none = NodeExtFlags::NoFlags;
    let staged = instance.staged().await;
    assert_eq!(
        staged_state::dirty_actions(&instance.fixture.repository, &staged).await,
        [
            ("Saved", (Action::Add, none)),
            ("Saved/Logs", (Action::Add, none)),
            ("Saved/Logs/build.log", (Action::Add, none)),
            ("a.txt", (Action::Modify, none)),
            ("gone.txt", (Action::Delete, none)),
            ("new.txt", (Action::Add, none)),
            ("old.txt", (Action::Delete, none)),
        ]
        .into_iter()
        .map(|(path, action)| (path.to_string(), action))
        .collect()
    );
    assert_eq!(
        reported,
        [
            ("Saved", LoreFileAction::Add),
            ("Saved/Logs", LoreFileAction::Add),
            ("Saved/Logs/build.log", LoreFileAction::Add),
            ("a.txt", LoreFileAction::Keep),
            ("gone.txt", LoreFileAction::Delete),
            ("new.txt", LoreFileAction::Add),
            ("old.txt", LoreFileAction::Delete),
        ]
        .into_iter()
        .map(|(path, action)| (path.to_string(), (action, true)))
        .collect()
    );
    m.unmount().await;

    let again = instance.mount().await;
    let root = again.root.clone();
    blocking(move || {
        assert_eq!(names(&root), vec!["Saved", "a.txt", "new.txt"]);
        assert_eq!(
            std::fs::read(root.join("Saved/Logs/build.log")).expect("read"),
            b"log"
        );
        assert_eq!(std::fs::read(root.join("a.txt")).expect("read"), b"ALPHA");
        assert_eq!(std::fs::read(root.join("new.txt")).expect("read"), b"old");
        assert!(!root.join("gone.txt").exists());
        assert!(!root.join("old.txt").exists());
    })
    .await;
    assert!(
        !instance.external.path().join("whiteouts").exists(),
        "no whiteout log is kept beside write/"
    );
    again.unmount().await;
}

/// The provider over a fresh rebuild of `instance`, not mounted: requests go to it directly.
async fn provider(instance: &Instance) -> Arc<FuseFilesystem> {
    FuseFilesystem::new_writable(
        instance.fixture.repository.clone(),
        instance.fixture.state.clone(),
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
        instance.external.path(),
    )
    .await
    .expect("writable provider")
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

/// The inode LOOKUP answers for `name` in `parent`; `ENOENT` for a negative entry too.
async fn lookup(fs: &FuseFilesystem, parent: u64, name: &str) -> Result<u64, Errno> {
    match fs.lookup(&request(), parent, name.as_bytes()).await?.ino {
        0 => Err(Errno::ENOENT),
        ino => Ok(ino),
    }
}

/// The node at `path` in the provider's filesystem State.
async fn node_at(instance: &Instance, fs: &FuseFilesystem, path: &str) -> NodeID {
    let mut node = ROOT_NODE;
    for name in path.split('/') {
        node = fs
            .state()
            .find_subnode(instance.fixture.repository.clone(), node, hash_string(name))
            .await
            .expect("a node on the path");
    }
    node
}

fn address(marker: u64) -> Address {
    Address {
        hash: Hash::from_u64(marker),
        context: Context::default(),
    }
}

fn file_node(size: u64, marker: u64) -> Node {
    Node {
        flags: NodeFlags::File.bits(),
        size,
        address: address(marker),
        ..Node::default()
    }
}

/// Attaches a file named `name` below `parent` in the provider's filesystem State.
async fn attach(instance: &Instance, fs: &FuseFilesystem, parent: NodeID, name: &str) -> NodeID {
    match fs
        .state()
        .node_attach(
            instance.fixture.repository.clone(),
            parent,
            file_node(7, 1),
            name,
        )
        .await
        .expect("attach")
    {
        AttachOutcome::Attached(node) => node,
        AttachOutcome::Exists(_) => panic!("{name} is already held"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_in_a_directory_changed_since_its_listing_finds_the_new_node_and_misses_the_detached_one()
 {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt")
                .await
                .expect("a.txt before the change");
            assert_eq!(lookup(&fs, dir, "new.txt").await, Err(Errno::ENOENT));

            let dir_node = node_at(&instance, &fs, "dir").await;
            attach(&instance, &fs, dir_node, "new.txt").await;
            let a = node_at(&instance, &fs, "dir/a.txt").await;
            fs.state()
                .node_detach(instance.fixture.repository.clone(), a, Detach::Always)
                .await
                .expect("detach");

            let new = lookup(&fs, dir, "new.txt")
                .await
                .expect("the attached node");
            assert_eq!(
                fs.getattr(&request(), new).await.expect("attr").attr.size,
                7
            );
            assert_eq!(lookup(&fs, dir, "a.txt").await, Err(Errno::ENOENT));
            lookup(&fs, dir, "b.txt")
                .await
                .expect("an untouched sibling");
        })
        .await;
}

/// The capacity of the chain log of directory `dir`, which a listing build installed.
async fn log_capacity(instance: &Instance, fs: &FuseFilesystem, dir: NodeID) -> usize {
    fs.state()
        .block(
            instance.fixture.repository.clone(),
            lore_revision::node::NodeBlock::index(dir),
        )
        .await
        .expect("the directory's block")
        .read()
        .chain_log(Node::index(dir))
        .map_or(0, lore_revision::node::ChainLog::capacity)
}

/// A listing of directory `dir` at `path` built now, which a test holds or caches later.
async fn listing_now(
    fs: &FuseFilesystem,
    dir: NodeID,
    path: &str,
) -> Arc<lore_revision::fs::fuse::listing::Listing> {
    lore_revision::fs::fuse::listing::Listing::build(
        &fs.served(),
        dir,
        lore_revision::filter::FilterStates::ROOT,
        &lore_revision::util::path::RelativePath::new().join(path),
    )
    .await
    .expect("build")
}

/// Sixteen attaches fit the log a directory's first listing installs; the seventeenth overflows
/// it, so the next lookup rebuilds once and the build grows the log, which holds the rest.
#[tokio::test(flavor = "multi_thread")]
async fn the_seventeenth_attach_rebuilds_once_and_grows_the_log() {
    use lore_revision::node::CHAIN_LOG_GROWN;
    use lore_revision::node::CHAIN_LOG_INITIAL;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let built = fs.listings_built();
            let dir_node = node_at(&instance, &fs, "dir").await;
            assert_eq!(
                log_capacity(&instance, &fs, dir_node).await,
                CHAIN_LOG_INITIAL
            );

            let attach_and_find = |index: usize| {
                let (instance, fs) = (&instance, &fs);
                async move {
                    let name = format!("attached-{index}.o");
                    attach(instance, fs, dir_node, &name).await;
                    lookup(fs, dir, &name)
                        .await
                        .expect("the node just attached");
                }
            };
            for index in 0..CHAIN_LOG_INITIAL {
                attach_and_find(index).await;
            }
            assert_eq!(
                fs.listings_built() - built,
                0,
                "sixteen patches fit the log"
            );
            attach_and_find(CHAIN_LOG_INITIAL).await;
            assert_eq!(
                fs.listings_built() - built,
                1,
                "the seventeenth overflows it"
            );
            assert_eq!(
                log_capacity(&instance, &fs, dir_node).await,
                CHAIN_LOG_GROWN
            );
            for index in CHAIN_LOG_INITIAL + 1..65 {
                attach_and_find(index).await;
            }
            for index in [0, 32, 64] {
                lookup(&fs, dir, &format!("attached-{index}.o"))
                    .await
                    .expect("an attached node");
            }
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            assert_eq!(
                fs.listings_built() - built,
                1,
                "the grown log holds the rest"
            );
        })
        .await;
}

/// A LOOKUP on the worker over a listing whose log overflowed answers `WouldFetch`, building
/// nothing and counting no inode; the deferred LOOKUP that follows rebuilds once.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_on_the_worker_over_an_overflowed_log_defers_and_builds_nothing() {
    use lore_revision::node::CHAIN_LOG_INITIAL;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            for index in 0..=CHAIN_LOG_INITIAL {
                attach(&instance, &fs, dir_node, &format!("attached-{index}.o")).await;
            }
            let (built, inodes) = (fs.listings_built(), fs.inodes().len());

            let deferred = fs.lookup_local(&request(), dir, b"attached-0.o").await;

            assert!(
                matches!(deferred, Err(ReadFault::WouldFetch)),
                "the worker defers the rebuild"
            );
            assert_eq!(fs.listings_built(), built, "the worker built nothing");
            assert_eq!(fs.inodes().len(), inodes, "the worker counted no inode");
            lookup(&fs, dir, "attached-0.o")
                .await
                .expect("the deferred lookup");
            assert_eq!(fs.listings_built(), built + 1);
        })
        .await;
}

/// A listing held while its directory's log overflows, and a later lookup's build grows the
/// log, finds every name attached since it was built: the grow keeps each patch at its
/// sequence and the floor where it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_through_a_listing_held_across_a_grow_finds_a_name_attached_in_the_gap() {
    use lore_revision::fs::fuse::listing::Hit;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            attach(&instance, &fs, dir_node, "first.o").await;
            let held = listing_now(&fs, dir_node, "dir").await;
            let mut gap = Vec::new();
            for index in 0..16 {
                let name = format!("gap-{index}.o");
                let node = attach(&instance, &fs, dir_node, &name).await;
                gap.push((name, node));
            }
            let built = fs.listings_built();
            lookup(&fs, dir, "gap-15.o").await.expect("gap-15.o");
            assert_eq!(
                fs.listings_built() - built,
                1,
                "the overflow costs one build"
            );
            assert_eq!(
                log_capacity(&instance, &fs, dir_node).await,
                lore_revision::node::CHAIN_LOG_GROWN,
                "the build grew the log"
            );
            for (name, node) in &gap {
                match held.lookup(name) {
                    Hit::Added(found, _) => assert_eq!(found, *node, "{name}"),
                    other => panic!("{name} answered {other:?} through the held listing"),
                }
            }
        })
        .await;
}

/// MKDIR caches its directory's empty listing with the directory's log installed, so a file
/// created in it is found through the log with no build.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_in_a_directory_mkdir_made_is_found_with_no_build() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let made_node = node_at(&instance, &fs, "made").await;
            assert_ne!(
                log_capacity(&instance, &fs, made_node).await,
                0,
                "MKDIR's empty listing installed the directory's log"
            );
            let built = fs.listings_built();
            let created = create_with(&fs, made, "new.txt", b"n").await;
            assert_eq!(lookup(&fs, made, "new.txt").await, Ok(created));
            assert_eq!(fs.listings_built(), built, "found through the log");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_in_a_listed_directory_is_found_without_a_build() {
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let built = fs.listings_built();
            let created = create_with(&fs, dir, "new.txt", b"n").await;
            assert_eq!(lookup(&fs, dir, "new.txt").await, Ok(created));
            assert_eq!(fs.listings_built(), built);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_answers_enoent_without_a_build() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let built = fs.listings_built();
            fs.unlink(&request(), dir, b"a.txt").await.expect("unlink");
            assert_eq!(lookup(&fs, dir, "a.txt").await, Err(Errno::ENOENT));
            lookup(&fs, dir, "b.txt").await.expect("b.txt");
            assert_eq!(fs.listings_built(), built);
        })
        .await;
}

/// A listed name is answered from its record and spelling alone: a detach and more attaches
/// than the log holds since the build leave its LOOKUP without a build.
#[tokio::test(flavor = "multi_thread")]
async fn a_listed_name_after_a_detach_and_an_overflow_answers_without_a_build() {
    let instance = Instance::new(&[
        ("dir/a.txt", b"a"),
        ("dir/b.txt", b"b"),
        ("dir/c.txt", b"c"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let a = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let built = fs.listings_built();

            fs.unlink(&request(), dir, b"c.txt").await.expect("unlink");
            for i in 0..1_100 {
                attach(&instance, &fs, dir_node, &format!("n{i:04}")).await;
            }
            assert!(
                log_capacity(&instance, &fs, dir_node).await < 1_100,
                "the attaches overflow the log"
            );

            assert_eq!(lookup(&fs, dir, "a.txt").await, Ok(a));
            lookup(&fs, dir, "b.txt").await.expect("b.txt");
            assert_eq!(fs.listings_built(), built, "the listed names cost no build");
        })
        .await;
}

/// A node the log names for a name, moved out of the directory and replaced under that name
/// between the log's scan and the read of its record, answers as the chain holds the name: the
/// record no longer names it, so the lookup reads the log again and finds the replacement.
#[tokio::test(flavor = "multi_thread")]
async fn added_child_of_a_moved_node_answers_as_the_chain_holds_it() {
    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("other/o.txt", b"o")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let other_node = node_at(&instance, &fs, "other").await;
            let moved = attach(&instance, &fs, dir_node, "m.o").await;
            let built = fs.listings_built();
            let state = fs.state();
            let repository = instance.fixture.repository.clone();
            let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            fs.set_lookup_hook(Arc::new(move |step| {
                let (state, repository, fired) = (state.clone(), repository.clone(), fired.clone());
                Box::pin(async move {
                    if !matches!(step, LookupStep::Added { node, .. } if node == moved)
                        || fired.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    state
                        .node_reattach(repository.clone(), moved, other_node, "m.o")
                        .await
                        .expect("move out");
                    state
                        .node_attach(repository, dir_node, file_node(3, 9), "m.o")
                        .await
                        .expect("replace");
                })
            }));
            let found = lookup(&fs, dir, "m.o").await.expect("the replacement");
            assert_eq!(
                fs.getattr(&request(), found).await.expect("attr").attr.size,
                3,
                "the replacement, not the moved node"
            );
            assert_eq!(fs.listings_built(), built);
        })
        .await;
}

/// A name that takes a new node between every log scan and record read of a LOOKUP ends the
/// lookup in a bounded number of passes: one more read of the log, then counted rebuilds, the
/// first of which lists the name's holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_replaced_under_every_lookup_pass_ends_in_bounded_passes() {
    use lore_revision::fs::fuse::filesystem::LookupStep;
    // Far past any bound the lookup keeps; the churn stops here so an unbounded lookup ends.
    const CHURN_CAP: usize = 50;
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("other/o.txt", b"o")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let other_node = node_at(&instance, &fs, "other").await;
            attach(&instance, &fs, dir_node, "m.o").await;
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let passes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = passes.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                let (state, repository, counted) =
                    (state.clone(), repository.clone(), counted.clone());
                Box::pin(async move {
                    let LookupStep::Added { node, .. } = step else {
                        return;
                    };
                    let pass = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if pass >= CHURN_CAP {
                        return;
                    }
                    state
                        .node_reattach(repository.clone(), node, other_node, &format!("m{pass}.o"))
                        .await
                        .expect("move out");
                    state
                        .node_attach(repository, dir_node, file_node(3, 9), "m.o")
                        .await
                        .expect("replace");
                })
            }));
            lookup(&fs, dir, "m.o").await.expect("the name's holder");
            let passes = passes.load(std::sync::atomic::Ordering::SeqCst);
            assert!(
                passes <= 2,
                "{passes} passes read a patched node: one, one more read of the log, then a rebuild"
            );
        })
        .await;
}

/// An UNLINK of a name no inode holds, whose lookup by name meets a new node at the name on
/// every pass, gives up with `EAGAIN` after its counted passes rather than following the churn.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_whose_name_is_replaced_under_every_lookup_pass_answers_eagain() {
    use lore_revision::fs::fuse::filesystem::LookupStep;
    // Far past any bound the lookup keeps; the churn stops here so an unbounded lookup ends.
    const CHURN_CAP: usize = 50;
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("other/o.txt", b"o")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let other_node = node_at(&instance, &fs, "other").await;
            attach(&instance, &fs, dir_node, "m.o").await;
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let passes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = passes.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                let (state, repository, counted) =
                    (state.clone(), repository.clone(), counted.clone());
                Box::pin(async move {
                    let LookupStep::Added { node, .. } = step else {
                        return;
                    };
                    let pass = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if pass >= CHURN_CAP {
                        return;
                    }
                    state
                        .node_reattach(repository.clone(), node, other_node, &format!("m{pass}.o"))
                        .await
                        .expect("move out");
                    state
                        .node_attach(repository, dir_node, file_node(3, 9), "m.o")
                        .await
                        .expect("replace");
                })
            }));

            let unlinked = fs.unlink(&request(), dir, b"m.o").await;

            let passes = passes.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(unlinked, Err(Errno::EAGAIN), "after {passes} passes");
            assert!(passes < CHURN_CAP, "{passes} passes followed the churn");
        })
        .await;
}

/// A LOOKUP that makes a new inode for a node the log names reads the node's name a second time
/// to copy it; a rename landing between the two reads leaves the name it asked for absent.
#[tokio::test(flavor = "multi_thread")]
async fn the_missing_path_rechecks_the_fold_under_a_racing_rename() {
    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let racing = attach(&instance, &fs, dir_node, "racing.o").await;
            let state = fs.state();
            let repository = instance.fixture.repository.clone();
            let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = fired.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                let (state, repository, fired) = (state.clone(), repository.clone(), fired.clone());
                Box::pin(async move {
                    if !matches!(step, LookupStep::Missing { node, .. } if node == racing)
                        || fired.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    state
                        .node_reattach(repository, racing, dir_node, "renamed.o")
                        .await
                        .expect("rename");
                })
            }));
            assert_eq!(lookup(&fs, dir, "racing.o").await, Err(Errno::ENOENT));
            assert!(
                seen.load(std::sync::atomic::Ordering::SeqCst),
                "the rename landed between the two reads"
            );
            lookup(&fs, dir, "renamed.o")
                .await
                .expect("found under its new name");
        })
        .await;
}

/// A directory replaced under its name by another directory node since its parent's listing
/// takes a new inode: the one the kernel held stands for the node that left.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_replaced_under_its_name_takes_a_new_inode() {
    let instance = Instance::new(&[("d/a.txt", b"a"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let old = lookup(&fs, ROOT_INO, "d").await.expect("d");
            lookup(&fs, old, "a.txt").await.expect("a.txt");
            let repository = instance.fixture.repository.clone();
            let d = node_at(&instance, &fs, "d").await;
            fs.state()
                .node_detach(repository.clone(), d, Detach::Always)
                .await
                .expect("detach d");
            let replacement = match fs
                .state()
                .node_attach(repository, ROOT_NODE, Node::default(), "d")
                .await
                .expect("attach")
            {
                AttachOutcome::Attached(node) => node,
                AttachOutcome::Exists(_) => panic!("d is held"),
            };

            let new = lookup(&fs, ROOT_INO, "d").await.expect("the new d");

            assert_ne!(new, old, "the replaced directory's inode is not reused");
            assert_eq!(
                fs.inodes().get(new).map(|entry| entry.node),
                Some(replacement)
            );
            assert_eq!(lookup(&fs, new, "a.txt").await, Err(Errno::ENOENT));
        })
        .await;
}

/// A LOOKUP in a directory detached since its listing was built finds nothing there: neither a
/// listed child nor one attached since.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_in_a_detached_directory_finds_nothing() {
    let instance = Instance::new(&[("d/a.txt", b"a"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            lookup(&fs, d, "a.txt").await.expect("a.txt");
            let d_node = node_at(&instance, &fs, "d").await;
            attach(&instance, &fs, d_node, "new.o").await;
            fs.state()
                .node_detach(instance.fixture.repository.clone(), d_node, Detach::Always)
                .await
                .expect("detach d");

            assert_eq!(lookup(&fs, d, "a.txt").await, Err(Errno::ENOENT), "listed");
            assert_eq!(
                lookup(&fs, d, "new.o").await,
                Err(Errno::ENOENT),
                "attached since the listing"
            );
        })
        .await;
}

/// A respelling in place of `name` in directory `dir_node`, which leaves every listing of the
/// directory stale for every name.
async fn respell(instance: &Instance, fs: &FuseFilesystem, dir_node: NodeID, path: &str, to: &str) {
    let node = node_at(instance, fs, path).await;
    fs.state()
        .node_reattach(instance.fixture.repository.clone(), node, dir_node, to)
        .await
        .expect("respell in place");
}

/// A CREATE that meets a stale listing resolves its one name from the State and leaves the
/// build to the next LOOKUP or READDIR.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_over_a_stale_listing_resolves_by_name_and_builds_nothing() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/churn", b"c")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            respell(&instance, &fs, dir_node, "dir/churn", "CHURN").await;
            let built = fs.listings_built();
            let created = create_with(&fs, dir, "new.txt", b"n").await;
            assert_eq!(fs.listings_built(), built, "the CREATE built no listing");
            assert_eq!(lookup(&fs, dir, "new.txt").await, Ok(created));
        })
        .await;
}

/// A CREATE on the worker that meets a stale listing answers there: it resolves its one name from
/// the State, as the fetching path does, and builds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_create_over_a_stale_listing_answers_on_the_worker() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/churn", b"c")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            respell(&instance, &fs, dir_node, "dir/churn", "CHURN").await;
            let built = fs.listings_built();
            let Ok(created) = fs
                .create_local(
                    &request(),
                    dir,
                    b"new.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    (libc::O_CREAT | libc::O_RDWR) as u32,
                )
                .await
            else {
                panic!("the CREATE did not answer on the worker");
            };
            assert_eq!(fs.listings_built(), built, "the CREATE built no listing");
            assert_eq!(lookup(&fs, dir, "new.txt").await, Ok(created.entry.ino));
        })
        .await;
}

/// An UNLINK of a name the kernel holds no inode for meets a stale listing in both its resolve
/// and its lookup of the inode, and builds nothing in either.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_by_path_over_a_stale_listing_builds_nothing() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/churn", b"c")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "churn").await.expect("churn");
            assert_eq!(
                fs.inodes().find_child(dir, hash_string("a.txt"), "a.txt"),
                None,
                "the test needs a.txt held by no inode"
            );
            let dir_node = node_at(&instance, &fs, "dir").await;
            respell(&instance, &fs, dir_node, "dir/churn", "CHURN").await;
            let built = fs.listings_built();
            fs.unlink(&request(), dir, b"a.txt").await.expect("unlink");
            assert_eq!(fs.listings_built(), built, "the UNLINK built no listing");
            assert_eq!(lookup(&fs, dir, "a.txt").await, Err(Errno::ENOENT));
        })
        .await;
}

/// Changes landing between a build's log mark and its walk are replayed from the log: the
/// listing's position precedes them, an attach the walk saw is shadowed by its entry, a detach it
/// missed answers absent, and neither costs another build.
#[tokio::test(flavor = "multi_thread")]
async fn patches_landing_during_a_build_are_replayed() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir_node = node_at(&instance, &fs, "dir").await;
            let a = node_at(&instance, &fs, "dir/a.txt").await;
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = fired.clone();
            fs.set_walk_hook(Arc::new(move |node| {
                let (state, repository, fired) = (state.clone(), repository.clone(), fired.clone());
                Box::pin(async move {
                    if node != dir_node || fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    state
                        .node_attach(repository.clone(), dir_node, file_node(5, 2), "during.o")
                        .await
                        .expect("attach during the build");
                    state
                        .node_detach(repository, a, Detach::Always)
                        .await
                        .expect("detach during the build");
                })
            }));
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let built = fs.listings_built();
            lookup(&fs, dir, "b.txt").await.expect("b.txt");
            assert!(
                seen.load(std::sync::atomic::Ordering::SeqCst),
                "the hook ran in the build"
            );
            let cached = fs
                .served()
                .listings
                .get(dir_node)
                .expect("the build cached its listing");
            assert!(
                !cached.is_current(),
                "the listing's log position precedes the changes made during its build"
            );
            assert!(
                cached.find(b"a.txt").is_none()
                    && matches!(
                        cached.lookup("a.txt"),
                        lore_revision::fs::fuse::listing::Hit::Miss
                    ),
                "a.txt, detached during the build, is absent from the entries and the log"
            );
            let during = lookup(&fs, dir, "during.o").await.expect("during.o");
            assert_eq!(
                fs.getattr(&request(), during)
                    .await
                    .expect("attr")
                    .attr
                    .size,
                5
            );
            assert_eq!(lookup(&fs, dir, "a.txt").await, Err(Errno::ENOENT));
            assert_eq!(
                fs.listings_built() - built,
                1,
                "one build, its gap replayed"
            );
        })
        .await;
}

/// Files in [`listed_big_directory`]'s directory.
const BIG_CHILDREN: usize = 2000;

/// Adds `count` files named `{prefix}{i}.o` below directory node `dir` through the State, with no
/// check of their names.
async fn add_files(
    instance: &Instance,
    fs: &FuseFilesystem,
    dir: NodeID,
    prefix: &str,
    count: usize,
) {
    for i in 0..count {
        let name = format!("{prefix}{i}.o");
        fs.state()
            .node_add(
                instance.fixture.repository.clone(),
                dir,
                Node {
                    flags: NodeFlags::File.bits(),
                    name_hash: hash_string(&name),
                    ..Node::default()
                },
                &name,
            )
            .await
            .expect("add a file");
    }
}

/// A directory `big` MKDIR made below the root, holding [`BIG_CHILDREN`] files the State added,
/// with a current listing a READDIR built: its inode and node.
async fn listed_big_directory(instance: &Instance, fs: &FuseFilesystem) -> (u64, NodeID) {
    let dir = fs
        .mkdir(&request(), ROOT_INO, b"big", libc::S_IFDIR | 0o755, 0o022)
        .await
        .expect("mkdir")
        .ino;
    let dir_node = node_at(instance, fs, "big").await;
    add_files(instance, fs, dir_node, "f", BIG_CHILDREN).await;
    assert_eq!(listed(fs, dir).await.len(), BIG_CHILDREN);
    (dir, dir_node)
}

/// What `work` answers, and the chain steps finds by name take on this thread while it runs.
async fn chain_steps<T>(work: impl std::future::Future<Output = T>) -> (T, u64) {
    use lore_revision::state::step_counting;
    // Left on: tests in this binary count at once, and one turning counting off would cut
    // another's count short.
    step_counting::set_chain_step_counting(true);
    let before = step_counting::thread_chain_steps();
    let answer = work.await;
    (answer, step_counting::thread_chain_steps() - before)
}

/// Runs `race` on the State to its end inside the first CREATE's attach in directory node `dir`,
/// after the CREATE's resolve, the CREATE waiting on it; answers whether it ran. The race runs on
/// another thread, so its own steps are not the CREATE's.
fn race_the_attach<F, Fut>(
    fs: &Arc<FuseFilesystem>,
    dir: NodeID,
    race: F,
) -> Arc<std::sync::atomic::AtomicBool>
where
    F: FnOnce(Arc<lore_revision::state::State>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let race = std::sync::Mutex::new(Some(race));
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let state = fs.state();
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let ran = ran.clone();
        move |step| {
            if let WriteStep::Attaching { dir: at } = step
                && at == dir
                && let Some(race) = race.lock().expect("the race").take()
            {
                let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
                let state = state.clone();
                runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        race(state).await;
                        let _ = done_tx.send(());
                    },
                ));
                tokio::task::block_in_place(|| {
                    if done_rx.recv_timeout(Duration::from_secs(30)).is_ok() {
                        ran.store(true, Ordering::SeqCst);
                    }
                });
            }
            true
        }
    }));
    ran
}

/// CREATE's flags in these tests.
const CREATE_FLAGS: u32 = (libc::O_CREAT | libc::O_RDWR) as u32;

/// H11: an attach landing in a directory while its RMDIR's emptiness walk passes over it keeps the
/// directory: the chain the walk read before it began no longer holds, so the RMDIR checks again,
/// on the worker and with fetching alike, before any detach or host step, finds the entry and
/// answers `ENOTEMPTY`. The directories are the revision's, with no host directory, so the host
/// takes no part in the answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_attach_landing_during_an_rmdirs_emptiness_walk_keeps_the_directory() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance =
        Instance::new(&[("top.txt", b"top"), ("d1/h.txt", b"h"), ("d2/h.txt", b"h")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            // Each directory's only child, h.txt, is staged for deletion, so the walk passes it.
            let mut shapes = Vec::new();
            for dir in ["d1", "d2"] {
                let ino = lookup(&fs, ROOT_INO, dir).await.expect("the directory");
                lookup(&fs, ino, "h.txt").await.expect("h.txt");
                let (dir_node, h) = (
                    node_at(&instance, &fs, dir).await,
                    node_at(&instance, &fs, &format!("{dir}/h.txt")).await,
                );
                fs.state()
                    .node_delete(repository.clone(), h)
                    .await
                    .expect("stage h.txt for deletion");
                shapes.push((dir_node, h, Arc::new(AtomicBool::new(false))));
            }
            let detaches = Arc::new(AtomicUsize::new(0));
            let (counted, removed) = (detaches.clone(), [shapes[0].0, shapes[1].0]);
            fs.set_write_hook(Arc::new(move |step| {
                if let WriteStep::Detaching { node } = step
                    && removed.contains(&node)
                {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                true
            }));
            let (state, hooked) = (fs.state(), shapes.clone());
            fs.set_emptiness_hook(Arc::new(move |child| {
                let landing = hooked
                    .iter()
                    .find(|(_, h, done)| *h == child && !done.swap(true, Ordering::SeqCst))
                    .map(|(dir, _, _)| *dir);
                let (state, repository) = (state.clone(), repository.clone());
                Box::pin(async move {
                    if let Some(dir) = landing {
                        state
                            .node_attach(repository, dir, file_node(5, 2), "t.o")
                            .await
                            .expect("an attach behind the walk");
                    }
                })
            }));

            let local = fs
                .rmdir_local(&request(), ROOT_INO, b"d1")
                .await
                .map_err(Errno::from);
            assert!(
                shapes[0].2.load(Ordering::SeqCst),
                "the attach landed in the d1 walk"
            );
            assert_eq!(
                local,
                Err(Errno::ENOTEMPTY),
                "the worker's check finds the entry"
            );
            let fetched = fs.rmdir(&request(), ROOT_INO, b"d2").await;
            assert!(
                shapes[1].2.load(Ordering::SeqCst),
                "the attach landed in the d2 walk"
            );
            assert_eq!(
                fetched,
                Err(Errno::ENOTEMPTY),
                "the fetching check finds the entry"
            );
            assert_eq!(
                detaches.load(Ordering::SeqCst),
                0,
                "a check whose chain moved under it is followed by another, not by a detach"
            );
            for dir in ["d1", "d2"] {
                node_at(&instance, &fs, &format!("{dir}/t.o")).await;
            }
        })
        .await;
}

/// A RENAME over a directory whose only live child a reattach moves behind the emptiness walk, to
/// the head the walk has passed, answers `ENOTEMPTY` and keeps the child: the check walks again
/// where a reattach moved the chain, the only guard a rename over a directory has. The directory
/// is the revision's, with no host directory, so the host takes no part in the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_over_a_directory_a_reattach_passes_answers_enotempty() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let instance =
        Instance::new(&[("top.txt", b"top"), ("d/a.txt", b"a"), ("d/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            lookup(&fs, d, "a.txt").await.expect("a.txt");
            lookup(&fs, d, "b.txt").await.expect("b.txt");
            fs.mkdir(&request(), ROOT_INO, b"s", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            let dir_node = node_at(&instance, &fs, "d").await;
            let (a, b) = (
                node_at(&instance, &fs, "d/a.txt").await,
                node_at(&instance, &fs, "d/b.txt").await,
            );
            // The child heading the chain is staged for deletion, so the walk passes it first;
            // the other is the live one the reattach moves behind the walk.
            let head = fs
                .state()
                .node(repository.clone(), dir_node)
                .await
                .expect("d's record")
                .child();
            let (passed, live) = if head == Some(a) { (a, b) } else { (b, a) };
            fs.state()
                .node_delete(repository.clone(), passed)
                .await
                .expect("stage the head for deletion");
            let moved = Arc::new(AtomicBool::new(false));
            let (state, done) = (fs.state(), moved.clone());
            fs.set_emptiness_hook(Arc::new(move |child| {
                let first = child == passed && !done.swap(true, Ordering::SeqCst);
                let (state, repository) = (state.clone(), repository.clone());
                Box::pin(async move {
                    if first {
                        state
                            .node_reattach(repository, live, dir_node, "moved.txt")
                            .await
                            .expect("rename the live child behind the walk");
                    }
                })
            }));

            let renamed = fs
                .rename(&request(), ROOT_INO, b"s", ROOT_INO, b"d", 0)
                .await;

            assert!(
                moved.load(Ordering::SeqCst),
                "the reattach landed behind the walk"
            );
            assert_eq!(renamed, Err(Errno::ENOTEMPTY));
            assert_eq!(node_at(&instance, &fs, "d/moved.txt").await, live);
        })
        .await;
}

/// An RMDIR of a name whose directory node another replaced since the kernel took its inode
/// answers `EAGAIN` at once: the emptiness check reads the inode's directory, not the one the
/// name resolves to, so every detach would be refused. No detach is tried and the host directory
/// is never set aside.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_directory_replaced_under_its_inode_answers_eagain_at_once() {
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let old = fs
                .mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let host = std::fs::metadata(instance.write_dir().join("d"))
                .expect("write/d")
                .ino();
            let repository = instance.fixture.repository.clone();
            let d = node_at(&instance, &fs, "d").await;
            fs.state()
                .node_detach(repository.clone(), d, Detach::Always)
                .await
                .expect("detach d");
            let AttachOutcome::Attached(replacement) = fs
                .state()
                .node_attach(repository, ROOT_NODE, Node::default(), "d")
                .await
                .expect("attach")
            else {
                panic!("d is held");
            };
            let detaching = Arc::new(AtomicUsize::new(0));
            fs.set_write_hook(Arc::new({
                let detaching = detaching.clone();
                move |step| {
                    if matches!(step, WriteStep::Detaching { .. }) {
                        detaching.fetch_add(1, Ordering::SeqCst);
                    }
                    true
                }
            }));

            let answered = fs.rmdir(&request(), ROOT_INO, b"d").await;

            assert_eq!(
                fs.inodes().get(old).map(|entry| entry.node),
                Some(d),
                "the kernel's inode stands for the node that left"
            );
            assert_eq!(answered, Err(Errno::EAGAIN));
            assert_eq!(detaching.load(Ordering::SeqCst), 0, "no detach is tried");
            assert_eq!(
                std::fs::metadata(instance.write_dir().join("d"))
                    .expect("write/d stays")
                    .ino(),
                host,
                "the host directory is never set aside"
            );
            assert_eq!(node_at(&instance, &fs, "d").await, replacement);
        })
        .await;
}

/// An RMDIR whose detach finds the chain changed by an attach and a detach since its emptiness
/// check, the directory empty again, puts the host directory back, checks again and removes the
/// directory, from the State and the host. One changed on every attempt answers `EAGAIN` after its
/// checks, the directory kept and its host directory back with its mode and times.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_whose_detach_is_refused_checks_again() {
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::state::AttachOutcome;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for dir in ["once", "always"] {
                fs.mkdir(
                    &request(),
                    ROOT_INO,
                    dir.as_bytes(),
                    libc::S_IFDIR | 0o700,
                    0o022,
                )
                .await
                .expect("mkdir");
            }
            let stamped =
                std::time::SystemTime::UNIX_EPOCH + Duration::new(1_600_000_000, 125_000_000);
            std::fs::File::open(instance.write_dir().join("always"))
                .expect("the host directory")
                .set_modified(stamped)
                .expect("set the host mtime");
            let (once, always) = (
                node_at(&instance, &fs, "once").await,
                node_at(&instance, &fs, "always").await,
            );
            let attempts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let attempts = attempts.clone();
                move |step| {
                    let WriteStep::Detaching { node } = step else {
                        return true;
                    };
                    let slot = match node {
                        node if node == once => 0,
                        node if node == always => 1,
                        _ => return true,
                    };
                    if attempts[slot].fetch_add(1, Ordering::SeqCst) > 0 && slot == 0 {
                        return true;
                    }
                    // An attach and a detach in the directory, which leave it empty.
                    let (state, repository) = (state.clone(), repository.clone());
                    let (done_tx, done_rx) = std::sync::mpsc::channel();
                    runtime.spawn(async move {
                        let attached = state
                            .node_attach(repository.clone(), node, file_node(5, 2), "t.o")
                            .await
                            .expect("an attach in the directory");
                        let AttachOutcome::Attached(t) = attached else {
                            panic!("the name is free, got {attached:?}");
                        };
                        state
                            .node_detach(repository, t, Detach::Always)
                            .await
                            .expect("its detach");
                        let _ = done_tx.send(());
                    });
                    tokio::task::block_in_place(|| {
                        done_rx
                            .recv_timeout(Duration::from_secs(30))
                            .expect("the attach and detach land");
                    });
                    true
                }
            }));

            let removed = fs.rmdir(&request(), ROOT_INO, b"once").await;
            assert_eq!(removed, Ok(()));
            assert_eq!(
                attempts[0].load(Ordering::SeqCst),
                2,
                "the second check's detach removes it"
            );
            assert!(lookup(&fs, ROOT_INO, "once").await.is_err(), "once goes");
            assert!(
                !instance.write_dir().join("once").exists(),
                "write/once goes"
            );

            let kept = fs.rmdir(&request(), ROOT_INO, b"always").await;
            assert_eq!(kept, Err(Errno::EAGAIN));
            assert_eq!(
                attempts[1].load(Ordering::SeqCst),
                65,
                "one detach for each of the RMDIR's checks"
            );
            lookup(&fs, ROOT_INO, "always").await.expect("always stays");
            let host =
                std::fs::metadata(instance.write_dir().join("always")).expect("write/always");
            assert!(host.is_dir());
            assert_eq!(host.mode() & 0o7777, 0o700);
            assert_eq!(
                (host.mtime(), host.mtime_nsec()),
                (1_600_000_000, 125_000_000)
            );
        })
        .await;
}

/// H11: a CREATE whose attach lands in a directory between an RMDIR's emptiness check and its
/// detach keeps the directory: the detach finds the chain changed since the check read it, so the
/// RMDIR checks again, finds the new entry and answers `ENOTEMPTY`, the entry kept in the State and
/// on the host, and the host directory the CREATE made again with the directory's mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_landing_inside_an_rmdir_keeps_the_directory() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o700, 0o022)
                .await
                .expect("mkdir");
            // A mode no host directory made again takes under any usual umask.
            std::fs::set_permissions(
                instance.write_dir().join("made"),
                std::os::unix::fs::PermissionsExt::from_mode(0o711),
            )
            .expect("chmod write/made");
            let made = lookup(&fs, ROOT_INO, "made").await.expect("made");
            let made_node = node_at(&instance, &fs, "made").await;
            let (created_tx, created_rx) = std::sync::mpsc::channel::<bool>();
            let rmdir_side = std::sync::Mutex::new(Some(created_tx));
            let runtime = tokio::runtime::Handle::current();
            let creator = Arc::downgrade(&fs);
            fs.set_write_hook(Arc::new(move |step| {
                // Once the RMDIR has checked the directory empty, a CREATE in it runs to its end.
                if let WriteStep::Detaching { node } = step
                    && node == made_node
                    && let Some(created) = rmdir_side.lock().expect("side").take()
                {
                    let creator = creator.clone();
                    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
                    runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let fs = creator.upgrade().expect("the provider");
                            let answer = fs
                                .create(
                                    &request(),
                                    made,
                                    b"x.o",
                                    libc::S_IFREG | 0o644,
                                    0o022,
                                    CREATE_FLAGS,
                                )
                                .await;
                            let _ = created.send(answer.is_ok());
                            let _ = done_tx.send(());
                        },
                    ));
                    tokio::task::block_in_place(|| {
                        done_rx
                            .recv_timeout(Duration::from_secs(30))
                            .expect("the CREATE finishes");
                    });
                }
                true
            }));

            let removed = fs.rmdir(&request(), ROOT_INO, b"made").await;
            let created = created_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the CREATE answers");

            assert!(created, "the CREATE lands");
            assert_eq!(removed, Err(Errno::ENOTEMPTY), "the RMDIR finds the entry");
            lookup(&fs, made, "x.o").await.expect("x.o stays");
            assert!(instance.write_dir().join("made/x.o").is_file());
            let host = std::fs::metadata(instance.write_dir().join("made")).expect("write/made");
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(&host.permissions()) & 0o7777,
                0o711,
                "the host directory the CREATE made again takes the directory's mode back"
            );
        })
        .await;
}

/// H11: a CREATE in a directory an RMDIR removes, landing between the RMDIR's removal of the host
/// directory and its detach, makes the host directory again for its entry; once its attach finds
/// the directory gone, it removes the entry and the directory, so no host directory is left for
/// the next mount to take as the user's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_losing_to_an_rmdir_removes_the_host_directory_it_made() {
    creates_losing_to_an_rmdir(1, false, false).await;
}

/// H11: a CREATE in the worker's form losing to an RMDIR, which reads the State only in memory,
/// removes the host directory it made as the fetching form does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_on_the_worker_losing_to_an_rmdir_removes_the_host_directory_it_made() {
    creates_losing_to_an_rmdir(1, true, false).await;
}

/// H11: a CREATE and a MKDIR losing to one RMDIR, the first making the host directory again and
/// the second finding it made: whichever removes its entry last removes the directory too. The
/// CREATE runs in the worker's form.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_creates_losing_to_an_rmdir_leave_no_host_directory() {
    creates_losing_to_an_rmdir(2, true, false).await;
}

/// H11: a CREATE losing to an RMDIR while a request moving a node to the directory's name claims
/// it removes its entry and keeps the host directory, which may be the one that request moved
/// there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_losing_to_an_rmdir_keeps_the_host_directory_a_claim_holds() {
    creates_losing_to_an_rmdir(1, false, true).await;
}

/// H11: a CREATE in a directory a RENAME replaces, its entry made in the renamed directory's host
/// directory, which the rename has moved there, and its attach failing once the rename replaced
/// the directory, removes its entry and keeps the host directory, which the renamed directory now
/// owns: the State holds the place, whether or not any inode for the renamed directory is still
/// known. The CREATE runs in the worker's form, which reads the State only in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_losing_to_a_rename_over_its_directory_keeps_the_renamed_host_directory() {
    use std::os::unix::fs::MetadataExt as _;

    use lore_fuse::filesystem::Filesystem as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir d");
            let s = fs
                .mkdir(&request(), ROOT_INO, b"s", libc::S_IFDIR | 0o700, 0o022)
                .await
                .expect("mkdir s")
                .ino;
            let s_host = std::fs::metadata(instance.write_dir().join("s"))
                .expect("write/s")
                .ino();
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let d_node = node_at(&instance, &fs, "d").await;
            let (at_attach_tx, at_attach_rx) = std::sync::mpsc::channel::<()>();
            let (created_tx, created_rx) = std::sync::mpsc::channel::<bool>();
            let renamed = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let rename_side = std::sync::Mutex::new(Some((at_attach_rx, created_tx)));
            let create_side = std::sync::Mutex::new(at_attach_tx);
            let runtime = tokio::runtime::Handle::current();
            let creator = Arc::downgrade(&fs);
            let held = renamed.clone();
            fs.set_write_hook(Arc::new(move |step| {
                match step {
                    // The rename has moved s's host directory to d's: a CREATE in d runs to its
                    // attach.
                    WriteStep::Rename => {
                        let Some((at_attach, created)) = rename_side.lock().expect("side").take()
                        else {
                            return true;
                        };
                        let creator = creator.clone();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let fs = creator.upgrade().expect("the provider");
                                let answer = fs
                                    .create_local(
                                        &request(),
                                        d,
                                        b"x.o",
                                        libc::S_IFREG | 0o644,
                                        0o022,
                                        CREATE_FLAGS,
                                    )
                                    .await;
                                let _ = created.send(answer.is_ok());
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            at_attach
                                .recv_timeout(Duration::from_secs(30))
                                .expect("the CREATE reaches its attach");
                        });
                    }
                    // The CREATE's attach waits for the rename to finish.
                    WriteStep::Attaching { dir } if dir == d_node => {
                        let _ = create_side.lock().expect("side").send(());
                        tokio::task::block_in_place(|| {
                            let (finished, ended) = &*held;
                            let finished = finished.lock().expect("the flag");
                            let (_finished, waited) = ended
                                .wait_timeout_while(finished, Duration::from_secs(30), |done| {
                                    !*done
                                })
                                .expect("the rename finishes");
                            assert!(!waited.timed_out(), "the rename finishes in time");
                        });
                    }
                    _ => {}
                }
                true
            }));

            fs.rename(&request(), ROOT_INO, b"s", ROOT_INO, b"d", 0)
                .await
                .expect("the rename over d");
            // As an instance write leaves it: no inode for the renamed directory is known.
            fs.forget(s, 1);
            *renamed.0.lock().expect("the flag") = true;
            renamed.1.notify_all();
            let created = tokio::task::block_in_place(|| {
                created_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the CREATE answers")
            });

            assert!(!created, "the CREATE loses to the rename");
            let host = std::fs::metadata(instance.write_dir().join("d"))
                .expect("the renamed directory keeps its host directory");
            assert_eq!(host.ino(), s_host, "the host directory is s's");
            assert_eq!(host.mode() & 0o7777, 0o700);
            assert!(
                !instance.write_dir().join("d/x.o").exists(),
                "the CREATE's entry goes"
            );
            lookup(&fs, ROOT_INO, "d")
                .await
                .expect("d, the renamed directory");
        })
        .await;
}

/// `creates` CREATEs in a directory an RMDIR removes, each run from the RMDIR's step before its
/// detach, one after another, and each held at its attach until the RMDIR has finished: the first
/// in the worker's form where `local`, the second a MKDIR. Where `claimed`, a request moving a node
/// to the directory's name claims it until they answer.
async fn creates_losing_to_an_rmdir(creates: usize, local: bool, claimed: bool) {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            let made = lookup(&fs, ROOT_INO, "made").await.expect("made");
            let made_node = node_at(&instance, &fs, "made").await;
            assert!(instance.write_dir().join("made").is_dir());
            let (at_attach_tx, at_attach_rx) = std::sync::mpsc::channel::<()>();
            let (created_tx, created_rx) = std::sync::mpsc::channel::<bool>();
            let removed = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let rmdir_side = std::sync::Mutex::new(Some((at_attach_rx, created_tx)));
            let create_side = std::sync::Mutex::new(at_attach_tx);
            let runtime = tokio::runtime::Handle::current();
            let creator = Arc::downgrade(&fs);
            let held = removed.clone();
            fs.set_write_hook(Arc::new(move |step| {
                match step {
                    // The RMDIR has taken the host directory: CREATEs in it run to their attach.
                    WriteStep::Detaching { node } if node == made_node => {
                        let Some((at_attach, created)) = rmdir_side.lock().expect("side").take()
                        else {
                            return true;
                        };
                        for i in 0..creates {
                            let (creator, created) = (creator.clone(), created.clone());
                            runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let fs = creator.upgrade().expect("the provider");
                                    // A MKDIR's host directory goes as one.
                                    let landed = if i == 1 {
                                        fs.mkdir(
                                            &request(),
                                            made,
                                            format!("x{i}.d").as_bytes(),
                                            libc::S_IFDIR | 0o755,
                                            0o022,
                                        )
                                        .await
                                        .is_ok()
                                    } else if local {
                                        fs.create_local(
                                            &request(),
                                            made,
                                            format!("x{i}.o").as_bytes(),
                                            libc::S_IFREG | 0o644,
                                            0o022,
                                            CREATE_FLAGS,
                                        )
                                        .await
                                        .is_ok()
                                    } else {
                                        fs.create(
                                            &request(),
                                            made,
                                            format!("x{i}.o").as_bytes(),
                                            libc::S_IFREG | 0o644,
                                            0o022,
                                            CREATE_FLAGS,
                                        )
                                        .await
                                        .is_ok()
                                    };
                                    let _ = created.send(landed);
                                },
                            ));
                            tokio::task::block_in_place(|| {
                                at_attach
                                    .recv_timeout(Duration::from_secs(30))
                                    .expect("the CREATE reaches its attach");
                            });
                        }
                    }
                    // Each CREATE's attach waits for the RMDIR to finish.
                    WriteStep::Attaching { dir } if dir == made_node => {
                        let _ = create_side.lock().expect("side").send(());
                        tokio::task::block_in_place(|| {
                            let (finished, ended) = &*held;
                            let finished = finished.lock().expect("the flag");
                            let (_finished, waited) = ended
                                .wait_timeout_while(finished, Duration::from_secs(30), |done| {
                                    !*done
                                })
                                .expect("the RMDIR finishes");
                            assert!(!waited.timed_out(), "the RMDIR finishes in time");
                        });
                    }
                    _ => {}
                }
                true
            }));

            fs.rmdir(&request(), ROOT_INO, b"made")
                .await
                .expect("the RMDIR");
            let claim = claimed.then(|| fs.claim_arriving(ROOT_INO, "made").expect("a claim"));
            *removed.0.lock().expect("the flag") = true;
            removed.1.notify_all();
            let answers: Vec<bool> = (0..creates)
                .map(|_| {
                    tokio::task::block_in_place(|| {
                        created_rx
                            .recv_timeout(Duration::from_secs(30))
                            .expect("each CREATE answers")
                    })
                })
                .collect();
            drop(claim);

            assert!(
                answers.iter().all(|created| !created),
                "every CREATE loses to the RMDIR"
            );
            if claimed {
                assert!(
                    instance.write_dir().join("made").is_dir(),
                    "the claimed place keeps its host directory"
                );
                assert!(
                    !instance.write_dir().join("made/x0.o").exists(),
                    "the CREATE's entry goes"
                );
            } else {
                assert!(
                    !instance.write_dir().join("made").exists(),
                    "no host directory is left"
                );
            }
        })
        .await;
}

/// What a CREATE's finds by name cost in a large directory: with a current listing none, since the
/// listing and the directory's log show the name absent and the attach checks the log under its
/// write locks; with an overflowed one, which a write request never rebuilds, the CREATE's resolve
/// and its attach each walk the chain.
#[tokio::test(flavor = "current_thread")]
async fn creates_over_an_overflowed_listing_walk_the_chain_by_name() {
    const CREATES: usize = 50;
    // Patches past the grown log's capacity, so the cached listing's position leaves its window.
    const OVERFLOW: usize = 65;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, dir_node) = listed_big_directory(&instance, &fs).await;
            let built = fs.listings_built();
            let creates = |phase: &'static str| {
                let fs = &fs;
                chain_steps(async move {
                    for i in 0..CREATES {
                        fs.create(
                            &request(),
                            dir,
                            format!("{phase}{i}.o").as_bytes(),
                            libc::S_IFREG | 0o644,
                            0o022,
                            CREATE_FLAGS,
                        )
                        .await
                        .expect("create");
                    }
                })
            };

            let ((), current) = creates("current").await;
            add_files(&instance, &fs, dir_node, "o", OVERFLOW).await;
            let ((), overflowed) = creates("overflowed").await;
            println!(
                "CHAIN STEPS over {CREATES} CREATEs in a directory of {}..{} children: \
                 {current} with a current listing, {overflowed} with an overflowed one",
                BIG_CHILDREN,
                BIG_CHILDREN + CREATES * 2 + OVERFLOW
            );
            assert_eq!(fs.listings_built(), built, "no CREATE builds a listing");
            assert_eq!(current, 0, "a CREATE over a current listing walks no chain");
            assert!(
                overflowed >= 2 * (CREATES * BIG_CHILDREN) as u64,
                "each CREATE's resolve and attach walk the chain once the listing has \
                 overflowed: {overflowed}"
            );
        })
        .await;
}

/// A RENAME to a name its directory's listing and log show free walks no chain by name: the
/// reattach checks the log under its write locks.
#[tokio::test(flavor = "current_thread")]
async fn a_rename_to_a_free_name_in_a_listed_directory_walks_no_chain() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, _) = listed_big_directory(&instance, &fs).await;
            create_with(&fs, dir, "mover.tmp", b"m").await;
            let (renamed, steps) =
                chain_steps(fs.rename(&request(), dir, b"mover.tmp", dir, b"mover.o", 0)).await;
            renamed.expect("rename");
            println!("CHAIN STEPS of a RENAME to a free name in {BIG_CHILDREN} children: {steps}");
            assert_eq!(steps, 0, "the RENAME walked the chain");
            lookup(&fs, dir, "mover.o").await.expect("the new name");
            assert_eq!(lookup(&fs, dir, "mover.tmp").await, Err(Errno::ENOENT));
        })
        .await;
}

/// A CREATE whose directory takes another name between the CREATE's resolve and its attach
/// publishes with no walk: the log since the resolve holds no attach of the CREATE's name.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_an_attach_of_another_name_publishes_without_a_walk() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, dir_node) = listed_big_directory(&instance, &fs).await;
            let repository = instance.fixture.repository.clone();
            let raced = race_the_attach(&fs, dir_node, move |state| async move {
                state
                    .node_attach(repository, dir_node, file_node(5, 2), "racer.o")
                    .await
                    .expect("the racing attach");
            });
            let (created, steps) = chain_steps(fs.create(
                &request(),
                dir,
                b"created.o",
                libc::S_IFREG | 0o644,
                0o022,
                CREATE_FLAGS,
            ))
            .await;
            let created = created.expect("create").entry.ino;
            assert!(
                raced.load(std::sync::atomic::Ordering::SeqCst),
                "the attach raced"
            );
            assert_eq!(steps, 0, "the CREATE walked the chain");
            assert_eq!(lookup(&fs, dir, "created.o").await, Ok(created));
            lookup(&fs, dir, "racer.o").await.expect("the racing name");
        })
        .await;
}

/// A CREATE whose name another attach takes between the CREATE's resolve and its attach answers
/// `EEXIST`, and the directory holds the name once.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_an_attach_of_the_same_name_answers_eexist() {
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let repository = instance.fixture.repository.clone();
            let raced = race_the_attach(&fs, dir_node, move |state| async move {
                state
                    .node_attach(repository, dir_node, file_node(5, 2), "same.o")
                    .await
                    .expect("the racing attach");
            });
            let (created, steps) = chain_steps(fs.create(
                &request(),
                dir,
                b"same.o",
                libc::S_IFREG | 0o644,
                0o022,
                CREATE_FLAGS,
            ))
            .await;
            assert!(
                raced.load(std::sync::atomic::Ordering::SeqCst),
                "the attach raced"
            );
            assert!(matches!(created, Err(Errno::EEXIST)), "the name is held");
            assert_eq!(steps, 0, "the log, not a walk, found the name held");
            assert_eq!(listed(&fs, dir).await, vec!["a.txt", "same.o"]);
        })
        .await;
}

/// The provider over a fresh rebuild of `instance` under a view excluding `globs`.
async fn viewed_provider(instance: &Instance, globs: &[&str]) -> Arc<FuseFilesystem> {
    use fuse_fixture::RepositoryContextCreationArgsExt as _;
    let mut filter = lore_revision::filter::Filter::default();
    for glob in globs {
        filter.view.add_exclusion(glob).expect("filter exclusion");
    }
    let viewed = Arc::new(lore_revision::repository::RepositoryContext::new(
        fuse_fixture::default_repository_creation_args(
            instance.fixture.immutable.clone(),
            instance.fixture.mutable.clone(),
        )
        .with_path(&instance.fixture.path)
        .with_id(instance.fixture.repository.id)
        .with_instance_id(instance.fixture.repository.instance_id)
        .with_filter(Arc::new(filter)),
    ));
    FuseFilesystem::new_writable(
        viewed,
        instance.fixture.state.clone(),
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
        instance.external.path(),
    )
    .await
    .expect("writable provider")
}

/// A name the view excludes is listed only hidden, and its node still holds the name: a CREATE
/// of it answers `EEXIST`.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_of_a_view_excluded_name_answers_eexist() {
    let instance = Instance::new(&[("keep.txt", b"k"), ("secret.txt", b"s")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = viewed_provider(&instance, &["secret.txt"]).await;
            assert_eq!(
                lookup(&fs, ROOT_INO, "secret.txt").await,
                Err(Errno::ENOENT),
                "the view hides secret.txt"
            );
            let created = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"secret.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    CREATE_FLAGS,
                )
                .await;
            assert!(
                matches!(created, Err(Errno::EEXIST)),
                "the State holds the name"
            );
            let children = fs
                .state()
                .node_children(instance.fixture.repository.clone(), ROOT_NODE)
                .await
                .expect("the root's children");
            assert_eq!(children.len(), 2, "keep.txt and secret.txt, once each");
        })
        .await;
}

/// A MKDIR of a name the view excludes needs no execution context of its caller's: the event
/// the exclusion reports goes out under the one the provider captured.
#[tokio::test(flavor = "multi_thread")]
async fn a_mkdir_of_a_view_excluded_name_needs_no_ambient_context() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            viewed_provider(&instance, &["hid/"]),
        )
        .await;

    let made = fs
        .mkdir(&request(), ROOT_INO, b"hid", libc::S_IFDIR | 0o755, 0o022)
        .await;

    assert!(made.is_ok_and(|entry| entry.ino != 0), "hid is made");
}

/// An RMDIR of a directory whose one child the view excludes needs no execution context of its
/// caller's: the event the exclusion reports goes out under the one the provider captured. The
/// view hides the child, so the directory is empty to the mount.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_meeting_a_view_excluded_child_needs_no_ambient_context() {
    let instance = Instance::new(&[("keep.txt", b"k"), ("dir/secret.txt", b"s")]).await;
    let fs = LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            viewed_provider(&instance, &["dir/secret.txt"]),
        )
        .await;
    lookup(&fs, ROOT_INO, "dir").await.expect("dir");

    let removed = fs.rmdir(&request(), ROOT_INO, b"dir").await;

    assert_eq!(removed, Ok(()));
}

/// A CREATE of a name its directory holds under another case answers `EEXIST` before it makes
/// anything, and so does one whose name an attach takes in another case inside the CREATE.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_of_a_name_held_in_another_case_answers_eexist() {
    let instance = Instance::new(&[("dir/Readme.md", b"r")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "Readme.md").await.expect("Readme.md");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let repository = instance.fixture.repository.clone();
            let raced = race_the_attach(&fs, dir_node, move |state| async move {
                state
                    .node_attach(repository, dir_node, file_node(5, 2), "Racer.o")
                    .await
                    .expect("the racing attach");
            });

            let held = fs
                .create(
                    &request(),
                    dir,
                    b"README.MD",
                    libc::S_IFREG | 0o644,
                    0o022,
                    CREATE_FLAGS,
                )
                .await
                .map(|reply| reply.entry.ino);
            assert_eq!(held, Err(Errno::EEXIST), "Readme.md holds the name");
            assert!(
                !raced.load(std::sync::atomic::Ordering::SeqCst),
                "the CREATE answered before it attached"
            );
            assert!(
                !instance.write_dir().join("dir").exists(),
                "nothing reached write/"
            );

            let taken = fs
                .create(
                    &request(),
                    dir,
                    b"RACER.O",
                    libc::S_IFREG | 0o644,
                    0o022,
                    CREATE_FLAGS,
                )
                .await
                .map(|reply| reply.entry.ino);
            assert!(
                raced.load(std::sync::atomic::Ordering::SeqCst),
                "the attach raced"
            );
            assert_eq!(taken, Err(Errno::EEXIST), "Racer.o took the name");
            assert!(!instance.write_dir().join("dir/RACER.O").exists());
            assert_eq!(listed(&fs, dir).await, vec!["Racer.o", "Readme.md"]);
        })
        .await;
}

/// A CREATE of a name whose inode outlived its node, as a detach through the State leaves it,
/// orphans that inode and makes its own: the old inode keeps serving what it held.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_over_a_name_whose_inode_outlived_its_node_orphans_that_inode() {
    let instance = Instance::new(&[("dir/x.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let old = lookup(&fs, dir, "x.txt").await.expect("x.txt");
            let x = node_at(&instance, &fs, "dir/x.txt").await;
            fs.state()
                .node_detach(instance.fixture.repository.clone(), x, Detach::Always)
                .await
                .expect("detach through the State");

            let created = create_with(&fs, dir, "x.txt", b"new").await;

            assert_ne!(created, old, "the CREATE made its own inode");
            assert_eq!(
                fs.inodes().get(old).map(|entry| entry.orphaned),
                Some(true),
                "the old inode is orphaned"
            );
            assert_eq!(lookup(&fs, dir, "x.txt").await, Ok(created));
            assert_eq!(read(&fs, created, 64).await, b"new");
            assert_eq!(read(&fs, old, 64).await, b"tracked");
        })
        .await;
}

/// A CREATE whose directory takes more changes between the CREATE's resolve and its attach than
/// the directory's log holds cannot rely on the absence its listing showed: the attach walks the
/// chain, and attaches.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_whose_log_left_its_window_walks_and_attaches() {
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let capacity = log_capacity(&instance, &fs, dir_node).await;
            let repository = instance.fixture.repository.clone();
            let raced = race_the_attach(&fs, dir_node, move |state| async move {
                for i in 0..=capacity {
                    let name = format!("w{i}.o");
                    state
                        .node_add(
                            repository.clone(),
                            dir_node,
                            Node {
                                flags: NodeFlags::File.bits(),
                                name_hash: hash_string(&name),
                                ..Node::default()
                            },
                            &name,
                        )
                        .await
                        .expect("a racing add");
                }
            });
            let (created, steps) = chain_steps(fs.create(
                &request(),
                dir,
                b"created.o",
                libc::S_IFREG | 0o644,
                0o022,
                CREATE_FLAGS,
            ))
            .await;
            let created = created.expect("create").entry.ino;
            assert!(
                raced.load(std::sync::atomic::Ordering::SeqCst),
                "the adds raced"
            );
            assert!(steps > 0, "the attach found its name by walking the chain");
            assert_eq!(lookup(&fs, dir, "created.o").await, Ok(created));
            let names = listed(&fs, dir).await;
            assert_eq!(
                names.len(),
                capacity + 3,
                "a.txt, the adds and created.o: {names:?}"
            );
        })
        .await;
}

/// What a LOOKUP and then a CREATE of `held.txt` in `dir` answer once `change` has rewritten the
/// record of the node holding it, in place, before `dir`'s listing is built.
async fn create_over_a_rewritten_child(
    change: impl FnOnce(&mut Node) + Send,
) -> (Result<u64, Errno>, Result<(), Errno>) {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/held.txt", b"h")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let held = node_at(&instance, &fs, "dir/held.txt").await;
            let block = fs
                .state()
                .block(
                    instance.fixture.repository.clone(),
                    lore_revision::node::NodeBlock::index(held),
                )
                .await
                .expect("held.txt's block");
            fs.state().node_edit_for_tests(&block, held, change);
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let looked_up = lookup(&fs, dir, "held.txt").await;
            let created = fs
                .create(
                    &request(),
                    dir,
                    b"held.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    CREATE_FLAGS,
                )
                .await
                .map(|_| ());
            (looked_up, created)
        })
        .await
}

/// A staged delete is left out of its directory's listing, but its node still holds its name in
/// the chain: a CREATE of the name answers `EEXIST`.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_of_a_name_a_staged_delete_holds_answers_eexist() {
    let (looked_up, created) = create_over_a_rewritten_child(|node| {
        node.flags |= NodeFlags::StagedDelete.bits();
    })
    .await;
    assert_eq!(
        looked_up,
        Err(Errno::ENOENT),
        "the listing leaves the staged delete out"
    );
    assert_eq!(
        created,
        Err(Errno::EEXIST),
        "the staged delete holds the name"
    );
}

/// A child whose name its block cannot give back is left out of its directory's listing, but its
/// node still holds the name's hash in the chain: a CREATE of the name answers `EEXIST`.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_of_a_name_whose_record_the_listing_cannot_name_answers_eexist() {
    let (looked_up, created) = create_over_a_rewritten_child(|node| {
        node.name_length = u32::MAX;
    })
    .await;
    assert_eq!(
        looked_up,
        Err(Errno::ENOENT),
        "the listing leaves the child out"
    );
    assert_eq!(
        created,
        Err(Errno::EEXIST),
        "the child holds the name's hash"
    );
}

/// An operation read of a name a stale listing cannot answer finds the name in the State, with
/// no build, and answers absent for what a listing leaves out: a staged delete and a name the
/// view excludes.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_read_by_name_over_a_stale_listing_answers_absent_for_what_listings_leave_out()
{
    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::listing::Hit;
    let instance = Instance::new(&[
        ("dir/a.txt", b"a"),
        ("dir/secret.txt", b"s"),
        ("dir/staged.txt", b"d"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = viewed_provider(&instance, &["dir/secret.txt"]).await;
            let repository = instance.fixture.repository.clone();
            let dir_node = node_at(&instance, &fs, "dir").await;
            let staged = node_at(&instance, &fs, "dir/staged.txt").await;
            let block = fs
                .state()
                .block(
                    repository.clone(),
                    lore_revision::node::NodeBlock::index(staged),
                )
                .await
                .expect("staged.txt's block");
            fs.state().node_edit_for_tests(&block, staged, |node| {
                node.flags |= NodeFlags::StagedDelete.bits();
            });
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            attach(&instance, &fs, dir_node, "added.o").await;
            let a = node_at(&instance, &fs, "dir/a.txt").await;
            fs.state()
                .node_reattach(repository.clone(), a, dir_node, "A.txt")
                .await
                .expect("respell a.txt");
            let listing = fs.served().listings.get(dir_node).expect("dir's listing");
            for name in ["secret.txt", "staged.txt", "added.o"] {
                assert!(
                    matches!(listing.lookup(name), Hit::Stale),
                    "{name}: the respelling leaves the listing unable to answer"
                );
            }
            drop(listing);

            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            let built = fs.listings_built();
            let info = async |at: &str| {
                operation
                    .file_info(&relative(at))
                    .await
                    .unwrap_or_else(|error| panic!("file_info of {at}: {error}"))
            };
            assert!(matches!(info("dir/secret.txt").await, FileInfo::NotExist));
            assert!(matches!(info("dir/staged.txt").await, FileInfo::NotExist));
            assert!(matches!(
                info("dir/added.o").await,
                FileInfo::File { size: 7, .. }
            ));
            assert_eq!(fs.listings_built(), built, "the State answered each name");
        })
        .await;
}

/// A node the log names for a name and detached between the log's scan and the read of its
/// record answers absent.
#[tokio::test(flavor = "multi_thread")]
async fn added_child_of_a_node_unlinked_after_the_scan_answers_enoent() {
    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let unlinked = attach(&instance, &fs, dir_node, "x.o").await;
            let built = fs.listings_built();
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = fired.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                let (state, repository, fired) = (state.clone(), repository.clone(), fired.clone());
                Box::pin(async move {
                    if !matches!(step, LookupStep::Added { node, .. } if node == unlinked)
                        || fired.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    state
                        .node_detach(repository, unlinked, Detach::Always)
                        .await
                        .expect("unlink after the scan");
                })
            }));
            assert_eq!(lookup(&fs, dir, "x.o").await, Err(Errno::ENOENT));
            assert!(
                seen.load(std::sync::atomic::Ordering::SeqCst),
                "the unlink landed after the scan"
            );
            assert_eq!(fs.listings_built(), built);
        })
        .await;
}

/// The names and stream offsets of the dirents in `page`.
fn dirents(page: &[u8]) -> Vec<(String, u64)> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 24 <= page.len() {
        let off = u64::from_ne_bytes(page[at + 8..at + 16].try_into().expect("u64"));
        let namelen = u32::from_ne_bytes(page[at + 16..at + 20].try_into().expect("u32")) as usize;
        entries.push((
            String::from_utf8_lossy(&page[at + 24..at + 24 + namelen]).into_owned(),
            off,
        ));
        at += (24 + namelen).div_ceil(8) * 8;
    }
    entries
}

/// A READDIR stream opened before creates and unlinks in its directory lists the directory as
/// its build held it, while lookups answer the changes through the log with no build.
#[tokio::test(flavor = "multi_thread")]
async fn readdir_pinned_across_creates_and_unlinks_lists_the_build_and_lookups_see_the_changes() {
    let files = ["a.txt", "b.txt", "c.txt", "d.txt"];
    let instance = Instance::new(&[
        ("dir/a.txt", b"a"),
        ("dir/b.txt", b"b"),
        ("dir/c.txt", b"c"),
        ("dir/d.txt", b"d"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let fh = fs.opendir(&request(), dir).expect("opendir").fh;
            // `.`, `..` and one five-byte name, 32 bytes each.
            let mut first = vec![0u8; 3 * 32];
            let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut first);
            fs.readdir(&request(), dir, fh, 0, &mut writer)
                .await
                .expect("page 1");
            let used = writer.finish();
            let page_one = dirents(&first[..used]);
            let built = fs.listings_built();

            let created = create_with(&fs, dir, "e.txt", b"e").await;
            let gone = *files
                .iter()
                .find(|name| page_one.iter().all(|(listed, _)| listed != *name))
                .expect("a name past the first page");
            fs.unlink(&request(), dir, gone.as_bytes())
                .await
                .expect("unlink");

            let mut rest = vec![0u8; 64 * 1024];
            let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut rest);
            let offset = page_one.last().expect("a first page").1;
            fs.readdir(&request(), dir, fh, offset, &mut writer)
                .await
                .expect("page 2");
            let used = writer.finish();
            fs.releasedir(dir, fh);
            let mut names: Vec<String> = page_one
                .into_iter()
                .chain(dirents(&rest[..used]))
                .map(|(name, _)| name)
                .filter(|name| name != "." && name != "..")
                .collect();
            names.sort();
            assert_eq!(
                names, files,
                "the stream lists the directory as its build held it"
            );

            assert_eq!(lookup(&fs, dir, "e.txt").await, Ok(created));
            assert_eq!(lookup(&fs, dir, gone).await, Err(Errno::ENOENT));
            assert_eq!(fs.listings_built(), built, "the lookups read the log");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_keeps_its_inode_number_when_its_address_changes() {
    let instance = Instance::new(&[("dir/a.txt", b"one")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let before = lookup(&fs, dir, "a.txt").await.expect("a.txt");

            let a = node_at(&instance, &fs, "dir/a.txt").await;
            let built = fs.listings_built();
            fs.state()
                .node_rewrite(
                    &instance.fixture.repository,
                    a,
                    &Content {
                        mode: 0,
                        size: 11,
                        address: address(9),
                    },
                    Rewrite::Plain,
                )
                .await
                .expect("modify");

            let after = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            assert_eq!(after, before, "the inode number survives the new address");
            assert_eq!(
                fs.getattr(&request(), after).await.expect("attr").attr.size,
                11
            );
            assert_eq!(
                fs.listings_built(),
                built,
                "the record change costs no build"
            );
        })
        .await;
}

/// A cached listing can be older than a child it lists; a lookup reads the child's record, and
/// answers what the record holds rather than what the listing remembers.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_through_a_listing_older_than_its_child_serves_the_child_record() {
    let instance = Instance::new(&[("dir/a.txt", b"one")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let before = lookup(&fs, dir, "a.txt").await.expect("a.txt");

            let a = node_at(&instance, &fs, "dir/a.txt").await;
            fs.state()
                .node_rewrite(
                    &instance.fixture.repository,
                    a,
                    &Content {
                        mode: 0,
                        size: 11,
                        address: address(9),
                    },
                    Rewrite::Plain,
                )
                .await
                .expect("modify");

            let after = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            assert_eq!(after, before);
            assert_eq!(
                fs.getattr(&request(), after).await.expect("attr").attr.size,
                11
            );
        })
        .await;
}

/// The names READDIR lists in `dir` through a fresh stream.
async fn listed(fs: &FuseFilesystem, dir: u64) -> Vec<String> {
    let fh = fs.opendir(&request(), dir).expect("opendir").fh;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut buffer);
    fs.readdir(&request(), dir, fh, 0, &mut writer)
        .await
        .expect("readdir");
    let used = writer.finish();
    fs.releasedir(dir, fh);
    let mut names = Vec::new();
    let mut at = 0;
    while at + 24 <= used {
        let namelen =
            u32::from_ne_bytes(buffer[at + 16..at + 20].try_into().expect("u32")) as usize;
        let name = String::from_utf8_lossy(&buffer[at + 24..at + 24 + namelen]).into_owned();
        if name != "." && name != ".." {
            names.push(name);
        }
        at += (24 + namelen).div_ceil(8) * 8;
    }
    names.sort();
    names
}

/// A detach moves its directory's chain change count, so a listing built before it is not
/// read again.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_after_a_detach_no_longer_lists_the_node() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            assert_eq!(listed(&fs, dir).await, vec!["a.txt", "b.txt"]);

            let a = node_at(&instance, &fs, "dir/a.txt").await;
            fs.state()
                .node_detach(instance.fixture.repository.clone(), a, Detach::Always)
                .await
                .expect("detach");
            assert_eq!(listed(&fs, dir).await, vec!["b.txt"]);
        })
        .await;
}

/// A listed node renamed within its directory keeps the directory but not the entry's name, so
/// the listing no longer counts it as the entry's node.
#[tokio::test(flavor = "multi_thread")]
async fn a_listed_node_renamed_within_its_directory_no_longer_holds_its_entry() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let a = node_at(&instance, &fs, "dir/a.txt").await;
            let listing = listing_now(&fs, dir_node, "dir").await;
            let index = listing.find(b"a.txt").expect("a.txt is listed");
            assert!(listing.holds_entry(index), "a.txt's node holds the entry");

            fs.state()
                .node_reattach(instance.fixture.repository.clone(), a, dir_node, "z.txt")
                .await
                .expect("rename within the directory");

            assert!(
                !listing.holds_entry(index),
                "the node left the entry's name"
            );
            assert!(listing.current_entry(index).is_none());
            fs.served().listings.insert(dir_node, listing);
            assert_eq!(lookup(&fs, dir, "a.txt").await, Err(Errno::ENOENT));
            lookup(&fs, dir, "z.txt").await.expect("the new name");
        })
        .await;
}

/// Writes `files` under `instance`'s `write/`, as earlier writes through a mount left them.
fn plant(instance: &Instance, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let at = instance.write_dir().join(path);
        std::fs::create_dir_all(at.parent().expect("a parent")).expect("parent directories");
        std::fs::write(&at, bytes).expect("a local file");
    }
}

/// Reads `len` bytes of `ino` from offset 0 through READ.
async fn read(fs: &FuseFilesystem, ino: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let Ok(read) = fs.read(&request(), ino, 0, 0, &mut out, true).await else {
        panic!("READ of inode {ino} failed");
    };
    out.truncate(read);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuilt_local_file_reads_back_the_bytes_under_write() {
    let instance = Instance::new(&[("a.txt", b"tracked"), ("dir/b.txt", b"b")]).await;
    plant(
        &instance,
        &[
            ("a.txt", b"edited locally"),
            ("dir/new.txt", b"added locally"),
        ],
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, a, 64).await, b"edited locally");
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            assert_eq!(lookup(&fs, dir, "NEW.txt").await, Err(Errno::ENOENT));
            let new = lookup(&fs, dir, "new.txt").await.expect("dir/new.txt");
            assert_eq!(read(&fs, new, 64).await, b"added locally");
            let b = lookup(&fs, dir, "b.txt").await.expect("dir/b.txt");
            assert_eq!(read(&fs, b, 64).await, b"b");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn getattr_of_a_local_file_reports_the_host_size_and_mtime() {
    let instance = Instance::new(&[("a.txt", b"tracked")]).await;
    plant(&instance, &[("a.txt", b"local"), ("new.o", b"object")]);
    let stamped = std::time::SystemTime::UNIX_EPOCH + Duration::new(1_650_000_000, 250_000_000);
    std::fs::File::options()
        .write(true)
        .open(instance.write_dir().join("new.o"))
        .expect("the host file")
        .set_modified(stamped)
        .expect("set the host mtime");
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(fs.getattr(&request(), a).await.expect("attr").attr.size, 5);
            let new = lookup(&fs, ROOT_INO, "new.o").await.expect("new.o");
            let attr = fs.getattr(&request(), new).await.expect("attr").attr;
            assert_eq!(attr.size, 6);
            assert_eq!((attr.mtime, attr.mtimensec), (1_650_000_000, 250_000_000));

            std::fs::write(instance.write_dir().join("new.o"), b"a longer object")
                .expect("rewrite");
            let attr = fs.getattr(&request(), new).await.expect("attr").attr;
            assert_eq!(attr.size, 15, "the size is the host file's now");
            assert!(
                attr.mtime > 1_650_000_000,
                "the mtime is the host file's now"
            );
        })
        .await;
}

/// GETATTR and READ of a local file whose descriptor is not cached open its host file by path
/// with no inode lock held: another thread takes the inode's write lock while the open waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn getattr_and_read_of_a_local_file_hold_no_inode_lock_across_the_host_open() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = create_with(&fs, ROOT_INO, "local.o", b"object").await;
            let opened = Arc::new(AtomicUsize::new(0));
            let locked = Arc::new(AtomicUsize::new(0));
            let weak = Arc::downgrade(&fs);
            fs.set_write_hook(Arc::new({
                let (opened, locked) = (opened.clone(), locked.clone());
                move |step| {
                    if step == (WriteStep::PathResolved { ino }) {
                        opened.fetch_add(1, Ordering::SeqCst);
                        let fs = weak.upgrade().expect("the provider");
                        let (done_tx, done_rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            drop(fs.inodes().get_mut(ino));
                            let _ = done_tx.send(());
                        });
                        if done_rx.recv_timeout(Duration::from_secs(10)).is_ok() {
                            locked.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    true
                }
            }));

            fs.close_local_file(ino);
            let attr = fs.getattr(&request(), ino).await.expect("getattr").attr;
            fs.close_local_file(ino);
            let content = read(&fs, ino, 64).await;

            assert_eq!(
                opened.load(Ordering::SeqCst),
                2,
                "GETATTR and READ each opened the host file by its path"
            );
            assert_eq!(
                locked.load(Ordering::SeqCst),
                2,
                "the inode's write lock was free while each open waited"
            );
            assert_eq!(attr.size, 6);
            assert_eq!(content, b"object");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_of_a_dirty_directory_holds_tracked_and_local_entries() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    plant(
        &instance,
        &[
            ("dir/a.txt", b"edited"),
            ("dir/c.txt", b"added"),
            ("dir/sub/d.txt", b"nested"),
        ],
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            assert_eq!(
                listed(&fs, dir).await,
                vec!["a.txt", "b.txt", "c.txt", "sub"]
            );
            let sub = lookup(&fs, dir, "sub").await.expect("dir/sub");
            assert_eq!(listed(&fs, sub).await, vec!["d.txt"]);
            let c = lookup(&fs, dir, "c.txt").await.expect("dir/c.txt");
            assert_eq!(read(&fs, c, 64).await, b"added");
        })
        .await;
}

fn relative(value: &str) -> lore_revision::util::path::RelativePath {
    lore_revision::util::path::RelativePath::new_from_initial_path(value).expect("path")
}

/// A listing build that read a tracked file before its copy-up and is cached after it, as a
/// build racing the copy-up leaves it: every reader takes the entry with the node's record as it
/// stands, so the copied-up file stays local and no write is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_cached_across_a_copy_up_serves_the_local_file() {
    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    let instance = Instance::new(&[("a.txt", b"tracked-old"), ("b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            let served = fs.served();
            let built = lore_revision::fs::fuse::listing::Listing::build(
                &served,
                ROOT_NODE,
                lore_revision::filter::FilterStates::ROOT,
                &lore_revision::util::path::RelativePath::new(),
            )
            .await
            .expect("build");
            fs.write(&request(), a, 0, 0, b"NEW-CONTENT", 0)
                .await
                .expect("write");
            served.listings.insert(ROOT_NODE, built);

            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            assert!(matches!(
                operation.file_info(&relative("a.txt")).await.expect("info"),
                FileInfo::File { size: 11, .. }
            ));
            let source = operation
                .content_source(&relative("a.txt"))
                .await
                .expect("content source");
            assert!(source.file_path().is_some(), "a.txt is read from write/");

            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let mut buffer = vec![0u8; 64 * 1024];
            let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut buffer);
            fs.readdirplus(&request(), ROOT_INO, fh, 0, &mut writer)
                .await
                .expect("readdirplus");
            fs.releasedir(ROOT_INO, fh);
            assert_eq!(read(&fs, a, 64).await, b"NEW-CONTENT");
            fs.write(&request(), a, 0, 11, b"+", 0)
                .await
                .expect("second write");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("host"),
                b"NEW-CONTENT+",
                "neither write is lost"
            );
        })
        .await;
}

/// A READDIRPLUS stream pinned before an UNLINK in its directory and read on after it counts
/// no inode for the removed name, wherever the name falls in the stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_readdirplus_stream_pinned_before_an_unlink_counts_no_inode_for_the_removed_name() {
    for gone in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        let instance = Instance::new(&[
            ("a.txt", b"a"),
            ("b.txt", b"b"),
            ("c.txt", b"c"),
            ("d.txt", b"d"),
        ])
        .await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
                let entry_size = std::mem::size_of::<lore_fuse::abi::EntryOut>() + 32;
                let mut first = vec![0u8; 2 * entry_size];
                let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut first);
                fs.readdirplus(&request(), ROOT_INO, fh, 0, &mut writer)
                    .await
                    .expect("page 1");
                fs.unlink(&request(), ROOT_INO, gone.as_bytes())
                    .await
                    .expect("unlink");
                let mut rest = vec![0u8; 64 * 1024];
                let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut rest);
                fs.readdirplus(&request(), ROOT_INO, fh, 2, &mut writer)
                    .await
                    .expect("page 2");
                fs.releasedir(ROOT_INO, fh);
                assert_eq!(
                    fs.inodes().find_child(ROOT_INO, hash_string(gone), gone),
                    None,
                    "{gone}: no inode for the removed name"
                );
            })
            .await;
    }
}

/// One READDIRPLUS page of stream `fh` on `dir` from `offset`, at most `capacity` bytes: each
/// record's name and node id, 0 for a plain one, and how many lookups the page counted.
async fn plus_page(
    fs: &FuseFilesystem,
    dir: u64,
    fh: u64,
    offset: u64,
    capacity: usize,
) -> (Vec<(String, u64)>, usize) {
    let mut page = vec![0u8; capacity];
    let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut page);
    let counted = fs
        .readdirplus(&request(), dir, fh, offset, &mut writer)
        .await
        .expect("readdirplus");
    let used = writer.finish();
    let entry_out = std::mem::size_of::<lore_fuse::abi::EntryOut>();
    let mut records = Vec::new();
    let mut at = 0;
    while at + entry_out + 24 <= used {
        let node = u64::from_ne_bytes(page[at..at + 8].try_into().expect("u64"));
        at += entry_out;
        let namelen = u32::from_ne_bytes(page[at + 16..at + 20].try_into().expect("u32")) as usize;
        let name = String::from_utf8_lossy(&page[at + 24..at + 24 + namelen]).into_owned();
        records.push((name, node));
        at += (24 + namelen).div_ceil(8) * 8;
    }
    (records, counted)
}

/// A READDIRPLUS stream on the root, its first page `.` and `..` alone, which pins the root's
/// listing as it stands with `files` created in it.
async fn root_stream_pinned_over(fs: &FuseFilesystem, files: &[&str]) -> u64 {
    for name in files {
        create_with(fs, ROOT_INO, name, name.as_bytes()).await;
    }
    let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
    let entry_size = std::mem::size_of::<lore_fuse::abi::EntryOut>() + 32;
    let (first, _) = plus_page(fs, ROOT_INO, fh, 0, 2 * entry_size).await;
    let first: Vec<&str> = first.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(first, [".", ".."], "the first page");
    fh
}

/// The local files the READDIRPLUS stream tests list, created through the mount so a RENAME of
/// one copies nothing up.
const STREAMED: [&str; 4] = ["a.txt", "b.txt", "c.txt", "d.txt"];

/// A READDIRPLUS stream pinned before a CREATE in its directory reads on with attributes: a
/// create detaches and respells nothing, so every row the stream lists is still a member.
#[tokio::test(flavor = "multi_thread")]
async fn a_readdirplus_stream_pinned_across_a_create_keeps_its_attributes() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let fh = root_stream_pinned_over(&fs, &STREAMED).await;
            create_with(&fs, ROOT_INO, "new.txt", b"n").await;
            let (rest, counted) = plus_page(&fs, ROOT_INO, fh, 2, 64 * 1024).await;
            fs.releasedir(ROOT_INO, fh);
            let mut names: Vec<&str> = rest.iter().map(|(name, _)| name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                ["a.txt", "b.txt", "c.txt", "d.txt", "keep.txt"],
                "the stream lists its build"
            );
            assert_eq!(counted, rest.len(), "every row keeps its attributes");
            assert!(
                rest.iter().all(|(_, node)| *node != 0),
                "a plain row: {rest:?}"
            );
        })
        .await;
}

/// A READDIRPLUS stream pinned before a RENAME takes a name out of its directory reads on plain,
/// wherever the name falls in the stream, and counts no inode under the old name.
#[tokio::test(flavor = "multi_thread")]
async fn readdirplus_of_a_pinned_stream_after_a_rename_away_is_plain() {
    for gone in STREAMED {
        let instance = Instance::new(&[("keep.txt", b"k")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let fh = root_stream_pinned_over(&fs, &STREAMED).await;
                fs.rename(
                    &request(),
                    ROOT_INO,
                    gone.as_bytes(),
                    ROOT_INO,
                    b"moved.txt",
                    0,
                )
                .await
                .expect("rename");
                let (rest, counted) = plus_page(&fs, ROOT_INO, fh, 2, 64 * 1024).await;
                fs.releasedir(ROOT_INO, fh);
                assert_eq!(
                    counted, 0,
                    "{gone}: rows counted after the rename: {rest:?}"
                );
                assert!(
                    rest.iter().all(|(_, node)| *node == 0),
                    "{gone}: a row with attributes: {rest:?}"
                );
                assert_eq!(
                    fs.inodes().find_child(ROOT_INO, hash_string(gone), gone),
                    None,
                    "{gone}: an inode under the old name"
                );
            })
            .await;
    }
}

/// A READDIRPLUS stream pinned before a RENAME respells a name only in case reads on plain,
/// wherever the name falls in the stream: no row reports a member under its old spelling.
#[tokio::test(flavor = "multi_thread")]
async fn readdirplus_of_a_pinned_stream_after_a_case_only_rename_is_plain() {
    for respelled in STREAMED {
        let instance = Instance::new(&[("keep.txt", b"k")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let fh = root_stream_pinned_over(&fs, &STREAMED).await;
                let upper = respelled.to_uppercase();
                fs.rename(
                    &request(),
                    ROOT_INO,
                    respelled.as_bytes(),
                    ROOT_INO,
                    upper.as_bytes(),
                    0,
                )
                .await
                .expect("respell");
                let (rest, counted) = plus_page(&fs, ROOT_INO, fh, 2, 64 * 1024).await;
                fs.releasedir(ROOT_INO, fh);
                assert_eq!(
                    counted, 0,
                    "{respelled}: rows counted after the respelling: {rest:?}"
                );
                assert!(
                    rest.iter().all(|(_, node)| *node == 0),
                    "{respelled}: a row with attributes: {rest:?}"
                );
            })
            .await;
    }
}

/// A READDIRPLUS reply the kernel aborts gives back the lookups it counted, though a RENAME
/// replaced one of its names between the count and the abort: each inode's count is back to what
/// it was before the request.
#[tokio::test(flavor = "multi_thread")]
async fn an_aborted_readdirplus_reply_gives_back_its_lookups_across_a_replacing_rename() {
    use zerocopy::IntoBytes as _;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let keep = lookup(&fs, ROOT_INO, "keep.txt").await.expect("keep.txt");
            let a = create_with(&fs, ROOT_INO, "a.txt", b"a").await;
            let b = create_with(&fs, ROOT_INO, "b.txt", b"b").await;
            let inodes = [a, b, keep];
            let lookups = move |fs: &FuseFilesystem| {
                inodes.map(|ino| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup))
            };
            let before = lookups(&fs);
            // A plain READDIR caches the root's listing, so the READDIRPLUS answers on the worker.
            let mut plain = vec![0u8; 4096];
            fs.readdir(
                &request(),
                ROOT_INO,
                0,
                0,
                &mut lore_fuse::codec::dirent::DirentWriter::new(&mut plain),
            )
            .await
            .expect("readdir");
            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;

            let (session, transport) = fuse_test_util::mock_session(&fs, 1).await;
            let counted = Arc::new(std::sync::Mutex::new(None));
            *transport.before_send.lock() = Some(Box::new({
                let fs = fs.clone();
                let counted = counted.clone();
                move || {
                    *counted.lock().expect("counted") = Some(lookups(&fs));
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(fs.rename(
                            &request(),
                            ROOT_INO,
                            b"b.txt",
                            ROOT_INO,
                            b"a.txt",
                            0,
                        ))
                    })
                    .expect("rename b.txt over a.txt");
                }
            }));
            transport
                .abort_next_send
                .store(true, std::sync::atomic::Ordering::SeqCst);
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
            // One worker answers in order: the GETATTR's reply follows the READDIRPLUS's abort.
            transport.push_request(fuse_test_util::raw_request(
                lore_fuse::abi::Opcode::Getattr,
                3,
                ROOT_INO,
                lore_fuse::abi::GetattrIn::default().as_bytes(),
            ));
            fuse_test_util::await_reply(&transport, 3).await;
            assert_eq!(session.deferred_requests(), 0, "answered on the worker");
            session.shutdown().await;
            fs.releasedir(ROOT_INO, fh);

            assert_eq!(
                *counted.lock().expect("counted"),
                Some(before.map(|n| n + 1)),
                "the reply counted every row before the rename"
            );
            assert_eq!(
                fs.inodes()
                    .find_child(ROOT_INO, hash_string("a.txt"), "a.txt"),
                Some(b),
                "the rename replaced a.txt"
            );
            assert_eq!(
                lookups(&fs),
                before,
                "a.txt's, b.txt's and keep.txt's lookups after the abort"
            );
        })
        .await;
}

/// RENAME onto a name a local file holds under another spelling replaces the host file of
/// that spelling, so `write/` holds only the new spelling and a rebuild serves the new content.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_onto_a_name_held_under_another_spelling_leaves_only_the_new_spelling() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let held = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"B.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
                .expect("create B.txt");
            fs.write(&request(), held.entry.ino, 0, 0, b"old", 0)
                .await
                .expect("write B.txt");
            let a = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"a.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
                .expect("create a.txt");
            fs.write(&request(), a.entry.ino, 0, 0, b"new", 0)
                .await
                .expect("write a.txt");
            fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"b.txt", 0)
                .await
                .expect("rename");
            assert_eq!(names(&instance.write_dir()), vec!["b.txt"]);
            let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, b, 64).await, b"new");

            let again = provider(&instance).await;
            let b = lookup(&again, ROOT_INO, "b.txt")
                .await
                .expect("b.txt again");
            assert_eq!(read(&again, b, 64).await, b"new");
        })
        .await;
}

/// A write hook failing the first State step of `step` it meets.
fn failing_once(
    step: lore_revision::fs::fuse::filesystem::WriteStep,
) -> lore_revision::fs::fuse::filesystem::WriteHook {
    let failed = std::sync::atomic::AtomicBool::new(false);
    Arc::new(move |at| at != step || failed.swap(true, std::sync::atomic::Ordering::SeqCst))
}

/// A copy-up whose State step fails takes its host file out of `write/` again: the file stays
/// tracked, and the next write copies it up whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_up_whose_state_step_fails_leaves_the_file_tracked() {
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("a.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.set_write_hook(failing_once(WriteStep::CopyUp));
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert!(fs.write(&request(), a, 0, 0, b"T", 0).await.is_err());
            assert!(
                names(&instance.write_dir()).is_empty(),
                "write/ holds nothing"
            );
            assert_eq!(read(&fs, a, 64).await, b"tracked");
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            let source = operation
                .content_source(&relative("a.txt"))
                .await
                .expect("content source");
            assert!(source.file_path().is_none(), "a.txt is read from the store");

            fs.write(&request(), a, 0, 0, b"T", 0)
                .await
                .expect("the next write");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("host"),
                b"Tracked"
            );
        })
        .await;
}

/// A rename whose State step fails puts both names back, in `write/` and in the State: the
/// source keeps its local bytes, and a tracked target its tracked content.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_whose_state_step_fails_puts_both_names_back() {
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("b.txt", b"tracked b"), ("c.txt", b"c")]).await;
    let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let created = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"a.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
                .expect("create a.txt");
            fs.write(&request(), created.entry.ino, 0, 0, b"local a", 0)
                .await
                .expect("write a.txt");
            let c = lookup(&fs, ROOT_INO, "c.txt").await.expect("c.txt");
            fs.write(&request(), c, 0, 0, b"C", 0)
                .await
                .expect("copy c.txt up");
            fs.set_write_hook(failing_once(WriteStep::Rename));
            assert!(
                fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"b.txt", 0)
                    .await
                    .is_err()
            );
            assert_eq!(names(&instance.write_dir()), vec!["a.txt", "c.txt"]);
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, a, 64).await, b"local a");
            let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, b, 64).await, b"tracked b");

            fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"c.txt", 0)
                .await
                .expect("a rename over a local file");
            assert_eq!(names(&instance.write_dir()), vec!["c.txt"]);
            let c = lookup(&fs, ROOT_INO, "c.txt").await.expect("c.txt");
            assert_eq!(read(&fs, c, 64).await, b"local a");
        })
        .await;
}

/// RENAME of a directory whose subtree is not all local content is `EXDEV` and changes nothing:
/// a tracked directory, and a directory added under `write/` holding a node the revision's
/// content backs.
#[tokio::test(flavor = "multi_thread")]
async fn renaming_a_tracked_directory_is_exdev() {
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(
                fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"moved", 0)
                    .await,
                Err(Errno::EXDEV)
            );
            assert_eq!(lookup(&fs, ROOT_INO, "moved").await, Err(Errno::ENOENT));
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir stays");
            assert_eq!(listed(&fs, dir).await, vec!["a.txt"]);
            assert!(!instance.write_dir().join("moved").exists());
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let made_node = node_at(&instance, &fs, "made").await;
            attach(&instance, &fs, made_node, "tracked.bin").await;
            assert_eq!(
                fs.rename(&request(), ROOT_INO, b"made", ROOT_INO, b"moved", 0)
                    .await,
                Err(Errno::EXDEV)
            );
            let empty = fs
                .mkdir(&request(), ROOT_INO, b"empty", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            fs.rename(&request(), ROOT_INO, b"empty", ROOT_INO, b"renamed", 0)
                .await
                .expect("a local directory renames");
            assert_ne!(made, empty);
        })
        .await;
}

/// A directory added under `write/` reports its host directory's mode and times.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_directory_reports_its_host_mode_and_times() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o750, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let stamped =
                std::time::SystemTime::UNIX_EPOCH + Duration::new(1_600_000_000, 125_000_000);
            std::fs::File::open(instance.write_dir().join("made"))
                .expect("the host directory")
                .set_modified(stamped)
                .expect("set the host mtime");
            let attr = fs.getattr(&request(), made).await.expect("attr").attr;
            assert_eq!(attr.mode & 0o7777, 0o750);
            assert_eq!(attr.mode & libc::S_IFMT, libc::S_IFDIR);
            assert_eq!((attr.mtime, attr.mtimensec), (1_600_000_000, 125_000_000));
        })
        .await;
}

/// GETATTR of a local directory or symlink the kernel still holds after RMDIR or UNLINK
/// orphaned it, leaving it no host path, answers what the inode holds.
#[tokio::test(flavor = "multi_thread")]
async fn getattr_of_an_orphaned_local_directory_or_symlink_answers_what_it_holds() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o750, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let link = fs
                .symlink(&request(), ROOT_INO, b"link", b"keep.txt")
                .await
                .expect("symlink")
                .ino;
            fs.rmdir(&request(), ROOT_INO, b"made")
                .await
                .expect("rmdir");
            fs.unlink(&request(), ROOT_INO, b"link")
                .await
                .expect("unlink");
            for ino in [made, link] {
                assert_eq!(
                    fs.inodes().get(ino).map(|entry| entry.orphaned),
                    Some(true),
                    "inode {ino} is orphaned"
                );
            }
            assert!(
                names(&instance.write_dir()).is_empty(),
                "neither has a host entry"
            );

            let attr = fs
                .getattr(&request(), made)
                .await
                .expect("the orphaned directory's attributes")
                .attr;
            assert_eq!(attr.mode & libc::S_IFMT, libc::S_IFDIR);
            let attr = fs
                .getattr(&request(), link)
                .await
                .expect("the orphaned symlink's attributes")
                .attr;
            assert_eq!(attr.mode & libc::S_IFMT, libc::S_IFLNK);
        })
        .await;
}

/// A symlink under `write/` at mount is served as a symlink through its directory's listing:
/// READDIR's type and number, LOOKUP's and READDIRPLUS's attributes, and READLINK.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_symlink_is_served_as_a_symlink_through_its_directory_listing() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    std::fs::create_dir_all(instance.write_dir()).expect("write/");
    std::os::unix::fs::symlink("keep.txt", instance.write_dir().join("link"))
        .expect("a local symlink");
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let mut page = vec![0u8; 64 * 1024];
            let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut page);
            fs.readdir(&request(), ROOT_INO, fh, 0, &mut writer)
                .await
                .expect("readdir");
            let used = writer.finish();
            fs.releasedir(ROOT_INO, fh);
            let mut listed = None;
            let mut at = 0;
            while at + 24 <= used {
                let ino = u64::from_ne_bytes(page[at..at + 8].try_into().expect("u64"));
                let namelen =
                    u32::from_ne_bytes(page[at + 16..at + 20].try_into().expect("u32")) as usize;
                let kind = u32::from_ne_bytes(page[at + 20..at + 24].try_into().expect("u32"));
                if &page[at + 24..at + 24 + namelen] == b"link" {
                    listed = Some((ino, kind));
                }
                at += (24 + namelen).div_ceil(8) * 8;
            }
            let (listed_ino, kind) = listed.expect("READDIR lists link");
            assert_eq!(kind, u32::from(libc::DT_LNK), "READDIR's type");

            let entry = fs
                .lookup(&request(), ROOT_INO, b"link")
                .await
                .expect("LOOKUP of link");
            assert_eq!(entry.attr.mode & libc::S_IFMT, libc::S_IFLNK);
            assert_eq!(entry.attr.size, 8, "the target's length");
            assert_eq!(entry.ino, listed_ino, "READDIR's number is LOOKUP's");
            let mut target = [0u8; 64];
            let len = fs
                .readlink(&request(), entry.ino, &mut target)
                .await
                .expect("READLINK");
            assert_eq!(&target[..len], b"keep.txt");
            let plus = plus_records(&fs, ROOT_INO).await;
            let (_, record) = plus
                .iter()
                .find(|(name, _)| name == "link")
                .expect("READDIRPLUS lists link");
            assert_eq!(record.attr.mode & libc::S_IFMT, libc::S_IFLNK);
        })
        .await;
}

/// A local entry whose host file went missing is listed without an inode, and the page is
/// answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_entry_whose_host_file_is_missing_is_listed_plain() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    plant(&instance, &[("dir/gone.txt", b"gone")]);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            std::fs::remove_file(instance.write_dir().join("dir/gone.txt")).expect("remove");
            let fh = fs.opendir(&request(), dir).expect("opendir").fh;
            let (records, emitted) = plus_page(&fs, dir, fh, 0, 64 * 1024).await;
            fs.releasedir(dir, fh);
            assert!(
                records.contains(&("gone.txt".to_string(), 0)),
                "gone.txt is listed with no inode: {records:?}"
            );
            assert_eq!(emitted, 0, "no inode is counted");
            assert_eq!(
                fs.inodes()
                    .find_child(dir, hash_string("gone.txt"), "gone.txt"),
                None
            );
        })
        .await;
}

/// Creates `name` in directory `parent` holding `content`, and answers its inode.
async fn create_with(fs: &FuseFilesystem, parent: u64, name: &str, content: &[u8]) -> u64 {
    let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
    let created = fs
        .create(
            &request(),
            parent,
            name.as_bytes(),
            libc::S_IFREG | 0o644,
            0o022,
            flags,
        )
        .await
        .expect("create");
    fs.write(&request(), created.entry.ino, 0, 0, content, 0)
        .await
        .expect("write");
    created.entry.ino
}

/// A listing of the root built now, which a test caches later, as a build that read the
/// directory before a change and finished after it leaves it.
async fn root_listing_now(fs: &FuseFilesystem) -> Arc<lore_revision::fs::fuse::listing::Listing> {
    lore_revision::fs::fuse::listing::Listing::build(
        &fs.served(),
        ROOT_NODE,
        lore_revision::filter::FilterStates::ROOT,
        &lore_revision::util::path::RelativePath::new(),
    )
    .await
    .expect("build")
}

/// The save-through-a-temporary pattern with a listing of the directory built before the first
/// save's rename and cached after each step: the output is found, the temporary created again,
/// the second save replaces the output and UNLINK removes it, and the first save's bytes stay at
/// the output until the second replaces them.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_cached_across_saves_through_a_temporary_serves_each_saved_output() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let out = instance.write_dir().join("out");
            create_with(&fs, ROOT_INO, "out", b"save 0").await;
            let first = create_with(&fs, ROOT_INO, "tmp", b"save 1").await;
            let built = root_listing_now(&fs).await;
            fs.rename(&request(), ROOT_INO, b"tmp", ROOT_INO, b"out", 0)
                .await
                .expect("save 1");
            fs.served().listings.insert(ROOT_NODE, built.clone());
            assert_eq!(lookup(&fs, ROOT_INO, "out").await, Ok(first));
            assert_eq!(read(&fs, first, 64).await, b"save 1");

            fs.served().listings.insert(ROOT_NODE, built.clone());
            let second = create_with(&fs, ROOT_INO, "tmp", b"save 2").await;
            assert_eq!(std::fs::read(&out).expect("write/out"), b"save 1");

            fs.served().listings.insert(ROOT_NODE, built.clone());
            fs.rename(&request(), ROOT_INO, b"tmp", ROOT_INO, b"out", 0)
                .await
                .expect("save 2");
            assert_eq!(std::fs::read(&out).expect("write/out"), b"save 2");
            assert_eq!(names(&instance.write_dir()), vec!["out"]);
            assert_eq!(lookup(&fs, ROOT_INO, "out").await, Ok(second));
            assert_eq!(read(&fs, second, 64).await, b"save 2");

            fs.served().listings.insert(ROOT_NODE, built);
            fs.unlink(&request(), ROOT_INO, b"out")
                .await
                .expect("unlink out");
            assert!(names(&instance.write_dir()).is_empty());
            assert_eq!(lookup(&fs, ROOT_INO, "out").await, Err(Errno::ENOENT));
        })
        .await;
}

/// A listing whose head moved to another directory and back under the same name, a node
/// created in between, finds that node.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_whose_head_moved_out_and_back_finds_a_node_created_in_between() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let other = fs
                .mkdir(&request(), ROOT_INO, b"other", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, ROOT_INO, "head", b"h").await;
            let built = root_listing_now(&fs).await;
            fs.rename(&request(), ROOT_INO, b"head", other, b"head", 0)
                .await
                .expect("move out");
            let between = create_with(&fs, ROOT_INO, "between", b"b").await;
            fs.rename(&request(), other, b"head", ROOT_INO, b"head", 0)
                .await
                .expect("move back");
            fs.served().listings.insert(ROOT_NODE, built);
            let builds = fs.listings_built();
            assert_eq!(lookup(&fs, ROOT_INO, "between").await, Ok(between));
            assert_eq!(fs.listings_built(), builds, "found through the log");
        })
        .await;
}

/// A listing whose head was renamed only in case, a node created before the rename, finds
/// that node.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_whose_head_was_renamed_in_case_finds_a_node_created_before() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "head.txt", b"h").await;
            let built = root_listing_now(&fs).await;
            let between = create_with(&fs, ROOT_INO, "between", b"b").await;
            fs.rename(&request(), ROOT_INO, b"head.txt", ROOT_INO, b"HEAD.txt", 0)
                .await
                .expect("respell");
            fs.served().listings.insert(ROOT_NODE, built);
            let builds = fs.listings_built();
            assert_eq!(lookup(&fs, ROOT_INO, "between").await, Ok(between));
            assert_eq!(
                fs.listings_built() - builds,
                1,
                "the respelling makes the listing stale, and one build finds the node"
            );
        })
        .await;
}

/// LOOKUP of a name a replacing reattach moves another node onto, through the directory's
/// cached listing and its rebuilds, always finds the name, held by the replaced node before
/// and by the moved one after, wherever the two stood in the chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_lookup_during_a_replacing_reattach_always_finds_the_name() {
    const ROUNDS: usize = 200;
    const READERS: usize = 4;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let missed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            // Odd while a replace runs.
            let replacing = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let overlapped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for round in 0..ROUNDS {
                let name = format!("d{round}");
                let dir = fs
                    .mkdir(
                        &request(),
                        ROOT_INO,
                        name.as_bytes(),
                        libc::S_IFDIR | 0o755,
                        0o022,
                    )
                    .await
                    .expect("mkdir")
                    .ino;
                let dir_node = node_at(&instance, &fs, &name).await;
                create_with(&fs, dir, "b.txt", b"held").await;
                for i in 0..(round % 3) * 4 {
                    create_with(&fs, dir, &format!("mid-{i}"), b"m").await;
                }
                create_with(&fs, dir, "a.txt", b"moved").await;
                for i in 0..(round % 2) * 4 {
                    create_with(&fs, dir, &format!("after-{i}"), b"x").await;
                }
                // Built afresh, the listing holds the holder and the node moved onto it.
                fs.served().listings.remove(dir_node);
                lookup(&fs, dir, "b.txt").await.expect("b.txt");
                let holder = node_at(&instance, &fs, &format!("{name}/b.txt")).await;
                let moved = node_at(&instance, &fs, &format!("{name}/a.txt")).await;
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let started = Arc::new(tokio::sync::Barrier::new(READERS + 1));
                let mut readers = tokio::task::JoinSet::new();
                for _ in 0..READERS {
                    let fs = fs.clone();
                    let (stop, started) = (stop.clone(), started.clone());
                    let (lookups, missed) = (lookups.clone(), missed.clone());
                    let (replacing, overlapped) = (replacing.clone(), overlapped.clone());
                    let _ = lore_base::lore_spawn!(readers, async move {
                        started.wait().await;
                        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                            lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let before = replacing.load(std::sync::atomic::Ordering::SeqCst);
                            let found = lookup(&fs, dir, "b.txt").await;
                            let after = replacing.load(std::sync::atomic::Ordering::SeqCst);
                            if before % 2 == 1 || before != after {
                                overlapped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            match found {
                                Ok(_) => {}
                                Err(Errno::ENOENT) => {
                                    missed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                Err(errno) => panic!("lookup: {errno:?}"),
                            }
                            tokio::task::yield_now().await;
                        }
                    });
                }
                started.wait().await;
                let host = instance.write_dir().join(&name);
                std::fs::rename(host.join("a.txt"), host.join("b.txt")).expect("host rename");
                replacing.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let replaced = fs
                    .state()
                    .node_reattach_replacing(repository.clone(), moved, dir_node, "b.txt", holder)
                    .await
                    .expect("replace");
                replacing.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(matches!(replaced, AttachOutcome::Attached(_)));
                match lookup(&fs, dir, "b.txt").await {
                    Ok(_) => {}
                    Err(Errno::ENOENT) => {
                        missed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(errno) => panic!("lookup: {errno:?}"),
                }
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                while let Some(joined) = readers.join_next().await {
                    joined.expect("reader");
                }
            }
            assert!(
                overlapped.load(std::sync::atomic::Ordering::SeqCst) > 0,
                "no LOOKUP ran during a replace, of {}",
                lookups.load(std::sync::atomic::Ordering::SeqCst)
            );
            assert_eq!(
                missed.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "LOOKUP missed the name during a replace"
            );
        })
        .await;
}

/// What a replacing-rename fixture makes at a name.
#[derive(Clone, Copy)]
enum Made {
    File(&'static [u8]),
    Dir,
    Symlink,
}

/// A directory `dir` in the root holding local files `k` and `tmp`, `k` looked up and listed:
/// answers its inode, `k`'s inode and node, and `tmp`'s inode and node.
async fn replacing_rename_fixture(
    instance: &Instance,
    fs: &FuseFilesystem,
) -> (u64, (u64, NodeID), (u64, NodeID)) {
    replacing_rename_fixture_of(instance, fs, Made::File(b"held"), Made::File(b"moved in")).await
}

/// [`replacing_rename_fixture`] with `k` and `tmp` made as `held` and `mover`.
async fn replacing_rename_fixture_of(
    instance: &Instance,
    fs: &FuseFilesystem,
    held: Made,
    mover: Made,
) -> (u64, (u64, NodeID), (u64, NodeID)) {
    let dir = fs
        .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
        .await
        .expect("mkdir")
        .ino;
    let mut made = Vec::new();
    for (name, kind) in [("k", held), ("tmp", mover)] {
        made.push(match kind {
            Made::File(content) => create_with(fs, dir, name, content).await,
            Made::Dir => {
                fs.mkdir(
                    &request(),
                    dir,
                    name.as_bytes(),
                    libc::S_IFDIR | 0o755,
                    0o022,
                )
                .await
                .expect("mkdir")
                .ino
            }
            Made::Symlink => {
                fs.symlink(&request(), dir, name.as_bytes(), b"keep.txt")
                    .await
                    .expect("symlink")
                    .ino
            }
        });
    }
    let (k, tmp) = (made[0], made[1]);
    assert_eq!(lookup(fs, dir, "k").await, Ok(k));
    assert_eq!(listed(fs, dir).await, vec!["k", "tmp"]);
    (
        dir,
        (k, node_at(instance, fs, "dir/k").await),
        (tmp, node_at(instance, fs, "dir/tmp").await),
    )
}

/// A LOOKUP of a name a RENAME is replacing, made once the inode at the target has left the
/// name, finds the mover's inode there: the name never lacks a live inode, so the lookup makes
/// none of its own and revives nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_while_a_rename_moves_inodes_finds_the_movers_inode() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, _), (tmp, tmp_node)) = replacing_rename_fixture(&instance, &fs).await;
            let reached = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (held, release) = (reached.clone(), released.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::TargetUnindexed { .. })
                    && !held.swap(true, Ordering::SeqCst)
                {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let renamer = fs.clone();
            let renaming = lore_base::lore_spawn!(async move {
                renamer
                    .rename(&request(), dir, b"tmp", dir, b"k", 0)
                    .await
                    .expect("rename over k");
            });
            while !reached.load(Ordering::SeqCst) {
                assert!(
                    !renaming.is_finished(),
                    "the rename ended before unindexing k"
                );
                tokio::task::yield_now().await;
            }

            let found = lookup(&fs, dir, "k").await;
            released.store(true, Ordering::SeqCst);
            renaming.await.expect("the rename");

            assert_eq!(found, Ok(tmp), "the mover's inode holds the name");
            assert_eq!(
                fs.inodes().get(k).map(|entry| entry.orphaned),
                Some(true),
                "the replaced inode stays orphaned"
            );
            let held = fs.inodes().get(tmp).expect("the mover's inode");
            assert_eq!((held.node, held.name.to_vec()), (tmp_node, b"k".to_vec()));
        })
        .await;
}

/// Sets a lookup hook on `fs` that renames `tmp` over `k` in `dir` once, as a LOOKUP has
/// confirmed `k_node` at `k` from its record and before it finds the inode.
fn rename_over_k_once_listed(fs: &Arc<FuseFilesystem>, dir: u64, k_node: NodeID) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    let fired = Arc::new(AtomicBool::new(false));
    let weak = Arc::downgrade(fs);
    fs.set_lookup_hook(Arc::new(move |step| {
        let (weak, fired) = (weak.clone(), fired.clone());
        Box::pin(async move {
            let LookupStep::Listed { node, .. } = step else {
                return;
            };
            if node != k_node || fired.swap(true, Ordering::SeqCst) {
                return;
            }
            let fs = weak.upgrade().expect("the provider");
            fs.rename(&request(), dir, b"tmp", dir, b"k", 0)
                .await
                .expect("rename over k");
        })
    }));
}

/// A LOOKUP that confirmed a listed holder from its record, before a RENAME replaced it,
/// leaves the inode now at the name mapped to the node that holds the name, never to the
/// replaced holder.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_overtaken_by_a_replacing_rename_maps_no_inode_to_the_replaced_holder() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (_, k_node), (tmp, tmp_node)) =
                replacing_rename_fixture(&instance, &fs).await;
            rename_over_k_once_listed(&fs, dir, k_node);

            let found = lookup(&fs, dir, "k").await;

            assert_eq!(found, Ok(tmp), "the inode at the name, the mover's");
            assert_eq!(
                fs.inodes().get(tmp).map(|entry| entry.node),
                Some(tmp_node),
                "mapped to the node holding the name, not to {k_node}"
            );
        })
        .await;
}

/// A LOOKUP overtaken as above, by a RENAME of `held` over `mover` kinds that differ from the
/// record it read or of another directory node, answers the mover's inode and leaves it live.
async fn an_overtaken_lookup_keeps_the_movers_inode(held: Made, mover: Made) {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, k_node), (tmp, _)) =
                replacing_rename_fixture_of(&instance, &fs, held, mover).await;
            rename_over_k_once_listed(&fs, dir, k_node);

            let found = lookup(&fs, dir, "k").await;

            assert_eq!(
                found,
                Ok(tmp),
                "the inode at the name, the mover's, not {k}"
            );
            assert_eq!(
                fs.inodes().get(tmp).map(|entry| entry.orphaned),
                Some(false),
                "the mover's inode stays live"
            );
        })
        .await;
}

/// A LOOKUP made while a RENAME of `mover` over `held` has moved the node in the State and not yet
/// the inode waits for the inode and finds it at the name: it makes no second inode for the
/// mover's node, and the next RENAME over the name leaves no inode there for a detached node.
async fn a_lookup_while_a_rename_over_the_name_is_under_way_finds_the_movers_inode(
    held: Made,
    mover: Made,
) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, _, (tmp, _)) = replacing_rename_fixture_of(&instance, &fs, held, mover).await;
            let reached = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (holding, release) = (reached.clone(), released.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::NodeMoved { .. })
                    && !holding.swap(true, Ordering::SeqCst)
                {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let waiting = Arc::new(AtomicBool::new(false));
            let waits = waiting.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                if matches!(step, LookupStep::Arriving { .. }) {
                    waits.store(true, Ordering::SeqCst);
                }
                Box::pin(async {})
            }));
            let renamer = fs.clone();
            let renaming = lore_base::lore_spawn!(async move {
                renamer
                    .rename(&request(), dir, b"tmp", dir, b"k", 0)
                    .await
                    .expect("rename over k");
            });
            while !reached.load(Ordering::SeqCst) {
                assert!(
                    !renaming.is_finished(),
                    "the rename ended before moving its node"
                );
                tokio::task::yield_now().await;
            }

            let looker = fs.clone();
            let looking = lore_base::lore_spawn!(async move { lookup(&looker, dir, "k").await });
            while !looking.is_finished() && !waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            released.store(true, Ordering::SeqCst);
            renaming.await.expect("the rename");
            let found = looking.await.expect("the lookup");

            assert_eq!(
                found,
                Ok(tmp),
                "the mover's inode, not a second one for its node"
            );
            assert!(
                waiting.load(Ordering::SeqCst),
                "the lookup waited for the rename"
            );
            let next = match mover {
                Made::File(content) => create_with(&fs, dir, "next", content).await,
                Made::Dir => {
                    fs.mkdir(&request(), dir, b"next", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("mkdir")
                        .ino
                }
                Made::Symlink => {
                    fs.symlink(&request(), dir, b"next", b"keep.txt")
                        .await
                        .expect("symlink")
                        .ino
                }
            };
            fs.rename(&request(), dir, b"next", dir, b"k", 0)
                .await
                .expect("the next rename over k");
            assert_eq!(lookup(&fs, dir, "k").await, Ok(next));
            assert_eq!(
                fs.inodes().get(tmp).map(|entry| entry.orphaned),
                Some(true),
                "the replaced mover left the name"
            );
        })
        .await;
}

/// A LOOKUP of a new name made while a RENAME to it has moved the node in the State and not yet
/// the inode waits for the inode and finds it there, making no second inode for the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_while_a_rename_to_a_new_name_is_under_way_finds_the_movers_inode() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, _, (tmp, _)) = replacing_rename_fixture(&instance, &fs).await;
            let reached = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (holding, release) = (reached.clone(), released.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::NodeMoved { .. })
                    && !holding.swap(true, Ordering::SeqCst)
                {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let waiting = Arc::new(AtomicBool::new(false));
            let waits = waiting.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                if matches!(step, LookupStep::Arriving { .. }) {
                    waits.store(true, Ordering::SeqCst);
                }
                Box::pin(async {})
            }));
            let renamer = fs.clone();
            let renaming = lore_base::lore_spawn!(async move {
                renamer
                    .rename(&request(), dir, b"tmp", dir, b"new", 0)
                    .await
                    .expect("rename to new");
            });
            while !reached.load(Ordering::SeqCst) {
                assert!(
                    !renaming.is_finished(),
                    "the rename ended before moving its node"
                );
                tokio::task::yield_now().await;
            }

            let looker = fs.clone();
            let looking = lore_base::lore_spawn!(async move { lookup(&looker, dir, "new").await });
            while !looking.is_finished() && !waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            released.store(true, Ordering::SeqCst);
            renaming.await.expect("the rename");
            let found = looking.await.expect("the lookup");

            assert_eq!(
                found,
                Ok(tmp),
                "the mover's inode, not a second one for its node"
            );
            assert!(
                waiting.load(Ordering::SeqCst),
                "the lookup waited for the rename"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_while_a_directory_rename_over_one_is_under_way_finds_the_movers_inode() {
    a_lookup_while_a_rename_over_the_name_is_under_way_finds_the_movers_inode(Made::Dir, Made::Dir)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_while_a_file_rename_over_a_symlink_is_under_way_finds_the_movers_inode() {
    a_lookup_while_a_rename_over_the_name_is_under_way_finds_the_movers_inode(
        Made::Symlink,
        Made::File(b"moved in"),
    )
    .await;
}

/// A LOOKUP of a directory a MKDIR has attached and not yet made the inode of waits for that
/// inode and answers it, rather than making one the MKDIR then orphans.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_while_a_mkdir_is_under_way_answers_its_inode() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt"]);
            let reached = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (holding, release) = (reached.clone(), released.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if step == WriteStep::Created && !holding.swap(true, Ordering::SeqCst) {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let waiting = Arc::new(AtomicBool::new(false));
            let waits = waiting.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                if matches!(step, LookupStep::Arriving { .. }) {
                    waits.store(true, Ordering::SeqCst);
                }
                Box::pin(async {})
            }));
            let maker = fs.clone();
            let making = lore_base::lore_spawn!(async move {
                maker
                    .mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            });
            while !reached.load(Ordering::SeqCst) {
                assert!(
                    !making.is_finished(),
                    "the mkdir ended before making its inode"
                );
                tokio::task::yield_now().await;
            }

            let looker = fs.clone();
            let looking =
                lore_base::lore_spawn!(async move { lookup(&looker, ROOT_INO, "d").await });
            while !looking.is_finished() && !waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            released.store(true, Ordering::SeqCst);
            let made = making.await.expect("the mkdir");
            let found = looking.await.expect("the lookup");

            assert_eq!(found, Ok(made), "the MKDIR's inode, not one it orphaned");
            assert_eq!(
                fs.inodes().get(made).map(|entry| entry.orphaned),
                Some(false)
            );
        })
        .await;
}

/// A LOOKUP on a worker, which may not fetch, of a name a RENAME is moving a node to answers
/// `WouldFetch` at once rather than waiting on the worker; the deferred LOOKUP that follows
/// finds the mover's inode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_lookup_of_a_name_a_rename_is_moving_to_defers_rather_than_waiting() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, _), (tmp, _)) = replacing_rename_fixture(&instance, &fs).await;
            let warm = fs
                .lookup_local(&request(), dir, b"k")
                .await
                .map(|reply| reply.ino);
            assert!(
                matches!(warm, Ok(ino) if ino == k),
                "the worker path answers here"
            );
            let reached = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let (holding, release) = (reached.clone(), released.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::NodeMoved { .. })
                    && !holding.swap(true, Ordering::SeqCst)
                {
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let renamer = fs.clone();
            let renaming = lore_base::lore_spawn!(async move {
                renamer
                    .rename(&request(), dir, b"tmp", dir, b"new", 0)
                    .await
                    .expect("rename to new");
            });
            while !reached.load(Ordering::SeqCst) {
                assert!(
                    !renaming.is_finished(),
                    "the rename ended before moving its node"
                );
                tokio::task::yield_now().await;
            }

            let local = tokio::time::timeout(
                Duration::from_secs(5),
                fs.lookup_local(&request(), dir, b"new"),
            )
            .await;
            released.store(true, Ordering::SeqCst);
            renaming.await.expect("the rename");

            let answered = match local {
                Ok(Ok(reply)) => format!("inode {}", reply.ino),
                Ok(Err(ReadFault::WouldFetch)) => "WouldFetch".to_string(),
                Ok(Err(ReadFault::Errno(errno))) => format!("{errno:?}"),
                Err(_) => "still waiting after 5 s".to_string(),
            };
            assert_eq!(answered, "WouldFetch");
            assert_eq!(lookup(&fs, dir, "new").await, Ok(tmp));
        })
        .await;
}

/// A LOOKUP waiting on a claim of its name while a stream of overlapping claims keeps the name
/// claimed finishes after one release, rather than waiting for the name to be free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_waiting_on_a_stream_of_claims_on_its_name_finishes_after_one_release() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("keep.txt", b"k"), ("k", b"held")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await.len(), 2);
            let waiting = Arc::new(AtomicBool::new(false));
            let waits = waiting.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                if matches!(step, LookupStep::Arriving { .. }) {
                    waits.store(true, Ordering::SeqCst);
                }
                Box::pin(async {})
            }));
            let first = fs.claim_arriving(ROOT_INO, "k").expect("a claim");
            let looker = fs.clone();
            let looking =
                lore_base::lore_spawn!(async move { lookup(&looker, ROOT_INO, "k").await });
            while !waiting.load(Ordering::SeqCst) {
                assert!(!looking.is_finished(), "the lookup ended without waiting");
                tokio::task::yield_now().await;
            }

            let second = fs.claim_arriving(ROOT_INO, "k").expect("a claim");
            drop(first);
            let found = tokio::time::timeout(Duration::from_secs(5), looking).await;
            drop(second);

            let found = found.expect("the lookup finished after one release");
            assert!(found.expect("the lookup task").is_ok());
        })
        .await;
}

/// A READDIRPLUS meeting a name claimed by a request moving a node to it takes that row plain at
/// once rather than waiting, and counts the rows after it. A failed reply of that page gives every
/// count back.
#[tokio::test(flavor = "multi_thread")]
async fn a_readdirplus_takes_a_claimed_row_plain_and_counts_the_rows_after_it() {
    let instance = Instance::new(&[
        ("a.txt", b"a"),
        ("k", b"held"),
        ("keep.txt", b"k"),
        ("z.txt", b"z"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            // The third row of a page, so rows stand before it whatever the listing's order;
            // its inode forgotten, so it has none.
            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let (first, _) = plus_page(&fs, ROOT_INO, fh, 0, 64 * 1024).await;
            fs.releasedir(ROOT_INO, fh);
            let first: Vec<(String, u64)> = first
                .into_iter()
                .filter(|(name, _)| name != "." && name != "..")
                .collect();
            assert_eq!(first.len(), 4);
            let (claimed, ino) = first[2].clone();
            fs.forget(ino, 1);
            let lookups = |ino: u64| fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
            let before: Vec<u64> = first.iter().map(|&(_, ino)| lookups(ino)).collect();
            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let claim = fs.claim_arriving(ROOT_INO, &claimed).expect("a claim");

            let page = tokio::time::timeout(
                Duration::from_secs(5),
                plus_page(&fs, ROOT_INO, fh, 0, 64 * 1024),
            )
            .await;
            drop(claim);
            fs.releasedir(ROOT_INO, fh);

            let (rows, counted) = page.expect("the page answered without waiting");
            let rows: Vec<(String, u64)> = rows
                .into_iter()
                .filter(|(name, _)| name != "." && name != "..")
                .collect();
            let at = rows
                .iter()
                .position(|(name, _)| *name == claimed)
                .expect("the claimed row is listed");
            assert_eq!(at, 2, "the page lists in the listing's order: {rows:?}");
            assert_eq!(rows[at].1, 0, "the claimed row is plain: {rows:?}");
            assert!(
                rows.iter()
                    .enumerate()
                    .all(|(row, (_, node))| row == at || *node != 0),
                "every other row keeps its attributes: {rows:?}"
            );
            assert_eq!(counted, rows.len() - 1, "every other row counted");

            for &(_, node) in rows.iter().filter(|&&(_, node)| node != 0) {
                fs.reply_failed(
                    &request(),
                    &lore_fuse::reply::ReplyOutcome::Entry { ino: node },
                );
            }
            let after: Vec<u64> = first.iter().map(|&(_, ino)| lookups(ino)).collect();
            assert_eq!(after, before, "a failed reply gives every count back");
        })
        .await;
}

/// A READDIRPLUS row whose node left its name after the page began goes plain, and the rows after
/// it keep their attributes and are counted.
#[tokio::test(flavor = "multi_thread")]
async fn a_readdirplus_row_whose_node_left_its_name_mid_page_is_plain_alone() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for name in STREAMED {
                create_with(&fs, ROOT_INO, name, name.as_bytes()).await;
            }
            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let (order, _) = plus_page(&fs, ROOT_INO, fh, 0, 64 * 1024).await;
            fs.releasedir(ROOT_INO, fh);
            let order: Vec<(String, u64)> = order
                .into_iter()
                .filter(|(name, _)| name != "." && name != "..")
                .collect();
            assert_eq!(order.len(), 5);
            // The second row makes a new inode, and its admission unlinks the third row's name.
            let (admitted, admitted_ino) = order[1].clone();
            let (gone, _) = order[2].clone();
            let admitted_node = node_at(&instance, &fs, &admitted).await;
            fs.forget(admitted_ino, 2);
            assert!(
                fs.inodes().get(admitted_ino).is_none(),
                "{admitted} has no inode"
            );
            let unlinked = Arc::new(std::sync::atomic::AtomicBool::new(false));
            fs.set_write_hook(Arc::new({
                let fs = Arc::downgrade(&fs);
                let gone = gone.clone();
                let unlinked = unlinked.clone();
                move |step| {
                    if step
                        == (WriteStep::Admitting {
                            node: admitted_node,
                        })
                        && !unlinked.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        let fs = fs.upgrade().expect("the provider");
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(fs.unlink(
                                &request(),
                                ROOT_INO,
                                gone.as_bytes(),
                            ))
                        })
                        .expect("unlink");
                    }
                    true
                }
            }));

            let fh = fs.opendir(&request(), ROOT_INO).expect("opendir").fh;
            let (rows, counted) = plus_page(&fs, ROOT_INO, fh, 0, 64 * 1024).await;
            fs.releasedir(ROOT_INO, fh);
            assert!(
                unlinked.load(std::sync::atomic::Ordering::SeqCst),
                "the unlink ran inside the page"
            );
            let rows: Vec<(String, u64)> = rows
                .into_iter()
                .filter(|(name, _)| name != "." && name != "..")
                .collect();
            let names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
            let expected: Vec<&str> = order.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(names, expected, "the page lists its pinned listing");
            assert_eq!(rows[2].1, 0, "{gone} is plain: {rows:?}");
            assert!(
                rows.iter()
                    .enumerate()
                    .all(|(row, (_, node))| row == 2 || *node != 0),
                "every other row keeps its attributes: {rows:?}"
            );
            assert_eq!(counted, 4, "every other row counted");
        })
        .await;
}

/// While claims overflow the slots, a LOOKUP's first miss on a worker defers, and one that may
/// fetch waits for one release of a claim past the slots and then makes its inode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn under_overflowing_claims_a_first_miss_defers_or_waits_for_one_overflow_release() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("j", b"j"), ("k", b"k"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await.len(), 3);
            let waiting = Arc::new(AtomicBool::new(false));
            let waits = waiting.clone();
            fs.set_lookup_hook(Arc::new(move |step| {
                if matches!(step, LookupStep::Arriving { .. }) {
                    waits.store(true, Ordering::SeqCst);
                }
                Box::pin(async {})
            }));
            // One past the 64 slots, all on other names.
            let mut claims: Vec<_> = (0..65)
                .map(|claim| {
                    fs.claim_arriving(ROOT_INO, &format!("c{claim}"))
                        .expect("a claim")
                })
                .collect();

            let local = fs.lookup_local(&request(), ROOT_INO, b"j").await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "a worker's first miss defers"
            );
            let looker = fs.clone();
            let looking =
                lore_base::lore_spawn!(async move { lookup(&looker, ROOT_INO, "k").await });
            while !waiting.load(Ordering::SeqCst) {
                assert!(!looking.is_finished(), "the lookup ended without waiting");
                tokio::task::yield_now().await;
            }
            drop(claims.pop());
            let found = tokio::time::timeout(Duration::from_secs(5), looking).await;
            drop(claims);

            let found = found.expect("the lookup finished after one overflow release");
            assert!(found.expect("the lookup task").is_ok());
        })
        .await;
}

/// A LOOKUP about to make the first inode for a new `k`, overtaken there by a RENAME that brings
/// back onto `k` the inode whose number `k` derives, does not give that inode the replaced node:
/// it stands for the node it brought.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_overtaken_by_the_inode_of_its_number_returning_leaves_it_its_node() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, held), _) = replacing_rename_fixture(&instance, &fs).await;
            fs.rename(&request(), dir, b"k", dir, b"away", 0)
                .await
                .expect("rename k away");
            let fresh = create_with(&fs, dir, "k", b"new").await;
            fs.forget(fresh, 1);
            let replaced = node_at(&instance, &fs, "dir/k").await;
            let fired = Arc::new(AtomicBool::new(false));
            let runtime = tokio::runtime::Handle::current();
            let weak = Arc::downgrade(&fs);
            fs.set_write_hook(Arc::new(move |step| {
                if step == (WriteStep::Admitting { node: replaced })
                    && !fired.swap(true, Ordering::SeqCst)
                {
                    let fs = weak.upgrade().expect("the provider");
                    let done = Arc::new(AtomicBool::new(false));
                    let finished = done.clone();
                    runtime.spawn(async move {
                        fs.rename(&request(), dir, b"away", dir, b"k", 0)
                            .await
                            .expect("rename back over k");
                        finished.store(true, Ordering::SeqCst);
                    });
                    while !done.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));

            let found = lookup(&fs, dir, "k").await;

            assert_eq!(found, Ok(k), "the inode brought back");
            assert_eq!(
                fs.inodes().get(k).map(|entry| entry.node),
                Some(held),
                "it stands for the node it brought, not the replaced {replaced}"
            );
        })
        .await;
}

/// A LOOKUP finding several file inodes of another node at its name, and none of its node's,
/// gives the first its node rather than making another inode beside them.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_among_file_inodes_of_another_node_adopts_the_first() {
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::fs::fuse::inode::NewChild;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, k_node), (_, tmp_node)) = replacing_rename_fixture(&instance, &fs).await;
            let k_hash = hash_string("k");
            let looked = fs
                .inodes()
                .get(k)
                .map(|entry| entry.nlookup)
                .expect("k's inode");
            fs.forget(k, looked);
            let mut planted = Vec::new();
            for name in ["s1", "s2"] {
                let (ino, ()) = fs.inodes().insert_child(
                    NewChild {
                        parent: dir,
                        name,
                        name_hash: hash_string(name),
                        kind: InodeKind::File {
                            address: Default::default(),
                            size: 0,
                            executable: false,
                            local: false,
                        },
                        mtime: 0,
                        node: tmp_node,
                    },
                    |_| (),
                );
                fs.inodes().move_child(ino, None, dir, "k", k_hash);
                planted.push(ino);
            }
            let live: Vec<u64> = fs
                .inodes()
                .live_children(dir, k_hash, "k")
                .into_iter()
                .map(|(ino, _)| ino)
                .collect();
            assert_eq!(
                live, planted,
                "two file inodes of another node at k, and no other"
            );

            let found = lookup(&fs, dir, "k").await;

            assert_eq!(found, Ok(planted[0]), "the first, adopted");
            assert_eq!(
                fs.inodes().live_children(dir, k_hash, "k"),
                vec![(planted[0], k_node), (planted[1], tmp_node)],
                "the first takes k's node, the second stays, and no inode is made"
            );
        })
        .await;
}

/// A tracked file inode a LOOKUP gives a local node, as an adopting pass does, keeps the content
/// it holds: only the request that changes a node moves its inode's content. A WRITE through it
/// finds the node already local and writes its host file, copying nothing up.
#[tokio::test(flavor = "multi_thread")]
async fn a_tracked_inode_adopting_a_local_node_keeps_its_content_until_a_write_reaches_the_host() {
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::fs::fuse::inode::NewChild;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (k, k_node), (_, tmp_node)) = replacing_rename_fixture(&instance, &fs).await;
            let looked = fs
                .inodes()
                .get(k)
                .map(|entry| entry.nlookup)
                .expect("k's inode");
            fs.forget(k, looked);
            let keep = lookup(&fs, ROOT_INO, "keep.txt").await.expect("keep.txt");
            let tracked = match fs.inodes().get(keep).expect("keep.txt's inode").kind {
                InodeKind::File { address, size, .. } => (address, size),
                InodeKind::Dir { .. } | InodeKind::Symlink => panic!("keep.txt is a file"),
            };
            let (planted, ()) = fs.inodes().insert_child(
                NewChild {
                    parent: dir,
                    name: "s1",
                    name_hash: hash_string("s1"),
                    kind: InodeKind::File {
                        address: tracked.0,
                        size: tracked.1,
                        executable: false,
                        local: false,
                    },
                    mtime: 0,
                    node: tmp_node,
                },
                |_| (),
            );
            fs.inodes()
                .move_child(planted, None, dir, "k", hash_string("k"));

            assert_eq!(lookup(&fs, dir, "k").await, Ok(planted), "the inode at k");
            {
                let entry = fs.inodes().get(planted).expect("the adopted inode");
                assert_eq!(entry.node, k_node, "it stands for k's node");
                assert!(
                    matches!(
                        entry.kind,
                        InodeKind::File { address, size, local: false, .. }
                            if (address, size) == tracked
                    ),
                    "it keeps the tracked content it held"
                );
            }

            fs.write(&request(), planted, 0, 0, b"HELD", 0)
                .await
                .expect("a write through the adopted inode");
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/k")).expect("write/dir/k"),
                b"HELD"
            );
            assert!(
                fs.inodes()
                    .get(planted)
                    .is_some_and(|entry| entry.kind.is_local_file()),
                "the inode serves the host file"
            );
            assert_eq!(read(&fs, planted, 64).await, b"HELD");
        })
        .await;
}

/// A LOOKUP finding a stale file inode at its name ahead of the one that stands for the name's
/// node takes that one, rather than giving the stale inode the node.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_takes_the_inode_of_its_node_over_a_stale_file_inode_ahead_of_it() {
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::fs::fuse::inode::NewChild;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (dir, (_, k_node), (tmp, _)) = replacing_rename_fixture(&instance, &fs).await;
            let k_hash = hash_string("k");
            let (stale, ()) = fs.inodes().insert_child(
                NewChild {
                    parent: dir,
                    name: "z",
                    name_hash: hash_string("z"),
                    kind: InodeKind::File {
                        address: Default::default(),
                        size: 0,
                        executable: false,
                        local: false,
                    },
                    mtime: 0,
                    node: k_node,
                },
                |_| (),
            );
            fs.inodes().move_child(stale, None, dir, "k", k_hash);
            fs.rename(&request(), dir, b"tmp", dir, b"k", 0)
                .await
                .expect("rename over k");
            let live: Vec<u64> = fs
                .inodes()
                .live_children(dir, k_hash, "k")
                .into_iter()
                .map(|(ino, _)| ino)
                .collect();
            assert_eq!(live, vec![stale, tmp], "the stale inode ahead of the mover");

            let found = lookup(&fs, dir, "k").await;

            assert_eq!(found, Ok(tmp), "the inode of the node at k");
            assert_eq!(
                fs.inodes().get(stale).map(|entry| entry.node),
                Some(k_node),
                "the stale inode keeps its node"
            );
        })
        .await;
}

/// A LOOKUP about to make the first inode for file `k`, listed or attached since the listing
/// where `attached`, overtaken there by a RENAME over `k` run to completion, after an UNLINK of
/// `k` where `unlinked`, leaves no live inode for `k`'s detached node at the name: the mover's
/// inode alone stands there.
async fn a_lookup_overtaken_at_its_insert_leaves_no_inode_for_the_detached_node(
    unlinked: bool,
    attached: bool,
) {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let files: &[(&str, &[u8])] = if attached {
        &[("keep.txt", b"k")]
    } else {
        &[("keep.txt", b"k"), ("k", b"held")]
    };
    let instance = Instance::new(files).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let tmp = create_with(&fs, ROOT_INO, "tmp", b"moved in").await;
            assert_eq!(
                listed(&fs, ROOT_INO).await.len(),
                if attached { 2 } else { 3 }
            );
            if attached {
                let k = create_with(&fs, ROOT_INO, "k", b"held").await;
                fs.forget(k, 1);
                assert_eq!(
                    fs.inodes().find_child(ROOT_INO, hash_string("k"), "k"),
                    None
                );
            }
            let held = node_at(&instance, &fs, "k").await;
            let tmp_node = node_at(&instance, &fs, "tmp").await;
            let fired = Arc::new(AtomicBool::new(false));
            let runtime = tokio::runtime::Handle::current();
            let weak = Arc::downgrade(&fs);
            fs.set_write_hook(Arc::new(move |step| {
                if step == (WriteStep::Admitting { node: held })
                    && !fired.swap(true, Ordering::SeqCst)
                {
                    let fs = weak.upgrade().expect("the provider");
                    let done = Arc::new(AtomicBool::new(false));
                    let finished = done.clone();
                    runtime.spawn(async move {
                        if unlinked {
                            fs.unlink(&request(), ROOT_INO, b"k")
                                .await
                                .expect("unlink k");
                        }
                        fs.rename(&request(), ROOT_INO, b"tmp", ROOT_INO, b"k", 0)
                            .await
                            .expect("rename onto k");
                        finished.store(true, Ordering::SeqCst);
                    });
                    while !done.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));

            let found = lookup(&fs, ROOT_INO, "k").await;

            assert_eq!(found, Ok(tmp), "the mover's inode");
            assert_eq!(
                fs.inodes().live_children(ROOT_INO, hash_string("k"), "k"),
                vec![(tmp, tmp_node)],
                "no live inode for the detached node {held} at k"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_overtaken_at_its_insert_by_a_replacing_rename_leaves_no_inode_for_the_holder() {
    a_lookup_overtaken_at_its_insert_leaves_no_inode_for_the_detached_node(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_overtaken_at_its_insert_by_an_unlink_and_rename_leaves_no_inode_for_the_holder() {
    a_lookup_overtaken_at_its_insert_leaves_no_inode_for_the_detached_node(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_added_lookup_overtaken_at_its_insert_by_a_replacing_rename_leaves_no_inode_for_it() {
    a_lookup_overtaken_at_its_insert_leaves_no_inode_for_the_detached_node(false, true).await;
}

/// A LOOKUP that confirmed a listed file from its record, before an UNLINK detached it and with
/// no inode for it yet, makes none for the detached node: it answers the name absent.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_overtaken_by_an_unlink_makes_no_inode_for_the_detached_node() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    let instance = Instance::new(&[("keep.txt", b"k"), ("gone.txt", b"held")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["gone.txt", "keep.txt"]);
            let gone = node_at(&instance, &fs, "gone.txt").await;
            let fired = Arc::new(AtomicBool::new(false));
            let weak = Arc::downgrade(&fs);
            fs.set_lookup_hook(Arc::new(move |step| {
                let (weak, fired) = (weak.clone(), fired.clone());
                Box::pin(async move {
                    let LookupStep::Listed { node, .. } = step else {
                        return;
                    };
                    if node != gone || fired.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let fs = weak.upgrade().expect("the provider");
                    fs.unlink(&request(), ROOT_INO, b"gone.txt")
                        .await
                        .expect("unlink");
                })
            }));

            let found = lookup(&fs, ROOT_INO, "gone.txt").await;

            assert_eq!(found, Err(Errno::ENOENT), "no inode for the detached node");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_overtaken_by_a_directory_renamed_over_one_keeps_the_movers_inode() {
    an_overtaken_lookup_keeps_the_movers_inode(Made::Dir, Made::Dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_overtaken_by_a_file_renamed_over_a_symlink_keeps_the_movers_inode() {
    an_overtaken_lookup_keeps_the_movers_inode(Made::Symlink, Made::File(b"moved in")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_overtaken_by_a_symlink_renamed_over_a_file_keeps_the_movers_inode() {
    an_overtaken_lookup_keeps_the_movers_inode(Made::File(b"held"), Made::Symlink).await;
}

/// LOOKUPs of names that two writers keep replacing with fresh temporaries, CREATE then RENAME
/// over the name under the directory's lock as the kernel takes it, never miss a name: a replace
/// logs the holder's detach and the mover's attach in one critical section. The writers save on
/// past their passes until a LOOKUP has run during a RENAME.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn lookups_during_replacing_renames_never_miss_the_name() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    const NAMES: usize = 8;
    const WRITERS: usize = 2;
    const PASSES: usize = 25;
    const READERS: usize = 3;
    // A bound on a hang, not a timing: an overlap is seen within the passes on an idle machine.
    const DEADLINE: Duration = Duration::from_secs(30);
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for k in 0..NAMES {
                create_with(&fs, dir, &format!("k{k}"), b"first").await;
            }
            let directory_lock = Arc::new(tokio::sync::Mutex::new(()));
            let stop = Arc::new(AtomicBool::new(false));
            let lookups = Arc::new(AtomicUsize::new(0));
            let missed = Arc::new(AtomicUsize::new(0));
            // Odd while a RENAME runs.
            let renaming = Arc::new(AtomicUsize::new(0));
            let overlapped = Arc::new(AtomicUsize::new(0));
            let mut readers = tokio::task::JoinSet::new();
            for _ in 0..READERS {
                let fs = fs.clone();
                let (stop, lookups, missed) = (stop.clone(), lookups.clone(), missed.clone());
                let (renaming, overlapped) = (renaming.clone(), overlapped.clone());
                let _ = lore_base::lore_spawn!(readers, async move {
                    while !stop.load(Ordering::SeqCst) {
                        for k in 0..NAMES {
                            let name = format!("k{k}");
                            lookups.fetch_add(1, Ordering::SeqCst);
                            let before = renaming.load(Ordering::SeqCst);
                            let found = lookup(&fs, dir, &name).await;
                            let after = renaming.load(Ordering::SeqCst);
                            if before % 2 == 1 || before != after {
                                overlapped.fetch_add(1, Ordering::SeqCst);
                            }
                            match found {
                                Ok(_) => {}
                                Err(Errno::ENOENT) => {
                                    missed.fetch_add(1, Ordering::SeqCst);
                                }
                                Err(errno) => panic!("lookup of {name}: {errno:?}"),
                            }
                        }
                        tokio::task::yield_now().await;
                    }
                });
            }
            let deadline = std::time::Instant::now() + DEADLINE;
            let mut writers = tokio::task::JoinSet::new();
            for writer in 0..WRITERS {
                let fs = fs.clone();
                let (directory_lock, renaming) = (directory_lock.clone(), renaming.clone());
                let overlapped = overlapped.clone();
                let _ = lore_base::lore_spawn!(writers, async move {
                    let mut failed = Vec::new();
                    let mut pass = 0;
                    while pass < PASSES
                        || (overlapped.load(Ordering::SeqCst) == 0
                            && std::time::Instant::now() < deadline)
                    {
                        for k in (writer..NAMES).step_by(WRITERS) {
                            let temporary = format!("w{writer}-{pass}-{k}.tmp");
                            let _locked = directory_lock.lock().await;
                            create_with(&fs, dir, &temporary, b"saved").await;
                            renaming.fetch_add(1, Ordering::SeqCst);
                            let renamed = fs
                                .rename(
                                    &request(),
                                    dir,
                                    temporary.as_bytes(),
                                    dir,
                                    format!("k{k}").as_bytes(),
                                    0,
                                )
                                .await;
                            renaming.fetch_add(1, Ordering::SeqCst);
                            if let Err(errno) = renamed {
                                failed.push((temporary, errno));
                            }
                        }
                        pass += 1;
                        tokio::task::yield_now().await;
                    }
                    failed
                });
            }
            let mut failed = Vec::new();
            while let Some(joined) = writers.join_next().await {
                failed.extend(joined.expect("writer"));
            }
            stop.store(true, Ordering::SeqCst);
            while let Some(joined) = readers.join_next().await {
                joined.expect("reader");
            }
            assert!(
                overlapped.load(Ordering::SeqCst) > 0,
                "no LOOKUP ran during a RENAME, of {}",
                lookups.load(Ordering::SeqCst)
            );
            assert_eq!(
                missed.load(Ordering::SeqCst),
                0,
                "LOOKUPs that missed a name a RENAME replaced, of {}",
                lookups.load(Ordering::SeqCst)
            );
            assert_eq!(failed, Vec::new(), "RENAMEs that failed");
            for k in 0..NAMES {
                let name = format!("k{k}");
                let held = node_at(&instance, &fs, &format!("dir/{name}")).await;
                let live: Vec<NodeID> = fs
                    .inodes()
                    .live_children(dir, hash_string(&name), &name)
                    .into_iter()
                    .map(|(_, node)| node)
                    .collect();
                assert_eq!(
                    live,
                    vec![held],
                    "{name}: one live inode, for the node the State holds"
                );
            }
        })
        .await;
}

/// RENAMEs of one file back and forth between two spellings, in bursts of one to four, each burst
/// followed by a LOOKUP of both: the spelling the chain holds answers the file's inode and the
/// other `ENOENT`, whichever is looked up first.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_storm_of_two_spellings_answers_each_spelling_as_the_chain_holds_it() {
    const BURSTS: usize = 40;
    const SPELLINGS: [&str; 2] = ["x.o", "y.o"];
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..4 {
                create_with(&fs, dir, &format!("other-{i}.o"), b"o").await;
            }
            let file = create_with(&fs, dir, SPELLINGS[0], b"x").await;
            assert_eq!(lookup(&fs, dir, SPELLINGS[0]).await, Ok(file));
            let mut held = 0;
            for burst in 0..BURSTS {
                for _ in 0..=burst % 4 {
                    let to = 1 - held;
                    fs.rename(
                        &request(),
                        dir,
                        SPELLINGS[held].as_bytes(),
                        dir,
                        SPELLINGS[to].as_bytes(),
                        0,
                    )
                    .await
                    .expect("rename");
                    held = to;
                }
                let mut order = [held, 1 - held];
                if burst % 2 == 1 {
                    order.reverse();
                }
                for spelling in order {
                    let expected = if spelling == held {
                        Ok(file)
                    } else {
                        Err(Errno::ENOENT)
                    };
                    assert_eq!(
                        lookup(&fs, dir, SPELLINGS[spelling]).await,
                        expected,
                        "burst {burst}: {} while the chain holds {}",
                        SPELLINGS[spelling],
                        SPELLINGS[held]
                    );
                }
            }
        })
        .await;
}

/// LOOKUP answers a name only in the spelling its directory holds, as a case-sensitive
/// filesystem does: were another spelling to answer with the same inode, the kernel would take a
/// rename between the two for a rename onto itself and never send it. Another spelling answers
/// absent, a RENAME to it respells the name, and then the old spelling answers absent.
#[tokio::test(flavor = "multi_thread")]
async fn a_lookup_answers_only_the_spelling_its_directory_holds() {
    let instance = Instance::new(&[("Docs/Guide.txt", b"g")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let docs = lookup(&fs, ROOT_INO, "Docs").await.expect("Docs");
            assert_eq!(lookup(&fs, ROOT_INO, "docs").await, Err(Errno::ENOENT));
            let file = lookup(&fs, docs, "Guide.txt").await.expect("Guide.txt");
            assert_eq!(lookup(&fs, docs, "guide.txt").await, Err(Errno::ENOENT));

            fs.rename(&request(), docs, b"Guide.txt", docs, b"guide.txt", 0)
                .await
                .expect("a rename to another spelling");
            assert_eq!(lookup(&fs, docs, "guide.txt").await, Ok(file));
            assert_eq!(lookup(&fs, docs, "Guide.txt").await, Err(Errno::ENOENT));
            assert_eq!(listed(&fs, docs).await, vec!["guide.txt"]);
        })
        .await;
}

/// A RENAME respelling a name only in case keeps its hash and its record: LOOKUP finds the new
/// spelling in the record with no build, and the old one absent; READDIR, which lists the
/// listing's spellings, builds once to list the new one.
#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_serves_the_new_spelling_without_a_build() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let file = create_with(&fs, dir, "foo.txt", b"f").await;
            create_with(&fs, dir, "bar.txt", b"b").await;
            assert_eq!(listed(&fs, dir).await, vec!["bar.txt", "foo.txt"]);
            let built = fs.listings_built();

            fs.rename(&request(), dir, b"foo.txt", dir, b"Foo.txt", 0)
                .await
                .expect("respell");
            assert_eq!(lookup(&fs, dir, "Foo.txt").await, Ok(file));
            assert_eq!(lookup(&fs, dir, "foo.txt").await, Err(Errno::ENOENT));
            assert_eq!(lookup(&fs, dir, "bar.txt").await.map(|_| ()), Ok(()));
            assert_eq!(
                fs.listings_built(),
                built,
                "LOOKUP reads the spelling from the record"
            );
            assert_eq!(listed(&fs, dir).await, vec!["Foo.txt", "bar.txt"]);
            assert_eq!(
                fs.listings_built() - built,
                1,
                "READDIR lists the new spelling from one build"
            );
            fs.write(&request(), file, 0, 0, b"F", 0)
                .await
                .expect("write");
            assert_eq!(
                names(&instance.write_dir().join("dir")),
                ["Foo.txt", "bar.txt"]
            );
        })
        .await;
}

/// A listed file renamed out of its name and back under another case, through a temporary in
/// its directory or, with `through_other_directory`, through another directory, keeps its hash
/// and its record but not its spelling: LOOKUP answers the new spelling, and UNLINK by it removes
/// the host file.
async fn moved_away_and_back_in_another_case(through_other_directory: bool) {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |name: &[u8]| {
                fs.mkdir(&request(), ROOT_INO, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let dir = mkdir(b"d1").await;
            let away = if through_other_directory {
                mkdir(b"d2").await
            } else {
                dir
            };
            let file = create_with(&fs, dir, "foo.txt", b"f").await;
            create_with(&fs, dir, "bar.txt", b"b").await;
            assert_eq!(listed(&fs, dir).await, vec!["bar.txt", "foo.txt"]);

            fs.rename(&request(), dir, b"foo.txt", away, b"tmp", 0)
                .await
                .expect("rename away");
            fs.rename(&request(), away, b"tmp", dir, b"FOO.txt", 0)
                .await
                .expect("rename back");

            let case = if through_other_directory {
                "through d2"
            } else {
                "in d1"
            };
            assert_eq!(lookup(&fs, dir, "FOO.txt").await, Ok(file), "{case}");
            assert_eq!(
                fs.inodes().get(file).map(|entry| entry.name.to_vec()),
                Some(b"FOO.txt".to_vec()),
                "{case}: the inode's spelling"
            );
            assert_eq!(
                fs.unlink(&request(), dir, b"FOO.txt").await,
                Ok(()),
                "{case}: UNLINK by the new spelling"
            );
            assert_eq!(
                names(&instance.write_dir().join("d1")),
                ["bar.txt"],
                "{case}: the host file is removed"
            );
            assert_eq!(lookup(&fs, dir, "FOO.txt").await, Err(Errno::ENOENT));
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_moved_away_and_back_in_another_case_answers_the_new_spelling() {
    moved_away_and_back_in_another_case(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_moved_to_another_directory_and_back_in_another_case_answers_the_new_spelling() {
    moved_away_and_back_in_another_case(true).await;
}

/// Saves through a temporary in a listed directory, each a LOOKUP of the temporary and of the
/// output as the kernel sends them, then CREATE, RENAME onto the output and UNLINK of the save
/// before: four patches a save, so once the log has grown to its largest it holds 256 saves and
/// the listing is built once per 256 saves, never once per save.
#[tokio::test(flavor = "multi_thread")]
async fn saves_through_a_temporary_build_their_listing_once_per_256_saves() {
    use lore_revision::node::CHAIN_LOG_MAX;
    // A save's create, its rename's detach and attach, and the unlink of the save before.
    const PATCHES_PER_SAVE: usize = 4;
    const SAVES: usize = 1024;
    // 16 + 64 + 256 patches fill the rings before the largest, with a few saves to spare.
    const WARM_UP_SAVES: usize = 128;
    let instance = Instance::new(&[("dir/f0.o", b"0"), ("dir/f1.o", b"1")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "f0.o").await.expect("f0.o");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let mut previous: Option<String> = None;
            let mut save = 0;
            let mut save_once = async |fs: &FuseFilesystem| {
                let temporary = format!("s{save}.tmp");
                let output = format!("s{save}.o");
                assert_eq!(lookup(fs, dir, &temporary).await, Err(Errno::ENOENT));
                create_with(fs, dir, &temporary, b"saved").await;
                assert_eq!(lookup(fs, dir, &output).await, Err(Errno::ENOENT));
                fs.rename(
                    &request(),
                    dir,
                    temporary.as_bytes(),
                    dir,
                    output.as_bytes(),
                    0,
                )
                .await
                .expect("rename");
                if let Some(before) = previous.replace(output) {
                    fs.unlink(&request(), dir, before.as_bytes())
                        .await
                        .expect("unlink");
                }
                lookup(fs, dir, "f1.o").await.expect("f1.o");
                save += 1;
            };
            // Warm-up: each overflow's build grows the log fourfold, up to its largest.
            for _ in 0..WARM_UP_SAVES {
                if log_capacity(&instance, &fs, dir_node).await == CHAIN_LOG_MAX {
                    break;
                }
                save_once(&fs).await;
            }
            assert_eq!(
                log_capacity(&instance, &fs, dir_node).await,
                CHAIN_LOG_MAX,
                "the log grew to its largest within {WARM_UP_SAVES} saves"
            );
            let built = fs.listings_built();
            for _ in 0..SAVES {
                save_once(&fs).await;
            }
            let builds = (fs.listings_built() - built) as usize;
            println!("{SAVES} saves built the listing {builds} times");
            assert!(builds > 0, "the log never overflowed");
            assert!(
                builds <= SAVES * PATCHES_PER_SAVE / CHAIN_LOG_MAX,
                "{SAVES} saves built the listing {builds} times"
            );
        })
        .await;
}

/// RENAME onto a name the State does not hold but `write/` does, as only a disagreement between
/// the two leaves it, answers `EEXIST` and replaces nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_onto_a_name_only_the_host_holds_replaces_nothing() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let tmp = create_with(&fs, ROOT_INO, "tmp", b"new").await;
            plant(&instance, &[("out", b"host only")]);
            assert_eq!(
                fs.rename(&request(), ROOT_INO, b"tmp", ROOT_INO, b"out", 0)
                    .await,
                Err(Errno::EEXIST)
            );
            assert_eq!(
                std::fs::read(instance.write_dir().join("out")).expect("write/out"),
                b"host only"
            );
            assert_eq!(lookup(&fs, ROOT_INO, "tmp").await, Ok(tmp));
            assert_eq!(read(&fs, tmp, 64).await, b"new");
        })
        .await;
}

/// CREATE and MKDIR of a name the State does not hold but `write/` does answer `EEXIST`, leave
/// the host entry as it was and attach nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_or_mkdir_of_a_name_only_the_host_holds_answers_eexist() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            plant(&instance, &[("out", b"host only")]);
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            let created = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"out",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await;
            assert_eq!(created.err(), Some(Errno::EEXIST), "CREATE");
            let made = fs.mkdir(&request(), ROOT_INO, b"out", 0o755, 0o022).await;
            assert_eq!(made.err(), Some(Errno::EEXIST), "MKDIR");
            assert_eq!(
                std::fs::read(instance.write_dir().join("out")).expect("write/out"),
                b"host only"
            );
            assert_eq!(lookup(&fs, ROOT_INO, "out").await, Err(Errno::ENOENT));
        })
        .await;
}

/// A copy-up of a rename's target that finishes while the rename waits for it leaves the target
/// local, so the rename sets the copied host file aside, and a failed State step puts it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_that_waited_on_a_copy_up_of_its_target_puts_the_copy_back_when_it_fails() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("out", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "tmp", b"new").await;
            let out = lookup(&fs, ROOT_INO, "out").await.expect("out");
            let copying = Arc::new(AtomicBool::new(false));
            let gated = Arc::new(AtomicBool::new(false));
            let abandoned = Arc::new(AtomicBool::new(false));
            let (hook_copying, hook_gated, hook_abandoned) =
                (copying.clone(), gated.clone(), abandoned.clone());
            // The copy-up holds the target's gate until the rename reaches it, so the rename has
            // resolved the target as tracked before the copy-up ends.
            fs.set_write_hook(Arc::new(move |step| match step {
                WriteStep::CopyUp => {
                    hook_copying.store(true, Ordering::SeqCst);
                    while !hook_gated.load(Ordering::SeqCst)
                        && !hook_abandoned.load(Ordering::SeqCst)
                    {
                        std::thread::yield_now();
                    }
                    true
                }
                WriteStep::TargetGate { ino } if ino == out => {
                    hook_gated.store(hook_copying.load(Ordering::SeqCst), Ordering::SeqCst);
                    true
                }
                WriteStep::Rename => false,
                _ => true,
            }));
            let writer = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    fs.write(&request(), out, 0, 0, b"TRACKED", 0).await
                })
            };
            while !copying.load(Ordering::SeqCst) {
                assert!(!writer.is_finished(), "the write ended before its copy-up");
                tokio::task::yield_now().await;
            }
            let renaming = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    fs.rename(&request(), ROOT_INO, b"tmp", ROOT_INO, b"out", 0)
                        .await
                })
            };
            // A bound on a hang, never reached by a passing run: a rename that takes the gate
            // without passing the seam waits on a copy-up that waits for the seam.
            let reached = tokio::time::timeout(Duration::from_secs(30), renaming).await;
            abandoned.store(true, Ordering::SeqCst);
            let renamed = reached
                .expect("the rename waited on the copy-up without reaching the gate")
                .expect("renamer");
            writer
                .await
                .expect("writer")
                .expect("the write copies out up");
            assert!(
                gated.load(Ordering::SeqCst),
                "the rename reached the target's copy-up gate while the copy-up held it"
            );
            assert!(renamed.is_err(), "the State step fails");
            assert_eq!(
                std::fs::read(instance.write_dir().join("out")).expect("write/out"),
                b"TRACKED"
            );
            assert_eq!(read(&fs, out, 64).await, b"TRACKED");
            let tmp = lookup(&fs, ROOT_INO, "tmp").await.expect("tmp");
            assert_eq!(read(&fs, tmp, 64).await, b"new");
        })
        .await;
}

/// Saves through a temporary, one after another as the kernel serializes them, in a directory
/// where a node is respelled in place right after every listing build of it, so every listing is
/// stale by the time it is read for a name it does not hold: no request answers `EAGAIN`, CREATE
/// and RENAME build nothing, and a LOOKUP of each output, listed by the build it forces, builds
/// only that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saves_through_a_temporary_in_a_directory_stale_at_every_read_never_answer_eagain() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    const FILES: usize = 100;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let obj = fs
                .mkdir(&request(), ROOT_INO, b"obj", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..FILES {
                create_with(&fs, obj, &format!("f{i}.o"), b"x").await;
            }
            create_with(&fs, obj, "churn", b"c").await;
            let obj_node = node_at(&instance, &fs, "obj").await;
            let churn = node_at(&instance, &fs, "obj/churn").await;
            let (state, repository) = (fs.state().clone(), instance.fixture.repository.clone());
            let flip = Arc::new(AtomicBool::new(false));
            fs.set_listing_hook(Arc::new(move |node| {
                let (state, repository, flip) = (state.clone(), repository.clone(), flip.clone());
                Box::pin(async move {
                    if node != obj_node {
                        return;
                    }
                    let name = if flip.fetch_xor(true, Ordering::SeqCst) {
                        "churn"
                    } else {
                        "CHURN"
                    };
                    state
                        .node_reattach(repository, churn, obj_node, name)
                        .await
                        .expect("rename the churning node");
                })
            }));
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            let mut errors = Vec::new();
            let mut building = Vec::new();
            let mut rebuilding = Vec::new();
            for i in 0..FILES {
                let tmp = format!("tmp{i}");
                let output = format!("f{i}.o");
                let built = fs.listings_built();
                let created = fs
                    .create(
                        &request(),
                        obj,
                        tmp.as_bytes(),
                        libc::S_IFREG | 0o644,
                        0o022,
                        flags,
                    )
                    .await;
                let rebuilt = fs.listings_built() - built;
                if rebuilt != 0 {
                    building.push(("create", i, rebuilt));
                }
                if let Err(errno) = created {
                    errors.push(("create", i, errno));
                    continue;
                }
                let built = fs.listings_built();
                if let Err(errno) = fs
                    .rename(&request(), obj, tmp.as_bytes(), obj, output.as_bytes(), 0)
                    .await
                {
                    errors.push(("rename", i, errno));
                }
                let rebuilt = fs.listings_built() - built;
                if rebuilt != 0 {
                    building.push(("rename", i, rebuilt));
                }
                // The LOOKUP builds afresh, with the hook's respelling inside the build.
                fs.served().listings.remove(obj_node);
                let built = fs.listings_built();
                if let Err(errno) = lookup(&fs, obj, &output).await {
                    errors.push(("lookup", i, errno));
                }
                let rebuilt = fs.listings_built() - built;
                if rebuilt != 1 {
                    rebuilding.push((i, rebuilt));
                }
            }
            assert_eq!(
                building,
                Vec::new(),
                "CREATEs and RENAMEs that built a listing"
            );
            assert_eq!(
                rebuilding,
                Vec::new(),
                "LOOKUPs of a listed output that built other than the one forced listing"
            );
            assert!(errors.is_empty(), "saves failed: {errors:?}");
        })
        .await;
}

/// What [`replace_loop_in_a_large_directory`] saw.
struct ReplaceLoop {
    rounds: usize,
    create_eagain: usize,
    reads: usize,
    /// Reads that ran while a replace did.
    overlapped: usize,
    missed: usize,
    failed: usize,
    /// Listings built while the rounds ran.
    built: u64,
}

/// A directory of a thousand files where `b.txt` is replaced again and again through the State,
/// as RENAME does it, each replacement a file CREATE makes, while three operation readers
/// ask for `b.txt`. Gives up once `limit` passes.
async fn replace_loop_in_a_large_directory(instance: &Instance, limit: Duration) -> ReplaceLoop {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    const FILLER: usize = 1000;
    const ROUNDS: usize = 100;
    const READERS: usize = 3;
    let fs = provider(instance).await;
    let repository = instance.fixture.repository.clone();
    let dir = fs
        .mkdir(&request(), ROOT_INO, b"big", libc::S_IFDIR | 0o755, 0o022)
        .await
        .expect("mkdir")
        .ino;
    let dir_node = node_at(instance, &fs, "big").await;
    create_with(&fs, dir, "b.txt", b"held").await;
    for i in 0..FILLER {
        create_with(&fs, dir, &format!("filler-{i}"), b"f").await;
    }
    // The root's listing current again after the MKDIR, so a reader's walk to `big` builds none.
    lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone())
        .file_info(&relative("big/b.txt"))
        .await
        .expect("a first read");
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let missed = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    // Odd while a replace runs.
    let replacing = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicUsize::new(0));
    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..READERS {
        let fs = fs.clone();
        let (stop, reads, missed, failed) =
            (stop.clone(), reads.clone(), missed.clone(), failed.clone());
        let (replacing, overlapped) = (replacing.clone(), overlapped.clone());
        let _ = lore_base::lore_spawn!(readers, async move {
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs);
            let path = relative("big/b.txt");
            while !stop.load(Ordering::SeqCst) {
                reads.fetch_add(1, Ordering::SeqCst);
                let before = replacing.load(Ordering::SeqCst);
                let info = operation.file_info(&path).await;
                let after = replacing.load(Ordering::SeqCst);
                if before % 2 == 1 || before != after {
                    overlapped.fetch_add(1, Ordering::SeqCst);
                }
                match info {
                    Ok(FileInfo::NotExist) => {
                        missed.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        failed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                tokio::task::yield_now().await;
            }
        });
    }
    let host = instance.write_dir().join("big");
    let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
    let began = std::time::Instant::now();
    let built = fs.listings_built();
    let mut create_eagain = 0;
    let mut rounds = 0;
    'rounds: for round in 0..ROUNDS {
        let name = format!("a{round}.txt");
        loop {
            match fs
                .create(
                    &request(),
                    dir,
                    name.as_bytes(),
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
            {
                Ok(_) => break,
                Err(Errno::EAGAIN) => {
                    create_eagain += 1;
                    if began.elapsed() > limit {
                        break 'rounds;
                    }
                    tokio::task::yield_now().await;
                }
                Err(errno) => panic!("create {name}: {errno:?}"),
            }
        }
        let holder = node_at(instance, &fs, "big/b.txt").await;
        let moved = node_at(instance, &fs, &format!("big/{name}")).await;
        std::fs::rename(host.join(&name), host.join("b.txt")).expect("host rename");
        replacing.fetch_add(1, Ordering::SeqCst);
        fs.state()
            .node_reattach_replacing(repository.clone(), moved, dir_node, "b.txt", holder)
            .await
            .expect("replace");
        replacing.fetch_add(1, Ordering::SeqCst);
        rounds += 1;
        tokio::task::yield_now().await;
    }
    let built = fs.listings_built() - built;
    stop.store(true, Ordering::SeqCst);
    while let Some(joined) = readers.join_next().await {
        joined.expect("reader");
    }
    ReplaceLoop {
        rounds,
        create_eagain,
        reads: reads.load(Ordering::SeqCst),
        overlapped: overlapped.load(Ordering::SeqCst),
        missed: missed.load(Ordering::SeqCst),
        failed: failed.load(Ordering::SeqCst),
        built,
    }
}

/// CREATE in a directory of a thousand files keeps succeeding at once while three operation
/// readers follow a name replaced again and again: neither a CREATE nor a reader builds a
/// listing, each answering its one name from the cached listing's log or the State.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn create_keeps_succeeding_while_readers_follow_replaces_in_a_large_directory() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let seen = replace_loop_in_a_large_directory(&instance, Duration::from_secs(20)).await;
            assert!(
                seen.overlapped > 0,
                "no read ran during a replace, of {} reads",
                seen.reads
            );
            assert_eq!(
                (seen.rounds, seen.create_eagain),
                (100, 0),
                "every CREATE succeeds at once ({} reads, {} failed)",
                seen.reads,
                seen.failed
            );
            assert_eq!(seen.built, 0, "listings built during the rounds");
        })
        .await;
}

/// An operation reader asking for a name held throughout replaces in a directory of a
/// thousand files always finds it: each replace is read from the directory's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn file_info_of_a_name_held_throughout_replaces_in_a_large_directory_finds_it() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let seen = replace_loop_in_a_large_directory(&instance, Duration::from_secs(20)).await;
            assert!(
                seen.overlapped > 0,
                "no read ran during a replace, of {} reads",
                seen.reads
            );
            assert_eq!(
                (seen.missed, seen.failed),
                (0, 0),
                "file_info answered NotExist or failed for a held name, of {} reads",
                seen.reads
            );
        })
        .await;
}

/// A directory where a node is respelled in place after every listing build of it still answers
/// each name: LOOKUP of a listed name reads it from its record and builds nothing; one attached
/// since the listing rebuilds once into a listing that holds it; one no listing holds rebuilds
/// past every rebuild a request allows and is found absent by name; CREATE and the operation
/// reader answer from the State.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_changing_past_every_rebuild_still_answers_each_name() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let held = create_with(&fs, dir, "held.txt", b"held").await;
            create_with(&fs, dir, "churn", b"c").await;
            let dir_node = node_at(&instance, &fs, "dir").await;
            let churn = node_at(&instance, &fs, "dir/churn").await;
            let (state, repository) = (fs.state().clone(), instance.fixture.repository.clone());
            let flip = Arc::new(AtomicBool::new(false));
            let rename_churn = move || {
                let (state, repository, flip) = (state.clone(), repository.clone(), flip.clone());
                async move {
                    let name = if flip.fetch_xor(true, Ordering::SeqCst) {
                        "churn"
                    } else {
                        "CHURN"
                    };
                    state
                        .node_reattach(repository, churn, dir_node, name)
                        .await
                        .expect("rename the churning node");
                }
            };
            let changing = rename_churn.clone();
            fs.set_listing_hook(Arc::new(move |node| {
                let change = changing.clone();
                Box::pin(async move {
                    if node == dir_node {
                        change().await;
                    }
                })
            }));
            // The provider's `STALE_REBUILDS`: each request through the kernel below rebuilds at
            // least as often.
            let past_the_bound = |built: u64, request: &str| {
                let rebuilt = fs.listings_built() - built;
                assert!(rebuilt >= 4, "{request} rebuilt {rebuilt} listings");
            };
            rename_churn().await;
            let built = fs.listings_built();
            let found = lookup(&fs, dir, "absent.txt").await;
            past_the_bound(built, "LOOKUP of absent.txt");
            assert_eq!(found, Err(Errno::ENOENT));
            rename_churn().await;
            let built = fs.listings_built();
            assert_eq!(lookup(&fs, dir, "held.txt").await, Ok(held));
            assert_eq!(
                fs.listings_built(),
                built,
                "a listed name is read from its record"
            );
            rename_churn().await;
            let built = fs.listings_built();
            let created = create_with(&fs, dir, "created.txt", b"new").await;
            assert_eq!(
                fs.listings_built(),
                built,
                "a CREATE answers its name from the State and builds nothing"
            );
            rename_churn().await;
            let built = fs.listings_built();
            let found = lookup(&fs, dir, "created.txt").await;
            assert_eq!(
                fs.listings_built() - built,
                1,
                "a name attached since the listing rebuilds once, into a listing that holds it"
            );
            assert_eq!(found, Ok(created));
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            rename_churn().await;
            assert!(matches!(
                operation
                    .file_info(&relative("dir/held.txt"))
                    .await
                    .expect("file_info"),
                FileInfo::File { .. }
            ));
            assert!(matches!(
                operation
                    .file_info(&relative("dir/absent.txt"))
                    .await
                    .expect("file_info"),
                FileInfo::NotExist
            ));
        })
        .await;
}

/// Renames landing inside a listing build, after its walk and before it is cached, are read from
/// the directory's log: an operation reader finds the name they bring in through the one build.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_landing_inside_a_build_is_read_from_the_log() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir_node = node_at(&instance, &fs, "dir").await;
            let moved = node_at(&instance, &fs, "dir/a.txt").await;
            let (state, repository) = (fs.state().clone(), instance.fixture.repository.clone());
            let builds = Arc::new(AtomicUsize::new(0));
            let hooked = builds.clone();
            fs.set_listing_hook(Arc::new(move |node| {
                let (state, repository, builds) =
                    (state.clone(), repository.clone(), hooked.clone());
                Box::pin(async move {
                    if node != dir_node || builds.fetch_add(1, Ordering::SeqCst) != 0 {
                        return;
                    }
                    for name in ["b.txt", "c.txt"] {
                        state
                            .node_reattach(repository.clone(), moved, dir_node, name)
                            .await
                            .expect("rename");
                    }
                })
            }));
            fs.served().listings.remove(dir_node);
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            let info = operation
                .file_info(&relative("dir/c.txt"))
                .await
                .expect("file_info");
            assert_eq!(
                builds.load(Ordering::SeqCst),
                1,
                "the read built the listing once"
            );
            assert!(matches!(info, FileInfo::File { .. }));
        })
        .await;
}

/// An operation read of one name in a directory that a rename touches right after every
/// listing build of it builds no listing for the name: only the walk's own build of the
/// directory runs, and the rename inside it is read from the log.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_read_in_a_changing_directory_builds_no_listing_for_its_name() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    let instance = Instance::new(&[("dir/held.txt", b"held"), ("dir/churn", b"c")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir_node = node_at(&instance, &fs, "dir").await;
            let churn = node_at(&instance, &fs, "dir/churn").await;
            let (state, repository) = (fs.state().clone(), instance.fixture.repository.clone());
            let flip = Arc::new(AtomicBool::new(false));
            fs.set_listing_hook(Arc::new(move |node| {
                let (state, repository, flip) = (state.clone(), repository.clone(), flip.clone());
                Box::pin(async move {
                    if node != dir_node {
                        return;
                    }
                    let name = if flip.fetch_xor(true, Ordering::SeqCst) {
                        "churn"
                    } else {
                        "churned"
                    };
                    state
                        .node_reattach(repository, churn, dir_node, name)
                        .await
                        .expect("rename the churning node");
                })
            }));
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            assert!(matches!(
                operation
                    .file_info(&relative("keep.txt"))
                    .await
                    .expect("file_info"),
                FileInfo::NotExist
            ));
            fs.served().listings.remove(dir_node);
            let before = fs.listings_built();
            assert!(matches!(
                operation
                    .file_info(&relative("dir/held.txt"))
                    .await
                    .expect("file_info"),
                FileInfo::File { .. }
            ));
            assert_eq!(
                fs.listings_built() - before,
                1,
                "listings built for one name"
            );
        })
        .await;
}

/// A directory read whose listing build has a listed entry's record changed inside it builds
/// once: a record change leaves the chain alone, and the read serves the record as it stands.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_read_builds_once_and_serves_a_record_changed_during_the_build() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::FileInfo;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    let instance = Instance::new(&[("dir/held.txt", b"held"), ("dir/changing", b"c")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir_node = node_at(&instance, &fs, "dir").await;
            let changing = node_at(&instance, &fs, "dir/changing").await;
            let (state, repository) = (fs.state().clone(), instance.fixture.repository.clone());
            let size = Arc::new(AtomicU64::new(1));
            let builds = size.clone();
            fs.set_listing_hook(Arc::new(move |node| {
                let (state, repository, size) = (state.clone(), repository.clone(), size.clone());
                Box::pin(async move {
                    if node != dir_node {
                        return;
                    }
                    let size = size.fetch_add(1, Ordering::SeqCst) + 1;
                    let record = state
                        .node(repository.clone(), changing)
                        .await
                        .expect("the changing node");
                    state
                        .node_rewrite(
                            &repository,
                            changing,
                            &Content {
                                mode: record.mode,
                                size,
                                address: record.address,
                            },
                            Rewrite::Plain,
                        )
                        .await
                        .expect("change the listed node");
                })
            }));
            fs.served().listings.remove(dir_node);
            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            let mut listed = operation
                .read_directory(&relative("dir"))
                .await
                .expect("the directory is read");
            let mut entries = Vec::new();
            while let Some(entry) = listed.next().await {
                let entry = entry.expect("entry");
                entries.push((entry.name, entry.info));
            }
            assert_eq!(builds.load(Ordering::SeqCst) - 1, 1, "one build of dir");
            assert!(
                entries.iter().any(|(name, _)| name == "held.txt"),
                "{entries:?}"
            );
            assert!(
                entries.iter().any(|(name, info)| name == "changing"
                    && matches!(info, FileInfo::File { size: 2, .. })),
                "the record as the build's change left it: {entries:?}"
            );
        })
        .await;
}

/// The lines of the instance's deletion list.
fn deleted(instance: &Instance) -> Vec<String> {
    std::fs::read_to_string(instance.external.path().join("deleted"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// CREATE puts the file in the State and in `write/` before its reply: the next LOOKUP finds
/// the inode CREATE answered, READDIR lists the name beside the tracked ones, and `write/` holds
/// the bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_file_is_found_by_the_next_lookup_and_listing() {
    let instance = Instance::new(&[("dir/tracked.txt", b"t")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            assert_eq!(listed(&fs, dir).await, vec!["tracked.txt"]);
            let created = create_with(&fs, dir, "new.txt", b"fresh").await;
            assert_eq!(lookup(&fs, dir, "new.txt").await, Ok(created));
            assert_eq!(listed(&fs, dir).await, vec!["new.txt", "tracked.txt"]);
            assert_eq!(read(&fs, created, 64).await, b"fresh");
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/new.txt")).expect("write/dir/new.txt"),
                b"fresh"
            );
            let node = fs
                .state()
                .node(
                    instance.fixture.repository.clone(),
                    node_at(&instance, &fs, "dir/new.txt").await,
                )
                .await
                .expect("the created node");
            assert!(node.is_dirty_add());
            assert!(
                node.ext_flags()
                    .contains(lore_revision::node::NodeExtFlags::LocalContent)
            );
        })
        .await;
}

/// A CREATE in a directory with no cached listing answers `WouldFetch` on the worker and changes
/// nothing; its fetch builds the listing. When that listing is evicted before the completion
/// runs, the completion builds it again and creates the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_create_whose_blocks_were_evicted_refetches_and_completes() {
    let instance = Instance::new(&[("dir/tracked.txt", b"t")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let dir_node = node_at(&instance, &fs, "dir").await;
            assert!(fs.served().listings.get(dir_node).is_none(), "dir is cold");
            let mode = libc::S_IFREG | 0o644;
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;

            let local = fs
                .create_local(&request(), dir, b"new.txt", mode, 0o022, flags)
                .await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "a CREATE in a cold directory is deferred"
            );
            assert!(
                !instance.write_dir().join("dir/new.txt").exists(),
                "nothing is created on the worker"
            );

            let what = Fetch::Entry {
                parent: dir,
                name: b"new.txt",
                request: EntryRequest::Create,
            };
            fs.fetch(&request(), what).await.expect("the fetch");
            assert!(
                fs.served().listings.get(dir_node).is_some(),
                "the fetch built the listing"
            );
            fs.served().listings.clear();
            let built = fs.listings_built();

            let created = fs
                .create(&request(), dir, b"new.txt", mode, 0o022, flags)
                .await
                .expect("the completion creates the file");
            assert!(
                fs.listings_built() > built,
                "the completion fetched the directory again"
            );
            assert_eq!(lookup(&fs, dir, "new.txt").await, Ok(created.entry.ino));
            assert_eq!(listed(&fs, dir).await, vec!["new.txt", "tracked.txt"]);
            assert!(instance.write_dir().join("dir/new.txt").exists());
        })
        .await;
}

/// Node slots in a block.
const BLOCK_SLOTS: usize = 512;
/// Blocks of `c/` in [`Instance::with_holed_blocks`].
const HOLED_BLOCKS: usize = 8;
/// One file of `c/` in this many is deleted, leaving about three free slots a block.
const HOLE_SPACING: usize = 171;

/// CREATEs in two directories at once, on a mount whose revision's free slots are in blocks no
/// listing loaded, racing for the last free slots of a fresh block and for the next one, never
/// wait on the store on the worker, and every CREATE completes: the served State takes new nodes
/// from fresh blocks, the one after a full fresh block too. A CREATE whose directory's listing
/// needs a build defers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn creates_racing_for_the_last_free_slots_never_read_the_store_on_the_worker() {
    const PER_DIR: usize = 4;
    const ROUNDS: usize = BLOCK_SLOTS / (2 * PER_DIR) + 4;
    let instance = Instance::with_holed_blocks().await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            use std::sync::atomic::AtomicUsize;
            use std::sync::atomic::Ordering;

            use lore_revision::fs::fuse::filesystem::WriteStep;

            let (fs, store) = holding_provider(&instance).await;
            let blocks = fs.served().state.block_count();
            let mut dirs = Vec::new();
            for dir in ["a", "b"] {
                let ino = lookup(&fs, ROOT_INO, dir).await.expect("dir");
                lookup(&fs, ino, "x0").await.expect("x0");
                dirs.push(ino);
            }
            // CREATEs about to attach.
            let attaching = Arc::new(AtomicUsize::new(0));
            fs.set_write_hook(Arc::new({
                let attaching = attaching.clone();
                move |step| {
                    if let WriteStep::Attaching { .. } = step {
                        attaching.fetch_add(1, Ordering::SeqCst);
                    }
                    true
                }
            }));
            let mode = libc::S_IFREG | 0o644;
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            // CREATEs that reached the attach and deferred there.
            let mut deferred_at_attach = 0;
            for round in 0..ROUNDS {
                assert!(
                    fs.served().state.attach_slot_resident(),
                    "round {round}: the next slot needs nothing from the store"
                );
                store.hold();
                let before = attaching.load(Ordering::SeqCst);
                let started = Arc::new(tokio::sync::Barrier::new(2 * PER_DIR));
                let mut creates = tokio::task::JoinSet::new();
                for &dir in &dirs {
                    for worker in 0..PER_DIR {
                        let (fs, started) = (fs.clone(), started.clone());
                        let name = format!("r{round}-{worker}.o");
                        let _ = lore_base::lore_spawn!(creates, async move {
                            started.wait().await;
                            let local = tokio::time::timeout(
                                LOCAL_ANSWER,
                                fs.create_local(
                                    &request(),
                                    dir,
                                    name.as_bytes(),
                                    mode,
                                    0o022,
                                    flags,
                                ),
                            )
                            .await;
                            (dir, name, local)
                        });
                    }
                }
                let mut answers = Vec::new();
                while let Some(joined) = creates.join_next().await {
                    answers.push(joined.expect("a create task"));
                }
                store.release();
                let attached_locally = answers
                    .iter()
                    .filter(|(_, _, local)| matches!(local, Ok(Ok(_))))
                    .count();
                deferred_at_attach += attaching.load(Ordering::SeqCst) - before - attached_locally;
                for (dir, name, local) in answers {
                    match local.expect("no worker CREATE waits on the store") {
                        Ok(_) => {}
                        Err(ReadFault::WouldFetch) => {
                            let what = Fetch::Entry {
                                parent: dir,
                                name: name.as_bytes(),
                                request: EntryRequest::Create,
                            };
                            fs.fetch(&request(), what).await.expect("the fetch");
                            fs.create(&request(), dir, name.as_bytes(), mode, 0o022, flags)
                                .await
                                .expect("the completion creates the file");
                        }
                        Err(ReadFault::Errno(errno)) => panic!("CREATE {name}: {errno:?}"),
                    }
                }
            }
            for &dir in &dirs {
                let listed = listed(&fs, dir).await;
                for round in 0..ROUNDS {
                    for worker in 0..PER_DIR {
                        let name = format!("r{round}-{worker}.o");
                        assert!(listed.contains(&name), "{name} was created");
                    }
                }
            }
            assert!(
                fs.served().state.block_count() >= blocks + 2,
                "the creates filled a fresh block and took another"
            );
            // A full fresh block gives way to another on the worker, so no attach defers.
            assert_eq!(deferred_at_attach, 0, "a CREATE deferred at its attach");
        })
        .await;
}

/// A writable provider over the zero revision serves a State taking new nodes from fresh
/// blocks: a CREATE on the worker attaches at once, with the store held, in a block past the
/// State's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_on_a_zero_revision_mount_attaches_on_the_worker_in_a_fresh_block() {
    let instance = Instance::new(&[("a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let store = holding_store::HoldingStore::new(instance.fixture.immutable.clone());
            let repository = Arc::new(
                lore_revision::repository::RepositoryContext::new_server_context(
                    store.clone(),
                    instance.fixture.mutable.clone(),
                    instance.fixture.repository.id,
                ),
            );
            let fs = FuseFilesystem::new_writable(
                repository,
                lore_revision::state::State::new(),
                AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
                instance.external.path(),
            )
            .await
            .expect("writable provider");
            assert_eq!(lookup(&fs, ROOT_INO, "new.o").await, Err(Errno::ENOENT));
            let blocks = fs.served().state.block_count();

            store.hold();
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            let local = tokio::time::timeout(
                LOCAL_ANSWER,
                fs.create_local(
                    &request(),
                    ROOT_INO,
                    b"new.o",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                ),
            )
            .await;
            store.release();
            let created = local.expect("the CREATE waited on the store");
            assert!(
                !matches!(created, Err(ReadFault::WouldFetch)),
                "the CREATE deferred"
            );
            assert!(created.is_ok(), "the CREATE failed");
            let node = node_at(&instance, &fs, "new.o").await;
            assert!(
                lore_revision::node::NodeBlock::index(node) >= blocks,
                "the node took a slot in block {} of the State's {blocks}",
                lore_revision::node::NodeBlock::index(node)
            );
        })
        .await;
}

/// The answer of a LOOKUP on the worker: `Some(ino)`, 0 for a negative entry, or `None` where it
/// deferred.
async fn lookup_on_worker(fs: &FuseFilesystem, parent: u64, name: &str) -> Option<u64> {
    match fs.lookup_local(&request(), parent, name.as_bytes()).await {
        Ok(reply) => Some(reply.ino),
        Err(ReadFault::Errno(Errno::ENOENT)) => Some(0),
        Err(ReadFault::WouldFetch) => None,
        Err(ReadFault::Errno(errno)) => panic!("LOOKUP of {name}: {errno:?}"),
    }
}

/// The first LOOKUP of a missing name in a directory MKDIR just made answers on the worker:
/// the MKDIR cached the directory's empty listing, so no build defers it.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_lookup_in_a_directory_mkdir_made_answers_on_the_worker() {
    let instance = Instance::new(&[("a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let built = fs.listings_built();
            assert_eq!(
                lookup_on_worker(&fs, made, "missing.o").await,
                Some(0),
                "the LOOKUP deferred"
            );
            assert_eq!(fs.listings_built(), built, "the LOOKUP built a listing");
        })
        .await;
}

/// A CREATE on the worker in a directory MKDIR just made answers there, and the file is found
/// by a LOOKUP on the worker and by one that may fetch, and READDIR lists it.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_in_a_directory_mkdir_made_is_found_and_listed() {
    let instance = Instance::new(&[("a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            let created = match fs
                .create_local(
                    &request(),
                    made,
                    b"new.o",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
            {
                Ok(created) => created.entry.ino,
                Err(ReadFault::WouldFetch) => panic!("the CREATE deferred"),
                Err(ReadFault::Errno(errno)) => panic!("CREATE: {errno:?}"),
            };
            assert_eq!(
                lookup_on_worker(&fs, made, "new.o").await,
                Some(created),
                "the LOOKUP on the worker"
            );
            assert_eq!(lookup(&fs, made, "new.o").await, Ok(created));
            assert_eq!(lookup(&fs, made, "other.o").await, Err(Errno::ENOENT));
            assert_eq!(listed(&fs, made).await, vec!["new.o"]);
        })
        .await;
}

/// An MKDIR while a swap applies caches the empty listing of the directory it makes: whatever
/// the swap attaches there later is read from the directory's log.
#[tokio::test(flavor = "multi_thread")]
async fn an_mkdir_while_a_swap_applies_caches_its_listing() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let swapping = Arc::new(AtomicBool::new(false));
            let made = Arc::new(AtomicU64::new(0));
            fs.set_swap_hook(at_first(&fs, SwapPhase::Placed, {
                let (swapping, made) = (swapping.clone(), made.clone());
                move |fs| {
                    let (swapping, made) = (swapping.clone(), made.clone());
                    Box::pin(async move {
                        swapping.store(fs.is_swapping(), Ordering::SeqCst);
                        let ino = fs
                            .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                            .await
                            .expect("an mkdir during the swap")
                            .ino;
                        made.store(ino, Ordering::SeqCst);
                    })
                }
            }));

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            assert!(
                swapping.load(Ordering::SeqCst),
                "the MKDIR ran while the swap applied"
            );
            let made = made.load(Ordering::SeqCst);
            let made_node = node_at(&instance, &fs, "made").await;
            assert!(
                fs.served().listings.get(made_node).is_some(),
                "an MKDIR during a swap cached no listing"
            );
            let built = fs.listings_built();
            assert_eq!(lookup(&fs, made, "missing.o").await, Err(Errno::ENOENT));
            let created = create_with(&fs, made, "new.o", b"object").await;
            assert_eq!(lookup(&fs, made, "new.o").await, Ok(created));
            assert_eq!(fs.listings_built(), built, "the cached listing answers");
            assert_eq!(listed(&fs, made).await, vec!["new.o"]);
        })
        .await;
}

/// An MKDIR that a whole swap overtakes between building the directory's empty listing and
/// caching it caches the listing: what the swap changed there since is in the directory's log.
#[tokio::test(flavor = "multi_thread")]
async fn an_mkdir_a_swap_overtakes_before_its_insert_caches_its_listing() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(sync_instance().await);
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let overtaken = Arc::new(AtomicBool::new(false));
            let spawned: Spawned = Arc::default();
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (instance, overtaken, spawned) =
                    (instance.clone(), overtaken.clone(), spawned.clone());
                move |step| {
                    if !matches!(step, WriteStep::EmptyListingBuilt { .. })
                        || overtaken.swap(true, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let instance = instance.clone();
                    let syncing = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            sync_through(&instance, &fs, first, Default::default())
                                .await
                                .expect("the sync");
                        },
                    ));
                    // The sync may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !syncing.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    spawned.lock().expect("handles").push(syncing);
                    true
                }
            }));

            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            settled_requests(&spawned).await;

            assert!(
                overtaken.load(Ordering::SeqCst),
                "the MKDIR built no empty listing"
            );
            assert_eq!(
                current_revision(&instance).await,
                first,
                "the swap ran inside the MKDIR"
            );
            let made_node = node_at(&instance, &fs, "made").await;
            assert!(
                fs.served().listings.get(made_node).is_some(),
                "an MKDIR a swap overtook cached no listing"
            );
            let built = fs.listings_built();
            assert_eq!(lookup(&fs, made, "missing.o").await, Err(Errno::ENOENT));
            assert_eq!(fs.listings_built(), built, "the cached listing answers");
        })
        .await;
}

/// An instance at the second of two revisions: `d/keep.txt` in both, `d/new.txt` only in the
/// first, `d/gone.txt` only in the second. A sync to the first detaches `gone.txt` from `d` and
/// attaches `new.txt`, and changes no record `d` lists.
async fn attach_and_detach_instance() -> Instance {
    Instance {
        fixture: fuse_fixture::released_deleting(
            &[
                &[("d/keep.txt", b"k"), ("d/new.txt", b"n")],
                &[("d/gone.txt", b"g")],
            ],
            &["d/new.txt"],
        )
        .await,
        external: TempDir::new("lore-fuse-write-mount-instance-"),
    }
}

/// A listing built while a swap applies is cached: after the swap, lookups of a name the swap
/// placed after the build, one it removed before it and one it left are read through it and the
/// directory's log, with no build.
#[tokio::test(flavor = "multi_thread")]
async fn a_build_across_a_swap_is_cached() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::hold::SwapPhase;

    let instance = attach_and_detach_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let d_node = node_at(&instance, &fs, "d").await;
            let built_during = Arc::new(AtomicU64::new(u64::MAX));
            fs.set_swap_hook(at_first(&fs, SwapPhase::Removed, {
                let built_during = built_during.clone();
                move |fs| {
                    let built_during = built_during.clone();
                    Box::pin(async move {
                        fs.served().listings.remove(d_node);
                        let before = fs.listings_built();
                        assert_eq!(lookup(&fs, d, "gone.txt").await, Err(Errno::ENOENT));
                        built_during.store(fs.listings_built() - before, Ordering::SeqCst);
                    })
                }
            }));

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            assert_eq!(
                built_during.load(Ordering::SeqCst),
                1,
                "the lookup during the swap built d's listing"
            );
            let built = fs.listings_built();
            lookup(&fs, d, "new.txt")
                .await
                .expect("new.txt, placed after the build");
            assert_eq!(lookup(&fs, d, "gone.txt").await, Err(Errno::ENOENT));
            lookup(&fs, d, "keep.txt").await.expect("keep.txt");
            assert_eq!(
                fs.listings_built(),
                built,
                "the listing built during the swap answers"
            );
        })
        .await;
}

/// A CREATE and an UNLINK in a listed directory while a swap applies there patch the same log
/// as the swap's removal and placement: after the swap, the listing built before it answers
/// every name as the chain holds it, with no build.
#[tokio::test(flavor = "multi_thread")]
async fn swap_during_patches() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::hold::SwapPhase;

    let instance = attach_and_detach_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let d_node = node_at(&instance, &fs, "d").await;
            create_with(&fs, d, "doomed.o", b"d").await;
            fs.served().listings.remove(d_node);
            lookup(&fs, d, "doomed.o").await.expect("doomed.o");
            let before = fs.served().listings.get(d_node).expect("d's listing");
            let during = Arc::new(AtomicU64::new(0));
            fs.set_swap_hook(at_first(&fs, SwapPhase::Removed, {
                let during = during.clone();
                move |fs| {
                    let during = during.clone();
                    Box::pin(async move {
                        let created = create_with(&fs, d, "during.o", b"o").await;
                        fs.unlink(&request(), d, b"doomed.o")
                            .await
                            .expect("unlink during the swap");
                        assert_eq!(lookup(&fs, d, "during.o").await, Ok(created));
                        assert_eq!(lookup(&fs, d, "doomed.o").await, Err(Errno::ENOENT));
                        during.store(created, Ordering::SeqCst);
                    })
                }
            }));
            let built = fs.listings_built();

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            let during = during.load(Ordering::SeqCst);
            assert_ne!(during, 0, "the patches landed during the swap");
            let after = fs
                .served()
                .listings
                .get(d_node)
                .expect("the swap left d's listing cached");
            assert!(Arc::ptr_eq(&after, &before), "d's listing was replaced");
            assert_eq!(lookup(&fs, d, "during.o").await, Ok(during));
            assert_eq!(lookup(&fs, d, "doomed.o").await, Err(Errno::ENOENT));
            lookup(&fs, d, "new.txt").await.expect("new.txt");
            assert_eq!(lookup(&fs, d, "gone.txt").await, Err(Errno::ENOENT));
            lookup(&fs, d, "keep.txt").await.expect("keep.txt");
            assert_eq!(
                fs.listings_built(),
                built,
                "a lookup during or after the swap built a listing"
            );
        })
        .await;
}

/// A sync that attaches 200 files to a listed directory, with LOOKUPs of each while the swap
/// applies and again after it: the directory's listing is built at most once per 64 changes, as
/// each build is cached and every other lookup reads the log.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_touching_a_listed_directory_builds_at_most_once_per_64_changes() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::hold::SwapPhase;
    use lore_revision::node::CHAIN_LOG_GROWN;
    const FILES: usize = 200;

    let names: Arc<Vec<String>> = Arc::new((0..FILES).map(|i| format!("f{i:03}.o")).collect());
    let paths: Vec<String> = names.iter().map(|name| format!("dir/{name}")).collect();
    let mut first: Vec<(&str, &[u8])> = vec![("dir/keep.txt", b"k")];
    first.extend(paths.iter().map(|path| (path.as_str(), &b"f"[..])));
    let deleted: Vec<&str> = paths.iter().map(String::as_str).collect();
    let instance = Instance {
        fixture: fuse_fixture::released_deleting(&[&first, &[]], &deleted).await,
        external: TempDir::new("lore-fuse-write-mount-instance-"),
    };
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "keep.txt").await.expect("keep.txt");
            let dir_node = node_at(&instance, &fs, "dir").await;
            let builds = Arc::new(AtomicU64::new(0));
            fs.set_listing_hook(Arc::new({
                let builds = builds.clone();
                move |node| {
                    if node == dir_node {
                        builds.fetch_add(1, Ordering::SeqCst);
                    }
                    Box::pin(async {})
                }
            }));
            let during = Arc::new(AtomicU64::new(u64::MAX));
            fs.set_swap_hook(at_first(&fs, SwapPhase::Placed, {
                let (names, builds, during) = (names.clone(), builds.clone(), during.clone());
                move |fs| {
                    let (names, builds, during) = (names.clone(), builds.clone(), during.clone());
                    Box::pin(async move {
                        for name in names.iter() {
                            lookup(&fs, dir, name)
                                .await
                                .unwrap_or_else(|errno| panic!("{name}: {errno:?}"));
                        }
                        during.store(builds.load(Ordering::SeqCst), Ordering::SeqCst);
                    })
                }
            }));

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");
            for name in names.iter() {
                lookup(&fs, dir, name)
                    .await
                    .unwrap_or_else(|errno| panic!("{name}: {errno:?}"));
            }

            let (during, builds) = (during.load(Ordering::SeqCst), builds.load(Ordering::SeqCst));
            println!("{FILES} attaches: {during} builds while the swap applied, {builds} in all");
            assert_ne!(during, u64::MAX, "the lookups ran during the swap");
            assert!(
                builds <= FILES.div_ceil(CHAIN_LOG_GROWN) as u64,
                "{builds} builds for {FILES} changes"
            );
        })
        .await;
}

/// The bytes the chain logs of `logs` directories hold from their first listing: a slot each in
/// its block's slot array, the log's header and its ring.
fn initial_log_bytes(logs: usize) -> usize {
    use lore_revision::node::CHAIN_LOG_INITIAL;
    // A slot is 16 bytes, a `ChainLog` header 32, a patch 24.
    logs * (16 + 32 + CHAIN_LOG_INITIAL * 24)
}

/// The chain-log bytes a provider's served State holds are what its listed directories' logs
/// hold.
#[tokio::test(flavor = "multi_thread")]
async fn chain_log_bytes_total_the_logs_of_listed_directories() {
    let instance = Instance::new(&[("dir/a.txt", b"a"), ("other/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup_path(&fs, "dir/a.txt").await.expect("dir/a.txt");
            assert_eq!(
                fs.chain_log_bytes(),
                initial_log_bytes(2),
                "the root's and dir's logs"
            );
            lookup_path(&fs, "other/b.txt").await.expect("other/b.txt");
            assert_eq!(fs.chain_log_bytes(), initial_log_bytes(3));
        })
        .await;
}

/// Unmounting a provider whose directories hold chain logs frees their blocks, the blocks the
/// logs' patches name included: a patch holds its node's block weakly.
#[tokio::test(flavor = "multi_thread")]
// The writes go through the mount, as a user's do.
#[allow(clippy::disallowed_methods)]
async fn an_unmounted_provider_frees_the_blocks_its_chain_logs_name() {
    skip_without_fuse!();
    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    let mounted = instance.mount().await;
    blocking({
        let root = mounted.root.clone();
        move || {
            std::fs::symlink_metadata(root.join("dir/a.txt")).expect("dir/a.txt");
            std::fs::write(root.join("dir/new.txt"), b"n").expect("create");
            std::fs::rename(root.join("dir/new.txt"), root.join("dir/moved.txt")).expect("rename");
        }
    })
    .await;
    let blocks = LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = &mounted.fs;
            assert!(
                fs.chain_log_bytes() >= initial_log_bytes(2),
                "the listings installed chain logs"
            );
            let mut blocks = Vec::new();
            for path in ["dir", "dir/moved.txt"] {
                let node = node_at(&instance, fs, path).await;
                let block = fs
                    .served()
                    .state
                    .resident_block(lore_revision::node::NodeBlock::index(node))
                    .expect("a resident block");
                blocks.push(Arc::downgrade(&block));
            }
            blocks
        })
        .await;
    mounted.unmount().await;
    for (path, block) in ["dir", "dir/moved.txt"].iter().zip(&blocks) {
        assert!(
            block.upgrade().is_none(),
            "the block of {path} outlived the provider"
        );
    }
}

/// A provider over `instance` reading through a store the test can hold, not mounted; the
/// instance's tree records file metadata.
async fn holding_provider(
    instance: &Instance,
) -> (Arc<FuseFilesystem>, Arc<holding_store::HoldingStore>) {
    let tree = instance
        .fixture
        .state
        .tree(instance.fixture.repository.clone())
        .await
        .expect("the tree");
    assert!(
        !tree.hash_file_metadata.is_zero(),
        "the tree records file metadata"
    );
    let store = holding_store::HoldingStore::new(instance.fixture.immutable.clone());
    let repository = Arc::new(
        lore_revision::repository::RepositoryContext::new_server_context(
            store.clone(),
            instance.fixture.mutable.clone(),
            instance.fixture.repository.id,
        ),
    );
    let fs = FuseFilesystem::new_writable(
        repository,
        instance.fixture.state.clone(),
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
        instance.external.path(),
    )
    .await
    .expect("writable provider");
    (fs, store)
}

/// Two commits, so the tree records file metadata.
const WITH_FILE_METADATA: [&[(&str, &[u8])]; 2] = [
    &[("dir/a.txt", b"a")],
    &[("dir/a.txt", b"a2"), ("dir/b.txt", b"b")],
];

/// How long a request reading nothing from the store is given.
const LOCAL_ANSWER: Duration = Duration::from_secs(5);

/// The first CREATE and the first MKDIR after a mount answer on the worker, reading nothing from
/// the store, whether the revision's free slots are in the block a listing holds or in blocks no
/// listing loaded: new nodes take fresh blocks.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_create_and_mkdir_after_a_mount_answer_on_the_worker() {
    for holed in [false, true] {
        for mkdir in [false, true] {
            let (instance, dir, child) = if holed {
                (Instance::with_holed_blocks().await, "a", "x0")
            } else {
                (
                    Instance::with_revisions(&WITH_FILE_METADATA).await,
                    "dir",
                    "a.txt",
                )
            };
            LORE_CONTEXT
                .scope(fuse_fixture::setup_test_execution(), async {
                    let (fs, store) = holding_provider(&instance).await;
                    let dir = lookup(&fs, ROOT_INO, dir).await.expect("dir");
                    lookup(&fs, dir, child).await.expect("a tracked child");

                    store.hold();
                    let local = tokio::time::timeout(LOCAL_ANSWER, async {
                        if mkdir {
                            fs.mkdir_local(&request(), dir, b"new", libc::S_IFDIR | 0o755, 0o022)
                                .await
                                .map(|reply| reply.ino)
                        } else {
                            fs.create_local(
                                &request(),
                                dir,
                                b"new",
                                libc::S_IFREG | 0o644,
                                0o022,
                                (libc::O_CREAT | libc::O_RDWR) as u32,
                            )
                            .await
                            .map(|reply| reply.entry.ino)
                        }
                    })
                    .await;
                    store.release();
                    let what = format!("holed {holed}, mkdir {mkdir}");
                    let ino = match local.expect("the worker reads nothing from the store") {
                        Ok(ino) => ino,
                        Err(ReadFault::WouldFetch) => panic!("{what}: deferred"),
                        Err(ReadFault::Errno(errno)) => panic!("{what}: {errno:?}"),
                    };
                    assert_eq!(lookup(&fs, dir, "new").await, Ok(ino), "{what}");
                })
                .await;
        }
    }
}

/// After a CREATE's fetch, which loads no slot, CREATEs read nothing from the store and do not
/// defer: the first takes a fresh block and the next a slot in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_warm_create_takes_its_slot_without_deferring() {
    let instance = Instance::with_revisions(&WITH_FILE_METADATA).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let (fs, store) = holding_provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let what = Fetch::Entry {
                parent: dir,
                name: b"new.txt",
                request: EntryRequest::Create,
            };
            fs.fetch(&request(), what).await.expect("the fetch");
            let mode = libc::S_IFREG | 0o644;
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;

            store.hold();
            let mut answers = Vec::new();
            for name in [&b"new.txt"[..], b"other.txt"] {
                answers.push(
                    tokio::time::timeout(
                        LOCAL_ANSWER,
                        fs.create_local(&request(), dir, name, mode, 0o022, flags),
                    )
                    .await,
                );
            }
            store.release();
            for answer in answers {
                let local = answer.expect("the worker's CREATE reads nothing from the store");
                assert!(local.is_ok(), "a warm CREATE does not defer");
            }
        })
        .await;
}

/// The names in `instance`'s `write-staging/`, where copy-ups stage their content.
fn staged_names(instance: &Instance) -> Vec<String> {
    names(&instance.external.path().join("write-staging"))
}

/// Polls until `instance`'s `write-staging/` is empty, for up to two seconds, and answers what it
/// holds then.
async fn staging_after_settling(instance: &Instance) -> Vec<String> {
    for _ in 0..200 {
        if staged_names(instance).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    staged_names(instance)
}

/// A copy-up fetch that INTERRUPT abandons while a host write into its staged file is still
/// landing, as a syscall pool job keeps running once its future drops, leaves nothing in
/// `write-staging/` once that write is done.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_copy_up_fetch_leaves_nothing_in_staging() {
    let instance = Instance::new(&[("dir/tracked.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "tracked.txt").await.expect("tracked.txt");
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let (hook_entered, hook_release) = (entered.clone(), release.clone());
            fs.set_staging_hook(Arc::new(move |path| {
                let (entered, release) = (hook_entered.clone(), hook_release.clone());
                Box::pin(async move {
                    let landing = lore_base::lore_spawn!(async move {
                        release.notified().await;
                        std::fs::write(&path, b"late").expect("the late host write");
                    });
                    entered.notify_one();
                    let _ = landing.await;
                })
            }));
            let local = fs.write_local(&request(), ino, 0, 0, b"edit", 0).await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "a copy-up defers"
            );

            let req = request();
            let fetch = fs.fetch(&req, Fetch::Content { ino, len: None });
            tokio::select! {
                _ = fetch => panic!("the fetch waits on the host write"),
                () = entered.notified() => {}
            }
            release.notify_one();
            assert_eq!(
                staging_after_settling(&instance).await,
                Vec::<String>::new(),
                "an interrupted fetch leaves nothing staged"
            );
            assert_eq!(fs.staged_copies(), 0);
        })
        .await;
}

/// A deferred RENAME that fails leaves no staged copy of its source: none is staged where the
/// target exists under `RENAME_NOREPLACE`, and one staged before the target appeared goes when
/// the rename returns. A later WRITE then skips the staged-copy map.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_rename_that_fails_leaves_no_staged_source() {
    let instance = Instance::new(&[("dir/a.txt", b"a content"), ("dir/b.txt", b"b content")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "a.txt").await.expect("a.txt");
            lookup(&fs, dir, "b.txt").await.expect("b.txt");
            let noreplace = libc::RENAME_NOREPLACE;
            let deferred_rename = |newname: &'static [u8]| {
                let fs = fs.clone();
                async move {
                    fs.served().listings.clear();
                    let local = fs
                        .rename_local(&request(), dir, b"a.txt", dir, newname, noreplace)
                        .await;
                    assert!(
                        matches!(local, Err(ReadFault::WouldFetch)),
                        "a cold RENAME defers"
                    );
                    let what = Fetch::Rename {
                        parent: dir,
                        name: b"a.txt",
                        newparent: dir,
                        newname,
                        flags: noreplace,
                    };
                    fs.fetch(&request(), what).await.expect("the fetch");
                }
            };

            deferred_rename(b"b.txt").await;
            let renamed = fs
                .rename(&request(), dir, b"a.txt", dir, b"b.txt", noreplace)
                .await;
            assert_eq!(renamed, Err(Errno::EEXIST));
            assert_eq!(
                staged_names(&instance),
                Vec::<String>::new(),
                "nothing staged"
            );
            assert_eq!(fs.staged_copies(), 0);

            deferred_rename(b"c.txt").await;
            let created = create_with(&fs, dir, "c.txt", b"c").await;
            let renamed = fs
                .rename(&request(), dir, b"a.txt", dir, b"c.txt", noreplace)
                .await;
            assert_eq!(renamed, Err(Errno::EEXIST));
            assert_eq!(
                staged_names(&instance),
                Vec::<String>::new(),
                "the staged copy went"
            );
            assert_eq!(fs.staged_copies(), 0);

            fs.write(&request(), created, 0, 1, b"more", 0)
                .await
                .expect("a warm write");
            assert_eq!(fs.staged_copies(), 0);
            assert_eq!(read(&fs, created, 64).await, b"cmore");
        })
        .await;
}

/// A deferred first WRITE of a tracked file whose directory's listing was dropped: its fetch
/// resolves the file as the completion does, so the completion builds no listing.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_write_completion_builds_no_listing() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("dir/tracked.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "tracked.txt").await.expect("tracked.txt");
            let builds = Arc::new(AtomicU64::new(0));
            let counted = builds.clone();
            fs.set_listing_hook(Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            }));
            fs.served().listings.clear();

            let local = fs.write_local(&request(), ino, 0, 0, b"edit", 0).await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "a copy-up defers"
            );
            fs.fetch(&request(), Fetch::Content { ino, len: None })
                .await
                .expect("the fetch");
            let before = builds.load(Ordering::SeqCst);
            assert!(before > 0, "the fetch built the dropped listing");
            fs.write(&request(), ino, 0, 0, b"edit", 0)
                .await
                .expect("the completion writes");
            assert_eq!(
                builds.load(Ordering::SeqCst),
                before,
                "the completion builds no listing"
            );
            assert_eq!(read(&fs, ino, 64).await, b"editked");
        })
        .await;
}

/// An RMDIR of a tracked directory nothing has listed, whose children's blocks are not in
/// memory, defers: its emptiness check reads no block on the worker. Its fetch loads the chain,
/// and the completion answers `ENOTEMPTY`.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_cold_nonempty_directory_defers_and_answers_enotempty() {
    let paths: Vec<String> = (0..1100).map(|i| format!("cold/c{i:04}.txt")).collect();
    let mut files: Vec<(&str, &[u8])> = paths
        .iter()
        .map(|path| (path.as_str(), &b"c"[..]))
        .collect();
    files.push(("top.txt", b"top"));
    let instance = Instance::new(&files).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup(&fs, ROOT_INO, "cold").await.expect("cold");
            let cold = node_at(&instance, &fs, "cold").await;
            let head = fs
                .state()
                .node(instance.fixture.repository.clone(), cold)
                .await
                .expect("cold's node")
                .child()
                .expect("cold has children");
            assert!(
                fs.state()
                    .resident_block(lore_revision::node::NodeBlock::index(head))
                    .is_none(),
                "cold's first child sits in a block not in memory"
            );

            let local = fs.rmdir_local(&request(), ROOT_INO, b"cold").await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "the RMDIR defers"
            );
            let what = Fetch::Entry {
                parent: ROOT_INO,
                name: b"cold",
                request: EntryRequest::Rmdir,
            };
            fs.fetch(&request(), what).await.expect("the fetch");
            assert_eq!(
                fs.rmdir(&request(), ROOT_INO, b"cold").await,
                Err(Errno::ENOTEMPTY)
            );
            lookup(&fs, ROOT_INO, "cold").await.expect("cold stays");
        })
        .await;
}

/// An RMDIR whose emptiness check a reattach passes, moving the directory's only live child to the
/// head the walk has passed, never removes the directory: on the worker the check defers, and
/// with fetching the RMDIR checks again and finds the child.
#[tokio::test(flavor = "multi_thread")]
async fn an_emptiness_check_a_reattach_passes_defers_or_walks_again() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("top.txt", b"top")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            // Each directory holds `y.txt` behind `h.txt` in its chain, `h.txt` staged for
            // deletion, so a walk reads `h.txt` first and passes over it.
            let mut shapes = Vec::new();
            for dir in ["d1", "d2"] {
                let ino = fs
                    .mkdir(&request(), ROOT_INO, dir.as_bytes(), 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino;
                create_with(&fs, ino, "y.txt", b"y").await;
                create_with(&fs, ino, "h.txt", b"h").await;
                let (dir_node, y, h) = (
                    node_at(&instance, &fs, dir).await,
                    node_at(&instance, &fs, &format!("{dir}/y.txt")).await,
                    node_at(&instance, &fs, &format!("{dir}/h.txt")).await,
                );
                assert_eq!(
                    fs.rmdir_local(&request(), ROOT_INO, dir.as_bytes())
                        .await
                        .map_err(Errno::from),
                    Err(Errno::ENOTEMPTY),
                    "{dir} is checked on the worker"
                );
                fs.state()
                    .node_delete(repository.clone(), h)
                    .await
                    .expect("stage h.txt for deletion");
                shapes.push((dir_node, y, h, Arc::new(AtomicBool::new(false))));
            }
            // Every child an emptiness walk reached, each walk's head first.
            let walked: Arc<std::sync::Mutex<Vec<NodeID>>> = Arc::default();
            let (state, hooked, reached) = (fs.state(), shapes.clone(), walked.clone());
            fs.set_emptiness_hook(Arc::new(move |child| {
                reached.lock().expect("walked").push(child);
                let (state, repository) = (state.clone(), repository.clone());
                let moved = hooked
                    .iter()
                    .find(|(_, _, h, done)| *h == child && !done.swap(true, Ordering::SeqCst))
                    .map(|(dir, y, _, _)| (*dir, *y));
                Box::pin(async move {
                    if let Some((dir, y)) = moved {
                        state
                            .node_reattach(repository, y, dir, "y2.txt")
                            .await
                            .expect("rename y.txt behind the walk");
                    }
                })
            }));

            let local = fs.rmdir_local(&request(), ROOT_INO, b"d1").await;
            assert!(
                shapes[0].3.load(Ordering::SeqCst),
                "the reattach landed behind the d1 walk"
            );
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "a walk a reattach passed defers"
            );
            let fetched = fs.rmdir(&request(), ROOT_INO, b"d2").await;
            assert!(
                shapes[1].3.load(Ordering::SeqCst),
                "the reattach landed behind the d2 walk"
            );
            assert!(
                walked.lock().expect("walked").contains(&shapes[1].1),
                "the d2 walk walked again, from y.txt at the head of its chain"
            );
            assert_eq!(
                fetched,
                Err(Errno::ENOTEMPTY),
                "a fetching walk a reattach passed walks again"
            );
        })
        .await;
}

/// An RMDIR whose emptiness check a reattach passes on every walk gives up after its checks with
/// `EAGAIN`, rather than walking for ever or answering empty; so does a RENAME over the directory,
/// whose emptiness check walks again on its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_emptiness_check_passed_on_every_walk_answers_eagain() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("top.txt", b"top")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"d", 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, dir, "h.txt", b"h").await;
            let (dir_node, h) = (
                node_at(&instance, &fs, "d").await,
                node_at(&instance, &fs, "d/h.txt").await,
            );
            fs.state()
                .node_delete(repository.clone(), h)
                .await
                .expect("stage h.txt for deletion");
            // Every walk meets h.txt at its head and renames it, so no walk ends unpassed.
            let renames = Arc::new(AtomicU64::new(0));
            let (state, counted) = (fs.state(), renames.clone());
            fs.set_emptiness_hook(Arc::new(move |child| {
                let (state, repository) = (state.clone(), repository.clone());
                let n = (child == h).then(|| counted.fetch_add(1, Ordering::SeqCst));
                Box::pin(async move {
                    if let Some(n) = n {
                        state
                            .node_reattach(repository, h, dir_node, &format!("h{n}.txt"))
                            .await
                            .expect("rename h.txt behind the walk");
                    }
                })
            }));

            let answered = fs.rmdir(&request(), ROOT_INO, b"d").await;
            assert!(
                renames.load(Ordering::SeqCst) > 64,
                "every restart was passed: {} renames",
                renames.load(Ordering::SeqCst)
            );
            assert_eq!(answered, Err(Errno::EAGAIN));
            lookup(&fs, ROOT_INO, "d").await.expect("d stays");

            fs.mkdir(&request(), ROOT_INO, b"s", 0o755, 0o022)
                .await
                .expect("mkdir s");
            let before = renames.load(Ordering::SeqCst);
            let answered = fs
                .rename(&request(), ROOT_INO, b"s", ROOT_INO, b"d", 0)
                .await;
            let walks = renames.load(Ordering::SeqCst) - before;
            assert!(walks > 64, "every walk again was passed: {walks} renames");
            assert_eq!(answered, Err(Errno::EAGAIN));
            lookup(&fs, ROOT_INO, "s").await.expect("s stays");
            lookup(&fs, ROOT_INO, "d").await.expect("d stays");
        })
        .await;
}

/// A deferred SETATTR truncating a tracked file to a smaller size stages only the bytes it
/// keeps, and its completion serves them.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_truncate_stages_only_what_it_keeps() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;

    let content = pattern(64 * 1024, 3);
    let instance = Instance::new(&[("dir/big.bin", &content)]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "big.bin").await.expect("big.bin");
            let staged = Arc::new(AtomicU64::new(0));
            let recorded = staged.clone();
            fs.set_staging_hook(Arc::new(move |path| {
                let len = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
                recorded.store(len, Ordering::SeqCst);
                Box::pin(async {})
            }));
            let attr = SetattrIn {
                valid: setattr_valid::SIZE,
                size: 1000,
                ..SetattrIn::default()
            };

            let local = fs.setattr_local(&request(), ino, &attr).await;
            assert!(
                matches!(local, Err(ReadFault::WouldFetch)),
                "the truncate defers"
            );
            let what = Fetch::Content {
                ino,
                len: Some(1000),
            };
            fs.fetch(&request(), what).await.expect("the fetch");
            assert_eq!(
                staged.load(Ordering::SeqCst),
                1000,
                "the fetch stages only the kept bytes"
            );
            let reply = fs
                .setattr(&request(), ino, &attr)
                .await
                .expect("the completion truncates");
            assert_eq!(reply.attr.size, 1000);
            assert_eq!(read(&fs, ino, 4096).await, &content[..1000]);
            assert_eq!(staged_names(&instance), Vec::<String>::new());
            assert_eq!(fs.staged_copies(), 0);
        })
        .await;
}

/// A SETATTR of `size` on `ino`.
fn truncate_to(size: u64) -> lore_fuse::abi::SetattrIn {
    lore_fuse::abi::SetattrIn {
        valid: lore_fuse::abi::entry::setattr_valid::SIZE,
        size,
        ..lore_fuse::abi::SetattrIn::default()
    }
}

/// A truncate to zero keeps none of a tracked file's content, so its copy-up reads nothing and
/// runs on the worker.
#[tokio::test(flavor = "multi_thread")]
async fn a_truncate_to_zero_of_a_tracked_file_copies_up_on_the_worker() {
    let instance = Instance::new(&[("dir/big.bin", &pattern(64 * 1024, 3))]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "big.bin").await.expect("big.bin");
            let answered = fs.setattr_local(&request(), ino, &truncate_to(0)).await;
            let reply = match answered {
                Ok(reply) => reply,
                Err(ReadFault::WouldFetch) => panic!("the truncate to zero deferred"),
                Err(ReadFault::Errno(errno)) => panic!("the truncate failed: {errno:?}"),
            };
            assert_eq!(reply.attr.size, 0);
            assert_eq!(read(&fs, ino, 4096).await, b"");
            assert_eq!(
                std::fs::metadata(instance.write_dir().join("dir/big.bin"))
                    .expect("the copied-up host file")
                    .len(),
                0
            );
        })
        .await;
}

/// A copy-up on the worker of a file whose copy-up gate another copy-up holds answers
/// `WouldFetch` at once rather than waiting on the gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_copy_up_meeting_a_held_gate_defers_rather_than_waiting() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("dir/a.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let (reached, held) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let released = std::sync::Mutex::new(released);
            fs.set_write_hook(Arc::new(move |step| {
                if step == WriteStep::CopyUp {
                    let _ = reached.send(());
                    // The copy-up holds its gate here until the test lets it go.
                    tokio::task::block_in_place(|| {
                        let _ = released.lock().expect("released").recv();
                    });
                }
                true
            }));
            let writer = {
                let fs = fs.clone();
                lore_base::lore_spawn!(
                    async move { fs.write(&request(), ino, 0, 0, b"first", 0).await }
                )
            };
            blocking(move || held.recv_timeout(Duration::from_secs(10)))
                .await
                .expect("the fetching copy-up reached its State step");

            let answered = tokio::time::timeout(
                Duration::from_secs(5),
                fs.setattr_local(&request(), ino, &truncate_to(0)),
            )
            .await;
            release.send(()).expect("release the copy-up");
            let answered = answered.expect("the worker copy-up answers without waiting");
            assert!(
                matches!(answered, Err(ReadFault::WouldFetch)),
                "the worker copy-up defers"
            );
            writer
                .await
                .expect("the writer task")
                .expect("the fetching copy-up writes");
            assert_eq!(read(&fs, ino, 64).await, b"firsted");
        })
        .await;
}

/// Content staged for a copy-up goes when the kernel forgets the inode before the completion.
#[tokio::test(flavor = "multi_thread")]
async fn content_staged_for_a_forgotten_inode_is_removed() {
    let instance = Instance::new(&[("dir/a.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            fs.fetch(&request(), Fetch::Content { ino, len: None })
                .await
                .expect("the fetch");
            assert_eq!(fs.staged_copies(), 1, "the fetch staged the content");
            assert_eq!(staged_names(&instance).len(), 1);

            fs.forget(ino, 1);
            assert!(fs.inodes().get(ino).is_none(), "the inode is forgotten");
            assert_eq!(fs.staged_copies(), 0);
            assert_eq!(
                staging_after_settling(&instance).await,
                Vec::<String>::new()
            );
        })
        .await;
}

/// A staged copy shorter than what a copy-up keeps is not used: the copy-up fetches the content
/// again, so a WRITE after a deferred truncate's fetch serves the whole file.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_copy_shorter_than_the_copy_up_keeps_is_fetched_again() {
    let content = pattern(64 * 1024, 5);
    let instance = Instance::new(&[("dir/big.bin", &content)]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "big.bin").await.expect("big.bin");
            let what = Fetch::Content {
                ino,
                len: Some(1000),
            };
            fs.fetch(&request(), what)
                .await
                .expect("the truncate's fetch");
            assert_eq!(fs.staged_copies(), 1, "1000 bytes are staged");

            fs.write(&request(), ino, 0, 0, b"edit", 0)
                .await
                .expect("a write copies up the whole file");
            let mut expected = content.clone();
            expected[..4].copy_from_slice(b"edit");
            assert_eq!(read(&fs, ino, content.len() + 1).await, expected);
            assert_eq!(fs.staged_copies(), 0);
        })
        .await;
}

/// A staged copy of content a sync replaced since the fetch is not used: the copy-up fetches
/// the file's content as it stands, so a WRITE edits the synced file.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_copy_of_content_a_sync_replaced_is_fetched_again() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.fetch(&request(), Fetch::Content { ino: f, len: None })
                .await
                .expect("the fetch stages two");
            assert_eq!(fs.staged_copies(), 1);
            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync to the first revision");
            assert_eq!(fs.staged_copies(), 1, "the staged copy outlives the sync");
            assert_eq!(read(&fs, f, 64).await, b"one", "the sync changed f.txt");

            fs.write(&request(), f, 0, 0, b"X", 0)
                .await
                .expect("the write copies up");
            assert_eq!(read(&fs, f, 64).await, b"Xne");
            assert_eq!(fs.staged_copies(), 0);
        })
        .await;
}

/// A staged copy longer than a truncate keeps is cut to the truncate's size by the copy-up,
/// before the truncate's own host write.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_copy_longer_than_a_truncate_keeps_is_cut_to_its_size() {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let content = pattern(64 * 1024, 7);
    let instance = Instance::new(&[("dir/big.bin", &content)]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "big.bin").await.expect("big.bin");
            fs.fetch(&request(), Fetch::Content { ino, len: None })
                .await
                .expect("a write's fetch stages the whole file");
            assert_eq!(fs.staged_copies(), 1);
            let copied_up = Arc::new(AtomicU64::new(u64::MAX));
            fs.set_write_hook(Arc::new({
                let (copied_up, host) =
                    (copied_up.clone(), instance.write_dir().join("dir/big.bin"));
                move |step| {
                    if step == WriteStep::BeforeHostWrite {
                        let len = std::fs::metadata(&host).map_or(0, |metadata| metadata.len());
                        copied_up.store(len, Ordering::SeqCst);
                    }
                    true
                }
            }));

            let reply = fs
                .setattr(&request(), ino, &truncate_to(1000))
                .await
                .expect("the truncate copies up from the staged copy");
            assert_eq!(
                copied_up.load(Ordering::SeqCst),
                1000,
                "the copied-up host file before the truncate's own write"
            );
            assert_eq!(reply.attr.size, 1000);
            assert_eq!(read(&fs, ino, content.len()).await, &content[..1000]);
            assert_eq!(fs.staged_copies(), 0);
        })
        .await;
}

/// An RMDIR of a directory whose only child the view hides answers on the worker and with
/// fetching: the hidden child does not make it non-empty.
#[tokio::test(flavor = "multi_thread")]
async fn an_rmdir_of_a_directory_holding_only_hidden_children_succeeds() {
    let instance = Instance::new(&[
        ("keep.txt", b"k"),
        ("d1/secret.txt", b"s"),
        ("d2/secret.txt", b"s"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = viewed_provider(&instance, &["secret.txt"]).await;
            for dir in ["d1", "d2"] {
                let ino = lookup(&fs, ROOT_INO, dir).await.expect("the directory");
                assert_eq!(
                    lookup(&fs, ino, "secret.txt").await,
                    Err(Errno::ENOENT),
                    "the view hides {dir}/secret.txt"
                );
            }
            let worker = fs.rmdir_local(&request(), ROOT_INO, b"d1").await;
            assert!(
                matches!(worker, Ok(())),
                "the worker RMDIR answers and succeeds"
            );
            assert_eq!(fs.rmdir(&request(), ROOT_INO, b"d2").await, Ok(()));
            for dir in ["d1", "d2"] {
                assert_eq!(
                    lookup(&fs, ROOT_INO, dir).await,
                    Err(Errno::ENOENT),
                    "{dir}"
                );
            }
        })
        .await;
}

/// `rm -rf` of a tree whose listings are warm defers no request: each UNLINK and RMDIR is
/// answered on its worker, an RMDIR finding its emptied directory in the State.
#[tokio::test(flavor = "multi_thread")]
async fn rm_rf_of_a_warm_tree_defers_no_request() {
    skip_without_fuse!();
    let paths: Vec<String> = (0..20)
        .flat_map(|dir| (0..20).map(move |file| format!("tree/d{dir:02}/f{file:02}.txt")))
        .collect();
    let files: Vec<(&str, &[u8])> = paths
        .iter()
        .map(|path| (path.as_str(), &b"x"[..]))
        .collect();
    let instance = Instance::new(&files).await;
    let m = instance.mount().await;
    let tree = m.root.join("tree");
    let walked = tree.clone();
    let listed = blocking(move || {
        let mut files = 0;
        for dir in std::fs::read_dir(&walked).expect("tree") {
            for file in std::fs::read_dir(dir.expect("dir").path()).expect("dir") {
                std::fs::metadata(file.expect("file").path()).expect("stat");
                files += 1;
            }
        }
        files
    })
    .await;
    assert_eq!(listed, 400);
    let deferred = m.session.deferred_requests();

    let removed = tree.clone();
    blocking(move || std::fs::remove_dir_all(&removed).expect("rm -rf")).await;
    assert_eq!(
        m.session.deferred_requests() - deferred,
        0,
        "rm -rf of a warm tree defers no request"
    );
    assert!(!tree.exists());
    m.unmount().await;
}

/// UNLINK of a file the revision holds detaches its node before the reply and appends its path
/// to the deletion list: LOOKUP misses it, READDIR no longer lists it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlinked_tracked_file_is_absent_and_in_the_deletion_list() {
    let instance = Instance::new(&[("dir/gone.txt", b"g"), ("dir/kept.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "gone.txt").await.expect("gone.txt");
            assert_eq!(listed(&fs, dir).await, vec!["gone.txt", "kept.txt"]);
            fs.unlink(&request(), dir, b"gone.txt")
                .await
                .expect("unlink");
            assert_eq!(lookup(&fs, dir, "gone.txt").await, Err(Errno::ENOENT));
            assert_eq!(listed(&fs, dir).await, vec!["kept.txt"]);
            assert_eq!(deleted(&instance), vec!["dir/gone.txt"]);
            let dir_node = node_at(&instance, &fs, "dir").await;
            assert!(
                fs.state()
                    .find_subnode(
                        instance.fixture.repository.clone(),
                        dir_node,
                        hash_string("gone.txt")
                    )
                    .await
                    .is_err_and(|error| error.is_node_not_found())
            );
        })
        .await;
}

/// UNLINK of a directory is `EISDIR` and RMDIR of a file `ENOTDIR`, whether the revision holds
/// it or it was made under the mount, and neither removes anything.
#[tokio::test(flavor = "multi_thread")]
async fn unlink_of_a_directory_is_eisdir_and_rmdir_of_a_file_enotdir() {
    let instance = Instance::new(&[("a.txt", b"a"), ("dir/b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            create_with(&fs, ROOT_INO, "new.o", b"object").await;
            for dir in ["dir", "made"] {
                assert_eq!(
                    fs.unlink(&request(), ROOT_INO, dir.as_bytes()).await,
                    Err(Errno::EISDIR),
                    "UNLINK of {dir}"
                );
            }
            for file in ["a.txt", "new.o"] {
                assert_eq!(
                    fs.rmdir(&request(), ROOT_INO, file.as_bytes()).await,
                    Err(Errno::ENOTDIR),
                    "RMDIR of {file}"
                );
            }
            assert_eq!(
                listed(&fs, ROOT_INO).await,
                vec!["a.txt", "dir", "made", "new.o"]
            );
            assert_eq!(names(&instance.write_dir()), vec!["made", "new.o"]);
            assert!(deleted(&instance).is_empty());
        })
        .await;
}

/// UNLINK of a file made under the mount removes its host file, and of a file the revision holds
/// its node; the inode the kernel still holds for each, as for an open handle, keeps reading the
/// file's bytes with no link left.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlinked_file_keeps_reading_through_the_inode_the_kernel_holds() {
    let instance = Instance::new(&[("keep.txt", b"k"), ("tracked.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = create_with(&fs, ROOT_INO, "temp.o", b"scratch").await;
            let tracked = lookup(&fs, ROOT_INO, "tracked.txt")
                .await
                .expect("tracked.txt");
            for name in ["temp.o", "tracked.txt"] {
                fs.unlink(&request(), ROOT_INO, name.as_bytes())
                    .await
                    .expect("unlink");
                assert_eq!(lookup(&fs, ROOT_INO, name).await, Err(Errno::ENOENT));
            }
            assert!(!instance.write_dir().join("temp.o").exists());
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt"]);
            for (ino, bytes) in [(made, &b"scratch"[..]), (tracked, b"tracked")] {
                assert_eq!(read(&fs, ino, 64).await, bytes);
                let attr = fs.getattr(&request(), ino).await.expect("getattr").attr;
                assert_eq!(attr.nlink, 0, "{}", String::from_utf8_lossy(bytes));
            }
        })
        .await;
}

/// A save through a temporary over a file the revision holds: LOOKUP finds the saved inode and
/// READ serves the saved bytes, from `write/`; the replaced inode keeps reading the tracked
/// bytes with no link left; the temporary is gone and nothing is recorded as deleted, since the
/// path is still held.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_over_a_tracked_file_serves_the_new_content() {
    let instance = Instance::new(&[("out.bin", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let tracked = lookup(&fs, ROOT_INO, "out.bin").await.expect("out.bin");
            assert_eq!(read(&fs, tracked, 64).await, b"tracked");
            let saved = create_with(&fs, ROOT_INO, "out.bin.tmp", b"saved").await;
            fs.rename(
                &request(),
                ROOT_INO,
                b"out.bin.tmp",
                ROOT_INO,
                b"out.bin",
                0,
            )
            .await
            .expect("rename over the tracked file");
            assert_eq!(lookup(&fs, ROOT_INO, "out.bin").await, Ok(saved));
            assert_eq!(read(&fs, saved, 64).await, b"saved");
            let replaced = fs.getattr(&request(), tracked).await.expect("getattr").attr;
            assert_eq!(replaced.nlink, 0, "the tracked inode the save replaced");
            assert_eq!(read(&fs, tracked, 64).await, b"tracked");
            assert_eq!(
                lookup(&fs, ROOT_INO, "out.bin.tmp").await,
                Err(Errno::ENOENT)
            );
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["out.bin"]);
            assert_eq!(names(&instance.write_dir()), vec!["out.bin"]);
            assert_eq!(
                std::fs::read(instance.write_dir().join("out.bin")).expect("write/out.bin"),
                b"saved"
            );
            assert!(deleted(&instance).is_empty());
        })
        .await;
}

/// RENAME over a file made under the mount replaces it: the name serves the moved file, and the
/// replaced inode, which the kernel still holds for an open handle, keeps reading its own bytes
/// with no link left.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_rename_replaced_keeps_reading_through_the_inode_the_kernel_holds() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let old = create_with(&fs, ROOT_INO, "final.o", b"old").await;
            let temp = create_with(&fs, ROOT_INO, "final.o.tmp", b"new!").await;
            fs.rename(
                &request(),
                ROOT_INO,
                b"final.o.tmp",
                ROOT_INO,
                b"final.o",
                0,
            )
            .await
            .expect("rename over");
            assert_eq!(lookup(&fs, ROOT_INO, "final.o").await, Ok(temp));
            assert_eq!(
                lookup(&fs, ROOT_INO, "final.o.tmp").await,
                Err(Errno::ENOENT)
            );
            assert_eq!(read(&fs, temp, 64).await, b"new!");
            assert_eq!(read(&fs, old, 64).await, b"old", "the replaced inode");
            let attr = fs.getattr(&request(), old).await.expect("getattr").attr;
            assert_eq!(attr.nlink, 0);
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["final.o", "keep.txt"]);
            assert_eq!(names(&instance.write_dir()), vec!["final.o"]);
            assert_eq!(
                std::fs::read(instance.write_dir().join("final.o")).expect("write/final.o"),
                b"new!"
            );
        })
        .await;
}

/// RENAME of a directory made under the mount moves it in the State and under `write/`: its
/// files, nested ones too, are found and read at the new path, and nothing is left at the old.
#[tokio::test(flavor = "multi_thread")]
async fn renaming_an_added_directory_moves_its_files_and_their_host_copies() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let sub = fs
                .mkdir(&request(), made, b"sub", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir sub")
                .ino;
            create_with(&fs, made, "a.txt", b"a").await;
            create_with(&fs, sub, "b.txt", b"b").await;
            fs.rename(&request(), ROOT_INO, b"made", ROOT_INO, b"moved", 0)
                .await
                .expect("rename the added directory");
            assert_eq!(lookup(&fs, ROOT_INO, "made").await, Err(Errno::ENOENT));
            let moved = lookup(&fs, ROOT_INO, "moved").await.expect("moved");
            assert_eq!(listed(&fs, moved).await, vec!["a.txt", "sub"]);
            let a = lookup(&fs, moved, "a.txt").await.expect("moved/a.txt");
            assert_eq!(read(&fs, a, 64).await, b"a");
            let sub = lookup(&fs, moved, "sub").await.expect("moved/sub");
            let b = lookup(&fs, sub, "b.txt").await.expect("moved/sub/b.txt");
            assert_eq!(read(&fs, b, 64).await, b"b");
            assert_eq!(names(&instance.write_dir()), vec!["moved"]);
            assert_eq!(
                std::fs::read(instance.write_dir().join("moved/sub/b.txt"))
                    .expect("write/moved/sub/b.txt"),
                b"b"
            );
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt", "moved"]);
        })
        .await;
}

/// RMDIR of a directory the revision holds, emptied first, then MKDIR of its name: the new
/// directory lists only what is created in it, and so does the provider rebuilt over the same
/// `write/` and deletion list, as a remount builds it.
#[tokio::test(flavor = "multi_thread")]
async fn rmdir_then_mkdir_of_a_tracked_directory_lists_only_what_is_created_in_it() {
    let instance =
        Instance::new(&[("dir/a.txt", b"a"), ("dir/b.txt", b"b"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            fs.unlink(&request(), dir, b"a.txt")
                .await
                .expect("unlink a");
            fs.unlink(&request(), dir, b"b.txt")
                .await
                .expect("unlink b");
            fs.rmdir(&request(), ROOT_INO, b"dir").await.expect("rmdir");
            assert_eq!(lookup(&fs, ROOT_INO, "dir").await, Err(Errno::ENOENT));
            let again = fs
                .mkdir(&request(), ROOT_INO, b"dir", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let created = create_with(&fs, again, "c.txt", b"c").await;
            assert_eq!(listed(&fs, again).await, vec!["c.txt"]);
            assert_eq!(lookup(&fs, again, "a.txt").await, Err(Errno::ENOENT));
            assert_eq!(read(&fs, created, 64).await, b"c");
            assert_eq!(deleted(&instance), vec!["dir/a.txt", "dir/b.txt", "dir"]);
            drop(fs);

            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir")
                .await
                .expect("dir after the remount");
            assert_eq!(listed(&fs, dir).await, vec!["c.txt"]);
            let c = lookup(&fs, dir, "c.txt")
                .await
                .expect("c.txt after the remount");
            assert_eq!(read(&fs, c, 64).await, b"c");
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["dir", "keep.txt"]);
        })
        .await;
}

/// CREATE, MKNOD, MKDIR, SYMLINK and RENAME refuse a name the State cannot store with `EINVAL`,
/// before anything reaches `write/`.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_the_state_cannot_store_is_einval_and_reaches_nothing() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let bad: &[u8] = b"back\\slash";
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            assert_eq!(
                fs.create(
                    &request(),
                    ROOT_INO,
                    bad,
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags
                )
                .await
                .map(|reply| reply.entry.ino),
                Err(Errno::EINVAL)
            );
            assert_eq!(
                fs.mknod(&request(), ROOT_INO, bad, libc::S_IFREG | 0o644, 0, 0o022)
                    .await
                    .map(|reply| reply.ino),
                Err(Errno::EINVAL)
            );
            assert_eq!(
                fs.mkdir(&request(), ROOT_INO, bad, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .map(|reply| reply.ino),
                Err(Errno::EINVAL)
            );
            assert_eq!(
                fs.symlink(&request(), ROOT_INO, bad, b"keep.txt")
                    .await
                    .map(|reply| reply.ino),
                Err(Errno::EINVAL)
            );
            create_with(&fs, ROOT_INO, "ok.txt", b"ok").await;
            assert_eq!(
                fs.rename(&request(), ROOT_INO, b"ok.txt", ROOT_INO, bad, 0)
                    .await,
                Err(Errno::EINVAL)
            );
            assert_eq!(names(&instance.write_dir()), vec!["ok.txt"]);
            assert!(lookup(&fs, ROOT_INO, "ok.txt").await.is_ok());
        })
        .await;
}

/// UNLINK of a tracked file whose path the deletion list cannot hold, a name with a line feed,
/// still succeeds: the lost record only means the file comes back at the next mount.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_the_deletion_list_cannot_record_still_succeeds() {
    let instance = Instance::new(&[("line\nfeed.txt", b"x"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup(&fs, ROOT_INO, "line\nfeed.txt")
                .await
                .expect("the tracked file");
            fs.unlink(&request(), ROOT_INO, b"line\nfeed.txt")
                .await
                .expect("unlink succeeds");
            assert_eq!(
                lookup(&fs, ROOT_INO, "line\nfeed.txt").await,
                Err(Errno::ENOENT)
            );
            assert!(deleted(&instance).is_empty());
        })
        .await;
}

/// An UNLINK or RMDIR whose State detach fails, as where the node left the State after the
/// request resolved it, answers `EIO` and puts the host entry back: a local file with its bytes,
/// a local directory with its mode and times.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_or_rmdir_whose_detach_fails_puts_the_host_entry_back() {
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "f.txt", b"local bytes").await;
            fs.mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o750, 0o022)
                .await
                .expect("mkdir");
            let stamped =
                std::time::SystemTime::UNIX_EPOCH + Duration::new(1_600_000_000, 125_000_000);
            std::fs::File::open(instance.write_dir().join("d"))
                .expect("the host directory")
                .set_modified(stamped)
                .expect("set the host mtime");
            let removed = [
                node_at(&instance, &fs, "f.txt").await,
                node_at(&instance, &fs, "d").await,
            ];
            let (state, repository) = (fs.state(), instance.fixture.repository.clone());
            let runtime = tokio::runtime::Handle::current();
            let detached = Arc::new(AtomicUsize::new(0));
            fs.set_write_hook(Arc::new({
                let detached = detached.clone();
                move |step| {
                    if let WriteStep::Detaching { node } = step
                        && removed.contains(&node)
                    {
                        let (state, repository) = (state.clone(), repository.clone());
                        let (done_tx, done_rx) = std::sync::mpsc::channel();
                        runtime.spawn(async move {
                            let _ = done_tx.send(
                                state
                                    .node_detach(repository, node, Detach::Always)
                                    .await
                                    .is_ok(),
                            );
                        });
                        tokio::task::block_in_place(|| {
                            if done_rx.recv_timeout(Duration::from_secs(30)) == Ok(true) {
                                detached.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    }
                    true
                }
            }));

            let unlinked = fs.unlink(&request(), ROOT_INO, b"f.txt").await;
            let removed_dir = fs.rmdir(&request(), ROOT_INO, b"d").await;

            assert_eq!(
                detached.load(Ordering::SeqCst),
                2,
                "each node left the State inside its request"
            );
            assert_eq!(unlinked, Err(Errno::EIO));
            assert_eq!(removed_dir, Err(Errno::EIO));
            assert_eq!(
                std::fs::read(instance.write_dir().join("f.txt")).expect("write/f.txt"),
                b"local bytes"
            );
            let host = std::fs::metadata(instance.write_dir().join("d")).expect("write/d");
            assert!(host.is_dir());
            assert_eq!(host.mode() & 0o7777, 0o750);
            assert_eq!(
                (host.mtime(), host.mtime_nsec()),
                (1_600_000_000, 125_000_000)
            );
        })
        .await;
}

/// UNLINK keeps its directory's listing, and each unlink finds its name live among the listed
/// entries, which never reads the log: removing every file of a directory of two thousand builds
/// no listing, and the READDIR after builds at most one.
#[tokio::test(flavor = "multi_thread")]
async fn removing_every_file_of_a_large_directory_builds_no_listing() {
    const FILES: usize = 2000;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"big", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..FILES {
                create_with(&fs, dir, &format!("f{i}.o"), b"x").await;
            }
            assert_eq!(listed(&fs, dir).await.len(), FILES);
            let before = fs.listings_built();
            for i in 0..FILES {
                fs.unlink(&request(), dir, format!("f{i}.o").as_bytes())
                    .await
                    .expect("unlink");
            }
            assert_eq!(
                fs.listings_built() - before,
                0,
                "no build during the unlinks"
            );
            assert!(listed(&fs, dir).await.is_empty());
            assert!(
                fs.listings_built() - before <= 1,
                "at most one for the READDIR"
            );
        })
        .await;
}

/// WRITE, SETATTR and FALLOCATE of a local file hold the inode's write generation odd while
/// their host operation runs, and leave it even and advanced by two: a SETATTR of the size, of
/// the mode alone and of the times alone each.
#[tokio::test(flavor = "multi_thread")]
async fn the_write_generation_is_odd_during_a_local_write_and_even_and_advanced_after() {
    use lore_fuse::abi::SetattrIn;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = create_with(&fs, ROOT_INO, "local.bin", b"start").await;
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let hook_seen = seen.clone();
            fs.set_write_hook(Arc::new(move |step| {
                if let WriteStep::HostWrite { generation } = step {
                    hook_seen.lock().expect("seen").push(generation);
                }
                true
            }));
            let generation = || fs.write_generation(ino).expect("the inode");
            let before = generation();
            fs.write(&request(), ino, 0, 0, b"written", 0)
                .await
                .expect("write");
            let after_write = generation();
            let truncate = SetattrIn {
                valid: lore_fuse::abi::entry::setattr_valid::SIZE,
                size: 3,
                ..SetattrIn::default()
            };
            fs.setattr(&request(), ino, &truncate)
                .await
                .expect("truncate");
            let after_truncate = generation();
            fs.fallocate(&request(), ino, 0, 0, 4096, 0)
                .await
                .expect("fallocate");
            let after_allocate = generation();
            let mode_only = SetattrIn {
                valid: lore_fuse::abi::entry::setattr_valid::MODE,
                mode: libc::S_IFREG | 0o600,
                ..SetattrIn::default()
            };
            fs.setattr(&request(), ino, &mode_only)
                .await
                .expect("chmod");
            let after_mode = generation();
            let times_only = SetattrIn {
                valid: lore_fuse::abi::entry::setattr_valid::ATIME
                    | lore_fuse::abi::entry::setattr_valid::MTIME,
                atime: 1_000,
                mtime: 2_000,
                ..SetattrIn::default()
            };
            fs.setattr(&request(), ino, &times_only)
                .await
                .expect("utimes");
            let after_times = generation();
            let seen = seen.lock().expect("seen").clone();
            assert_eq!(
                seen,
                vec![
                    before + 1,
                    after_write + 1,
                    after_truncate + 1,
                    after_allocate + 1,
                    after_mode + 1
                ],
                "odd while each host operation runs"
            );
            assert_eq!(before % 2, 0);
            assert_eq!(
                [
                    after_write,
                    after_truncate,
                    after_allocate,
                    after_mode,
                    after_times
                ],
                [before + 2, before + 4, before + 6, before + 8, before + 10]
            );
            let attr = fs.getattr(&request(), ino).await.expect("attr").attr;
            assert_eq!(attr.size, 4096, "the allocation extends the file");
            assert_eq!(attr.mode & 0o777, 0o600, "the mode is set");
            assert_eq!(attr.mtime, 2_000, "the times are set");
        })
        .await;
}

/// WRITE into a file the revision holds copies it up before the reply: it reads back the
/// tracked bytes with the written ones over them, under the inode the kernel holds, and its node
/// is a dirty, zero-address local-content node whose bytes live under `write/`.
#[tokio::test(flavor = "multi_thread")]
async fn a_written_tracked_file_reads_back_what_was_written_and_is_dirty() {
    let instance = Instance::new(&[("dir/tracked.txt", b"tracked bytes")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "tracked.txt").await.expect("tracked.txt");
            fs.write(&request(), ino, 0, 8, b"BYTES", 0)
                .await
                .expect("write");
            assert_eq!(read(&fs, ino, 64).await, b"tracked BYTES");
            assert_eq!(lookup(&fs, dir, "tracked.txt").await, Ok(ino));
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/tracked.txt"))
                    .expect("write/dir/tracked.txt"),
                b"tracked BYTES"
            );
            let node = fs
                .state()
                .node(
                    instance.fixture.repository.clone(),
                    node_at(&instance, &fs, "dir/tracked.txt").await,
                )
                .await
                .expect("the node");
            assert!(node.is_dirty_modify(), "the node is dirty");
            assert!(
                node.ext_flags()
                    .contains(lore_revision::node::NodeExtFlags::LocalContent)
            );
            assert!(
                node.address.hash.is_zero(),
                "its content is unknown to the store"
            );
        })
        .await;
}

/// What a provider serves below the root: every path, with a file's bytes, sorted by path.
async fn served_tree(fs: &FuseFilesystem) -> Vec<(String, Option<Vec<u8>>)> {
    let mut tree = Vec::new();
    let mut pending = vec![(ROOT_INO, String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for name in listed(fs, dir).await {
            let path = format!("{prefix}{name}");
            let ino = lookup(fs, dir, &name).await.expect("a listed name");
            let attr = fs.getattr(&request(), ino).await.expect("attr").attr;
            if attr.mode & libc::S_IFMT == libc::S_IFDIR {
                pending.push((ino, format!("{path}/")));
                tree.push((path, None));
            } else {
                tree.push((path, Some(read(fs, ino, 1 << 16).await)));
            }
        }
    }
    tree.sort();
    tree
}

/// A provider rebuilt over the same `write/` and deletion list, as a remount builds it, serves
/// what the requests left: created files and directories, writes to tracked and added files,
/// deletions of tracked and added names, and renames of both.
#[tokio::test(flavor = "multi_thread")]
async fn a_remount_reproduces_creates_writes_deletes_and_renames() {
    let instance = Instance::new(&[
        ("src/main.c", b"int main;"),
        ("src/gone.c", b"gone"),
        ("src/old.c", b"old"),
        ("README", b"readme"),
    ])
    .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let src = lookup(&fs, ROOT_INO, "src").await.expect("src");
            let obj = fs
                .mkdir(&request(), ROOT_INO, b"obj", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, obj, "main.o", b"object").await;
            let scratch = create_with(&fs, obj, "scratch", b"scratch").await;
            fs.write(&request(), scratch, 0, 7, b" more", 0)
                .await
                .expect("write an added file");
            let main = lookup(&fs, src, "main.c").await.expect("main.c");
            fs.write(&request(), main, 0, 0, b"INT", 0)
                .await
                .expect("write a tracked file");
            fs.unlink(&request(), src, b"gone.c")
                .await
                .expect("unlink tracked");
            create_with(&fs, obj, "tmp.o", b"t").await;
            fs.unlink(&request(), obj, b"tmp.o")
                .await
                .expect("unlink added");
            fs.rename(&request(), src, b"old.c", src, b"new.c", 0)
                .await
                .expect("rename tracked");
            fs.rename(&request(), obj, b"main.o", ROOT_INO, b"main.o", 0)
                .await
                .expect("rename added");
            let before = served_tree(&fs).await;
            assert_eq!(
                before
                    .iter()
                    .map(|(path, _)| path.as_str())
                    .collect::<Vec<_>>(),
                vec![
                    "README",
                    "main.o",
                    "obj",
                    "obj/scratch",
                    "src",
                    "src/main.c",
                    "src/new.c"
                ]
            );
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(served_tree(&fs).await, before);
        })
        .await;
}

/// Files created and written from many tasks at once in one directory are all listed and each
/// reads back its own bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writes_from_many_threads_into_one_directory_are_all_served() {
    const WRITERS: usize = 8;
    const FILES: usize = 40;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let out = fs
                .mkdir(&request(), ROOT_INO, b"out", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let mut writers = tokio::task::JoinSet::new();
            for writer in 0..WRITERS {
                let fs = fs.clone();
                let _ = lore_base::lore_spawn!(writers, async move {
                    for file in 0..FILES {
                        let name = format!("w{writer}-f{file}.o");
                        let ino = create_with(&fs, out, &name, b"").await;
                        for chunk in 0..4u64 {
                            let bytes = format!("{name}:{chunk};");
                            fs.write(&request(), ino, 0, chunk * 32, bytes.as_bytes(), 0)
                                .await
                                .expect("write");
                        }
                    }
                });
            }
            while let Some(joined) = writers.join_next().await {
                joined.expect("writer");
            }
            let listed = listed(&fs, out).await;
            assert_eq!(listed.len(), WRITERS * FILES);
            for writer in 0..WRITERS {
                for file in 0..FILES {
                    let name = format!("w{writer}-f{file}.o");
                    let ino = lookup(&fs, out, &name).await.expect("a written file");
                    let bytes = read(&fs, ino, 256).await;
                    for chunk in 0..4usize {
                        let expected = format!("{name}:{chunk};");
                        assert_eq!(
                            &bytes[chunk * 32..chunk * 32 + expected.len()],
                            expected.as_bytes(),
                            "{name} chunk {chunk}"
                        );
                    }
                }
            }
            assert_eq!(
                names(&instance.write_dir().join("out")).len(),
                WRITERS * FILES
            );
        })
        .await;
}

/// READ and WRITE latency of an open local file: 4 KiB `O_DIRECT` requests, each reaching the
/// provider, timed at the client, with the provider's own figures from the session's metrics.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_local_file_read_and_write_latency() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    use lore_fuse::abi::Opcode;
    skip_without_fuse!();
    const SAMPLES: usize = 20_000;
    const LEN: usize = 4096;
    const SPAN: usize = 256;
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    let m = instance.mount().await;
    let path = m.root.join("local.bin");
    let (mut writes, mut reads) = blocking(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
            .expect("create through the mount");
        let buf = fuse_test_util::Aligned::new(LEN);
        let pwrite = |offset: usize| {
            // SAFETY: the fd is open and `buf` holds `LEN` readable bytes.
            let n = unsafe {
                libc::pwrite(
                    file.as_raw_fd(),
                    buf.as_slice().as_ptr().cast(),
                    LEN,
                    offset as libc::off_t,
                )
            };
            assert_eq!(
                n,
                LEN as isize,
                "pwrite: {}",
                std::io::Error::last_os_error()
            );
        };
        for block in 0..SPAN {
            pwrite(block * LEN);
        }
        let mut writes = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            let started = std::time::Instant::now();
            pwrite((i % SPAN) * LEN);
            writes.push(started.elapsed().as_nanos() as u64);
        }
        let mut reads = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            let started = std::time::Instant::now();
            assert_eq!(
                fuse_test_util::pread(&file, &buf, LEN, ((i % SPAN) * LEN) as u64),
                LEN
            );
            reads.push(started.elapsed().as_nanos() as u64);
        }
        (writes, reads)
    })
    .await;
    let write = bench_stats::Summary::of(&mut writes);
    let read = bench_stats::Summary::of(&mut reads);
    let metrics = m.session.metrics();
    let (served_write, served_read) = (
        metrics.snapshot(Opcode::Write),
        metrics.snapshot(Opcode::Read),
    );
    println!(
        "local WRITE 4 KiB O_DIRECT ({SAMPLES}): client p50 {:.1} us p99 {:.1} us | daemon-side p50 {:.1} us p99 {:.1} us",
        write.p50 as f64 / 1000.0,
        write.p99 as f64 / 1000.0,
        served_write.p50_ns as f64 / 1000.0,
        served_write.p99_ns as f64 / 1000.0
    );
    println!(
        "local READ 4 KiB O_DIRECT ({SAMPLES}): client p50 {:.1} us p99 {:.1} us | daemon-side p50 {:.1} us p99 {:.1} us",
        read.p50 as f64 / 1000.0,
        read.p99 as f64 / 1000.0,
        served_read.p50_ns as f64 / 1000.0,
        served_read.p99_ns as f64 / 1000.0
    );
    m.unmount().await;
}

/// A commit's flip landing between a WRITE's copy-up check and its host write, simulated from
/// the hook between them: the inode turned store-served, `write/<path>` unlinked and the
/// descriptor dropped. The WRITE goes back through the copy-up, so its bytes land in a live
/// local file, which READ returns.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_racing_a_commit_flip_lands_in_a_live_local_file() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::node::NodeExtFlags;
    let instance = Instance::new(&[("f.txt", b"committed")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            let ino = lookup(&fs, ROOT_INO, "f.txt").await.expect("f.txt");
            let node = node_at(&instance, &fs, "f.txt").await;
            let committed = fs
                .state()
                .node(repository.clone(), node)
                .await
                .expect("the tracked node");
            fs.write(&request(), ino, 0, 0, b"C", 0)
                .await
                .expect("copy up");
            // The flip's State step: the node takes the committed content back, clean.
            fs.state()
                .node_rewrite(
                    &repository,
                    node,
                    &Content {
                        mode: committed.mode,
                        size: committed.size,
                        address: committed.address,
                    },
                    Rewrite::Dirty(NodeFlags::NoFlags, NodeExtFlags::NoFlags),
                )
                .await
                .expect("flip the node");
            let flipped = Arc::new(AtomicBool::new(false));
            let before_host_writes = Arc::new(AtomicUsize::new(0));
            let (hook_fs, host) = (Arc::downgrade(&fs), instance.write_dir().join("f.txt"));
            fs.set_write_hook(Arc::new({
                let (flipped, before_host_writes) = (flipped.clone(), before_host_writes.clone());
                move |step| {
                    if step == WriteStep::BeforeHostWrite {
                        before_host_writes.fetch_add(1, Ordering::SeqCst);
                    }
                    if step == WriteStep::BeforeHostWrite
                        && !flipped.swap(true, Ordering::SeqCst)
                        && let Some(fs) = hook_fs.upgrade()
                    {
                        if let Some(mut entry) = fs.inodes().get_mut(ino) {
                            entry.kind = InodeKind::File {
                                address: committed.address,
                                size: committed.size,
                                executable: false,
                                local: false,
                            };
                        }
                        std::fs::remove_file(&host).expect("unlink write/f.txt");
                        fs.close_local_file(ino);
                    }
                    true
                }
            }));
            fs.write(&request(), ino, 0, 0, b"W", 0)
                .await
                .expect("the write lands");
            assert!(
                flipped.load(Ordering::SeqCst),
                "the flip landed inside the write"
            );
            assert_eq!(read(&fs, ino, 64).await, b"Wommitted");
            assert_eq!(
                before_host_writes.load(Ordering::SeqCst),
                2,
                "the write went back through the copy-up once"
            );
            assert_eq!(
                std::fs::read(instance.write_dir().join("f.txt")).expect("write/f.txt"),
                b"Wommitted"
            );
        })
        .await;
}

/// A commit's flip and then a copy-up by another WRITE, both landing between a WRITE's copy-up
/// check and its host write: the inode is local again, under another descriptor. The WRITE goes
/// back through the copy-up rather than writing the unlinked host file, so both writes are
/// served.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_racing_a_flip_and_a_new_copy_up_lands_in_the_new_host_file() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::node::NodeExtFlags;
    let instance = Instance::new(&[("f.txt", b"committed")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let repository = instance.fixture.repository.clone();
            let ino = lookup(&fs, ROOT_INO, "f.txt").await.expect("f.txt");
            let node = node_at(&instance, &fs, "f.txt").await;
            let committed = fs
                .state()
                .node(repository.clone(), node)
                .await
                .expect("the tracked node");
            fs.write(&request(), ino, 0, 0, b"C", 0)
                .await
                .expect("copy up");
            // The flip's State step: the node takes the committed content back, clean.
            fs.state()
                .node_rewrite(
                    &repository,
                    node,
                    &Content {
                        mode: committed.mode,
                        size: committed.size,
                        address: committed.address,
                    },
                    Rewrite::Dirty(NodeFlags::NoFlags, NodeExtFlags::NoFlags),
                )
                .await
                .expect("flip the node");
            let raced = Arc::new(AtomicBool::new(false));
            let before_host_writes = Arc::new(AtomicUsize::new(0));
            let spawned: Spawned = Arc::default();
            let (hook_fs, host) = (Arc::downgrade(&fs), instance.write_dir().join("f.txt"));
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (raced, before_host_writes, spawned) =
                    (raced.clone(), before_host_writes.clone(), spawned.clone());
                move |step| {
                    if step != WriteStep::BeforeHostWrite {
                        return true;
                    }
                    before_host_writes.fetch_add(1, Ordering::SeqCst);
                    if raced.swap(true, Ordering::SeqCst) {
                        return true;
                    }
                    let Some(fs) = hook_fs.upgrade() else {
                        return true;
                    };
                    if let Some(mut entry) = fs.inodes().get_mut(ino) {
                        entry.kind = InodeKind::File {
                            address: committed.address,
                            size: committed.size,
                            executable: false,
                            local: false,
                        };
                    }
                    std::fs::remove_file(&host).expect("unlink write/f.txt");
                    fs.close_local_file(ino);
                    let copy_up = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            fs.write(&request(), ino, 0, 1, b"N", 0)
                                .await
                                .expect("a write copying the file up again");
                        },
                    ));
                    // The copy-up may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !copy_up.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    spawned.lock().expect("handles").push(copy_up);
                    true
                }
            }));
            fs.write(&request(), ino, 0, 0, b"W", 0)
                .await
                .expect("the write lands");
            settled_requests(&spawned).await;
            assert!(
                raced.load(Ordering::SeqCst),
                "the flip and the copy-up landed inside the write"
            );
            assert_eq!(read(&fs, ino, 64).await, b"WNmmitted");
            assert_eq!(
                std::fs::read(instance.write_dir().join("f.txt")).expect("write/f.txt"),
                b"WNmmitted"
            );
            assert_eq!(
                before_host_writes.load(Ordering::SeqCst),
                3,
                "the write went back through the copy-up once"
            );
        })
        .await;
}

/// A WRITE that finds its descriptor gone at every host write, as a run of flips and copy-ups
/// would leave it, gives up with `EAGAIN` after its attempts, having written nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_whose_descriptor_keeps_changing_gives_up_with_eagain() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    let instance = Instance::new(&[("f.txt", b"committed")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "f.txt").await.expect("f.txt");
            fs.write(&request(), ino, 0, 0, b"C", 0)
                .await
                .expect("copy up");
            let before_host_writes = Arc::new(AtomicUsize::new(0));
            let hook_fs = Arc::downgrade(&fs);
            fs.set_write_hook(Arc::new({
                let before_host_writes = before_host_writes.clone();
                move |step| {
                    if step == WriteStep::BeforeHostWrite
                        && let Some(fs) = hook_fs.upgrade()
                    {
                        before_host_writes.fetch_add(1, Ordering::SeqCst);
                        fs.close_local_file(ino);
                    }
                    true
                }
            }));
            let written = fs.write(&request(), ino, 0, 0, b"W", 0).await;
            // The provider's `HOST_WRITE_ATTEMPTS`.
            assert_eq!(
                before_host_writes.load(Ordering::SeqCst),
                8,
                "every attempt found its descriptor gone"
            );
            assert_eq!(written, Err(Errno::EAGAIN));
            assert_eq!(
                std::fs::read(instance.write_dir().join("f.txt")).expect("write/f.txt"),
                b"Committed",
                "an attempt wrote"
            );
        })
        .await;
}

/// A commit read a file no inode held. After its flip has checked the names arriving, a LOOKUP
/// makes an inode holding no write, and a RENAME of the file to another spelling of its name
/// claims the name and waits before it moves the host file. The flip leaves the file alone while
/// the claim is held: it stays local and `write/` keeps it, and the rename, let go, moves it with
/// no copy-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_claiming_the_name_inside_the_flip_keeps_the_file_local() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    /// Lets the waiting rename go however the test ends, so no worker spins past it.
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let first = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), first, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            // No inode holds the file when the commit reads it.
            fs.forget(first, 1);
            let armed = Arc::new(AtomicBool::new(true));
            let claimed = Arc::new(AtomicBool::new(false));
            let release = Release(Arc::new(AtomicBool::new(false)));
            let copy_ups = Arc::new(AtomicUsize::new(0));
            let reader = Arc::new(AtomicU64::new(0));
            // The reader's write generation before the rename starts; `u64::MAX` for none.
            let generation = Arc::new(AtomicU64::new(u64::MAX));
            let spawned: Spawned = Arc::default();
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (armed, claimed, released) =
                    (armed.clone(), claimed.clone(), release.0.clone());
                let (copy_ups, reader, generation, spawned) = (
                    copy_ups.clone(),
                    reader.clone(),
                    generation.clone(),
                    spawned.clone(),
                );
                move |step| {
                    match step {
                        WriteStep::CopyUp => {
                            copy_ups.fetch_add(1, Ordering::SeqCst);
                        }
                        // The rename holds its claim on the name here, before it moves the host
                        // file.
                        WriteStep::RenameTargetBuilt { .. }
                            if !claimed.swap(true, Ordering::SeqCst) =>
                        {
                            tokio::task::block_in_place(|| {
                                while !released.load(Ordering::SeqCst) {
                                    std::thread::yield_now();
                                }
                            });
                        }
                        WriteStep::FlipChecked { .. } if armed.swap(false, Ordering::SeqCst) => {
                            let Some(fs) = weak.upgrade() else {
                                return true;
                            };
                            let (reader, generation) = (reader.clone(), generation.clone());
                            let renaming = runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let looked =
                                        lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
                                    reader.store(looked, Ordering::SeqCst);
                                    generation.store(
                                        fs.write_generation(looked).unwrap_or(u64::MAX),
                                        Ordering::SeqCst,
                                    );
                                    fs.rename(
                                        &request(),
                                        ROOT_INO,
                                        b"a.txt",
                                        ROOT_INO,
                                        b"A.TXT",
                                        0,
                                    )
                                    .await
                                    .expect("the rename");
                                },
                            ));
                            // The rename may wait in this worker's queue: it is handed to another
                            // worker first.
                            tokio::task::block_in_place(|| {
                                while !claimed.load(Ordering::SeqCst) && !renaming.is_finished() {
                                    std::thread::yield_now();
                                }
                            });
                            spawned.lock().expect("handles").push(renaming);
                        }
                        _ => {}
                    }
                    true
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(!armed.load(Ordering::SeqCst), "the flip checked no file");
            // Generation 0 sends the flip to `flip_held`'s re-check under the guard.
            assert_eq!(
                generation.load(Ordering::SeqCst),
                0,
                "the inode the flip found held a write before the rename"
            );
            assert!(
                claimed.load(Ordering::SeqCst),
                "the rename claimed the name inside the flip"
            );
            let reader = reader.load(Ordering::SeqCst);
            assert!(
                fs.inodes()
                    .get(reader)
                    .is_some_and(|entry| entry.kind.is_local_file()),
                "the flip turned the file store-served while its name was claimed"
            );
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"ONE.."
            );
            release.0.store(true, Ordering::SeqCst);
            settled_requests(&spawned).await;
            assert_eq!(read(&fs, reader, 64).await, b"ONE..");
            assert_eq!(
                std::fs::read(instance.write_dir().join("A.TXT")).expect("write/A.TXT"),
                b"ONE.."
            );
            assert!(
                fs_node(&instance, &fs, "A.TXT").await.is_dirty(),
                "A.TXT is clean"
            );
            assert_eq!(
                copy_ups.load(Ordering::SeqCst),
                0,
                "the rename copied the file up again"
            );
        })
        .await;
}

/// A read-only provider over `instance`, with no `write/`.
async fn read_only_provider(instance: &Instance) -> Arc<FuseFilesystem> {
    FuseFilesystem::new(
        instance.fixture.repository.clone(),
        instance.fixture.state.clone(),
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
    )
    .await
    .expect("read-only provider")
}

/// A provider without `write/` refuses a write with `EROFS`, and its files carry no write bit.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_provider_refuses_writes_with_erofs() {
    let instance = Instance::new(&[("a.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = read_only_provider(&instance).await;
            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
            let refused = fs
                .create(
                    &request(),
                    ROOT_INO,
                    b"new.txt",
                    libc::S_IFREG | 0o644,
                    0o022,
                    flags,
                )
                .await
                .err();
            assert_eq!(refused, Some(Errno::EROFS));
            let tracked = fs
                .lookup(&request(), ROOT_INO, b"a.txt")
                .await
                .expect("lookup");
            assert_eq!(tracked.attr.mode, libc::S_IFREG | 0o444);
        })
        .await;
}

/// FSYNC on a provider without `write/` is `ENOSYS`, so the kernel treats every later fsync as
/// a success.
#[tokio::test(flavor = "multi_thread")]
async fn fsync_on_a_read_only_provider_is_enosys() {
    let instance = Instance::new(&[("a.txt", b"tracked")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = read_only_provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            // FSYNC reaches the provider as `fsync_local`; `fsync` is the trait's default.
            assert!(matches!(
                fs.fsync_local(&request(), ino, 0, false).await,
                Err(ReadFault::Errno(Errno::ENOSYS))
            ));
        })
        .await;
}

/// LINK on a writable provider is `EPERM`: no hard link is made.
#[tokio::test(flavor = "multi_thread")]
async fn link_on_a_writable_provider_is_eperm() {
    let instance = Instance::new(&[("a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(
                fs.link(&request(), ino, ROOT_INO, b"hard").await.err(),
                Some(Errno::EPERM)
            );
            assert_eq!(lookup(&fs, ROOT_INO, "hard").await, Err(Errno::ENOENT));
        })
        .await;
}

/// RENAME under `RENAME_NOREPLACE` onto an existing name is `EEXIST` and changes nothing; onto
/// a free name it renames.
#[tokio::test(flavor = "multi_thread")]
async fn rename_noreplace_onto_an_existing_name_is_eexist() {
    let instance = Instance::new(&[("a.txt", b"a"), ("b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let noreplace = libc::RENAME_NOREPLACE;
            assert_eq!(
                fs.rename(
                    &request(),
                    ROOT_INO,
                    b"a.txt",
                    ROOT_INO,
                    b"b.txt",
                    noreplace
                )
                .await,
                Err(Errno::EEXIST)
            );
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(
                (read(&fs, a, 8).await, read(&fs, b, 8).await),
                (b"a".to_vec(), b"b".to_vec())
            );
            assert_eq!(
                fs.rename(
                    &request(),
                    ROOT_INO,
                    b"missing",
                    ROOT_INO,
                    b"c.txt",
                    noreplace
                )
                .await,
                Err(Errno::ENOENT)
            );
            fs.rename(
                &request(),
                ROOT_INO,
                b"a.txt",
                ROOT_INO,
                b"c.txt",
                noreplace,
            )
            .await
            .expect("a rename onto a free name");
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["b.txt", "c.txt"]);
        })
        .await;
}

/// RENAME under `RENAME_EXCHANGE` is `EINVAL` and changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn rename_exchange_is_einval() {
    let instance = Instance::new(&[("a.txt", b"a"), ("b.txt", b"b")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(
                fs.rename(
                    &request(),
                    ROOT_INO,
                    b"a.txt",
                    ROOT_INO,
                    b"b.txt",
                    libc::RENAME_EXCHANGE
                )
                .await,
                Err(Errno::EINVAL)
            );
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(
                (read(&fs, a, 8).await, read(&fs, b, 8).await),
                (b"a".to_vec(), b"b".to_vec())
            );
        })
        .await;
}

/// FLUSH is `ENOSYS`: nothing is buffered, so the kernel spares every close the round trip.
#[tokio::test(flavor = "multi_thread")]
async fn flush_is_left_to_the_kernel() {
    let instance = Instance::new(&[("a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = create_with(&fs, ROOT_INO, "new.o", b"object").await;
            assert_eq!(
                fs.flush(&request(), ino, 0, 0).await.err(),
                Some(Errno::ENOSYS)
            );
        })
        .await;
}

/// SETATTR of a tracked file's mode copies it up with the new mode and its content.
#[tokio::test(flavor = "multi_thread")]
async fn chmod_of_a_tracked_file_copies_it_up_with_the_new_mode() {
    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;

    let instance = Instance::new(&[("run.sh", b"#!/bin/sh\n")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "run.sh").await.expect("run.sh");
            let chmod = SetattrIn {
                valid: setattr_valid::MODE,
                mode: libc::S_IFREG | 0o755,
                ..SetattrIn::default()
            };
            let attr = fs
                .setattr(&request(), ino, &chmod)
                .await
                .expect("chmod")
                .attr;
            assert_eq!(attr.mode, libc::S_IFREG | 0o755);
            let host = instance.write_dir().join("run.sh");
            assert_eq!(
                std::fs::metadata(&host).expect("write/run.sh").mode() & 0o7777,
                0o755
            );
            assert_eq!(std::fs::read(&host).expect("write/run.sh"), b"#!/bin/sh\n");
            assert_eq!(read(&fs, ino, 64).await, b"#!/bin/sh\n");
        })
        .await;
}

/// A local file whose mode refuses writing is opened read-only; `chmod u+w` closes that
/// descriptor, so the next WRITE and truncate open the file read-write.
#[tokio::test(flavor = "multi_thread")]
async fn chmod_adding_write_permission_lets_a_read_only_local_file_be_written() {
    use std::os::unix::fs::PermissionsExt as _;

    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;

    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // Root opens a 0444 file read-write: there is no read-only descriptor to drop.
        return;
    }
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    plant(&instance, &[("ro.txt", b"first")]);
    let host = instance.write_dir().join("ro.txt");
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o444))
        .expect("a read-only host file");
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "ro.txt").await.expect("ro.txt");
            assert_eq!(read(&fs, ino, 64).await, b"first");
            assert_eq!(
                fs.write(&request(), ino, 0, 0, b"refused", 0).await.err(),
                Some(Errno::EBADF),
                "READ opened the host file read-only"
            );
            let chmod = SetattrIn {
                valid: setattr_valid::MODE,
                mode: libc::S_IFREG | 0o644,
                ..SetattrIn::default()
            };
            let attr = fs
                .setattr(&request(), ino, &chmod)
                .await
                .expect("chmod u+w")
                .attr;
            assert_eq!(attr.mode, libc::S_IFREG | 0o644);
            fs.write(&request(), ino, 0, 0, b"SECOND", 0)
                .await
                .expect("a write after chmod u+w");
            let truncate = SetattrIn {
                valid: setattr_valid::SIZE,
                size: 3,
                ..SetattrIn::default()
            };
            fs.setattr(&request(), ino, &truncate)
                .await
                .expect("truncate");
            assert_eq!(read(&fs, ino, 64).await, b"SEC");
            assert_eq!(std::fs::read(&host).expect("write/ro.txt"), b"SEC");
        })
        .await;
}

/// SETATTR of a directory the revision holds records a new modification time, to the
/// millisecond, and keeps its mode: a chmod to another mode is `EPERM`, a truncate `EISDIR`, and
/// neither changes the directory.
#[tokio::test(flavor = "multi_thread")]
async fn touching_a_tracked_directory_records_its_time_and_keeps_its_mode() {
    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;

    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let touch = SetattrIn {
                valid: setattr_valid::MTIME,
                mtime: 1_800_000_000,
                mtimensec: 250_999_999,
                ..SetattrIn::default()
            };
            let attr = fs
                .setattr(&request(), dir, &touch)
                .await
                .expect("touch")
                .attr;
            let touched = (1_800_000_000, 250_000_000, libc::S_IFDIR | 0o755);
            assert_eq!((attr.mtime, attr.mtimensec, attr.mode), touched);
            let same_mode = SetattrIn {
                valid: setattr_valid::MODE,
                mode: libc::S_IFDIR | 0o755,
                ..SetattrIn::default()
            };
            fs.setattr(&request(), dir, &same_mode)
                .await
                .expect("a chmod to the mode it has");
            let chmod = SetattrIn {
                valid: setattr_valid::MODE,
                mode: libc::S_IFDIR | 0o700,
                ..SetattrIn::default()
            };
            let truncate = SetattrIn {
                valid: setattr_valid::SIZE,
                size: 0,
                ..SetattrIn::default()
            };
            assert_eq!(
                fs.setattr(&request(), dir, &chmod).await.err(),
                Some(Errno::EPERM)
            );
            assert_eq!(
                fs.setattr(&request(), dir, &truncate).await.err(),
                Some(Errno::EISDIR)
            );
            let attr = fs.getattr(&request(), dir).await.expect("getattr").attr;
            assert_eq!((attr.mtime, attr.mtimensec, attr.mode), touched);
            assert!(
                !instance.write_dir().join("dir").exists(),
                "the directory stays tracked"
            );
        })
        .await;
}

/// SETATTR of a tracked directory to a time past what milliseconds hold saturates the time it
/// records rather than wrapping.
#[tokio::test(flavor = "multi_thread")]
async fn touching_a_tracked_directory_far_into_the_future_saturates_its_time() {
    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;

    let instance = Instance::new(&[("dir/a.txt", b"a")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            // 2^62 seconds in milliseconds wraps to 0, so a wrapping multiply shows.
            let touch = SetattrIn {
                valid: setattr_valid::MTIME,
                mtime: 1 << 62,
                ..SetattrIn::default()
            };
            let attr = fs
                .setattr(&request(), dir, &touch)
                .await
                .expect("touch")
                .attr;
            assert_eq!(attr.mtime, u64::MAX / 1000);
        })
        .await;
}

/// FALLOCATE of a file the revision holds copies it up first, under the inode the kernel holds,
/// then extends the copy: the tracked bytes read back ahead of the allocated zeros.
#[tokio::test(flavor = "multi_thread")]
async fn fallocate_of_a_tracked_file_copies_it_up_and_extends_it() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let tracked = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.fallocate(&request(), tracked, 0, 0, 4096, 0)
                .await
                .expect("fallocate");
            let attr = fs.getattr(&request(), tracked).await.expect("getattr").attr;
            assert_eq!((attr.ino, attr.size), (tracked, 4096));
            assert_eq!(lookup(&fs, ROOT_INO, "a.txt").await, Ok(tracked));
            let mut expected = b"alpha".to_vec();
            expected.resize(4096, 0);
            assert_eq!(read(&fs, tracked, 8192).await, expected);
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                expected
            );
            assert!(is_local(&fs_node(&instance, &fs, "a.txt").await));
        })
        .await;
}

/// A directory MKDIR makes and a file CREATE makes take the numbers their parent and folded name
/// derive, as tracked ones would; a truncate and a rewrite keep the file's number and advance its
/// modification time.
#[tokio::test(flavor = "multi_thread")]
async fn made_inodes_derive_from_parent_and_name_and_survive_a_rewrite() {
    use lore_fuse::abi::SetattrIn;
    use lore_fuse::abi::entry::setattr_valid;
    use lore_revision::fs::fuse::inode_number::Identity;
    use lore_revision::fs::fuse::inode_number::IdentityKind;
    use lore_revision::fs::fuse::inode_number::derive_inode_number;

    let derived = |name, kind| {
        derive_inode_number(&Identity {
            parent_ino: ROOT_INO,
            name,
            name_hash: hash_string(name),
            kind,
        })
    };
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(
                    &request(),
                    ROOT_INO,
                    b"Intermediate",
                    libc::S_IFDIR | 0o755,
                    0o022,
                )
                .await
                .expect("mkdir")
                .ino;
            assert_eq!(made, derived("intermediate", IdentityKind::Dir));
            let ino = create_with(&fs, ROOT_INO, "Out.o", b"first build").await;
            assert_eq!(ino, derived("out.o", IdentityKind::File));
            let stamped = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
            set_host_time(&instance, "Out.o", stamped);
            let first = fs.getattr(&request(), ino).await.expect("getattr").attr;
            assert_eq!(first.mtime, 1_600_000_000);
            let truncate = SetattrIn {
                valid: setattr_valid::SIZE,
                size: 0,
                ..SetattrIn::default()
            };
            let truncated = fs
                .setattr(&request(), ino, &truncate)
                .await
                .expect("truncate")
                .attr;
            assert_eq!((truncated.ino, truncated.size), (ino, 0));
            fs.write(&request(), ino, 0, 0, b"second", 0)
                .await
                .expect("rewrite");
            let second = fs.getattr(&request(), ino).await.expect("getattr").attr;
            assert_eq!((second.ino, second.size), (ino, 6));
            assert!(second.mtime > first.mtime, "the rewrite advances the mtime");
            assert_eq!(lookup(&fs, ROOT_INO, "Out.o").await, Ok(ino));
            assert_eq!(lookup(&fs, ROOT_INO, "OUT.O").await, Err(Errno::ENOENT));
            assert_eq!(
                std::fs::read(instance.write_dir().join("Out.o")).expect("write/Out.o"),
                b"second"
            );
        })
        .await;
}

/// SYMLINK makes a host symlink READLINK answers the target of; READLINK of a file is `EINVAL`;
/// UNLINK of the symlink removes the host symlink.
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_made_through_the_provider_reads_back_and_its_unlink_removes_it() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let entry = fs
                .symlink(&request(), ROOT_INO, b"latest", b"Binaries/app-1.2")
                .await
                .expect("symlink");
            assert_eq!(entry.attr.mode & libc::S_IFMT, libc::S_IFLNK);
            assert_eq!(entry.attr.size, 16);
            let mut out = [0u8; 256];
            let len = fs
                .readlink(&request(), entry.ino, &mut out)
                .await
                .expect("readlink");
            assert_eq!(&out[..len], b"Binaries/app-1.2");
            let host = instance.write_dir().join("latest");
            assert!(
                std::fs::symlink_metadata(&host)
                    .expect("write/latest")
                    .file_type()
                    .is_symlink()
            );
            let tracked = lookup(&fs, ROOT_INO, "keep.txt").await.expect("keep.txt");
            assert_eq!(
                fs.readlink(&request(), tracked, &mut out).await.err(),
                Some(Errno::EINVAL)
            );
            fs.unlink(&request(), ROOT_INO, b"latest")
                .await
                .expect("unlink");
            assert!(std::fs::symlink_metadata(&host).is_err());
            assert_eq!(lookup(&fs, ROOT_INO, "latest").await, Err(Errno::ENOENT));
        })
        .await;
}

/// STATFS of a writable provider adds `write/`'s file system to the tree's figures and reports
/// its free space and inodes, which the tree alone reports as none.
#[tokio::test(flavor = "multi_thread")]
async fn statfs_reports_the_free_space_of_the_write_file_system() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let tree = read_only_provider(&instance)
                .await
                .statfs(&request())
                .expect("statfs");
            let st = provider(&instance)
                .await
                .statfs(&request())
                .expect("statfs");
            assert!(st.bavail > 0 && st.bfree >= st.bavail, "{st:?}");
            assert!(st.ffree > 0, "{st:?}");
            assert!(
                st.blocks > tree.blocks && st.blocks > st.bfree,
                "{st:?} {tree:?}"
            );
            assert!(st.files > tree.files, "{st:?} {tree:?}");
        })
        .await;
}

/// Stages the working tree through `fs` and commits it, as `lore commit` relayed to the service
/// does, as a dry run where `dry_run`; answers the stored revision, or the error as text.
async fn commit_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    dry_run: bool,
) -> Result<Hash, String> {
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path)]),
                lore_revision::stage::StageOptions::default(),
            ),
        )
        .await
        .map_err(|error| format!("stage: {error}"))?;
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs {
            dry_run: u8::from(dry_run),
            ..Default::default()
        },
        EventDispatcher::no_dispatch(),
        "test-user".to_string(),
    ));
    LORE_CONTEXT
        .scope(
            execution,
            lore_revision::commit::commit_boxed(
                context,
                &token,
                lore_revision::commit::CommitOptions::new("commit through the mount".to_string()),
            ),
        )
        .await
        .map_err(|error| format!("commit: {error}"))
}

/// Stages the working tree through `fs`, as `lore stage` relayed to the service does; answers
/// the staged revision, or the error as text.
async fn stage_through(instance: &Instance, fs: &Arc<FuseFilesystem>) -> Result<Hash, String> {
    stage_hosts_through(instance, fs, vec![instance.fixture.path.clone()]).await
}

/// [`stage_through`] of `paths` alone, as `lore stage <paths>` names them.
async fn stage_paths_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    paths: &[&str],
) -> Result<Hash, String> {
    let hosts = paths
        .iter()
        .map(|path| instance.fixture.path.join(path))
        .collect();
    stage_hosts_through(instance, fs, hosts).await
}

async fn stage_hosts_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    hosts: Vec<PathBuf>,
) -> Result<Hash, String> {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::file::stage::stage(
                instance.command_context(fs, Some(token.share())),
                &token,
                LoreArray::from_vec(hosts.iter().map(LoreString::from).collect()),
                lore_revision::stage::StageOptions::default(),
            ),
        )
        .await
        .map_err(|error| format!("stage: {error}"))
}

/// Commits what is staged, as `lore commit` relayed to the service does, staging nothing first;
/// answers the stored revision, or the error as text.
async fn commit_staged(instance: &Instance, fs: &Arc<FuseFilesystem>) -> Result<Hash, String> {
    let token =
        lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path).await;
    commit_staged_holding(instance, fs, &token).await
}

/// [`commit_staged`] under `token`, which the caller holds.
async fn commit_staged_holding(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    token: &lore_revision::repository::RepositoryWriteToken,
) -> Result<Hash, String> {
    use lore_revision::interface::ExecutionContext;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::relay::EventDispatcher;

    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        LoreGlobalArgs::default(),
        EventDispatcher::no_dispatch(),
        "test-user".to_string(),
    ));
    LORE_CONTEXT
        .scope(
            execution,
            lore_revision::commit::commit_boxed(
                instance.command_context(fs, Some(token.share())),
                token,
                lore_revision::commit::CommitOptions::new("commit what is staged".to_string()),
            ),
        )
        .await
        .map_err(|error| format!("commit: {error}"))
}

/// Gives `write/<name>` the modified time `mtime`.
fn set_host_time(instance: &Instance, name: &str, mtime: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(instance.write_dir().join(name))
        .expect(name)
        .set_modified(mtime)
        .expect("a modified time");
}

/// Waits until the filesystem stamps later than the time the provider measures `write/<name>`
/// at, the later of its modification and its status change, so that a commit from now on
/// records it. Setting a time stamps the status change with the current tick, and a commit
/// whose clock reads that tick records no time it cannot tell apart from a later write.
async fn wait_past_host_time(instance: &Instance, fs: &Arc<FuseFilesystem>, name: &str) {
    let metadata = std::fs::metadata(instance.write_dir().join(name)).expect(name);
    let millis = |seconds: i64, nanos: i64| {
        u64::try_from(seconds).unwrap_or_default() * 1000
            + u64::try_from(nanos).unwrap_or_default() / 1_000_000
    };
    let measured = millis(metadata.mtime(), metadata.mtime_nsec())
        .max(millis(metadata.ctime(), metadata.ctime_nsec()));
    lore_revision::state::wait_until_settled(&instance.command_context(fs, None), measured).await;
}

/// The State of `revision`, loaded afresh.
async fn state_at(instance: &Instance, revision: Hash) -> Arc<lore_revision::state::State> {
    lore_revision::state::State::deserialize(instance.fixture.repository.clone(), revision)
        .await
        .expect("the revision loads")
}

/// A writable provider over the instance at `revision`, as a remount after a commit builds it.
async fn provider_at(instance: &Instance, revision: Hash) -> Arc<FuseFilesystem> {
    FuseFilesystem::new_writable(
        instance.fixture.repository.clone(),
        state_at(instance, revision).await,
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
        instance.external.path(),
    )
    .await
    .expect("writable provider")
}

/// What `revision` holds at `path`, read through a read-only provider of it; `None` where it
/// holds nothing there.
async fn revision_content(instance: &Instance, revision: Hash, path: &str) -> Option<Vec<u8>> {
    let fs = FuseFilesystem::new(
        instance.fixture.repository.clone(),
        state_at(instance, revision).await,
        AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
    )
    .await
    .expect("read-only provider");
    let mut ino = ROOT_INO;
    for name in path.split('/') {
        ino = lookup(&fs, ino, name).await.ok()?;
    }
    Some(read(&fs, ino, 1 << 16).await)
}

/// The record the provider's State holds at `path`.
async fn fs_node(instance: &Instance, fs: &FuseFilesystem, path: &str) -> Node {
    let node = if path.is_empty() {
        ROOT_NODE
    } else {
        node_at(instance, fs, path).await
    };
    fs.state()
        .node(instance.fixture.repository.clone(), node)
        .await
        .expect("a node at the path")
}

/// The record `revision` holds at `path`.
async fn committed_node(instance: &Instance, revision: Hash, path: &str) -> Node {
    let state = state_at(instance, revision).await;
    let repository = instance.fixture.repository.clone();
    let mut node = ROOT_NODE;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        node = state
            .find_subnode(repository.clone(), node, hash_string(name))
            .await
            .expect("a committed node on the path");
    }
    state
        .node(repository, node)
        .await
        .expect("a committed node")
}

fn is_local(node: &Node) -> bool {
    node.ext_flags()
        .contains(lore_revision::node::NodeExtFlags::LocalContent)
}

/// The paths a status through `fs` reports.
async fn changed_paths(instance: &Instance, fs: &Arc<FuseFilesystem>) -> Vec<String> {
    staged_state::status_files(instance.command_context(fs, None), false)
        .await
        .into_keys()
        .collect()
}

/// A hook that runs `step` with the provider, as a request racing a commit does.
fn commit_step<F>(
    fs: &Arc<FuseFilesystem>,
    step: impl Fn(Arc<FuseFilesystem>) -> F + Send + Sync + 'static,
) -> lore_revision::fs::fuse::filesystem::CommitHook
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let weak = Arc::downgrade(fs);
    Arc::new(move || {
        let fs = weak.upgrade();
        let ran = fs.map(&step);
        Box::pin(async move {
            if let Some(ran) = ran {
                ran.await;
            }
        })
    })
}

/// A committed edit is served from the stored revision: its node takes the committed address,
/// clean, its inode turns store-served, `write/` holds it no more, and status reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_file_is_served_from_the_revision_and_leaves_write() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
                .await
                .expect("write");
            assert!(instance.write_dir().join("a.txt").exists());

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert!(
                !instance.write_dir().join("a.txt").exists(),
                "write/a.txt is left behind"
            );
            let node = fs_node(&instance, &fs, "a.txt").await;
            assert_eq!(
                node.address,
                committed_node(&instance, revision, "a.txt").await.address
            );
            assert!(!node.is_dirty() && !is_local(&node), "{node:?}");
            assert!(
                !fs.inodes()
                    .get(ino)
                    .expect("the inode")
                    .kind
                    .is_local_file(),
                "the inode still reads write/"
            );
            assert_eq!(read(&fs, ino, 64).await, b"ALPHA");
            assert_eq!(fs.served().revision(), revision);
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
            assert_eq!(fs.commit_outcomes(), (1, 0));
        })
        .await;
}

/// A flip keeps its directory's listing, which still lists the file as local: the next write
/// reads the flipped record, so it copies the file up and the edit is recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_after_a_commit_flip_in_a_listed_directory_copies_up() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    plant(&instance, &[("dir/a.txt", b"edited")]);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            commit_through(&instance, &fs, false).await.expect("commit");
            assert!(
                !is_local(&fs_node(&instance, &fs, "dir/a.txt").await),
                "the commit flipped a.txt"
            );

            fs.write(&request(), ino, 0, 0, b"AFTER", 0)
                .await
                .expect("write after the flip");

            let node = fs_node(&instance, &fs, "dir/a.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/a.txt")).expect("write/dir/a.txt"),
                b"AFTERd"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["dir/a.txt"]);
        })
        .await;
}

/// A copy-up keeps its directory's listing, which still lists the file as tracked: the commit
/// reads the copied-up record, so it stores the edit rather than the old content.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_after_a_copy_up_in_a_listed_directory_stores_the_edit() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
                .await
                .expect("write");

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "dir/a.txt").await,
                Some(b"ALPHA".to_vec())
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// Every READDIRPLUS record of `dir` from a fresh stream, with the entry it carries.
async fn plus_records(fs: &FuseFilesystem, dir: u64) -> Vec<(String, lore_fuse::abi::EntryOut)> {
    use zerocopy::FromBytes as _;
    let fh = fs.opendir(&request(), dir).expect("opendir").fh;
    let mut page = vec![0u8; 64 * 1024];
    let mut writer = lore_fuse::codec::dirent::DirentWriter::new(&mut page);
    fs.readdirplus(&request(), dir, fh, 0, &mut writer)
        .await
        .expect("readdirplus");
    let used = writer.finish();
    fs.releasedir(dir, fh);
    let entry_out = std::mem::size_of::<lore_fuse::abi::EntryOut>();
    let mut records = Vec::new();
    let mut at = 0;
    while at + entry_out + 24 <= used {
        let (entry, _) =
            lore_fuse::abi::EntryOut::read_from_prefix(&page[at..]).expect("an entry record");
        at += entry_out;
        let namelen = u32::from_ne_bytes(page[at + 16..at + 20].try_into().expect("u32")) as usize;
        let name = String::from_utf8_lossy(&page[at + 24..at + 24 + namelen]).into_owned();
        records.push((name, entry));
        at += (24 + namelen).div_ceil(8) * 8;
    }
    records
}

/// A flip keeps its directory's listing, which lists the file as it stood before its copy-up:
/// READDIRPLUS reports the flipped record's size and the attributes GETATTR answers, and builds
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn readdirplus_after_a_flip_reports_the_new_size_and_attributes() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let ino = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA-AND-MORE", 0)
                .await
                .expect("write");
            commit_through(&instance, &fs, false).await.expect("commit");
            assert!(!is_local(&fs_node(&instance, &fs, "dir/a.txt").await));
            let built = fs.listings_built();

            let records = plus_records(&fs, dir).await;

            let (_, a) = records
                .iter()
                .find(|(name, _)| name == "a.txt")
                .expect("a.txt is listed");
            assert_eq!(a.nodeid, ino, "a.txt is listed with its attributes");
            assert_eq!(a.attr.size, 14, "the flipped size");
            let attr = fs.getattr(&request(), ino).await.expect("getattr").attr;
            assert_eq!(
                (a.attr.size, a.attr.mode, a.attr.mtime),
                (attr.size, attr.mode, attr.mtime)
            );
            assert_eq!(fs.listings_built(), built, "the flip cost no build");
        })
        .await;
}

/// A settle keeps its directory's listing, which still lists the file as local: an operation
/// locating the file reads the settled record and serves the committed content from the store.
#[tokio::test(flavor = "multi_thread")]
async fn located_after_a_settle_reads_the_new_address() {
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_storage::ContentSource;
    use lore_storage::content::VirtualContent;
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    plant(&instance, &[("dir/a.txt", b"edited")]);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            assert_eq!(listed(&fs, dir).await, vec!["a.txt", "b.txt"]);
            let revision = commit_through(&instance, &fs, false).await.expect("commit");
            assert!(
                !is_local(&fs_node(&instance, &fs, "dir/a.txt").await),
                "the commit settled a.txt"
            );
            let committed = committed_node(&instance, revision, "dir/a.txt").await;

            let operation = lore_revision::fs::fuse::operation::FuseOperation::new(fs.clone());
            match operation
                .content_source(&relative("dir/a.txt"))
                .await
                .expect("content source")
            {
                ContentSource::Virtual(VirtualContent::Immutable { address, size }) => {
                    assert_eq!((address, size), (committed.address, 6));
                }
                other @ ContentSource::File(_) => {
                    panic!("expected the committed address, got {other}")
                }
            }
        })
        .await;
}

/// A copy-up, a flip and a settle change records in place: the listings of their directories
/// stay cached and serve every name, and none is built again.
#[tokio::test(flavor = "multi_thread")]
async fn a_content_change_in_a_listed_directory_costs_no_build() {
    let instance = Instance::new(&[
        ("dir/a.txt", b"alpha"),
        ("dir/b.txt", b"bee"),
        ("dir/c.txt", b"sea"),
    ])
    .await;
    plant(&instance, &[("dir/c.txt", b"settled")]);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let a = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let b = lookup(&fs, dir, "b.txt").await.expect("b.txt");
            let built = fs.listings_built();

            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("copy-up");
            assert_eq!(lookup(&fs, dir, "a.txt").await, Ok(a));
            commit_through(&instance, &fs, false).await.expect("commit");
            assert!(!is_local(&fs_node(&instance, &fs, "dir/a.txt").await));
            assert!(!is_local(&fs_node(&instance, &fs, "dir/c.txt").await));

            assert_eq!(lookup(&fs, ROOT_INO, "dir").await, Ok(dir));
            assert_eq!(lookup(&fs, dir, "a.txt").await, Ok(a));
            assert_eq!(lookup(&fs, dir, "b.txt").await, Ok(b));
            let c = lookup(&fs, dir, "c.txt").await.expect("c.txt");
            assert_eq!(read(&fs, c, 64).await, b"settled");
            assert_eq!(listed(&fs, dir).await, vec!["a.txt", "b.txt", "c.txt"]);
            assert_eq!(
                plus_records(&fs, dir).await.len(),
                5,
                "., .. and three files"
            );
            assert_eq!(fs.listings_built(), built, "record changes cost no build");
        })
        .await;
}

/// A file the kernel holds open across its commit keeps its inode number and modification time,
/// reads the committed bytes through the open descriptor, and takes a later write through it.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_file_keeps_its_inode_number_mtime_and_open_descriptor() {
    use std::os::unix::fs::FileExt as _;

    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    let m = instance.mount().await;
    let path = m.root.join("a.txt");
    let (file, before) = blocking(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open a.txt");
        file.write_all_at(b"ALPHA", 0)
            .expect("write through the mount");
        let before = file.metadata().expect("fstat");
        (file, before)
    })
    .await;
    let ino = lookup(&m.fs, ROOT_INO, "a.txt").await.expect("a.txt");
    let mtime = m.fs.getattr(&request(), ino).await.expect("getattr").attr;

    commit_through(&instance, &m.fs, false)
        .await
        .expect("commit");

    assert!(!instance.write_dir().join("a.txt").exists());
    assert_eq!(lookup(&m.fs, ROOT_INO, "a.txt").await, Ok(ino));
    assert_eq!(before.ino(), ino);
    let after = m.fs.getattr(&request(), ino).await.expect("getattr").attr;
    assert_eq!(
        (after.mtime, after.mtimensec / 1_000_000),
        (mtime.mtime, mtime.mtimensec / 1_000_000),
        "the commit moved the modification time"
    );
    let (committed, rewritten) = blocking(move || {
        let mut committed = vec![0; 16];
        let n = file.read_at(&mut committed, 0).expect("read the open file");
        committed.truncate(n);
        file.write_all_at(b"Z", 0).expect("write after the commit");
        let mut rewritten = vec![0; 16];
        let n = file.read_at(&mut rewritten, 0).expect("read again");
        rewritten.truncate(n);
        (committed, rewritten)
    })
    .await;
    assert_eq!(committed, b"ALPHA");
    assert_eq!(rewritten, b"ZLPHA");
    assert_eq!(
        std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
        b"ZLPHA"
    );
    m.unmount().await;
}

/// A write landing between the commit's read and its flip leaves the file dirty under `write/`
/// with the later bytes, which status reports, while the revision holds the bytes read.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_during_a_commit_stays_dirty_and_is_recorded() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                fs.write(&request(), ino, 0, 0, b"TWO", 0)
                    .await
                    .expect("write during the commit");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "a.txt").await,
                Some(b"ONE..".to_vec())
            );
            assert_eq!(read(&fs, ino, 64).await, b"TWO..");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"TWO.."
            );
            let node = fs_node(&instance, &fs, "a.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a.txt"]);
            assert_eq!(fs.commit_outcomes(), (0, 1));
        })
        .await;
}

/// A save through a temporary renamed over the file between the commit's read and its flip is
/// kept: the new bytes stay under `write/` and are served, and status reports the file.
#[tokio::test(flavor = "multi_thread")]
async fn an_atomic_save_during_a_commit_is_kept() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                create_with(&fs, ROOT_INO, "a.txt.tmp", b"SAVED").await;
                fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                    .await
                    .expect("rename over the file");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "a.txt").await,
                Some(b"ONE..".to_vec())
            );
            let saved = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, saved, 64).await, b"SAVED");
            assert_eq!(names(&instance.write_dir()), vec!["a.txt"]);
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"SAVED"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a.txt"]);
            let remounted = provider_at(&instance, revision).await;
            assert_eq!(
                served_tree(&remounted).await,
                vec![("a.txt".to_string(), Some(b"SAVED".to_vec()))]
            );
        })
        .await;
}

/// A host file a commit's flip opens for an inode that the kernel forgets, and a save replaces,
/// before the descriptor is cached never serves the inode that takes the number next: a write
/// through that inode reaches `write/` and is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_opened_for_a_forgotten_inode_never_serves_its_successor() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            let armed = Arc::new(AtomicBool::new(false));
            fs.set_commit_hook(commit_step(&fs, {
                let armed = armed.clone();
                move |fs| {
                    let armed = armed.clone();
                    async move {
                        // Held with no descriptor cached, the inode's flip opens one.
                        fs.close_local_file(ino);
                        armed.store(true, Ordering::SeqCst);
                    }
                }
            }));
            let spawned: Spawned = Arc::default();
            let handles = spawned.clone();
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new(move |step| {
                if step != (WriteStep::DescriptorOpened { ino })
                    || !armed.swap(false, Ordering::SeqCst)
                {
                    return true;
                }
                let Some(fs) = weak.upgrade() else {
                    return true;
                };
                let saved = runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        fs.forget(ino, 1);
                        let temporary = create_with(&fs, ROOT_INO, "a.txt.tmp", b"SAVED").await;
                        fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                            .await
                            .expect("save over the file");
                        fs.forget(temporary, 1);
                    },
                ));
                // The save may wait in this worker's queue: it is handed to another worker first.
                tokio::task::block_in_place(|| {
                    while !saved.is_finished() {
                        std::thread::yield_now();
                    }
                });
                handles.lock().expect("handles").push(saved);
                true
            }));

            commit_through(&instance, &fs, false).await.expect("commit");
            assert_eq!(
                spawned.lock().expect("handles").len(),
                1,
                "the flip opened no descriptor"
            );
            settled_requests(&spawned).await;

            let successor = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(successor, ino, "the name's inode number");
            fs.write(&request(), successor, 0, 0, b"NEW", 0)
                .await
                .expect("write through the successor");
            fs.forget(successor, 1);
            let fresh = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, fresh, 64).await, b"NEWED");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"NEWED"
            );
        })
        .await;
}

/// A CREATE that takes the number of an inode the kernel forgets, inside the FORGET once the
/// inode has left the table, never gets the forgotten inode's host file, written, unlinked while
/// held and so pinned: the new file's write reaches `write/` and is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_inside_a_forget_never_takes_the_forgotten_inodes_host_file() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"OLD..", 0)
                .await
                .expect("write");
            fs.unlink(&request(), ROOT_INO, b"a.txt")
                .await
                .expect("unlink");
            let armed = Arc::new(AtomicBool::new(true));
            let created = Arc::new(AtomicU64::new(0));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let created = created.clone();
                move |step| {
                    if step != (WriteStep::Forgotten { ino })
                        || !armed.swap(false, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let created = created.clone();
                    let raced = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let new = create_with(&fs, ROOT_INO, "a.txt", b"NEW").await;
                            created.store(new, Ordering::SeqCst);
                        },
                    ));
                    // The create may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !raced.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    true
                }
            }));

            fs.forget(ino, 1);
            assert_eq!(
                created.load(Ordering::SeqCst),
                ino,
                "the create took the forgotten number inside the FORGET"
            );
            fs.forget(ino, 1);
            let fresh = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, fresh, 64).await, b"NEW");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"NEW"
            );
        })
        .await;
}

/// A host file a READ opens for a local file, before a commit's flip turns the file
/// store-served and a WRITE copies it up again, is never cached over the copy-up's: the next
/// write reaches `write/` and is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_opened_before_a_flip_and_a_copy_up_never_replaces_the_copy_ups() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            let armed = Arc::new(AtomicBool::new(false));
            let raced = Arc::new(AtomicBool::new(false));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (armed, raced, instance) = (armed.clone(), raced.clone(), instance.clone());
                move |step| {
                    if step != (WriteStep::DescriptorOpened { ino })
                        || !armed.swap(false, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let (instance, raced) = (instance.clone(), raced.clone());
                    let racing = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            commit_through(&instance, &fs, false).await.expect("commit");
                            let flipped = fs
                                .inodes()
                                .get(ino)
                                .is_some_and(|entry| !entry.kind.is_local_file());
                            fs.write(&request(), ino, 0, 0, b"TWO", 0)
                                .await
                                .expect("write through a copy-up");
                            let copied_up = fs
                                .inodes()
                                .get(ino)
                                .is_some_and(|entry| entry.kind.is_local_file());
                            raced.store(flipped && copied_up, Ordering::SeqCst);
                        },
                    ));
                    // The race may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !racing.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    true
                }
            }));

            // With no descriptor cached, the READ opens one.
            fs.close_local_file(ino);
            armed.store(true, Ordering::SeqCst);
            let served = read(&fs, ino, 64).await;
            assert!(
                raced.load(Ordering::SeqCst),
                "inside the READ, the commit flipped the file and the write copied it up"
            );
            assert_eq!(
                served, b"TWO..",
                "the READ served the host file it opened before the flip"
            );
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write after the race");
            fs.forget(ino, 1);
            let fresh = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert_eq!(read(&fs, fresh, 64).await, b"NEW..");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"NEW.."
            );
        })
        .await;
}

/// Renames directory `from` in the root to `to` through `fs` while a CREATE of `new.txt` inside
/// it races the rename on another thread: the CREATE starts once the rename has moved the host
/// directory, and the rename goes on to respell the inode once the CREATE's host entry exists,
/// or, where `after_gather`, the CREATE builds its host path then and makes its entry only once
/// the rename has gathered, before the rename stops counting as under way.
async fn rename_racing_a_create(
    fs: &Arc<FuseFilesystem>,
    dir: u64,
    (from, to): (&str, &str),
    after_gather: bool,
) -> usize {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(10);
    let armed = Arc::new(AtomicBool::new(true));
    let built_armed = AtomicBool::new(after_gather);
    // Every handshake that timed out, so the window did not run as the test means it to.
    let missed: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let built = Arc::new(AtomicUsize::new(0));
    let created: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
    let (made_tx, made_rx) = mpsc::channel::<()>();
    let (built_tx, built_rx) = mpsc::channel::<()>();
    let (gathered_tx, gathered_rx) = mpsc::channel::<()>();
    let (made_tx, made_rx) = (Mutex::new(made_tx), Mutex::new(made_rx));
    let (built_tx, built_rx) = (Mutex::new(built_tx), Mutex::new(built_rx));
    let (gathered_tx, gathered_rx) = (Mutex::new(gathered_tx), Mutex::new(gathered_rx));
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let (armed, created, missed, built) = (
            armed.clone(),
            created.clone(),
            missed.clone(),
            built.clone(),
        );
        let wait = move |rx: &Mutex<mpsc::Receiver<()>>, what: &'static str| {
            tokio::task::block_in_place(|| {
                if rx.lock().expect(what).recv_timeout(WAIT).is_err() {
                    missed.lock().expect("missed").push(what);
                }
            });
        };
        move |step| {
            match step {
                WriteStep::Rename if armed.swap(false, Ordering::SeqCst) => {
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let create = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let made = create_with(&fs, dir, "new.txt", b"new").await;
                            fs.forget(made, 1);
                        },
                    ));
                    *created.lock().expect("created") = Some(create);
                    // Held until the create's host entry exists under the old name, or, where
                    // `after_gather`, until it has built its host path from the old names.
                    if after_gather {
                        wait(&built_rx, "built");
                    } else {
                        wait(&made_rx, "made");
                    }
                }
                WriteStep::HostPathBuilt { parent } if parent == dir => {
                    built.fetch_add(1, Ordering::SeqCst);
                    if built_armed.swap(false, Ordering::SeqCst) {
                        let _ = built_tx.lock().expect("built").send(());
                        wait(&gathered_rx, "gathered");
                    }
                }
                WriteStep::HostEntryMade { parent } if parent == dir => {
                    let _ = made_tx.lock().expect("made").send(());
                }
                WriteStep::Gathered if after_gather => {
                    let _ = gathered_tx.lock().expect("gathered").send(());
                    wait(&made_rx, "made after the gather");
                }
                _ => {}
            }
            true
        }
    }));
    fs.rename(
        &request(),
        ROOT_INO,
        from.as_bytes(),
        ROOT_INO,
        to.as_bytes(),
        0,
    )
    .await
    .expect("the rename");
    assert!(!armed.load(Ordering::SeqCst), "the create raced the rename");
    let create = created.lock().expect("created").take().expect("the create");
    create.await.expect("the create");
    assert_eq!(
        *missed.lock().expect("missed"),
        Vec::<&str>::new(),
        "every handshake of the window completed"
    );
    built.load(Ordering::SeqCst)
}

/// A CREATE inside a tracked directory, landing while a RENAME changes only the directory's case,
/// keeps its file below the new spelling: after a remount the file is there, and `write/` holds
/// no directory under the old spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_a_case_rename_of_its_tracked_directory_keeps_its_file() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            rename_racing_a_create(&fs, dir, ("dir", "Dir"), false).await;
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/new.txt").await, b"new");
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"alpha");
            assert!(
                !instance.write_dir().join("dir").exists(),
                "write/dir came back"
            );
        })
        .await;
}

/// A CREATE inside a local directory, landing while a RENAME moves the directory, keeps its file
/// below the new name: after a remount the file is there, and the old name does not come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_a_rename_of_its_local_directory_keeps_its_file() {
    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            rename_racing_a_create(&fs, made, ("made", "moved"), false).await;
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt", "moved"]);
            assert_eq!(served_at(&fs, "moved/new.txt").await, b"new");
            assert!(
                !instance.write_dir().join("made").exists(),
                "write/made came back"
            );
        })
        .await;
}

/// A CREATE inside a tracked directory that built its host path before a case-only RENAME
/// respelled the directory, and makes its host entry only after the rename gathered what stood
/// under the old name, before the rename stops counting as under way, lands below the new
/// spelling: the old host directory is gone, so the create builds its path again. After a remount
/// the file is there, and `write/` holds no directory under the old spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_after_a_renames_gather_rebuilds_its_path() {
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let built = rename_racing_a_create(&fs, dir, ("dir", "Dir"), true).await;
            assert_eq!(built, 2, "the create built its host path again");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/new.txt").await, b"new");
            assert!(
                !instance.write_dir().join("dir").exists(),
                "write/dir came back"
            );
        })
        .await;
}

/// A CREATE inside a tracked directory with no host directory yet makes that directory from the
/// inode names: here it built the path before a case-only RENAME respelled the directory and made
/// it only once the rename had gathered, so the old host directory stands again and the create's
/// entry lands in it. The create moves the entry below the new spelling once the rename ends and
/// removes the directory it made: after a remount the file is there, and `write/` holds no
/// directory under the old spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_made_under_the_old_host_path_after_a_renames_gather_moves_its_entry() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(10);
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let write_dir = instance.write_dir();
            let armed = AtomicBool::new(true);
            let built_armed = AtomicBool::new(true);
            let made_under_old = Arc::new(AtomicBool::new(false));
            let created: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
            let (built_tx, built_rx) = mpsc::channel::<()>();
            let (gathered_tx, gathered_rx) = mpsc::channel::<()>();
            let (made_tx, made_rx) = mpsc::channel::<()>();
            let (built_tx, built_rx) = (Mutex::new(built_tx), Mutex::new(built_rx));
            let (gathered_tx, gathered_rx) = (Mutex::new(gathered_tx), Mutex::new(gathered_rx));
            let (made_tx, made_rx) = (Mutex::new(made_tx), Mutex::new(made_rx));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (created, made_under_old, write_dir) =
                    (created.clone(), made_under_old.clone(), write_dir.clone());
                move |step| {
                    match step {
                        WriteStep::Rename if armed.swap(false, Ordering::SeqCst) => {
                            let Some(fs) = weak.upgrade() else {
                                return true;
                            };
                            let create = runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let made = create_with(&fs, dir, "new.txt", b"new").await;
                                    fs.forget(made, 1);
                                },
                            ));
                            *created.lock().expect("created") = Some(create);
                            tokio::task::block_in_place(|| {
                                built_rx
                                    .lock()
                                    .expect("built")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        // The create's missing host directory, named before the respell.
                        WriteStep::HostDirBuilt { dir: built }
                            if built == dir && built_armed.swap(false, Ordering::SeqCst) =>
                        {
                            let _ = built_tx.lock().expect("built").send(());
                            tokio::task::block_in_place(|| {
                                gathered_rx
                                    .lock()
                                    .expect("gathered")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        WriteStep::HostEntryMade { parent } if parent == dir => {
                            made_under_old
                                .store(write_dir.join("dir/new.txt").exists(), Ordering::SeqCst);
                            let _ = made_tx.lock().expect("made").send(());
                        }
                        WriteStep::Gathered => {
                            let _ = gathered_tx.lock().expect("gathered").send(());
                            tokio::task::block_in_place(|| {
                                made_rx
                                    .lock()
                                    .expect("made")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        _ => {}
                    }
                    true
                }
            }));
            fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"Dir", 0)
                .await
                .expect("the rename");
            let create = created.lock().expect("created").take().expect("the create");
            create.await.expect("the create");
            assert!(
                made_under_old.load(Ordering::SeqCst),
                "the entry was made under the old host path"
            );
            assert!(!write_dir.join("dir").exists(), "write/dir stays");
            assert!(write_dir.join("Dir/new.txt").exists(), "write/Dir/new.txt");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/new.txt").await, b"new");
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"alpha");
        })
        .await;
}

/// A CREATE racing a RENAME of its local directory, whose entry lands in a directory the user made
/// under the old name once the rename had gathered, moves the entry below the new name and leaves
/// the user's directory standing: the create made no host directory, so it removes none.
#[tokio::test(flavor = "multi_thread")]
async fn settling_a_create_keeps_a_directory_made_under_the_old_name_meanwhile() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(10);
    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"made", 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let write_dir = instance.write_dir();
            let made_under_old = Arc::new(AtomicBool::new(false));
            let armed = AtomicBool::new(true);
            let built_armed = AtomicBool::new(true);
            let created: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
            let (built_tx, built_rx) = mpsc::channel::<()>();
            let (remade_tx, remade_rx) = mpsc::channel::<()>();
            let (made_tx, made_rx) = mpsc::channel::<()>();
            let (built_tx, built_rx) = (Mutex::new(built_tx), Mutex::new(built_rx));
            let remade_rx = Mutex::new(remade_rx);
            let (made_tx, made_rx) = (Mutex::new(made_tx), Mutex::new(made_rx));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (created, made_under_old, write_dir) =
                    (created.clone(), made_under_old.clone(), write_dir.clone());
                move |step| {
                    match step {
                        WriteStep::Rename if armed.swap(false, Ordering::SeqCst) => {
                            let Some(fs) = weak.upgrade() else {
                                return true;
                            };
                            let create = runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let new = create_with(&fs, made, "new.txt", b"new").await;
                                    fs.forget(new, 1);
                                },
                            ));
                            *created.lock().expect("created") = Some(create);
                            tokio::task::block_in_place(|| {
                                built_rx
                                    .lock()
                                    .expect("built")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        WriteStep::HostPathBuilt { parent }
                            if parent == made && built_armed.swap(false, Ordering::SeqCst) =>
                        {
                            let _ = built_tx.lock().expect("built").send(());
                            tokio::task::block_in_place(|| {
                                remade_rx
                                    .lock()
                                    .expect("remade")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        WriteStep::HostEntryMade { parent } if parent == made => {
                            made_under_old
                                .store(write_dir.join("made/new.txt").exists(), Ordering::SeqCst);
                            let _ = made_tx.lock().expect("made").send(());
                        }
                        // The user makes a directory under the old name.
                        WriteStep::Gathered => {
                            let Some(fs) = weak.upgrade() else {
                                return true;
                            };
                            let remade_tx = remade_tx.clone();
                            let _mkdir = runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let remade = fs
                                        .mkdir(&request(), ROOT_INO, b"made", 0o755, 0o022)
                                        .await
                                        .expect("mkdir under the old name");
                                    fs.forget(remade.ino, 1);
                                    let _ = remade_tx.send(());
                                },
                            ));
                            tokio::task::block_in_place(|| {
                                made_rx
                                    .lock()
                                    .expect("made")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        _ => {}
                    }
                    true
                }
            }));
            fs.rename(&request(), ROOT_INO, b"made", ROOT_INO, b"moved", 0)
                .await
                .expect("the rename");
            let create = created.lock().expect("created").take().expect("the create");
            create.await.expect("the create");
            assert!(
                made_under_old.load(Ordering::SeqCst),
                "the entry was made in the directory made under the old name"
            );
            assert!(
                write_dir.join("made").is_dir(),
                "the directory made under the old name stays"
            );
            assert!(
                write_dir.join("moved/new.txt").exists(),
                "write/moved/new.txt"
            );
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(
                listed(&fs, ROOT_INO).await,
                vec!["keep.txt", "made", "moved"]
            );
            assert_eq!(served_at(&fs, "moved/new.txt").await, b"new");
            let remade = lookup(&fs, ROOT_INO, "made").await.expect("made");
            assert_eq!(listed(&fs, remade).await, Vec::<String>::new());
        })
        .await;
}

/// A CREATE whose settle gives up waiting on a RENAME of a directory above, held before it
/// changes the State, leaves its entry under the old host path, in a directory that also stands
/// under the new one: the rename's gather merges the two and moves it. After a remount the file
/// is there and the old name does not come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_whose_settle_gave_up_is_moved_by_the_renames_gather() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"dir", 0o755, 0o022)
                .await
                .expect("mkdir dir")
                .ino;
            let sub = fs
                .mkdir(&request(), dir, b"sub", 0o755, 0o022)
                .await
                .expect("mkdir sub")
                .ino;
            let local = create_with(&fs, sub, "local.txt", b"local").await;
            fs.forget(local, 1);
            let write_dir = instance.write_dir();
            assert!(write_dir.join("dir/sub").is_dir(), "write/dir/sub stands");
            let armed = AtomicBool::new(true);
            let made_under_old = Arc::new(AtomicBool::new(false));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (made_under_old, write_dir) = (made_under_old.clone(), write_dir.clone());
                move |step| {
                    if step == WriteStep::Rename && armed.swap(false, Ordering::SeqCst) {
                        let Some(fs) = weak.upgrade() else {
                            return true;
                        };
                        let (done_tx, done_rx) = mpsc::channel();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let made = create_with(&fs, sub, "new.txt", b"new").await;
                                fs.forget(made, 1);
                                let _ = done_tx.send(());
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            done_rx
                                .recv_timeout(Duration::from_secs(10))
                                .expect("the create ends while the rename is held");
                        });
                        made_under_old
                            .store(write_dir.join("dir/sub/new.txt").exists(), Ordering::SeqCst);
                    }
                    true
                }
            }));
            fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"moved", 0)
                .await
                .expect("the rename");
            assert!(
                made_under_old.load(Ordering::SeqCst),
                "the create's settle gave up and left its entry under the old host path"
            );
            assert!(
                write_dir.join("moved/sub/new.txt").exists(),
                "write/moved/sub/new.txt"
            );
            assert!(!write_dir.join("dir").exists(), "write/dir is gone");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(served_at(&fs, "moved/sub/new.txt").await, b"new");
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt", "moved"]);
        })
        .await;
}

/// Two RENAMEs of one directory can overlap, as an instance rename takes no kernel lock: a CREATE
/// inside the directory that builds its host path while both are under way, before either
/// respelled the inode, and makes its entry once one has gathered, sees a rename under way and
/// builds its path again rather than answering `ENOENT`. After a remount the file is there.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_two_overlapping_renames_of_its_directory_keeps_its_file() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(10);
    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let renames = AtomicUsize::new(0);
            let built_armed = AtomicBool::new(true);
            let gathered_armed = AtomicBool::new(true);
            type Task<T> = Arc<Mutex<Option<tokio::task::JoinHandle<T>>>>;
            let second: Task<Result<(), Errno>> = Arc::default();
            let created: Task<Result<(), Errno>> = Arc::default();
            let (second_tx, second_rx) = mpsc::channel::<()>();
            let (built_tx, built_rx) = mpsc::channel::<()>();
            let (gathered_tx, gathered_rx) = mpsc::channel::<()>();
            let (made_tx, made_rx) = mpsc::channel::<()>();
            let second_rx = Mutex::new(second_rx);
            let (built_tx, built_rx) = (Mutex::new(built_tx), Mutex::new(built_rx));
            let (gathered_tx, gathered_rx) = (Mutex::new(gathered_tx), Mutex::new(gathered_rx));
            let (made_tx, made_rx) = (Mutex::new(made_tx), Mutex::new(made_rx));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (second, created) = (second.clone(), created.clone());
                move |step| {
                    match step {
                        WriteStep::Rename => {
                            let Some(fs) = weak.upgrade() else {
                                return true;
                            };
                            match renames.fetch_add(1, Ordering::SeqCst) {
                                // The first rename, its host directory moved, holds until the
                                // second has ended.
                                0 => {
                                    let second_tx = second_tx.clone();
                                    let renamed = runtime.spawn(LORE_CONTEXT.scope(
                                        fuse_fixture::setup_test_execution(),
                                        async move {
                                            let renamed = fs
                                                .rename(
                                                    &request(),
                                                    ROOT_INO,
                                                    b"dir",
                                                    ROOT_INO,
                                                    b"Dir",
                                                    0,
                                                )
                                                .await;
                                            let _ = second_tx.send(());
                                            renamed
                                        },
                                    ));
                                    *second.lock().expect("second") = Some(renamed);
                                    tokio::task::block_in_place(|| {
                                        second_rx
                                            .lock()
                                            .expect("second")
                                            .recv_timeout(WAIT)
                                            .expect("the second rename finished");
                                    });
                                }
                                // The second, before it respells the inode, starts the create.
                                1 => {
                                    let made_tx = made_tx.lock().expect("made").clone();
                                    let create = runtime.spawn(LORE_CONTEXT.scope(
                                        fuse_fixture::setup_test_execution(),
                                        async move {
                                            let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
                                            let made = fs
                                                .create(
                                                    &request(),
                                                    dir,
                                                    b"new.txt",
                                                    libc::S_IFREG | 0o644,
                                                    0o022,
                                                    flags,
                                                )
                                                .await;
                                            let _ = made_tx.send(());
                                            let made = made?.entry.ino;
                                            fs.write(&request(), made, 0, 0, b"new", 0).await?;
                                            fs.forget(made, 1);
                                            Ok(())
                                        },
                                    ));
                                    *created.lock().expect("created") = Some(create);
                                    tokio::task::block_in_place(|| {
                                        built_rx
                                            .lock()
                                            .expect("built")
                                            .recv_timeout(WAIT)
                                            .expect("a handshake of the window");
                                    });
                                }
                                _ => {}
                            }
                        }
                        WriteStep::HostPathBuilt { parent }
                            if parent == dir && built_armed.swap(false, Ordering::SeqCst) =>
                        {
                            let _ = built_tx.lock().expect("built").send(());
                            tokio::task::block_in_place(|| {
                                gathered_rx
                                    .lock()
                                    .expect("gathered")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        WriteStep::HostEntryMade { parent } if parent == dir => {
                            let _ = made_tx.lock().expect("made").send(());
                        }
                        // The second rename, gathered, holds until the create's entry exists.
                        WriteStep::Gathered if gathered_armed.swap(false, Ordering::SeqCst) => {
                            let _ = gathered_tx.lock().expect("gathered").send(());
                            tokio::task::block_in_place(|| {
                                made_rx
                                    .lock()
                                    .expect("made")
                                    .recv_timeout(WAIT)
                                    .expect("a handshake of the window");
                            });
                        }
                        _ => {}
                    }
                    true
                }
            }));
            fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"Dir", 0)
                .await
                .expect("the first rename");
            let renamed = second
                .lock()
                .expect("second")
                .take()
                .expect("the second rename");
            renamed
                .await
                .expect("the second rename")
                .expect("the second rename");
            let create = created.lock().expect("created").take().expect("the create");
            assert_eq!(create.await.expect("the create"), Ok(()));
            assert!(
                instance.write_dir().join("Dir/new.txt").exists(),
                "write/Dir/new.txt"
            );
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/new.txt").await, b"new");
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"alpha");
        })
        .await;
}

/// A RENAME into a directory whose host path it built just before a case-only RENAME of the
/// directory moved and respelled it builds the path again rather than answering `ENOENT`: the file
/// lands below the new spelling, and is there after a remount.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_into_a_directory_being_renamed_lands_below_its_new_name() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(10);
    let instance = Instance::new(&[("dir/a.txt", b"alpha"), ("other/f.txt", b"eff")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let other = lookup(&fs, ROOT_INO, "other").await.expect("other");
            let armed = AtomicBool::new(true);
            let renamed: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
            let (renamed_tx, renamed_rx) = mpsc::channel::<()>();
            let renamed_rx = Mutex::new(renamed_rx);
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let renamed = renamed.clone();
                move |step| {
                    if let WriteStep::RenameTargetBuilt { dir: target } = step
                        && target == dir
                        && armed.swap(false, Ordering::SeqCst)
                    {
                        let Some(fs) = weak.upgrade() else {
                            return true;
                        };
                        let renamed_tx = renamed_tx.clone();
                        let rename = runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"Dir", 0)
                                    .await
                                    .expect("the directory's rename");
                                let _ = renamed_tx.send(());
                            },
                        ));
                        *renamed.lock().expect("renamed") = Some(rename);
                        tokio::task::block_in_place(|| {
                            renamed_rx
                                .lock()
                                .expect("renamed")
                                .recv_timeout(WAIT)
                                .expect("a handshake of the window");
                        });
                    }
                    true
                }
            }));
            let moved = fs
                .rename(&request(), other, b"f.txt", dir, b"f.txt", 0)
                .await;
            let rename = renamed
                .lock()
                .expect("renamed")
                .take()
                .expect("the directory's rename");
            rename.await.expect("the directory's rename");
            assert_eq!(moved, Ok(()), "the rename into the directory");
            let write_dir = instance.write_dir();
            assert!(write_dir.join("Dir/f.txt").exists(), "write/Dir/f.txt");
            assert!(!write_dir.join("dir").exists(), "write/dir came back");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir", "other"]);
            assert_eq!(served_at(&fs, "Dir/f.txt").await, b"eff");
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"alpha");
            let other = lookup(&fs, ROOT_INO, "other").await.expect("other");
            assert_eq!(listed(&fs, other).await, Vec::<String>::new());
        })
        .await;
}

/// Races at the resolutions of an inode's host path for its descriptor
/// ([`race_path_resolutions`]).
#[derive(Default)]
struct PathRaces {
    /// Resolutions still to race; each takes one.
    remaining: std::sync::atomic::AtomicUsize,
    /// Races run to their end.
    ran: std::sync::atomic::AtomicUsize,
    /// Resolutions seen, raced or not.
    resolved: std::sync::atomic::AtomicUsize,
}

/// At each resolution of inode `ino`'s host path for its descriptor, before the file at it is
/// opened, runs `race` to its end while [`PathRaces::remaining`] allows, handed the number of
/// races run before.
fn race_path_resolutions<F, Fut>(fs: &Arc<FuseFilesystem>, ino: u64, race: F) -> Arc<PathRaces>
where
    F: Fn(Arc<FuseFilesystem>, usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let races = Arc::new(PathRaces::default());
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let races = races.clone();
        move |step| {
            if step != (WriteStep::PathResolved { ino }) {
                return true;
            }
            races.resolved.fetch_add(1, Ordering::SeqCst);
            let armed = races
                .remaining
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            let Some(fs) = weak.upgrade().filter(|_| armed) else {
                return true;
            };
            let racing = runtime.spawn(LORE_CONTEXT.scope(
                fuse_fixture::setup_test_execution(),
                race(fs, races.ran.load(Ordering::SeqCst)),
            ));
            // The race may wait in this worker's queue: it is handed to another worker first.
            if tokio::task::block_in_place(|| runtime.block_on(racing)).is_ok() {
                races.ran.fetch_add(1, Ordering::SeqCst);
            }
            true
        }
    }));
    races
}

/// `write/<path>`'s bytes.
fn host_bytes(instance: &Instance, path: &str) -> Vec<u8> {
    std::fs::read(instance.write_dir().join(path)).expect(path)
}

/// A READ of a copied-up file whose host path resolves before a RENAME of the file away and a
/// CREATE at its old name serves the file's own bytes, and the next write lands in its own host
/// file: the file at the old path is another file's.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_resolved_across_a_rename_and_a_create_never_takes_the_new_file() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"AAAAA", 0)
                .await
                .expect("write");
            let races = race_path_resolutions(&fs, ino, |fs, _| async move {
                fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"b.txt", 0)
                    .await
                    .expect("rename away");
                let other = create_with(&fs, ROOT_INO, "a.txt", b"OTHER").await;
                fs.forget(other, 1);
            });

            // With no descriptor cached, as after an eviction, the READ opens one.
            fs.close_local_file(ino);
            races.remaining.store(1, Ordering::SeqCst);
            assert_eq!(read(&fs, ino, 64).await, b"AAAAA");
            assert_eq!(races.ran.load(Ordering::SeqCst), 1, "the race ran");
            assert_eq!(
                races.resolved.load(Ordering::SeqCst),
                2,
                "the race cost one attempt more"
            );
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write through the renamed file");
            assert_eq!(host_bytes(&instance, "b.txt"), b"NEWAA");
            assert_eq!(host_bytes(&instance, "a.txt"), b"OTHER");
        })
        .await;
}

/// The same with the parent directory renamed away and made again, holding a file of the same
/// name, between the resolution of a created file's host path and the open.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_resolved_across_a_parent_rename_never_takes_the_new_file() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = |fs: Arc<FuseFilesystem>| async move {
                fs.mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir d")
                    .ino
            };
            let dir = mkdir(fs.clone()).await;
            let ino = create_with(&fs, dir, "a.txt", b"AAAAA").await;
            let races = race_path_resolutions(&fs, ino, move |fs, _| async move {
                fs.rename(&request(), ROOT_INO, b"d", ROOT_INO, b"e", 0)
                    .await
                    .expect("rename the directory away");
                let again = mkdir(fs.clone()).await;
                let other = create_with(&fs, again, "a.txt", b"OTHER").await;
                fs.forget(other, 1);
                fs.forget(again, 1);
            });

            fs.close_local_file(ino);
            races.remaining.store(1, Ordering::SeqCst);
            assert_eq!(read(&fs, ino, 64).await, b"AAAAA");
            assert_eq!(races.ran.load(Ordering::SeqCst), 1, "the race ran");
            assert_eq!(
                races.resolved.load(Ordering::SeqCst),
                2,
                "the race cost one attempt more"
            );
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write through the moved file");
            assert_eq!(host_bytes(&instance, "e/a.txt"), b"NEWAA");
            assert_eq!(host_bytes(&instance, "d/a.txt"), b"OTHER");
        })
        .await;
}

/// A created file's descriptor opened again after an eviction, across a RENAME of the file away
/// and a CREATE at its old name, is the file's own: its identity is recorded at CREATE as well
/// as at copy-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_files_descriptor_after_an_eviction_is_its_own() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = create_with(&fs, ROOT_INO, "c.txt", b"CCCCC").await;
            let races = race_path_resolutions(&fs, ino, |fs, _| async move {
                fs.rename(&request(), ROOT_INO, b"c.txt", ROOT_INO, b"d.txt", 0)
                    .await
                    .expect("rename away");
                let other = create_with(&fs, ROOT_INO, "c.txt", b"OTHER").await;
                fs.forget(other, 1);
            });

            fs.close_local_file(ino);
            races.remaining.store(1, Ordering::SeqCst);
            assert_eq!(read(&fs, ino, 64).await, b"CCCCC");
            assert_eq!(races.ran.load(Ordering::SeqCst), 1, "the race ran");
            assert_eq!(
                races.resolved.load(Ordering::SeqCst),
                2,
                "the race cost one attempt more"
            );
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write through the renamed file");
            assert_eq!(host_bytes(&instance, "d.txt"), b"NEWCC");
            assert_eq!(host_bytes(&instance, "c.txt"), b"OTHER");
        })
        .await;
}

/// A local file's inode made by a LOOKUP, whose host file nothing has identified yet, takes its
/// own host file as its first descriptor across a RENAME of the file away and a CREATE at its old
/// name.
#[tokio::test(flavor = "multi_thread")]
async fn a_looked_up_local_files_first_descriptor_is_its_own() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let created = create_with(&fs, ROOT_INO, "l.txt", b"LLLLL").await;
            fs.forget(created, 1);
            let races = race_path_resolutions(&fs, created, |fs, _| async move {
                fs.rename(&request(), ROOT_INO, b"l.txt", ROOT_INO, b"m.txt", 0)
                    .await
                    .expect("rename away");
                let other = create_with(&fs, ROOT_INO, "l.txt", b"OTHER").await;
                fs.forget(other, 1);
            });

            races.remaining.store(1, Ordering::SeqCst);
            let ino = lookup(&fs, ROOT_INO, "l.txt").await.expect("l.txt");
            assert_eq!(ino, created, "the name's inode number");
            assert_eq!(read(&fs, ino, 64).await, b"LLLLL");
            assert_eq!(races.ran.load(Ordering::SeqCst), 1, "the race ran");
            assert_eq!(
                races.resolved.load(Ordering::SeqCst),
                2,
                "the race cost one attempt more"
            );
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write through the renamed file");
            assert_eq!(host_bytes(&instance, "m.txt"), b"NEWLL");
            assert_eq!(host_bytes(&instance, "l.txt"), b"OTHER");
        })
        .await;
}

/// A descriptor whose path reaches another file at every attempt answers `EIO` after its
/// attempts, caching nothing; once the renames stop, the file is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_reaching_another_file_at_every_attempt_answers_eio() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = create_with(&fs, ROOT_INO, "f0.txt", b"FFFFF").await;
            let races = race_path_resolutions(&fs, ino, |fs, raced| async move {
                let (from, to) = (format!("f{raced}.txt"), format!("f{}.txt", raced + 1));
                fs.rename(
                    &request(),
                    ROOT_INO,
                    from.as_bytes(),
                    ROOT_INO,
                    to.as_bytes(),
                    0,
                )
                .await
                .expect("rename away");
                let other = create_with(&fs, ROOT_INO, &from, b"OTHER").await;
                fs.forget(other, 1);
            });

            fs.close_local_file(ino);
            races.remaining.store(usize::MAX, Ordering::SeqCst);
            let answered = fs.descriptor(ino).map(|_| ());
            let attempts = races.resolved.load(Ordering::SeqCst);
            println!("descriptor attempts before it answered {answered:?}: {attempts}");
            assert_eq!(answered, Err(Errno::EIO));
            // `DESCRIPTOR_ATTEMPTS`.
            assert_eq!(attempts, 8, "attempts before EIO");
            assert_eq!(
                attempts,
                races.ran.load(Ordering::SeqCst),
                "every attempt raced"
            );
            races.remaining.store(0, Ordering::SeqCst);
            assert_eq!(read(&fs, ino, 64).await, b"FFFFF");
        })
        .await;
}

/// Runs `race(fs, step)` to its end at inode `ino`'s descriptor steps, once each in order: step 0
/// at the path's build, before the open, and step 1 at the open, before the entry guard. Answers
/// the steps run.
fn race_descriptor_steps<F, Fut>(
    fs: &Arc<FuseFilesystem>,
    ino: u64,
    race: F,
) -> Arc<std::sync::atomic::AtomicUsize>
where
    F: Fn(Arc<FuseFilesystem>, usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let ran = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let ran = ran.clone();
        move |step| {
            let next = match step {
                WriteStep::PathResolved { ino: at } if at == ino => 0,
                WriteStep::DescriptorOpened { ino: at } if at == ino => 1,
                _ => return true,
            };
            // Taken before the race runs, so a descriptor the race opens itself races nothing.
            if ran
                .compare_exchange(next, next + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return true;
            }
            let Some(fs) = weak.upgrade() else {
                return true;
            };
            let racing = runtime
                .spawn(LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), race(fs, next)));
            // The race may wait in this worker's queue: it is handed to another worker first.
            tokio::task::block_in_place(|| {
                while !racing.is_finished() {
                    std::thread::yield_now();
                }
            });
            true
        }
    }));
    ran
}

/// A looked-up local file's first descriptor, whose path is renamed away with a CREATE at the old
/// name before the open and renamed back over the new file after it, is its own: the path built
/// again names it, but the file opened is the new one's, which the file at the path is not.
#[tokio::test(flavor = "multi_thread")]
async fn an_identification_across_a_rename_away_and_back_takes_the_files_own() {
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let created = create_with(&fs, ROOT_INO, "l.txt", b"LLLLL").await;
            fs.forget(created, 1);
            let ran = race_descriptor_steps(&fs, created, |fs, step| async move {
                if step == 0 {
                    fs.rename(&request(), ROOT_INO, b"l.txt", ROOT_INO, b"m.txt", 0)
                        .await
                        .expect("rename away");
                    let other = create_with(&fs, ROOT_INO, "l.txt", b"OTHER").await;
                    fs.forget(other, 1);
                } else {
                    fs.rename(&request(), ROOT_INO, b"m.txt", ROOT_INO, b"l.txt", 0)
                        .await
                        .expect("rename back over the new file");
                }
            });

            let ino = lookup(&fs, ROOT_INO, "l.txt").await.expect("l.txt");
            assert_eq!(ino, created, "the name's inode number");
            assert_eq!(ran.load(Ordering::SeqCst), 2, "both races ran");
            assert_eq!(read(&fs, ino, 64).await, b"LLLLL");
            fs.write(&request(), ino, 0, 0, b"NEW", 0)
                .await
                .expect("write through the file");
            assert_eq!(host_bytes(&instance, "l.txt"), b"NEWLL");
            fs.close_local_file(ino);
            assert_eq!(read(&fs, ino, 64).await, b"NEWLL", "after an eviction");
        })
        .await;
}

/// A READ whose path reaches another file, a RENAME of the file away and a CREATE at its old name
/// landing before the open, while a commit turns the file store-served between the open and the
/// entry guard, never serves the other file: it serves the committed bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptor_whose_file_flips_inside_the_open_never_serves_another_file() {
    use std::sync::atomic::Ordering;

    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"AAAAA", 0)
                .await
                .expect("write");
            let ran = race_descriptor_steps(&fs, ino, {
                let instance = instance.clone();
                move |fs, step| {
                    let instance = instance.clone();
                    async move {
                        if step == 0 {
                            fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"b.txt", 0)
                                .await
                                .expect("rename away");
                            let other = create_with(&fs, ROOT_INO, "a.txt", b"OTHER").await;
                            fs.forget(other, 1);
                        } else {
                            commit_through(&instance, &fs, false).await.expect("commit");
                        }
                    }
                }
            });

            // With no descriptor cached, as after an eviction, the READ opens one.
            fs.close_local_file(ino);
            let served = read(&fs, ino, 64).await;
            assert_eq!(ran.load(Ordering::SeqCst), 2, "both races ran");
            assert!(
                fs.inodes()
                    .get(ino)
                    .is_some_and(|entry| !entry.kind.is_local_file()),
                "the commit turned the file store-served"
            );
            assert_eq!(served, b"AAAAA");
        })
        .await;
}

/// A commit read a file no inode held. After its flip has checked the writes forgotten since,
/// and before it looks for the inode now at the name, a write lands through an inode the kernel
/// then forgets, and a LOOKUP makes another inode, holding no write. The flip never turns the
/// file store-served with the content read before that write: the file serves the write, `write/`
/// holds it, and it stays dirty for the next commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_forgotten_between_the_flips_check_and_the_flip_is_never_reverted() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let first = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), first, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            // No inode holds the file when the commit reads it.
            fs.forget(first, 1);
            let armed = Arc::new(AtomicBool::new(true));
            let reader = Arc::new(AtomicU64::new(0));
            let writer = Arc::new(AtomicU64::new(0));
            let reader_generation = Arc::new(AtomicU64::new(u64::MAX));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (armed, reader, writer, reader_generation) = (
                    armed.clone(),
                    reader.clone(),
                    writer.clone(),
                    reader_generation.clone(),
                );
                move |step| {
                    if !matches!(step, WriteStep::FlipChecked { .. })
                        || !armed.swap(false, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let (reader, writer, reader_generation) =
                        (reader.clone(), writer.clone(), reader_generation.clone());
                    let racing = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let wrote = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
                            fs.write(&request(), wrote, 0, 0, b"TWO", 0)
                                .await
                                .expect("a write inside the flip");
                            fs.forget(wrote, 1);
                            writer.store(wrote, Ordering::SeqCst);
                            let looked = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
                            reader.store(looked, Ordering::SeqCst);
                            reader_generation.store(
                                fs.write_generation(looked).unwrap_or(u64::MAX),
                                Ordering::SeqCst,
                            );
                        },
                    ));
                    // The race may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !racing.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    true
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(
                !armed.load(Ordering::SeqCst),
                "the write ran inside the flip"
            );
            let reader = reader.load(Ordering::SeqCst);
            assert_eq!(
                reader,
                writer.load(Ordering::SeqCst),
                "the LOOKUP took the forgotten writer's number"
            );
            assert_eq!(
                reader_generation.load(Ordering::SeqCst),
                0,
                "the LOOKUP's inode held no write, so the flip found it through `flip_held`"
            );
            assert_eq!(read(&fs, reader, 64).await, b"TWO..");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"TWO.."
            );
            assert!(
                fs_node(&instance, &fs, "a.txt").await.is_dirty(),
                "a.txt is clean"
            );
        })
        .await;
}

/// An add staged and then removed leaves the staged State at the next stage, which has nothing
/// else to stage: the commit after it reads nothing it cannot find, and status is clean.
#[tokio::test(flavor = "multi_thread")]
async fn an_add_staged_then_removed_leaves_the_staged_state_at_the_next_stage() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let added = create_with(&fs, ROOT_INO, "b.txt", b"B").await;
            stage_through(&instance, &fs).await.expect("stage the add");
            fs.unlink(&request(), ROOT_INO, b"b.txt")
                .await
                .expect("unlink");
            fs.forget(added, 1);

            let committed = commit_through(&instance, &fs, false).await;
            assert!(
                committed
                    .as_ref()
                    .is_err_and(|error| error.contains("Nothing staged")),
                "the commit: {committed:?}"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A staged temporary saved over its name before the next stage, which stages the saved file,
/// leaves the staged State with it: the commit stores the save, and status is clean.
#[tokio::test(flavor = "multi_thread")]
async fn a_staged_temporary_saved_over_its_name_leaves_the_staged_state() {
    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let temporary = create_with(&fs, ROOT_INO, "a.txt.tmp", b"SAVED").await;
            stage_through(&instance, &fs)
                .await
                .expect("stage the temporary");
            fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                .await
                .expect("save over the name");
            fs.forget(temporary, 1);

            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("commit the save");
            assert_eq!(
                revision_content(&instance, revision, "a.txt")
                    .await
                    .as_deref(),
                Some(b"SAVED".as_slice())
            );
            assert_eq!(
                revision_content(&instance, revision, "a.txt.tmp").await,
                None
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A local file last changed before a commit read the filesystem clock has the time the commit
/// read it at recorded, the later of its modification and status change: the fixture's clock
/// reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_records_the_time_of_a_local_file_written_before_its_clock() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.forget(ino, 1);
            let host = instance.write_dir().join("a.txt");
            std::fs::File::options()
                .write(true)
                .open(&host)
                .expect("a.txt")
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
                .expect("an older time");
            let changed = {
                use std::os::unix::fs::MetadataExt as _;

                let metadata = std::fs::metadata(&host).expect("a.txt");
                let ctime = u64::try_from(metadata.ctime()).expect("a time") * 1000
                    + u64::try_from(metadata.ctime_nsec()).expect("a time") / 1_000_000;
                lore_revision::util::fs::file_mtime(&metadata).max(ctime)
            };
            // Past the change's tick, so the commit's clock reads later than it.
            tokio::time::sleep(Duration::from_millis(20)).await;

            commit_through(&instance, &fs, false).await.expect("commit");
            assert_eq!(
                lore_revision::state::file_modified_time(
                    instance.command_context(&fs, None),
                    &lore_revision::util::path::RelativePath::new_from_initial_path("a.txt")
                        .expect("a path"),
                )
                .await,
                changed
            );
        })
        .await;
}

/// The modified time recorded for `path`, 0 for none.
async fn recorded_time(instance: &Instance, fs: &Arc<FuseFilesystem>, path: &str) -> u64 {
    lore_revision::state::file_modified_time(
        instance.command_context(fs, None),
        &lore_revision::util::path::RelativePath::new_from_initial_path(path).expect("a path"),
    )
    .await
}

/// A save over a file inside a commit, after the commit read the file's time and in the same
/// timestamp tick, at the same size, is staged by the next commit: the commit recorded no time
/// it could not tell apart from a later write. The tick is forced, as a coarse clock makes it.
/// A file changed before the commit's clock has its time recorded, so the clock was read.
#[tokio::test(flavor = "multi_thread")]
async fn a_save_in_the_commits_timestamp_tick_is_staged_by_the_next_commit() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.forget(ino, 1);
            let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            fs.write(&request(), b, 0, 0, b"BEE", 0)
                .await
                .expect("write");
            fs.forget(b, 1);
            set_host_time(
                &instance,
                "b.txt",
                std::time::SystemTime::now() - Duration::from_secs(3600),
            );
            // Past the change's tick, so the commit's clock reads later than it.
            tokio::time::sleep(Duration::from_millis(20)).await;
            // Written in the tick the commit reads its clock in: a time no older than the clock.
            let host = instance.write_dir().join("a.txt");
            let tick = std::time::SystemTime::now() + Duration::from_secs(3600);
            std::fs::File::options()
                .write(true)
                .open(&host)
                .expect("a.txt")
                .set_modified(tick)
                .expect("a time in the commit's tick");
            let saved = Arc::new(AtomicBool::new(false));
            fs.set_commit_hook(commit_step(&fs, {
                let saved = saved.clone();
                move |fs| {
                    let (saved, host) = (saved.clone(), host.clone());
                    async move {
                        if saved.swap(true, Ordering::SeqCst) {
                            return;
                        }
                        // After the commit read the file's time, before it records it.
                        let temporary = create_with(&fs, ROOT_INO, "a.txt.tmp", b"TWO..").await;
                        fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                            .await
                            .expect("save over the file");
                        fs.forget(temporary, 1);
                        std::fs::File::options()
                            .write(true)
                            .open(&host)
                            .expect("the saved file")
                            .set_modified(tick)
                            .expect("the same tick");
                    }
                }
            }));

            commit_through(&instance, &fs, false)
                .await
                .expect("the commit the save lands in");
            assert!(
                saved.load(Ordering::SeqCst),
                "the save ran inside the commit"
            );
            assert_ne!(
                recorded_time(&instance, &fs, "b.txt").await,
                0,
                "the commit recorded a time for b.txt: its clock read"
            );
            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("the next commit stages the save");
            assert_eq!(
                revision_content(&instance, revision, "a.txt")
                    .await
                    .as_deref(),
                Some(b"TWO..".as_slice())
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A save's temporary written in the millisecond of the file it saves over, before a commit, and
/// renamed over the file after the commit recorded the file's time, is staged by the next commit:
/// the file renamed onto the name never matches the time recorded for the one it replaced.
#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_from_the_files_millisecond_renamed_over_it_after_a_commit_is_staged() {
    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.forget(ino, 1);
            let millisecond = std::time::SystemTime::now() - Duration::from_secs(3600);
            set_host_time(&instance, "a.txt", millisecond);
            wait_past_host_time(&instance, &fs, "a.txt").await;
            stage_through(&instance, &fs).await.expect("stage a.txt");
            let temporary = create_with(&fs, ROOT_INO, "a.txt.tmp", b"TWO..").await;
            set_host_time(&instance, "a.txt.tmp", millisecond);
            commit_staged(&instance, &fs).await.expect("commit a.txt");
            assert_ne!(
                recorded_time(&instance, &fs, "a.txt").await,
                0,
                "the commit recorded a time for a.txt"
            );
            fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                .await
                .expect("save over a.txt");
            fs.forget(temporary, 1);

            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("the next commit stages the save");
            assert_eq!(
                revision_content(&instance, revision, "a.txt")
                    .await
                    .as_deref(),
                Some(b"TWO..".as_slice())
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A file created at a name after the file there is unlinked, at the same size and carrying the
/// unlinked file's recorded time, as a create in the same millisecond or `cp -p` leaves it, is
/// staged by the next commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_created_after_an_unlink_with_the_recorded_time_is_staged() {
    let instance = Arc::new(Instance::new(&[("a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ONE..", 0)
                .await
                .expect("write");
            fs.forget(ino, 1);
            let millisecond = std::time::SystemTime::now() - Duration::from_secs(3600);
            set_host_time(&instance, "a.txt", millisecond);
            wait_past_host_time(&instance, &fs, "a.txt").await;
            stage_through(&instance, &fs).await.expect("stage a.txt");
            commit_staged(&instance, &fs).await.expect("commit a.txt");
            assert_ne!(
                recorded_time(&instance, &fs, "a.txt").await,
                0,
                "the commit recorded a time for a.txt"
            );
            fs.unlink(&request(), ROOT_INO, b"a.txt")
                .await
                .expect("unlink a.txt");
            let created = create_with(&fs, ROOT_INO, "a.txt", b"TWO..").await;
            set_host_time(&instance, "a.txt", millisecond);
            fs.forget(created, 1);

            let revision = commit_through(&instance, &fs, false)
                .await
                .expect("the next commit stages the new file");
            assert_eq!(
                revision_content(&instance, revision, "a.txt")
                    .await
                    .as_deref(),
                Some(b"TWO..".as_slice())
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A committed deletion leaves the deletion list, and one made during the commit stays in it;
/// a remount at the revision serves neither file.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_deletion_leaves_the_deletion_list() {
    let instance =
        Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee"), ("c.txt", b"sea")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.unlink(&request(), ROOT_INO, b"b.txt")
                .await
                .expect("unlink b.txt");
            assert_eq!(deleted(&instance), vec!["b.txt"]);
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                fs.unlink(&request(), ROOT_INO, b"c.txt")
                    .await
                    .expect("unlink during the commit");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(revision_content(&instance, revision, "b.txt").await, None);
            assert_eq!(deleted(&instance), vec!["c.txt"]);
            assert_eq!(changed_paths(&instance, &fs).await, vec!["c.txt"]);
            let remounted = provider_at(&instance, revision).await;
            assert_eq!(
                served_tree(&remounted).await,
                vec![("a.txt".to_string(), Some(b"alpha".to_vec()))]
            );
        })
        .await;
}

/// An added file unlinked while a commit that stored it still reads, where the commit never
/// reconciles the mount with its revision, stays deleted: the unlink records the path, since the
/// commit may have stored it, and a remount at the revision serves no such file.
#[tokio::test(flavor = "multi_thread")]
async fn an_added_file_unlinked_while_a_commit_reads_stays_deleted_where_the_commit_never_reconciles()
 {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "n.txt", b"new").await;
            let armed = AtomicBool::new(true);
            let unlinked = Arc::new(Mutex::new(None::<bool>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let unlinked = unlinked.clone();
                move |step| {
                    if step != WriteStep::Reconcile {
                        return true;
                    }
                    if armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let done = fs.unlink(&request(), ROOT_INO, b"n.txt").await.is_ok();
                                let _ = done_tx.send(done);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *unlinked.lock().expect("unlinked") =
                                done_rx.recv_timeout(Duration::from_secs(30)).ok();
                        });
                    }
                    false
                }
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                *unlinked.lock().expect("unlinked"),
                Some(true),
                "n.txt was unlinked while the commit read"
            );
            assert_eq!(
                revision_content(&instance, revision, "n.txt").await,
                Some(b"new".to_vec()),
                "the commit stored n.txt"
            );
            assert_eq!(deleted(&instance), vec!["n.txt"]);
            let remounted = provider_at(&instance, revision).await;
            assert_eq!(
                served_tree(&remounted).await,
                vec![("a.txt".to_string(), Some(b"alpha".to_vec()))]
            );
        })
        .await;
}

/// An added file a commit stored and a request unlinked before the commit flipped it stays
/// deleted once the commit has reconciled the mount: the revision holds it, the deletion list
/// holds it, and a remount at the revision serves no such file.
#[tokio::test(flavor = "multi_thread")]
async fn an_added_file_unlinked_before_its_commit_flips_it_stays_deleted() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "n.txt", b"new").await;
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                fs.unlink(&request(), ROOT_INO, b"n.txt")
                    .await
                    .expect("unlink during the commit");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "n.txt").await,
                Some(b"new".to_vec()),
                "the commit stored n.txt"
            );
            assert_eq!(deleted(&instance), vec!["n.txt"]);
            assert_eq!(changed_paths(&instance, &fs).await, vec!["n.txt"]);
            let remounted = provider_at(&instance, revision).await;
            assert_eq!(
                served_tree(&remounted).await,
                vec![("a.txt".to_string(), Some(b"alpha".to_vec()))]
            );
        })
        .await;
}

/// A directory made through the mount and committed with its files becomes a clean directory of
/// the revision: its node and theirs take the committed addresses, its inode turns tracked, and
/// `write/` holds nothing of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_new_directory_becomes_clean_with_its_files() {
    use lore_revision::fs::fuse::inode::InodeKind;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = fs
                .mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, dir, "f.txt", b"eff").await;
            let sub = fs
                .mkdir(&request(), dir, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, sub, "g.txt", b"gee").await;

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            for path in ["", "d", "d/f.txt", "d/e", "d/e/g.txt"] {
                let node = fs_node(&instance, &fs, path).await;
                assert!(!node.is_dirty() && !is_local(&node), "{path}: {node:?}");
                assert_eq!(
                    node.address,
                    committed_node(&instance, revision, path).await.address,
                    "{path}"
                );
            }
            for ino in [dir, sub] {
                assert!(matches!(
                    fs.inodes().get(ino).expect("the inode").kind,
                    InodeKind::Dir { local: false, .. }
                ));
            }
            assert_eq!(names(&instance.write_dir()), Vec::<String>::new());
            let f = lookup(&fs, dir, "f.txt").await.expect("d/f.txt");
            assert_eq!(read(&fs, f, 64).await, b"eff");
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A dry run stores nothing, so the mount is left as it was: the file stays dirty under `write/`
/// and status still reports it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_commit_changes_nothing_in_the_mount() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
                .await
                .expect("write");
            let before = fs.served().revision();

            commit_through(&instance, &fs, true)
                .await
                .expect("dry-run commit");

            assert_eq!(fs.served().revision(), before);
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"ALPHA"
            );
            let node = fs_node(&instance, &fs, "a.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert!(
                fs.inodes()
                    .get(ino)
                    .expect("the inode")
                    .kind
                    .is_local_file()
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a.txt"]);
            assert_eq!(fs.commit_outcomes(), (0, 0));
        })
        .await;
}

/// A file whose inode the kernel forgot before the commit read it is flipped where no inode
/// holds it at the flip, or the one looked up since has taken no write, and stays dirty where
/// one looked up since was written.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_the_kernel_forgot_before_the_commit_is_flipped_unless_written_since() {
    let instance =
        Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee"), ("c.txt", b"sea")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for (name, bytes) in [
                ("a.txt", b"AAAAA"),
                ("b.txt", b"BBBBB"),
                ("c.txt", b"CCCCC"),
            ] {
                let ino = lookup(&fs, ROOT_INO, name).await.expect("a file");
                fs.write(&request(), ino, 0, 0, bytes, 0)
                    .await
                    .expect("write");
                fs.forget(ino, 1);
                assert!(fs.inodes().get(ino).is_none(), "{name} is still held");
            }
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                let b = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
                fs.write(&request(), b, 0, 0, b"bbbbb", 0)
                    .await
                    .expect("write during the commit");
                lookup(&fs, ROOT_INO, "c.txt").await.expect("c.txt");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            for name in ["a.txt", "c.txt"] {
                let node = fs_node(&instance, &fs, name).await;
                assert!(!node.is_dirty() && !is_local(&node), "{name}: {node:?}");
                assert!(!instance.write_dir().join(name).exists(), "{name}");
            }
            let c = lookup(&fs, ROOT_INO, "c.txt").await.expect("c.txt");
            assert!(!fs.inodes().get(c).expect("c.txt").kind.is_local_file());
            assert_eq!(read(&fs, c, 64).await, b"CCCCC");
            assert_eq!(
                revision_content(&instance, revision, "b.txt").await,
                Some(b"BBBBB".to_vec())
            );
            let node = fs_node(&instance, &fs, "b.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(
                std::fs::read(instance.write_dir().join("b.txt")).expect("write/b.txt"),
                b"bbbbb"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["b.txt"]);
            assert_eq!(fs.commit_outcomes(), (2, 1));
        })
        .await;
}

/// Writes `bytes` to each of `names` in the root through `fs`, looking each up first; answers
/// their inodes.
async fn write_each(fs: &FuseFilesystem, names: &[&str], bytes: &[u8]) -> Vec<u64> {
    let mut inos = Vec::with_capacity(names.len());
    for name in names {
        let ino = lookup(fs, ROOT_INO, name).await.expect("a file");
        fs.write(&request(), ino, 0, 0, bytes, 0)
            .await
            .expect("write");
        inos.push(ino);
    }
    inos
}

/// Asserts that each of `names` was flipped: clean, store-served, and out of `write/`.
async fn assert_flipped(instance: &Instance, fs: &FuseFilesystem, names: &[&str]) {
    for name in names {
        let node = fs_node(instance, fs, name).await;
        assert!(!node.is_dirty() && !is_local(&node), "{name}: {node:?}");
        assert!(
            !instance.write_dir().join(name).exists(),
            "{name} is left in write/"
        );
    }
}

/// A written file whose inode the kernel forgets between the commit's read and its flip stays
/// dirty, while the other files the commit read flip.
#[tokio::test(flavor = "multi_thread")]
async fn a_written_file_forgotten_during_a_commit_stays_dirty_and_the_others_flip() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee"), ("x.txt", b"ex")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "b.txt", "x.txt"], b"WRITE").await;
            let x = inos[2];
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                let held = fs.inodes().get(x).map_or(0, |entry| entry.nlookup);
                fs.forget(x, held);
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_flipped(&instance, &fs, &["a.txt", "b.txt"]).await;
            let node = fs_node(&instance, &fs, "x.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(
                std::fs::read(instance.write_dir().join("x.txt")).expect("write/x.txt"),
                b"WRITE"
            );
            assert_eq!(fs.commit_outcomes(), (2, 1));
        })
        .await;
}

/// A save through a temporary over one file during a commit, the inode it replaced then
/// forgotten, keeps the saved bytes and leaves the other files the commit read to flip.
#[tokio::test(flavor = "multi_thread")]
async fn a_save_whose_replaced_inode_is_forgotten_during_a_commit_flips_the_others() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee"), ("x.txt", b"ex")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "b.txt", "x.txt"], b"WRITE").await;
            let x = inos[2];
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                create_with(&fs, ROOT_INO, "x.txt.tmp", b"SAVED").await;
                fs.rename(&request(), ROOT_INO, b"x.txt.tmp", ROOT_INO, b"x.txt", 0)
                    .await
                    .expect("save over x.txt");
                let held = fs.inodes().get(x).map_or(0, |entry| entry.nlookup);
                fs.forget(x, held);
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_flipped(&instance, &fs, &["a.txt", "b.txt"]).await;
            let saved = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
            assert_eq!(read(&fs, saved, 64).await, b"SAVED");
            assert_eq!(
                std::fs::read(instance.write_dir().join("x.txt")).expect("write/x.txt"),
                b"SAVED"
            );
            assert_eq!(fs.commit_outcomes(), (2, 1));
        })
        .await;
}

/// A file the commit read with no inode held, written during the commit through an inode a
/// rename carried away and the kernel then forgot, and renamed back through an inode of another
/// number, keeps that write and stays dirty, while the other files the commit read flip.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_through_an_inode_renamed_away_and_forgotten_during_a_commit_is_kept() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee"), ("x.txt", b"ex")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for ino in write_each(&fs, &["a.txt", "b.txt", "x.txt"], b"WRITE").await {
                let held = fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
                fs.forget(ino, held);
            }
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                let away = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
                fs.rename(&request(), ROOT_INO, b"x.txt", ROOT_INO, b"y.txt", 0)
                    .await
                    .expect("rename away");
                fs.write(&request(), away, 0, 0, b"LATE!", 0)
                    .await
                    .expect("write");
                let held = fs.inodes().get(away).map_or(0, |entry| entry.nlookup);
                fs.forget(away, held);
                let back = lookup(&fs, ROOT_INO, "y.txt").await.expect("y.txt");
                assert_ne!(back, away, "an inode of another number");
                fs.rename(&request(), ROOT_INO, b"y.txt", ROOT_INO, b"x.txt", 0)
                    .await
                    .expect("rename back");
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_flipped(&instance, &fs, &["a.txt", "b.txt"]).await;
            let x = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
            assert_eq!(read(&fs, x, 64).await, b"LATE!");
            let node = fs_node(&instance, &fs, "x.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(fs.commit_outcomes(), (2, 1));
        })
        .await;
}

/// Requests a test runs inside a step of a commit's flip, awaited after the commit.
type Spawned = Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

/// How long a settle step [`on_settle`] holds waits for the requests it runs.
#[derive(Clone, Copy)]
enum Until {
    /// Until they finish.
    Done,
    /// Until one waits for a settle, or they finish.
    Waiting,
    /// Not at all.
    Started,
}

/// What a test runs at a settle step: the requests, and how long the step waits for them.
type SettleWork = Option<(
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    Until,
)>;

/// Installs a write hook that, at each settle step of a commit's flip of a file no inode held
/// ([`WriteStep::Settle`]), and once before the settles as the root's step before its take-aside
/// ([`WriteStep::Classified`]), runs the requests `work` answers for the node and the step,
/// polling until they reach the point `work` names before the step goes on. Every other step
/// proceeds. Answers the requests' handles. A step that waits on a request for the name being
/// settled never ends: a LOOKUP of that name waits for the settle.
fn on_settle(
    fs: &Arc<FuseFilesystem>,
    work: impl Fn(Arc<FuseFilesystem>, NodeID, bool) -> SettleWork + Send + Sync + 'static,
) -> Spawned {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let spawned: Spawned = Arc::default();
    let handles = spawned.clone();
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new(move |step| {
        let (node, aside) = match step {
            WriteStep::Settle { node, aside } => (node, aside),
            WriteStep::Classified => (ROOT_NODE, false),
            _ => return true,
        };
        let Some(fs) = weak.upgrade() else {
            return true;
        };
        let Some((requests, until)) = work(fs.clone(), node, aside) else {
            return true;
        };
        let waiting = fs.settle_waits_begun();
        let handle =
            runtime.spawn(LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), requests));
        let reached = || match &until {
            Until::Done => handle.is_finished(),
            Until::Waiting => handle.is_finished() || fs.settle_waits_begun() > waiting,
            Until::Started => true,
        };
        // The requests may wait in this worker's queue: it is handed to another worker first.
        tokio::task::block_in_place(|| {
            while !reached() {
                std::thread::yield_now();
            }
        });
        handles.lock().expect("handles").push(handle);
        true
    }));
    spawned
}

/// Awaits the requests [`on_settle`] ran, failing on any that panicked.
async fn settled_requests(spawned: &Spawned) {
    let handles: Vec<_> = spawned.lock().expect("handles").drain(..).collect();
    for handle in handles {
        handle.await.expect("a request run inside a settle");
    }
}

/// The names in the staging directory beside `write/`.
fn names_in_staging(instance: &Instance) -> Vec<String> {
    let staging = instance.write_dir().with_file_name("write-staging");
    std::fs::read_dir(staging)
        .map(|dir| {
            dir.flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Forgets every lookup the kernel holds on each of `inos`.
fn forget_all(fs: &FuseFilesystem, inos: &[u64]) {
    for &ino in inos {
        let held = fs.inodes().get(ino).map_or(0, |entry| entry.nlookup);
        fs.forget(ino, held);
    }
}

/// A file the commit read with no inode held, written after the read through an inode then
/// forgotten, is never settled: the commit finds the forget of a written inode before it takes the
/// host file aside, and leaves the file dirty. A LOOKUP, READ and WRITE afterwards serve that
/// write and land on it, and the file stays dirty under `write/`, nothing left in the staging
/// directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_written_through_a_forgotten_inode_after_the_commits_read_is_never_settled() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "b.txt"], b"BETA").await;
            forget_all(&fs, &inos);
            let b = node_at(&instance, &fs, "b.txt").await;
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                let x = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
                fs.write(&request(), x, 0, 0, b"BETA2", 0)
                    .await
                    .expect("write");
                forget_all(&fs, &[x]);
            }));
            let window = Arc::new(AtomicBool::new(false));
            let seen = window.clone();
            let spawned = on_settle(&fs, move |fs, node, aside| {
                (node == b && aside && !seen.swap(true, Ordering::SeqCst)).then(|| {
                    let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                        Box::pin(async move {
                            let y = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
                            assert_eq!(read(&fs, y, 64).await, b"BETA2");
                            fs.write(&request(), y, 0, 0, b"Z", 0)
                                .await
                                .expect("write Z");
                        });
                    (requests, Until::Waiting)
                })
            });

            let flipped_before = fs.commit_outcomes();
            commit_through(&instance, &fs, false).await.expect("commit");
            settled_requests(&spawned).await;
            assert!(
                !window.load(Ordering::SeqCst),
                "the commit took b.txt's host file aside after a written inode of it was forgotten"
            );
            let flipped_after = fs.commit_outcomes();
            assert_eq!(
                (
                    flipped_after.0 - flipped_before.0,
                    flipped_after.1 - flipped_before.1
                ),
                (1, 1),
                "a.txt flipped and b.txt left dirty"
            );
            let y = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, y, 64).await, b"BETA2");
            fs.write(&request(), y, 0, 0, b"Z", 0)
                .await
                .expect("write Z");

            let y = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, y, 64).await, b"ZETA2");
            assert_eq!(
                std::fs::read(instance.write_dir().join("b.txt")).expect("write/b.txt"),
                b"ZETA2"
            );
            assert_eq!(names_in_staging(&instance), Vec::<String>::new());
            let node = fs_node(&instance, &fs, "b.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            forget_all(&fs, &[y]);
            let fresh = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, fresh, 64).await, b"ZETA2");
            assert_flipped(&instance, &fs, &["a.txt"]).await;
        })
        .await;
}

/// A write through an inode made and forgotten after a commit found no inode for a file and
/// before it settles the file puts the file's host file back with the node unchanged, and a
/// LOOKUP, READ and WRITE with its host file aside wait for it and land on the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_forgotten_as_a_commit_settles_a_file_puts_it_back_unchanged() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "b.txt"], b"BETA").await;
            forget_all(&fs, &inos);
            let b = node_at(&instance, &fs, "b.txt").await;
            let spawned = on_settle(&fs, move |fs, node, aside| {
                if node != if aside { b } else { ROOT_NODE } {
                    return None;
                }
                let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    if aside {
                        Box::pin(async move {
                            let y = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
                            assert_eq!(read(&fs, y, 64).await, b"BETA2");
                            fs.write(&request(), y, 0, 0, b"Z", 0)
                                .await
                                .expect("write Z");
                        })
                    } else {
                        Box::pin(async move {
                            let x = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
                            fs.write(&request(), x, 0, 0, b"BETA2", 0)
                                .await
                                .expect("write");
                            forget_all(&fs, &[x]);
                        })
                    };
                Some((requests, if aside { Until::Waiting } else { Until::Done }))
            });

            commit_through(&instance, &fs, false).await.expect("commit");
            settled_requests(&spawned).await;

            let y = lookup(&fs, ROOT_INO, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, y, 64).await, b"ZETA2");
            assert_eq!(
                std::fs::read(instance.write_dir().join("b.txt")).expect("write/b.txt"),
                b"ZETA2"
            );
            assert_eq!(names_in_staging(&instance), Vec::<String>::new());
            let node = fs_node(&instance, &fs, "b.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_flipped(&instance, &fs, &["a.txt"]).await;
            assert_eq!(fs.commit_outcomes(), (1, 1));
        })
        .await;
}

/// UNLINK, RENAME, LOOKUP, READ and GETATTR of files whose host file a commit's flip holds
/// aside answer as for files that exist: they wait for the flip and start again. Local inodes of
/// two of them are made after the commit found none, before the settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_on_a_host_file_a_commit_holds_aside_wait_for_it() {
    let instance = Instance::new(&[("g.txt", b"gee"), ("r.txt", b"are"), ("u.txt", b"you")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["g.txt", "r.txt", "u.txt"], b"NEW").await;
            forget_all(&fs, &inos);
            let r = node_at(&instance, &fs, "r.txt").await;
            let u = node_at(&instance, &fs, "u.txt").await;
            let g = node_at(&instance, &fs, "g.txt").await;
            let g_ino = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let made = g_ino.clone();
            let spawned = on_settle(&fs, move |fs, node, aside| {
                let made = made.clone();
                let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    match (aside, node) {
                        (false, ROOT_NODE) => Box::pin(async move {
                            for name in ["r.txt", "u.txt"] {
                                lookup(&fs, ROOT_INO, name).await.expect("a local inode");
                            }
                            // As an eviction would: READ and GETATTR open the host file by path.
                            let g = lookup(&fs, ROOT_INO, "g.txt").await.expect("a local inode");
                            fs.close_local_file(g);
                            made.store(g, std::sync::atomic::Ordering::SeqCst);
                        }),
                        (true, node) if node == r => Box::pin(async move {
                            fs.rename(&request(), ROOT_INO, b"r.txt", ROOT_INO, b"r2.txt", 0)
                                .await
                                .expect("rename");
                        }),
                        (true, node) if node == u => Box::pin(async move {
                            fs.unlink(&request(), ROOT_INO, b"u.txt")
                                .await
                                .expect("unlink");
                        }),
                        (true, node) if node == g => Box::pin(async move {
                            let ino = made.load(std::sync::atomic::Ordering::SeqCst);
                            let asked = request();
                            let (read, attr) =
                                tokio::join!(read(&fs, ino, 64), fs.getattr(&asked, ino));
                            assert_eq!(read, b"NEW");
                            assert_eq!(attr.expect("getattr").attr.size, 3);
                        }),
                        _ => return None,
                    };
                Some((requests, if aside { Until::Waiting } else { Until::Done }))
            });

            let begun = fs.settle_waits_begun();
            commit_through(&instance, &fs, false).await.expect("commit");
            settled_requests(&spawned).await;
            assert!(
                fs.settle_waits_begun() >= begun + 3,
                "the RENAME, the UNLINK and the READ or GETATTR each waited for a settle"
            );

            let moved = lookup(&fs, ROOT_INO, "r2.txt").await.expect("r2.txt");
            assert_eq!(read(&fs, moved, 64).await, b"NEW");
            assert_eq!(lookup(&fs, ROOT_INO, "r.txt").await, Err(Errno::ENOENT));
            assert_eq!(lookup(&fs, ROOT_INO, "u.txt").await, Err(Errno::ENOENT));
            let g = lookup(&fs, ROOT_INO, "g.txt").await.expect("g.txt");
            assert_eq!(read(&fs, g, 64).await, b"NEW");
            assert_eq!(names_in_staging(&instance), Vec::<String>::new());
        })
        .await;
}

/// A directory renamed while a commit's flip holds a file below it aside takes the file back
/// when the flip puts it back: nothing is left in the staging directory and the write stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_put_back_after_its_directory_moved_goes_into_the_directory() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = fs
                .mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let f = create_with(&fs, d, "f.txt", b"F1").await;
            let g = create_with(&fs, ROOT_INO, "g.txt", b"G").await;
            forget_all(&fs, &[f, g]);
            let node = node_at(&instance, &fs, "d/f.txt").await;
            let spawned = on_settle(&fs, move |fs, settling, aside| {
                if settling != if aside { node } else { ROOT_NODE } {
                    return None;
                }
                let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    if aside {
                        Box::pin(async move {
                            fs.rename(&request(), ROOT_INO, b"d", ROOT_INO, b"d2", 0)
                                .await
                                .expect("rename d");
                        })
                    } else {
                        Box::pin(async move {
                            let x = lookup(&fs, d, "f.txt").await.expect("f.txt");
                            fs.write(&request(), x, 0, 0, b"F2", 0)
                                .await
                                .expect("write");
                        })
                    };
                Some((requests, Until::Done))
            });

            commit_through(&instance, &fs, false).await.expect("commit");
            assert_eq!(
                spawned.lock().expect("handles").len(),
                2,
                "the write ran before the settles, and the rename while f.txt was aside"
            );
            settled_requests(&spawned).await;

            assert_eq!(
                std::fs::read(instance.write_dir().join("d2/f.txt")).expect("write/d2/f.txt"),
                b"F2"
            );
            assert_eq!(names_in_staging(&instance), Vec::<String>::new());
            let moved = lookup(&fs, ROOT_INO, "d2").await.expect("d2");
            let f = lookup(&fs, moved, "f.txt").await.expect("d2/f.txt");
            assert_eq!(read(&fs, f, 64).await, b"F2");
            let node = fs_node(&instance, &fs, "d2/f.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
        })
        .await;
}

/// A RENAME bringing a file back to the name a commit read it at, held between moving its node
/// and its inode, leaves the file dirty: the write it carries stays under `write/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_arriving_at_a_file_a_commit_read_leaves_it_dirty() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("x.txt", b"ex")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "x.txt"], b"WRITE").await;
            forget_all(&fs, &inos);
            let armed = Arc::new(AtomicBool::new(false));
            let reached = Arc::new(AtomicBool::new(false));
            let resume = Arc::new(AtomicBool::new(false));
            let (hold, held, released) = (armed.clone(), reached.clone(), resume.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if matches!(step, WriteStep::NodeMoved { file: true, .. })
                    && hold.load(Ordering::SeqCst)
                    && !held.swap(true, Ordering::SeqCst)
                {
                    while !released.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let renaming: Spawned = Arc::default();
            let handles = renaming.clone();
            let (arm, arrived) = (armed.clone(), reached.clone());
            fs.set_commit_hook(commit_step(&fs, move |fs| {
                let (arm, arrived, handles) = (arm.clone(), arrived.clone(), handles.clone());
                async move {
                    let e = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
                    fs.write(&request(), e, 0, 0, b"LATE!", 0)
                        .await
                        .expect("write");
                    fs.rename(&request(), ROOT_INO, b"x.txt", ROOT_INO, b"y.txt", 0)
                        .await
                        .expect("rename away");
                    arm.store(true, Ordering::SeqCst);
                    let back = fs.clone();
                    let renaming = lore_base::lore_spawn!(async move {
                        back.rename(&request(), ROOT_INO, b"y.txt", ROOT_INO, b"x.txt", 0)
                            .await
                            .expect("rename back");
                    });
                    while !arrived.load(Ordering::SeqCst) && !renaming.is_finished() {
                        tokio::task::yield_now().await;
                    }
                    handles.lock().expect("handles").push(renaming);
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");
            resume.store(true, Ordering::SeqCst);
            settled_requests(&renaming).await;
            assert!(
                reached.load(Ordering::SeqCst),
                "the rename back was held at its reattach during the commit"
            );

            let node = fs_node(&instance, &fs, "x.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(
                std::fs::read(instance.write_dir().join("x.txt")).expect("write/x.txt"),
                b"LATE!"
            );
            let e = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
            forget_all(&fs, &[e]);
            let fresh = lookup(&fs, ROOT_INO, "x.txt").await.expect("x.txt");
            assert_eq!(read(&fs, fresh, 64).await, b"LATE!");
            assert_flipped(&instance, &fs, &["a.txt"]).await;
        })
        .await;
}

/// The file `name` holds below directory `dir` of a provider, if it holds both.
async fn read_below(fs: &FuseFilesystem, dir: &str, name: &str) -> Option<Vec<u8>> {
    let dir = lookup(fs, ROOT_INO, dir).await.ok()?;
    let file = lookup(fs, dir, name).await.ok()?;
    Some(read(fs, file, 64).await)
}

/// Directory `n` and its file a commit added, the file written after the commit read it, `n`
/// renamed to `m` by `rename` (with the provider, the commit's revision once known): the
/// deletion list holds `n`, and a remount at the revision serves `m` with the write, and no `n`.
async fn a_renamed_committed_directory_is_deleted<F>(
    rename_in_commit: bool,
    late: impl Fn(Arc<FuseFilesystem>, u64) -> F + Send + Sync + 'static,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    let revision = LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let n = fs
                .mkdir(&request(), ROOT_INO, b"n", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            let f = create_with(&fs, n, "f.txt", b"EFF").await;
            fs.set_commit_hook(commit_step(&fs, move |fs| late(fs, f)));
            let revision = commit_through(&instance, &fs, false).await.expect("commit");
            if !rename_in_commit {
                fs.rename(&request(), ROOT_INO, b"n", ROOT_INO, b"m", 0)
                    .await
                    .expect("rename n");
            }
            let listed = std::fs::read_to_string(instance.external.path().join("deleted"))
                .unwrap_or_default();
            assert!(
                listed.lines().any(|line| line == "n"),
                "deletion list {listed:?}"
            );
            assert_eq!(
                read_below(&fs, "m", "f.txt").await.as_deref(),
                Some(&b"LATE"[..])
            );
            revision
        })
        .await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider_at(&instance, revision).await;
            assert_eq!(lookup(&fs, ROOT_INO, "n").await, Err(Errno::ENOENT));
            assert_eq!(
                read_below(&fs, "m", "f.txt").await.as_deref(),
                Some(&b"LATE"[..])
            );
        })
        .await;
}

/// A directory a commit added whose file stays dirty, the file written after the commit's read,
/// renamed after the commit: the revision holds the old name, so the rename records it deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_directory_whose_file_stayed_dirty_renamed_later_is_deleted() {
    Box::pin(a_renamed_committed_directory_is_deleted(
        false,
        |fs, f| async move {
            fs.write(&request(), f, 0, 0, b"LATE", 0)
                .await
                .expect("late write");
        },
    ))
    .await;
}

/// A directory a commit adds, renamed while the commit reconciles, with its file still local:
/// the rename records the old name deleted, since the commit may store it.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_renamed_while_a_commit_stores_it_is_deleted() {
    Box::pin(a_renamed_committed_directory_is_deleted(
        true,
        |fs, f| async move {
            fs.write(&request(), f, 0, 0, b"LATE", 0)
                .await
                .expect("late write");
            fs.rename(&request(), ROOT_INO, b"n", ROOT_INO, b"m", 0)
                .await
                .expect("rename n");
        },
    ))
    .await;
}

/// What [`lookup_meeting_a_settle`] saw: the LOOKUP held after its inode's insert check, how
/// many new inodes were dropped for a host file found absent meanwhile, and what the LOOKUP
/// answered.
struct MetASettle {
    held: bool,
    host_absent: u64,
    answered: Result<u64, Errno>,
}

/// Commits through `fs`, and once the commit has classified the files it stored, looks up `name`
/// in the root, the file of node `node`, which no inode holds: the LOOKUP is held right after its
/// new inode's insert check, before the commit's settle of the file claims the name, and let go
/// once the settle holds the file's host file aside, which the settle then waits for the LOOKUP to
/// wait on.
async fn lookup_meeting_a_settle(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    name: &'static str,
    node: NodeID,
) -> MetASettle {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::LookupStep;
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let held = Arc::new(AtomicBool::new(false));
    let resume = Arc::new(AtomicBool::new(false));
    let armed = Arc::new(AtomicBool::new(true));
    fs.set_lookup_hook(Arc::new({
        let (held, resume, armed) = (held.clone(), resume.clone(), armed.clone());
        move |step| {
            let hold = matches!(step, LookupStep::Inserted { node: at } if at == node)
                && armed.swap(false, Ordering::SeqCst);
            let (held, resume) = (held.clone(), resume.clone());
            Box::pin(async move {
                if hold {
                    held.store(true, Ordering::SeqCst);
                    while !resume.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                }
            })
        }
    }));
    type Lookup = tokio::task::JoinHandle<Result<u64, Errno>>;
    let looking: Arc<Mutex<Option<Lookup>>> = Arc::default();
    let started = AtomicBool::new(false);
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let (held, resume, looking) = (held.clone(), resume.clone(), looking.clone());
        move |step| {
            let Some(fs) = weak.upgrade() else {
                return true;
            };
            let finished = || {
                looking
                    .lock()
                    .expect("looking")
                    .as_ref()
                    .is_none_or(tokio::task::JoinHandle::is_finished)
            };
            match step {
                WriteStep::Classified if !started.swap(true, Ordering::SeqCst) => {
                    let handle = runtime.spawn(
                        LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), async move {
                            lookup(&fs, ROOT_INO, name).await
                        }),
                    );
                    *looking.lock().expect("looking") = Some(handle);
                    tokio::task::block_in_place(|| {
                        while !held.load(Ordering::SeqCst) && !finished() {
                            std::thread::yield_now();
                        }
                    });
                }
                WriteStep::Settle {
                    node: at,
                    aside: true,
                } if at == node => {
                    let begun = fs.settle_waits_begun();
                    resume.store(true, Ordering::SeqCst);
                    tokio::task::block_in_place(|| {
                        while fs.settle_waits_begun() == begun && !finished() {
                            std::thread::yield_now();
                        }
                    });
                }
                _ => {}
            }
            true
        }
    }));

    let (_, _, absent_before) = fs.lookup_refusals();
    commit_through(instance, fs, false).await.expect("commit");
    resume.store(true, Ordering::SeqCst);
    let handle = looking
        .lock()
        .expect("looking")
        .take()
        .expect("the commit classified its files");
    let answered = handle.await.expect("the lookup");
    MetASettle {
        held: held.load(Ordering::SeqCst),
        host_absent: fs.lookup_refusals().2 - absent_before,
        answered,
    }
}

/// A LOOKUP of a file its directory's listing holds, which no inode held, that made the file's
/// inode before a commit's settle claimed the name and then found the host file aside, waits for
/// the settle and answers the settled file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_of_a_listed_file_finding_its_host_file_aside_waits_for_the_settle() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt"], b"NEW").await;
            forget_all(&fs, &inos);
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["a.txt"]);
            let node = node_at(&instance, &fs, "a.txt").await;

            let met = lookup_meeting_a_settle(&instance, &fs, "a.txt", node).await;

            assert!(
                met.held,
                "the LOOKUP made the inode before the settle claimed the name"
            );
            assert_eq!(
                met.host_absent, 1,
                "the LOOKUP found its inode's host file aside"
            );
            let ino = met.answered.expect("the LOOKUP answers the settled file");
            assert_eq!(read(&fs, ino, 64).await, b"NEWha");
            assert_eq!(fs.commit_outcomes(), (1, 0));
        })
        .await;
}

/// [`a_lookup_of_a_listed_file_finding_its_host_file_aside_waits_for_the_settle`] for a file
/// created after its directory's listing was built, which the LOOKUP finds through the
/// directory's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_of_a_file_added_since_its_listing_finding_its_host_file_aside_waits_for_the_settle()
 {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["a.txt"]);
            let n = create_with(&fs, ROOT_INO, "n.txt", b"NEW").await;
            forget_all(&fs, &[n]);
            let node = node_at(&instance, &fs, "n.txt").await;

            let met = lookup_meeting_a_settle(&instance, &fs, "n.txt", node).await;

            assert!(
                met.held,
                "the LOOKUP made the inode before the settle claimed the name"
            );
            assert_eq!(
                met.host_absent, 1,
                "the LOOKUP found its inode's host file aside"
            );
            let ino = met.answered.expect("the LOOKUP answers the settled file");
            assert_eq!(read(&fs, ino, 64).await, b"NEW");
            assert_eq!(fs.commit_outcomes(), (1, 0));
        })
        .await;
}

/// A LOOKUP of a file no inode holds, which read the file before a commit settled it and makes
/// its inode after, makes no inode of what it read: the inode is refused, the lookup starts
/// again, and it serves the settled file, store-served, never making a local inode of a file
/// whose host file is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_that_read_a_file_before_its_settle_makes_no_inode_of_it() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("n.txt", b"en")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let inos = write_each(&fs, &["a.txt", "n.txt"], b"NEW").await;
            forget_all(&fs, &inos);
            let n = node_at(&instance, &fs, "n.txt").await;
            let (held, resume) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            let (hold, resumed) = (held.clone(), resume.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if step == (WriteStep::Admitting { node: n }) && !hold.swap(true, Ordering::SeqCst)
                {
                    tokio::task::block_in_place(|| {
                        while !resumed.load(Ordering::SeqCst) {
                            std::thread::yield_now();
                        }
                    });
                }
                true
            }));
            let looking = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    let ino = lookup(&fs, ROOT_INO, "n.txt").await.expect("n.txt");
                    let local = fs.inodes().get(ino).map(|entry| entry.kind.is_local_file());
                    (local, read(&fs, ino, 64).await)
                })
            };
            while !held.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }

            commit_through(&instance, &fs, false).await.expect("commit");
            resume.store(true, Ordering::SeqCst);
            let (local, served) = looking.await.expect("the lookup");

            assert_eq!(local, Some(false), "the lookup made a store-served inode");
            assert_eq!(served, b"NEW");
            let node = fs_node(&instance, &fs, "n.txt").await;
            assert!(!node.is_dirty() && !is_local(&node), "{node:?}");
            let (settled, swapped, absent) = fs.lookup_refusals();
            println!("lookup refusals: settled {settled}, swapped {swapped}, host absent {absent}");
            assert!(settled >= 1, "the settled name refused the inode");
            assert_eq!(absent, 0, "no local inode of the settled file was made");
            assert_eq!(swapped, 0);
        })
        .await;
}

/// LOOKUPs of other names, each making a new inode, while a commit settles 2,000 files no inode
/// held in another directory, are never refused and never answer `EAGAIN`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_of_other_names_during_settles_are_never_refused() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let local = write_each(&fs, &["b.txt"], b"LOCAL").await;
            let out = fs
                .mkdir(&request(), ROOT_INO, b"out", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..2000 {
                let name = format!("o{i:05}.o");
                let ino = create_with(&fs, out, &name, name.as_bytes()).await;
                fs.forget(ino, 1);
            }
            let stop = Arc::new(AtomicBool::new(false));
            let sent = Arc::new(AtomicU64::new(0));
            let started = Arc::new(AtomicBool::new(false));
            // LOOKUPs that ran while a settle held its file aside.
            let during = Arc::new(AtomicU64::new(0));
            let watched = Arc::new(AtomicBool::new(false));
            let (halt, count, begun) = (stop.clone(), sent.clone(), started.clone());
            let (seen, watching) = (during.clone(), watched.clone());
            let spawned = on_settle(&fs, move |fs, _, aside| {
                if aside {
                    if watching.swap(true, Ordering::SeqCst) {
                        return None;
                    }
                    let (count, seen) = (count.clone(), seen.clone());
                    drop(fs);
                    let requests: std::pin::Pin<
                        Box<dyn std::future::Future<Output = ()> + Send>,
                    > = Box::pin(async move {
                        let at = count.load(Ordering::SeqCst);
                        for _ in 0..1_000_000 {
                            if count.load(Ordering::SeqCst) > at {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        seen.store(count.load(Ordering::SeqCst) - at, Ordering::SeqCst);
                    });
                    return Some((requests, Until::Done));
                }
                if begun.swap(true, Ordering::SeqCst) {
                    return None;
                }
                let (halt, count) = (halt.clone(), count.clone());
                let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    Box::pin(async move {
                        while !halt.load(Ordering::SeqCst) {
                            for name in ["a.txt", "b.txt"] {
                                let ino = lookup(&fs, ROOT_INO, name)
                                    .await
                                    .unwrap_or_else(|errno| panic!("look up {name}: {errno:?}"));
                                fs.forget(ino, 1);
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                Some((requests, Until::Started))
            });

            commit_through(&instance, &fs, false).await.expect("commit");
            stop.store(true, Ordering::SeqCst);
            settled_requests(&spawned).await;

            let (settled, swapped, absent) = fs.lookup_refusals();
            println!(
                "lookups of other names during the settles: {}; refused: settled {settled}, swapped {swapped}, host absent {absent}",
                sent.load(Ordering::Relaxed)
            );
            assert!(sent.load(Ordering::Relaxed) > 0, "no lookup ran during the settles");
            assert!(
                during.load(Ordering::SeqCst) > 0,
                "no lookup ran while a settle held its file aside"
            );
            assert_eq!((settled, swapped), (0, 0));
            drop(local);
        })
        .await;
}

/// `e.txt` created empty and committed, with `during` run in that commit's hook and `after` run
/// once it is stored: status then reports `e.txt`, and the next commit stores `FILLED`.
async fn an_empty_committed_file_filled_later_is_committed<D, A>(
    during: impl Fn(Arc<FuseFilesystem>, u64) -> D + Send + Sync + 'static,
    after: impl FnOnce(Arc<FuseFilesystem>, u64) -> A,
) where
    D: std::future::Future<Output = ()> + Send + 'static,
    A: std::future::Future<Output = ()>,
{
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let e = create_with(&fs, ROOT_INO, "e.txt", b"").await;
            fs.set_commit_hook(commit_step(&fs, move |fs| during(fs, e)));
            let first = commit_through(&instance, &fs, false).await.expect("commit");
            assert_eq!(
                revision_content(&instance, first, "e.txt").await,
                Some(Vec::new())
            );
            after(fs.clone(), e).await;
            assert_eq!(changed_paths(&instance, &fs).await, vec!["e.txt"]);
            let second = commit_through(&instance, &fs, false).await.expect("commit");
            assert_eq!(
                revision_content(&instance, second, "e.txt")
                    .await
                    .as_deref(),
                Some(&b"FILLED"[..])
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A file committed while empty and written after the commit is reported and committed.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_committed_file_written_later_is_committed() {
    Box::pin(an_empty_committed_file_filled_later_is_committed(
        |_, _| async {},
        |fs, e| async move {
            fs.write(&request(), e, 0, 0, b"FILLED", 0)
                .await
                .expect("write");
        },
    ))
    .await;
}

/// A file committed while empty, then removed and created again with content, is reported and
/// committed.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_committed_file_created_again_is_committed() {
    Box::pin(an_empty_committed_file_filled_later_is_committed(
        |_, _| async {},
        |fs, _| async move {
            fs.unlink(&request(), ROOT_INO, b"e.txt")
                .await
                .expect("unlink");
            create_with(&fs, ROOT_INO, "e.txt", b"FILLED").await;
        },
    ))
    .await;
}

/// A file a commit stores empty, written while that commit reconciles, is reported and committed
/// by the next commit.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_file_written_during_its_commit_is_committed_next() {
    Box::pin(an_empty_committed_file_filled_later_is_committed(
        |fs, e| async move {
            fs.write(&request(), e, 0, 0, b"FILLED", 0)
                .await
                .expect("write");
        },
        |_, _| async {},
    ))
    .await;
}

/// The commit of one file beside `deleted` of `files` tracked files deleted and not staged, in
/// one directory or, where `spread`, one per sibling directory: how long it takes.
async fn commit_beside_deletions(files: usize, deleted: usize, spread: bool) -> Duration {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    let path_of = |i: usize| {
        if spread {
            format!("d{i:05}/f.txt")
        } else {
            format!("big/f{i:05}.txt")
        }
    };
    let owned: Vec<(String, Vec<u8>)> = (0..files)
        .map(|i| (path_of(i), b"x".to_vec()))
        .chain(std::iter::once(("keep.txt".to_string(), b"keep".to_vec())))
        .collect();
    let planted: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_slice()))
        .collect();
    let instance = Box::pin(Instance::new(&planted)).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for i in 0..deleted {
                let path = path_of(i);
                let (dir, name) = path.split_once('/').expect("a directory");
                let dir = lookup(&fs, ROOT_INO, dir).await.expect("the directory");
                fs.unlink(&request(), dir, name.as_bytes())
                    .await
                    .expect("unlink");
            }
            let keep = lookup(&fs, ROOT_INO, "keep.txt").await.expect("keep.txt");
            fs.write(&request(), keep, 0, 0, b"KEEP", 0)
                .await
                .expect("write");
            let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
            let context = instance.command_context(&fs, Some(token.share()));
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(vec![LoreString::from(
                    &instance.fixture.path.join("keep.txt"),
                )]),
                lore_revision::stage::StageOptions::default(),
            )
            .await
            .expect("stage keep.txt");
            let started = std::time::Instant::now();
            lore_revision::commit::commit_boxed(
                context.clone(),
                &token,
                lore_revision::commit::CommitOptions::new("keep".to_string()),
            )
            .await
            .expect("commit");
            let took = started.elapsed();
            drop(token);
            assert_eq!(fs.commit_outcomes(), (1, 0));
            took
        })
        .await
}

/// A commit's cost grows linearly with the deletions it leaves listed, in one directory and one
/// per sibling directory: 3,200 of 4,000 cost at most a bounded multiple of 800 of 1,000, where
/// resolving each path or directory from the root costs some sixteen times. Each takes the
/// fastest of three runs, so load on the machine skews the ratio less.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_is_linear_in_the_deletions_it_leaves_listed() {
    for spread in [false, true] {
        let mut small = Duration::MAX;
        let mut large = Duration::MAX;
        for _ in 0..3 {
            small = small.min(commit_beside_deletions(1000, 800, spread).await);
            large = large.min(commit_beside_deletions(4000, 3200, spread).await);
        }
        println!(
            "commit beside 800 of 1,000 deleted: {small:?}; 3,200 of 4,000: {large:?}; one per directory {spread}"
        );
        assert!(
            large < small * 8,
            "3,200 deletions took {large:?} against {small:?} for 800, one per directory {spread}"
        );
    }
}

/// Files created through one provider and committed through a rebuilt one, as after a service
/// restart, all flip.
#[tokio::test(flavor = "multi_thread")]
async fn files_created_before_a_rebuild_flip_at_the_next_commit() {
    const FILES: usize = 10;
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    let names: Vec<String> = (0..FILES).map(|i| format!("new{i}.txt")).collect();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            for name in &names {
                create_with(&fs, ROOT_INO, name, name.as_bytes()).await;
            }
            drop(fs);
            let fs = provider(&instance).await;

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            assert_flipped(&instance, &fs, &names).await;
            for name in names {
                assert_eq!(
                    revision_content(&instance, revision, name).await,
                    Some(name.as_bytes().to_vec())
                );
            }
            assert_eq!(fs.commit_outcomes(), (FILES as u64, 0));
            assert_eq!(names_in_write(&instance), Vec::<String>::new());
        })
        .await;
}

/// Files created, written and forgotten by the kernel before a commit all flip.
#[tokio::test(flavor = "multi_thread")]
async fn created_files_the_kernel_forgot_flip_at_the_next_commit() {
    const FILES: usize = 20;
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let out = fs
                .mkdir(&request(), ROOT_INO, b"out", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..FILES {
                let name = format!("o{i}.o");
                let ino = create_with(&fs, out, &name, name.as_bytes()).await;
                fs.forget(ino, 1);
            }

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(fs.commit_outcomes(), (FILES as u64, 0));
            assert_eq!(names_in_write(&instance), Vec::<String>::new());
            let o0 = fs_node(&instance, &fs, "out/o0.o").await;
            assert!(!o0.is_dirty() && !is_local(&o0), "{o0:?}");
        })
        .await;
}

/// A CREATE held between attaching its node and making its inode while a commit reads and flips
/// the new file is not flipped, and the write that follows the CREATE stays under `write/`.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_in_flight_during_a_commit_is_not_flipped_and_keeps_its_write() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let attached = Arc::new(AtomicBool::new(false));
            let resume = Arc::new(AtomicBool::new(false));
            let (held, released) = (attached.clone(), resume.clone());
            fs.set_write_hook(Arc::new(move |step| {
                if step == WriteStep::Created && !held.swap(true, Ordering::SeqCst) {
                    while !released.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                true
            }));
            let creating = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    create_with(&fs, ROOT_INO, "new.txt", b"NEW").await
                })
            };
            while !attached.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }

            let committed = commit_through(&instance, &fs, false).await;
            resume.store(true, Ordering::SeqCst);
            let ino = creating.await.expect("the CREATE");
            let revision = committed.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "new.txt").await,
                Some(Vec::new())
            );
            assert_eq!(read(&fs, ino, 64).await, b"NEW");
            assert_eq!(
                std::fs::read(instance.write_dir().join("new.txt")).expect("write/new.txt"),
                b"NEW"
            );
            let node = fs_node(&instance, &fs, "new.txt").await;
            assert!(node.is_dirty() && is_local(&node), "{node:?}");
            assert_eq!(fs.commit_outcomes(), (0, 1));
        })
        .await;
}

/// The names under `write/`, recursively, as paths.
fn names_in_write(instance: &Instance) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![instance.write_dir()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            } else {
                found.push(
                    path.strip_prefix(instance.write_dir())
                        .expect("below write/")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    found.sort();
    found
}

/// The reconciliation of a commit of `count` files created in one new directory, or where
/// `spread` one per new directory, their inodes forgotten: how long it took from its start to
/// the end of the commit, and the files it flipped and left dirty.
async fn reconciliation_of(count: usize, spread: bool) -> (Duration, (u64, u64)) {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = |name: String| {
                let fs = fs.clone();
                async move {
                    fs.mkdir(
                        &request(),
                        ROOT_INO,
                        name.as_bytes(),
                        libc::S_IFDIR | 0o755,
                        0o022,
                    )
                    .await
                    .expect("mkdir")
                    .ino
                }
            };
            let out = mkdir("out".to_string()).await;
            for i in 0..count {
                let name = format!("o{i:05}.o");
                let dir = if spread {
                    mkdir(format!("d{i:05}")).await
                } else {
                    out
                };
                let ino = create_with(&fs, dir, &name, name.as_bytes()).await;
                fs.forget(ino, 1);
            }
            let started = Arc::new(std::sync::Mutex::new(None));
            let start = started.clone();
            fs.set_commit_hook(Arc::new(move || {
                *start.lock().expect("the start") = Some(std::time::Instant::now());
                Box::pin(async {})
            }));
            commit_through(&instance, &fs, false).await.expect("commit");
            let took = started
                .lock()
                .expect("the start")
                .expect("a reconciliation")
                .elapsed();
            (took, fs.commit_outcomes())
        })
        .await
}

/// The reconciliation of a commit grows linearly with the files of one directory: 4,000 cost at
/// most a bounded multiple of eight times 500, where walking each file's path from the root
/// costs some sixty-four times.
#[tokio::test(flavor = "multi_thread")]
async fn reconciliation_is_linear_in_the_files_of_one_directory() {
    let mut runs = Vec::new();
    for count in [500, 400, 2000, 4000] {
        let (took, outcomes) = Box::pin(reconciliation_of(count, false)).await;
        println!("reconciliation of {count} files: {took:?}, flipped and left {outcomes:?}");
        runs.push((count, took, outcomes));
    }
    let (base, largest) = (runs[0].1, runs[3].1);
    assert!(
        largest < base * 20,
        "4,000 files took {largest:?} against {base:?} for 500"
    );
    for (count, _, outcomes) in runs {
        assert_eq!(outcomes, (count as u64, 0), "{count} files");
    }
}

/// The reconciliation of a commit grows linearly with new directories of one file each: 4,000
/// cost at most a bounded multiple of eight times 500, where resolving each directory from the
/// root costs some sixty-four times. Each size takes the fastest of three runs, so load on the
/// machine skews the ratio less.
#[tokio::test(flavor = "multi_thread")]
async fn reconciliation_is_linear_in_new_directories() {
    let mut runs = Vec::new();
    for count in [500, 2000, 4000] {
        let mut fastest = None;
        for _ in 0..3 {
            let run = Box::pin(reconciliation_of(count, true)).await;
            if fastest.as_ref().is_none_or(|(took, _)| run.0 < *took) {
                fastest = Some(run);
            }
        }
        let (took, outcomes) = fastest.expect("three runs");
        println!(
            "reconciliation of {count} files, one per new directory: {took:?}, flipped and left {outcomes:?}"
        );
        runs.push((count, took, outcomes));
    }
    let (base, largest) = (runs[0].1, runs[2].1);
    assert!(
        largest < base * 12,
        "4,000 directories took {largest:?} against {base:?} for 500"
    );
    for (count, _, outcomes) in runs {
        assert_eq!(outcomes, (count as u64, 0), "{count} files");
    }
}

/// UNLINKs of names that do not exist, sent while a commit settles 2,000 files no inode held in
/// their directory, neither wait for a settle nor start again: the settle wait is the name's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlinks_of_absent_names_during_settles_neither_wait_nor_retry() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let out = fs
                .mkdir(&request(), ROOT_INO, b"out", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            for i in 0..2000 {
                let name = format!("o{i:05}.o");
                let ino = create_with(&fs, out, &name, name.as_bytes()).await;
                fs.forget(ino, 1);
            }
            let stop = Arc::new(AtomicBool::new(false));
            let (sent, slowest) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            let started = Arc::new(AtomicBool::new(false));
            // UNLINKs that ran while a settle held its file aside.
            let during = Arc::new(AtomicU64::new(0));
            let watched = Arc::new(AtomicBool::new(false));
            let (halt, count, worst, begun) =
                (stop.clone(), sent.clone(), slowest.clone(), started.clone());
            let (seen, watching) = (during.clone(), watched.clone());
            let spawned = on_settle(&fs, move |fs, _, aside| {
                if aside {
                    if watching.swap(true, Ordering::SeqCst) {
                        return None;
                    }
                    let (count, seen) = (count.clone(), seen.clone());
                    drop(fs);
                    let requests: std::pin::Pin<
                        Box<dyn std::future::Future<Output = ()> + Send>,
                    > = Box::pin(async move {
                        let at = count.load(Ordering::SeqCst);
                        for _ in 0..1_000_000 {
                            if count.load(Ordering::SeqCst) > at {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        seen.store(count.load(Ordering::SeqCst) - at, Ordering::SeqCst);
                    });
                    return Some((requests, Until::Done));
                }
                if begun.swap(true, Ordering::SeqCst) {
                    return None;
                }
                let (halt, count, worst) = (halt.clone(), count.clone(), worst.clone());
                let requests: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    Box::pin(async move {
                        let mut i = 0u64;
                        while !halt.load(Ordering::SeqCst) {
                            let name = format!("absent-{i}");
                            let at = std::time::Instant::now();
                            let answered = fs.unlink(&request(), out, name.as_bytes()).await;
                            worst.fetch_max(at.elapsed().as_micros() as u64, Ordering::Relaxed);
                            assert_eq!(answered, Err(Errno::ENOENT));
                            count.fetch_add(1, Ordering::Relaxed);
                            i += 1;
                        }
                    });
                Some((requests, Until::Started))
            });

            commit_through(&instance, &fs, false).await.expect("commit");
            stop.store(true, Ordering::SeqCst);
            settled_requests(&spawned).await;

            let (waited, retried) = fs.settle_waits();
            println!(
                "unlinks of absent names during the settles: {}, slowest {} us, waited {waited}, started again {retried}",
                sent.load(Ordering::Relaxed),
                slowest.load(Ordering::Relaxed)
            );
            assert!(sent.load(Ordering::Relaxed) > 0, "no unlink ran during the settles");
            assert!(
                during.load(Ordering::SeqCst) > 0,
                "no unlink ran while a settle held its file aside"
            );
            assert_eq!((waited, retried), (0, 0));
            assert_eq!(fs.commit_outcomes(), (2000, 0));
        })
        .await;
}

/// The 32 bytes `name` holds when created or saved by its `round`th operation, four slots of
/// eight that a rewrite replaces one at a time.
fn race_content(name: &str, round: usize) -> Vec<u8> {
    format!("{name:<20}{round:>12}").into_bytes()
}

/// One worker's rewrites of one slot, saves through a temporary, renames, unlinks and creates of
/// its own files, at least `ops.0` and at most `ops.1` of them, going on past the first while
/// `more` answers true; each name's bytes kept as its operations left them, and read back after
/// each operation. With `forgets` 1, some operations forget their inode after; with 2, every one
/// does and the read-back forgets its lookups too, so files mostly stand with no inode. Each
/// operation counts in `progress`. Answers the files left and the operations made.
async fn race_worker(
    fs: Arc<FuseFilesystem>,
    (worker, files, forgets): (usize, usize, u8),
    (ops, progress): ((usize, usize), Arc<std::sync::atomic::AtomicU64>),
    more: impl Fn() -> bool,
) -> (Vec<(String, Vec<u8>)>, usize) {
    let mut held: std::collections::BTreeMap<String, Vec<u8>> = (0..files)
        .map(|file| {
            let name = format!("w{worker}-{file}");
            let content = race_content(&name, 0);
            (name, content)
        })
        .collect();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64 ^ (worker as u64 + 1);
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut op = 0;
    while op < ops.1 && (op < ops.0 || more()) {
        op += 1;
        progress.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let base = format!("w{worker}-{}", next() as usize % files);
        let moved = format!("{base}.moved");
        let current = [&base, &moved]
            .into_iter()
            .find(|name| held.contains_key(name.as_str()))
            .cloned();
        let choice = next() % 20;
        let touched = match current {
            None => {
                let content = race_content(&base, op);
                let ino = create_with(&fs, ROOT_INO, &base, &content).await;
                held.insert(base.clone(), content);
                Some(ino)
            }
            Some(name) if choice < 14 => {
                let slot = (next() % 4) as usize * 8;
                let bytes = format!("{op:>8}").into_bytes();
                let ino = lookup(&fs, ROOT_INO, &name).await.expect("a held name");
                fs.write(&request(), ino, 0, slot as u64, &bytes, 0)
                    .await
                    .unwrap_or_else(|errno| panic!("rewrite {name}: {errno:?}"));
                let content = held.get_mut(&name).expect("a held name");
                content[slot..slot + 8].copy_from_slice(&bytes);
                Some(ino)
            }
            Some(name) if choice == 14 => {
                let to = if name == base {
                    moved.clone()
                } else {
                    base.clone()
                };
                fs.rename(
                    &request(),
                    ROOT_INO,
                    name.as_bytes(),
                    ROOT_INO,
                    to.as_bytes(),
                    0,
                )
                .await
                .unwrap_or_else(|errno| panic!("rename {name}: {errno:?}"));
                let content = held.remove(&name).expect("a held name");
                held.insert(to, content);
                None
            }
            Some(name) if choice == 15 => {
                fs.unlink(&request(), ROOT_INO, name.as_bytes())
                    .await
                    .unwrap_or_else(|errno| panic!("unlink {name}: {errno:?}"));
                held.remove(&name);
                None
            }
            Some(name) => {
                let temporary = format!("{name}.tmp");
                let content = race_content(&name, op);
                create_with(&fs, ROOT_INO, &temporary, &content).await;
                fs.rename(
                    &request(),
                    ROOT_INO,
                    temporary.as_bytes(),
                    ROOT_INO,
                    name.as_bytes(),
                    0,
                )
                .await
                .unwrap_or_else(|errno| panic!("save {name}: {errno:?}"));
                held.insert(name, content);
                None
            }
        };
        if let Some(ino) = touched
            && (forgets == 2 || (forgets == 1 && next() % 3 == 0))
            && let Some(held) = fs.inodes().get(ino).map(|entry| entry.nlookup)
        {
            fs.forget(ino, held);
        }
        for name in [&base, &moved] {
            let served = match lookup(&fs, ROOT_INO, name).await {
                Ok(ino) => {
                    let served = read(&fs, ino, 64).await;
                    if forgets == 2 {
                        fs.forget(ino, 1);
                    }
                    Some(served)
                }
                Err(Errno::ENOENT) => None,
                Err(errno) => panic!("look up {name}: {errno:?}"),
            };
            assert_eq!(
                served.as_ref(),
                held.get(name.as_str()),
                "{name} after operation {op}"
            );
        }
        tokio::task::yield_now().await;
    }
    (held.into_iter().collect(), op)
}

/// Threads rewrite, save through a temporary, rename, unlink and create the committed files of
/// their own while commits run, until several commits have stored what they wrote, round after
/// round, every other round forgetting inodes as it goes, and the last two also holding each
/// settle of a file no inode held, its host file aside, until two more operations land or one
/// waits for it.
/// Once they stop and a last commit stores what they left, every file is served with its last
/// operation's bytes, the revision records the same, status reports nothing, and a remount at the
/// revision serves the same tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn writes_renames_and_unlinks_racing_commits_are_never_lost() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const ROUNDS: usize = 6;
    const WINDOW_POLLS: usize = 100_000;
    const WORKERS: usize = 4;
    const FILES: usize = 6;
    const OPS: (usize, usize) = (200, 20_000);
    const COMMITS: usize = 6;
    for round in 0..ROUNDS {
        let files: Vec<(String, Vec<u8>)> = (0..WORKERS)
            .flat_map(|worker| {
                (0..FILES).map(move |file| {
                    let name = format!("w{worker}-{file}");
                    let content = race_content(&name, 0);
                    (name, content)
                })
            })
            .collect();
        let planted: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(name, content)| (name.as_str(), content.as_slice()))
            .collect();
        let instance = Arc::new(Instance::new(&planted).await);
        let execution = fuse_fixture::setup_test_execution();
        let (forgets, window) = (round % 2 == 0 || round >= 4, round >= 4);
        let settles = Arc::new(AtomicU64::new(0));
        let (expected, ops, commits, failed, outcomes) = LORE_CONTEXT
            .scope(execution, {
                let (instance, settles) = (instance.clone(), settles.clone());
                async move {
                    let fs = provider(&instance).await;
                    let stop = Arc::new(AtomicBool::new(false));
                    let progress = Arc::new(AtomicU64::new(0));
                    if window {
                        let (stopped, landed) = (stop.clone(), progress.clone());
                        let weak = Arc::downgrade(&fs);
                        fs.set_write_hook(Arc::new(move |step| {
                            if let WriteStep::Settle { aside: true, .. } = step
                                && let Some(fs) = weak.upgrade()
                            {
                                settles.fetch_add(1, Ordering::Relaxed);
                                let seen = landed.load(Ordering::SeqCst);
                                let waits = fs.settle_waits_begun();
                                tokio::task::block_in_place(|| {
                                    // Bounded, so a round whose workers stopped goes on.
                                    for _ in 0..WINDOW_POLLS {
                                        if landed.load(Ordering::SeqCst) >= seen + 2
                                            || fs.settle_waits_begun() > waits
                                            || stopped.load(Ordering::SeqCst)
                                        {
                                            break;
                                        }
                                        std::thread::yield_now();
                                    }
                                });
                            }
                            true
                        }));
                    }
                    let commits = Arc::new(AtomicUsize::new(0));
                    let failed = Arc::new(AtomicUsize::new(0));
                    let committer = {
                        let (instance, fs, stop) = (instance.clone(), fs.clone(), stop.clone());
                        let (commits, failed) = (commits.clone(), failed.clone());
                        lore_base::lore_spawn!(async move {
                            while !stop.load(Ordering::Acquire) {
                                match commit_through(&instance, &fs, false).await {
                                    Ok(_) => commits.fetch_add(1, Ordering::Relaxed),
                                    Err(_) => failed.fetch_add(1, Ordering::Relaxed),
                                };
                            }
                        })
                    };
                    let mut workers = tokio::task::JoinSet::new();
                    for worker in 0..WORKERS {
                        let committed = commits.clone();
                        let _ = lore_base::lore_spawn!(
                            workers,
                            race_worker(
                                fs.clone(),
                                (worker, FILES, u8::from(forgets) + u8::from(window)),
                                (OPS, progress.clone()),
                                move || { committed.load(Ordering::Relaxed) < COMMITS }
                            )
                        );
                    }
                    let mut expected = Vec::new();
                    let mut ops = 0;
                    while let Some(joined) = workers.join_next().await {
                        let (held, made) = joined.expect("worker");
                        expected.extend(held);
                        ops += made;
                    }
                    stop.store(true, Ordering::Release);
                    committer.await.expect("committer");
                    let last = match commit_through(&instance, &fs, false).await {
                        Ok(revision) => format!("stored {revision}"),
                        Err(error) if error.contains("Nothing staged") => error,
                        Err(error) => panic!("round {round}: the last commit failed: {error}"),
                    };
                    expected.sort();

                    assert_eq!(
                        served_tree(&fs).await,
                        expected
                            .iter()
                            .map(|(name, content)| (name.clone(), Some(content.clone())))
                            .collect::<Vec<_>>(),
                        "round {round}: served"
                    );
                    let changed = changed_paths(&instance, &fs).await;
                    if !changed.is_empty() {
                        let (revision, _) = lore_revision::instance::load_current_anchor_boxed(
                            &instance.fixture.repository,
                        )
                        .await
                        .expect("the current anchor");
                        for name in &changed {
                            let node = fs_node(&instance, &fs, name).await;
                            println!(
                                "round {round}: {name} reported after the last commit ({last}): revision {:?}, served {:?}, node flags {:#x} ext {:#x} size {}",
                                revision_content(&instance, revision, name)
                                    .await
                                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                                expected
                                    .iter()
                                    .find(|(held, _)| held == name)
                                    .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned()),
                                node.flags,
                                node.ext_flags,
                                node.size
                            );
                        }
                    }
                    assert_eq!(
                        changed,
                        Vec::<String>::new(),
                        "round {round}: status after the last commit ({last})"
                    );
                    let (settled, swapped, absent) = fs.lookup_refusals();
                    println!(
                        "inodes refused in round {round}: settled {settled} swapped {swapped} host absent {absent}"
                    );
                    (
                        expected,
                        ops,
                        commits.load(Ordering::Relaxed),
                        failed.load(Ordering::Relaxed),
                        fs.commit_outcomes(),
                    )
                }
            })
            .await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let (revision, _) = lore_revision::instance::load_current_anchor_boxed(
                    &instance.fixture.repository,
                )
                .await
                .expect("the current anchor");
                for (name, content) in &expected {
                    assert_eq!(
                        revision_content(&instance, revision, name).await.as_ref(),
                        Some(content),
                        "round {round}: {name} in the revision"
                    );
                }
                let remounted = provider_at(&instance, revision).await;
                assert_eq!(
                    served_tree(&remounted).await,
                    expected
                        .iter()
                        .map(|(name, content)| (name.clone(), Some(content.clone())))
                        .collect::<Vec<_>>(),
                    "round {round}: remounted"
                );
            })
            .await;
        assert!(
            commits >= COMMITS,
            "round {round}: {commits} commits stored while the files changed"
        );
        assert!(outcomes.0 > 0, "round {round}: no commit flipped a file");
        assert!(
            !window || settles.load(Ordering::Relaxed) > 0,
            "round {round}: no settle held a file aside for the requests"
        );
        println!(
            "settles held in round {round}: {}",
            settles.load(Ordering::Relaxed)
        );
        println!(
            "round {round}: {ops} ops, {commits} commits, {failed} failed, {} files flipped, {} left dirty, forgets {}",
            outcomes.0,
            outcomes.1,
            if window {
                "window"
            } else if forgets {
                "true"
            } else {
                "false"
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_stage_and_diff_see_a_write_at_once() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreFileAction;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"alpha\n"), ("b.txt", b"bee\n")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || std::fs::write(root.join("a.txt"), b"ALPHA\n").expect("rewrite")).await;
    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(&m.fs, Some(token.share()));

    assert_eq!(
        staged_state::status_files(context.clone(), false).await,
        std::collections::BTreeMap::from([("a.txt".to_string(), (LoreFileAction::Keep, true))]),
        "status without --scan sees the write at once"
    );

    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path.join("a.txt"))]),
                lore_revision::stage::StageOptions::default(),
            ),
        )
        .await
        .expect("the write stages");
    let staged = staged_state::status_files(context.clone(), false).await;
    assert_eq!(
        staged.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["a.txt"],
        "the staged write is reported once"
    );

    let diffs = working_diffs(context).await;
    assert_eq!(diffs.len(), 1, "{diffs:?}");
    let (path, action, patch) = &diffs[0];
    assert_eq!((path.as_str(), *action), ("a.txt", LoreFileAction::Keep));
    assert!(
        patch.contains("-alpha") && patch.contains("+ALPHA"),
        "{patch}"
    );
    drop(token);
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_of_a_created_and_a_deleted_file_through_the_mount() {
    use lore_revision::interface::LoreFileAction;

    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"alpha\n"), ("b.txt", b"bee\n")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || {
        std::fs::write(root.join("new.txt"), b"new\n").expect("create");
        std::fs::remove_file(root.join("b.txt")).expect("unlink");
    })
    .await;

    let diffs = working_diffs(instance.command_context(&m.fs, None)).await;
    assert_eq!(
        diffs
            .iter()
            .map(|(path, action, _)| (path.as_str(), *action))
            .collect::<Vec<_>>(),
        vec![
            ("b.txt", LoreFileAction::Delete),
            ("new.txt", LoreFileAction::Add)
        ],
        "{diffs:?}"
    );
    assert!(diffs[0].2.contains("-bee"), "{}", diffs[0].2);
    assert!(diffs[1].2.contains("+new"), "{}", diffs[1].2);
    m.unmount().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_of_a_directory_right_after_a_write_stages_it() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    skip_without_fuse!();
    let instance = Instance::new(&[
        ("a.txt", b"alpha\n"),
        ("b.txt", b"bee\n"),
        ("dir/c.txt", b"sea\n"),
    ])
    .await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || {
        std::fs::write(root.join("a.txt"), b"ALPHA\n").expect("rewrite");
        std::fs::write(root.join("dir/new.txt"), b"new\n").expect("create");
        std::fs::remove_file(root.join("b.txt")).expect("unlink");
    })
    .await;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(&m.fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            lore_revision::file::stage::stage(
                context.clone(),
                &token,
                LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path)]),
                lore_revision::stage::StageOptions::default(),
            ),
        )
        .await
        .expect("the stage succeeds");
    drop(token);

    let staged = instance.staged().await;
    let repository = &instance.fixture.repository;
    let mut actions = Vec::new();
    for path in ["a.txt", "b.txt", "dir/c.txt", "dir/new.txt"] {
        let link = staged
            .find_node_link(repository.clone(), path)
            .await
            .unwrap_or_else(|_| panic!("{path} is in the staged State"));
        let node = staged
            .node(repository.clone(), link.node)
            .await
            .expect("a staged node");
        let action = if node.is_staged_delete() {
            "delete"
        } else if node.is_staged_add() {
            "add"
        } else if node.is_staged_modify() {
            "modify"
        } else {
            "none"
        };
        actions.push((path, action));
    }
    assert_eq!(
        actions,
        vec![
            ("a.txt", "modify"),
            ("b.txt", "delete"),
            ("dir/c.txt", "none"),
            ("dir/new.txt", "add")
        ]
    );
    m.unmount().await;
}

/// Files the timing benches track, rewrite and create.
const BENCH_TRACKED: usize = 2000;
const BENCH_REWRITTEN: usize = 100;
const BENCH_CREATED: usize = 300;

/// A mount over [`BENCH_TRACKED`] tracked files, [`BENCH_REWRITTEN`] of them rewritten with a
/// new size and [`BENCH_CREATED`] created in a new directory.
async fn written_bench_mount() -> (Instance, Mounted) {
    let files: Vec<(String, Vec<u8>)> = (0..BENCH_TRACKED)
        .map(|i| {
            (
                format!("src/d{}/f{i}.c", i % 20),
                pattern(64 + i % 64, i as u8),
            )
        })
        .collect();
    let tracked: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_slice()))
        .collect();
    let instance = Box::pin(Instance::new(&tracked)).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || {
        for i in 0..BENCH_REWRITTEN {
            std::fs::write(
                root.join(format!("src/d{}/f{i}.c", i % 20)),
                pattern(200 + i, 7),
            )
            .expect("rewrite");
        }
        std::fs::create_dir_all(root.join("out")).expect("mkdir");
        for i in 0..BENCH_CREATED {
            std::fs::write(root.join(format!("out/o{i}.o")), pattern(128, i as u8))
                .expect("create");
        }
    })
    .await;
    (instance, m)
}

/// `lore status` without `--scan` over a mount holding a few hundred written files: the
/// provider's diff and the marks it records, the first run storing them and each later one
/// finding them in place. Run pinned on a quiet machine: `cargo test -p lore-revision --release
/// --test unit -- --ignored --nocapture bench_status_without_scan_over_written_files`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_status_without_scan_over_written_files() {
    use lore_revision::repository::RepositoryWriteToken;

    const RUNS: usize = 7;
    skip_without_fuse!();
    let (instance, m) = written_bench_mount().await;
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
        let context = instance.command_context(&m.fs, Some(token));
        let start = std::time::Instant::now();
        let reported = staged_state::status_files(context, false).await;
        times.push(start.elapsed());
        assert_eq!(
            reported.len(),
            BENCH_REWRITTEN + BENCH_CREATED + 1,
            "every write reported"
        );
    }
    let first = times[0];
    times.sort();
    println!(
        "status without --scan, {} written files over {BENCH_TRACKED} tracked: first {first:?}, min {:?}, median {:?}, max {:?}",
        BENCH_REWRITTEN + BENCH_CREATED,
        times[0],
        times[RUNS / 2],
        times[RUNS - 1]
    );
    m.unmount().await;
}

/// `lore stage .` right after a few hundred writes to a mount, with no status before it: the
/// provider's diff marking them, then the stage of each. Each run stages a fresh mount. Run pinned
/// on a quiet machine: `cargo test -p lore-revision --release --test unit -- --ignored
/// --nocapture bench_stage_of_the_root_over_written_files`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_stage_of_the_root_over_written_files() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    const RUNS: usize = 5;
    skip_without_fuse!();
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let (instance, m) = written_bench_mount().await;
        let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
        let context = instance.command_context(&m.fs, Some(token.share()));
        let start = std::time::Instant::now();
        LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                lore_revision::file::stage::stage(
                    context,
                    &token,
                    LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path)]),
                    lore_revision::stage::StageOptions::default(),
                ),
            )
            .await
            .expect("the stage succeeds");
        times.push(start.elapsed());
        drop(token);
        let staged = instance.staged().await;
        let last = format!("out/o{}.o", BENCH_CREATED - 1);
        let link = staged
            .find_node_link(instance.fixture.repository.clone(), &last)
            .await
            .expect("the last created file is staged");
        assert!(
            staged
                .node(instance.fixture.repository.clone(), link.node)
                .await
                .expect("a staged node")
                .is_staged_add()
        );
        m.unmount().await;
    }
    times.sort();
    println!(
        "stage of the root, {} written files over {BENCH_TRACKED} tracked: min {:?}, median {:?}, max {:?}",
        BENCH_REWRITTEN + BENCH_CREATED,
        times[0],
        times[RUNS / 2],
        times[RUNS - 1]
    );
}

/// `lore commit` of a few hundred writes to a mount, staged first: the commit storing them and
/// flipping each to the stored revision, the flips timed from the start of the reconciliation to
/// the end of the commit. Each run commits a fresh mount. Run pinned on a quiet
/// machine: `cargo test -p lore-revision --release --test unit -- --ignored
/// --nocapture bench_commit_of_written_files`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_commit_of_written_files() {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    const RUNS: usize = 5;
    skip_without_fuse!();
    let mut times = Vec::with_capacity(RUNS);
    let mut reconciled = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let (instance, m) = written_bench_mount().await;
        let flips = Arc::new(std::sync::Mutex::new(None));
        let started = flips.clone();
        m.fs.set_commit_hook(Arc::new(move || {
            *started.lock().expect("the start") = Some(std::time::Instant::now());
            Box::pin(async {})
        }));
        let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
        let context = instance.command_context(&m.fs, Some(token.share()));
        LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                lore_revision::file::stage::stage(
                    context.clone(),
                    &token,
                    LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path)]),
                    lore_revision::stage::StageOptions::default(),
                ),
            )
            .await
            .expect("the stage succeeds");
        let start = std::time::Instant::now();
        LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                lore_revision::commit::commit_boxed(
                    context,
                    &token,
                    lore_revision::commit::CommitOptions::new("bench".to_string()),
                ),
            )
            .await
            .expect("the commit succeeds");
        times.push(start.elapsed());
        reconciled.push(
            flips
                .lock()
                .expect("the start")
                .expect("a reconciliation")
                .elapsed(),
        );
        assert_eq!(
            m.fs.commit_outcomes(),
            ((BENCH_REWRITTEN + BENCH_CREATED) as u64, 0),
            "every written file flipped"
        );
        assert_eq!(names(&instance.write_dir()), Vec::<String>::new());
        drop(token);
        m.unmount().await;
    }
    times.sort();
    reconciled.sort();
    println!(
        "commit, {} written files over {BENCH_TRACKED} tracked: min {:?}, median {:?}, max {:?}; from the reconciliation on: min {:?}, median {:?}, max {:?}",
        BENCH_REWRITTEN + BENCH_CREATED,
        times[0],
        times[RUNS / 2],
        times[RUNS - 1],
        reconciled[0],
        reconciled[RUNS / 2],
        reconciled[RUNS - 1]
    );
}

/// `lore commit` of the directories `make` creates under a fresh mount's root, staged first,
/// then `lore status` without `--scan`: the commit's hand-over and settles of the directories it
/// adds, and what the status after it still walks. Each run commits a fresh mount; the commit,
/// the status and their sum are printed under `label`.
async fn bench_commit_of_directories(label: &str, make: fn(&std::path::Path)) {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    const RUNS: usize = 5;
    let mut commits = Vec::with_capacity(RUNS);
    let mut statuses = Vec::with_capacity(RUNS);
    let mut sums = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let instance = Box::pin(Instance::new(&[("keep/k.txt", b"kept".as_slice())])).await;
        let m = instance.mount().await;
        let root = m.root.clone();
        blocking(move || make(&root)).await;
        let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
        let context = instance.command_context(&m.fs, Some(token.share()));
        LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                lore_revision::file::stage::stage(
                    context.clone(),
                    &token,
                    LoreArray::from_vec(vec![LoreString::from(&instance.fixture.path)]),
                    lore_revision::stage::StageOptions::default(),
                ),
            )
            .await
            .expect("the stage succeeds");
        let start = std::time::Instant::now();
        LORE_CONTEXT
            .scope(
                fuse_fixture::setup_test_execution(),
                lore_revision::commit::commit_boxed(
                    context,
                    &token,
                    lore_revision::commit::CommitOptions::new("bench".to_string()),
                ),
            )
            .await
            .expect("the commit succeeds");
        let committed = start.elapsed();
        let context = instance.command_context(&m.fs, Some(token.share()));
        let start = std::time::Instant::now();
        let reported = staged_state::status_files(context, false).await;
        let status = start.elapsed();
        drop(reported);
        commits.push(committed);
        statuses.push(status);
        sums.push(committed + status);
        drop(token);
        m.unmount().await;
    }
    for times in [&mut commits, &mut statuses, &mut sums] {
        times.sort();
    }
    println!(
        "commit of {label}: min {:?}, median {:?}, max {:?}; status after it: min {:?}, median {:?}, max {:?}; both: min {:?}, median {:?}, max {:?}",
        commits[0],
        commits[RUNS / 2],
        commits[RUNS - 1],
        statuses[0],
        statuses[RUNS / 2],
        statuses[RUNS - 1],
        sums[0],
        sums[RUNS / 2],
        sums[RUNS - 1],
    );
}

/// [`bench_commit_of_directories`] of 1000 new empty sibling directories. Run pinned on a quiet
/// machine: `cargo test -p lore-revision --release --test unit -- --ignored --nocapture
/// bench_commit_of_empty_sibling_directories`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_commit_of_empty_sibling_directories() {
    skip_without_fuse!();
    bench_commit_of_directories("1000 empty sibling directories", |root| {
        for i in 0..1000 {
            std::fs::create_dir_all(root.join(format!("new/d{i}"))).expect("mkdir");
        }
    })
    .await;
}

/// [`bench_commit_of_directories`] of a local tree of 1000 empty directories, 100 holding 9 each,
/// moved by a rename of its root before the stage. Run pinned on a quiet machine: `cargo test -p
/// lore-revision --release --test unit -- --ignored --nocapture
/// bench_commit_of_a_moved_tree_of_empty_directories`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_commit_of_a_moved_tree_of_empty_directories() {
    skip_without_fuse!();
    bench_commit_of_directories("a moved tree of 1000 empty directories", |root| {
        for i in 0..100 {
            for j in 0..9 {
                std::fs::create_dir_all(root.join(format!("tree/d{i}/e{j}"))).expect("mkdir");
            }
        }
        std::fs::rename(root.join("tree"), root.join("moved")).expect("rename");
    })
    .await;
}

/// [`bench_commit_of_directories`] of 1000 new directories each holding a file, which the commit
/// reads and flips. Run pinned on a quiet machine: `cargo test -p lore-revision --release --test
/// unit -- --ignored --nocapture bench_commit_of_directories_holding_files`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn bench_commit_of_directories_holding_files() {
    skip_without_fuse!();
    bench_commit_of_directories("1000 directories holding a file each", |root| {
        for i in 0..1000 {
            let dir = root.join(format!("files/d{i}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("f.txt"), pattern(64, i as u8)).expect("create");
        }
    })
    .await;
}

/// The first revision a sync goes back to: the file `dir/f.txt` and `dir/g.txt` differ from the
/// second, `keep/` is the same, only it holds `new/a/b.txt`, and only the second holds `gone/`.
const SYNC_FIRST: [(&str, &[u8]); 4] = [
    ("dir/f.txt", b"one"),
    ("dir/g.txt", b"g-one"),
    ("keep/k.txt", b"kept"),
    ("new/a/b.txt", b"bee"),
];
const SYNC_SECOND: [(&str, &[u8]); 3] = [
    ("dir/f.txt", b"two"),
    ("dir/g.txt", b"g-two"),
    ("gone/x.txt", b"x"),
];

/// An instance at the second of two revisions, [`SYNC_FIRST`] then [`SYNC_SECOND`] less `new/`.
async fn sync_instance() -> Instance {
    Instance {
        fixture: fuse_fixture::released_deleting(&[&SYNC_FIRST, &SYNC_SECOND], &["new/"]).await,
        external: TempDir::new("lore-fuse-write-mount-instance-"),
    }
}

/// The revision the instance's anchor stands on.
async fn current_revision(instance: &Instance) -> Hash {
    lore_revision::instance::load_current_anchor_boxed(&instance.fixture.repository)
        .await
        .expect("the current anchor")
        .0
}

/// Runs `lore sync <revision>` through `fs`, as relayed to the service, under `globals`.
async fn sync_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    revision: Hash,
    globals: lore_revision::interface::LoreGlobalArgs,
) -> Result<(), lore_revision::revision::sync::SyncError> {
    let options = lore_revision::revision::sync::SyncOptions {
        revision: Some(revision.to_string()),
        ..Default::default()
    };
    sync_reporting(instance, fs, options, globals).await.0
}

/// Runs `lore sync` with `options` through `fs`, as relayed to the service, under `globals`,
/// answering the files it reports, each as `<action> <path>`, sorted.
async fn sync_reporting(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    options: lore_revision::revision::sync::SyncOptions,
    globals: lore_revision::interface::LoreGlobalArgs,
) -> (
    Result<(), lore_revision::revision::sync::SyncError>,
    Vec<String>,
) {
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::ExecutionContext;
    use lore_revision::relay::EventDispatcher;
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
    let execution = Arc::new(ExecutionContext::new_client_with_user_id(
        globals,
        EventDispatcher::new(Some(Box::new({
            let reported = reported.clone();
            move |event: &LoreEvent| {
                if let LoreEvent::RevisionSyncFile(file) = event {
                    reported
                        .lock()
                        .expect("the reported files")
                        .push(format!("{:?} {}", file.action, file.path));
                }
            }
        }))),
        "test-user".to_string(),
    ));
    let synced = LORE_CONTEXT
        .scope(
            execution.clone(),
            lore_revision::revision::sync::sync_boxed(context, &token, options),
        )
        .await;
    execution.dispatcher.drain().await;
    let mut reported = reported.lock().expect("the reported files").clone();
    reported.sort();
    (synced, reported)
}

/// Creates branch `name` at the current revision through `fs`.
async fn create_branch_through(instance: &Instance, fs: &Arc<FuseFilesystem>, name: &str) {
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            Box::pin(lore_revision::branch::create::create(
                context,
                &token,
                name.to_string(),
                None,
                String::new(),
                false,
            )),
        )
        .await
        .expect("the branch is created");
}

/// Runs `lore branch switch <name>` through `fs`, as relayed to the service.
async fn switch_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    name: &str,
) -> Result<Hash, String> {
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            Box::pin(lore_revision::repository::branch_switch(
                context,
                &token,
                name.to_string(),
                lore_revision::repository::BranchSwitchOptions {
                    local: true,
                    ..Default::default()
                },
            )),
        )
        .await
        .map_err(|error| format!("{error:?}"))
}

/// The inode of `path` below the root, looked up name by name.
async fn lookup_path(fs: &FuseFilesystem, path: &str) -> Result<u64, Errno> {
    let mut ino = ROOT_INO;
    for name in path.split('/') {
        ino = lookup(fs, ino, name).await?;
    }
    Ok(ino)
}

/// A sync to a revision that changes a file edited locally fails with `LocalModifications`, as
/// on an ordinary clone, and applies nothing: the edit, the other files and the anchor stay.
#[tokio::test(flavor = "multi_thread")]
async fn sync_over_an_edit_the_target_changes_fails_with_local_modifications() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO", 0)
                .await
                .expect("write");

            let synced = sync_through(&instance, &fs, first, Default::default()).await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(read(&fs, f, 64).await, b"TWO");
            let g = lookup_path(&fs, "dir/g.txt").await.expect("g.txt");
            assert_eq!(
                read(&fs, g, 64).await,
                b"g-two",
                "the sync applied a change"
            );
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
        })
        .await;
}

/// A branch switch carries an edit of a file the target branch leaves as it is: the file still
/// serves the edit and status still reports it, while the files the target changes take its
/// content.
#[tokio::test(flavor = "multi_thread")]
async fn switch_carries_an_edit_the_target_leaves() {
    let instance = sync_instance().await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_branch_through(&instance, &fs, "other").await;
            switch_through(&instance, &fs, "other")
                .await
                .expect("switch to other");
            let g = lookup_path(&fs, "dir/g.txt").await.expect("g.txt");
            fs.write(&request(), g, 0, 0, b"g-three", 0)
                .await
                .expect("write");
            commit_through(&instance, &fs, false)
                .await
                .expect("commit on other");
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO", 0)
                .await
                .expect("write");

            switch_through(&instance, &fs, lore_revision::branch::DEFAULT_DEFAULT_NAME)
                .await
                .expect("switch back");

            let g = lookup_path(&fs, "dir/g.txt").await.expect("g.txt");
            assert_eq!(read(&fs, g, 64).await, b"g-two");
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            assert_eq!(read(&fs, f, 64).await, b"TWO");
            assert!(is_local(&fs_node(&instance, &fs, "dir/f.txt").await));
            assert!(
                fs_node(&instance, &fs, "dir").await.is_dirty(),
                "dir was settled clean over the carried edit"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["dir/f.txt"]);
        })
        .await;
}

/// A forced sync over an edit of a file the target changes serves the target's content: the
/// node takes the target's address, clean, `write/` holds the file no more, its directory takes
/// the target's address, and status reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_sync_serves_the_target_over_a_local_edit() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            assert_eq!(read(&fs, f, 64).await, b"one");
            assert!(
                !instance.write_dir().join("dir/f.txt").exists(),
                "write/dir/f.txt is left behind"
            );
            let node = fs_node(&instance, &fs, "dir/f.txt").await;
            assert_eq!(
                node.address,
                committed_node(&instance, first, "dir/f.txt").await.address
            );
            assert!(!node.is_dirty() && !is_local(&node), "{node:?}");
            let dir = fs_node(&instance, &fs, "dir").await;
            assert!(!dir.is_dirty(), "{dir:?}");
            assert_eq!(
                dir.address,
                committed_node(&instance, first, "dir").await.address
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A respell of a directory a swap settles, landing between the settle's comparison and its
/// clear: the directory keeps the new spelling and stays dirty, so status reports it.
#[tokio::test(flavor = "multi_thread")]
async fn a_respell_landing_while_a_swap_settles_its_directory_keeps_it_dirty() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");
            let armed = Arc::new(AtomicBool::new(true));
            let weak = Arc::downgrade(&fs);
            fs.set_swap_hook(Arc::new({
                let armed = armed.clone();
                move |phase| {
                    let fire = phase == SwapPhase::Settle && armed.swap(false, Ordering::SeqCst);
                    let fs = weak.upgrade();
                    Box::pin(async move {
                        if let (true, Some(fs)) = (fire, fs) {
                            fs.rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"DIR", 0)
                                .await
                                .expect("the respell");
                        }
                        true
                    })
                }
            }));

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            assert!(!armed.load(Ordering::SeqCst), "no settle ran");
            let dir = fs_node(&instance, &fs, "DIR").await;
            assert!(
                dir.is_dirty(),
                "the respelled directory settled clean: {dir:?}"
            );
        })
        .await;
}

/// A tracked file deleted locally that the target changes is restored with the target's content,
/// as an ordinary clone restores it, and leaves the deletion list.
#[tokio::test(flavor = "multi_thread")]
async fn a_locally_deleted_file_the_target_changes_is_restored() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup_path(&fs, "dir").await.expect("dir");
            lookup(&fs, dir, "f.txt").await.expect("f.txt");
            fs.unlink(&request(), dir, b"f.txt").await.expect("unlink");
            assert_eq!(deleted(&instance), vec!["dir/f.txt"]);

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            let f = lookup_path(&fs, "dir/f.txt")
                .await
                .expect("f.txt is restored");
            assert_eq!(read(&fs, f, 64).await, b"one");
            assert_eq!(deleted(&instance), Vec::<String>::new());
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A dry-run sync changes nothing in the mount: every file serves what it did, and the State and
/// the anchor stay at the revision they stood on.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_sync_changes_nothing() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    dry_run: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the dry run");

            assert_eq!(read(&fs, f, 64).await, b"two");
            assert_eq!(lookup_path(&fs, "new").await, Err(Errno::ENOENT));
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
        })
        .await;
}

/// A dry-run sync checks local changes as a sync does: an edit of a file the target changes
/// fails it with `LocalModifications`.
#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_sync_over_an_edit_the_target_changes_fails_with_local_modifications() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO", 0)
                .await
                .expect("write");

            let synced = sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    dry_run: 1,
                    ..Default::default()
                },
            )
            .await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(read(&fs, f, 64).await, b"TWO");
            assert_eq!(fs.served().revision(), second);
        })
        .await;
}

/// A sync forwarding local changes keeps an edit of a file the target changes, and applies the
/// rest of the target.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_forwarding_local_changes_keeps_an_edit_the_target_changes() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO", 0)
                .await
                .expect("write");

            let (synced, _) = sync_reporting(
                &instance,
                &fs,
                lore_revision::revision::sync::SyncOptions {
                    revision: Some(first.to_string()),
                    forward_changes: true,
                    ..Default::default()
                },
                Default::default(),
            )
            .await;

            synced.expect("the sync");
            assert_eq!(read(&fs, f, 64).await, b"TWO");
            assert_eq!(served_at(&fs, "dir/g.txt").await, b"g-one");
            assert_eq!(fs.served().revision(), first);
            assert_eq!(current_revision(&instance).await, first);
        })
        .await;
}

/// A sync reports each change it applies, as a sync onto a working tree reports each file it
/// realizes, and a dry run reports the same, applying nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_reports_each_change_it_applies() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let options = lore_revision::revision::sync::SyncOptions {
                revision: Some(first.to_string()),
                ..Default::default()
            };

            let (dry, dry_reported) = sync_reporting(
                &instance,
                &fs,
                options.clone(),
                lore_revision::interface::LoreGlobalArgs {
                    dry_run: 1,
                    ..Default::default()
                },
            )
            .await;
            dry.expect("the dry run");
            assert_eq!(fs.served().revision(), second);
            let (synced, reported) =
                sync_reporting(&instance, &fs, options, Default::default()).await;
            synced.expect("the sync");

            assert!(
                reported.iter().any(|line| line.ends_with(" dir/f.txt")),
                "{reported:?}"
            );
            assert!(
                reported.iter().any(|line| line.ends_with(" new/a/b.txt")),
                "{reported:?}"
            );
            assert_eq!(dry_reported, reported);
        })
        .await;
}

/// A sync that changes a file the kernel holds keeps its inode number and drops its cached
/// pages: a read through the mount after the sync returns the target's bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_that_changes_a_file_keeps_its_inode_and_invalidates_its_pages() {
    use std::os::unix::fs::MetadataExt as _;

    skip_without_fuse!();
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    let m = instance.mount().await;
    let path = m.root.join("dir/f.txt");
    let read_through = move || {
        let path = path.clone();
        blocking(move || {
            let read = std::fs::read(&path).expect("read through the mount");
            (read, std::fs::metadata(&path).expect("stat").ino())
        })
    };
    let (before, ino) = read_through().await;
    assert_eq!(before, b"two");

    sync_through(&instance, &m.fs, first, Default::default())
        .await
        .expect("the sync");

    let (after, synced_ino) = read_through().await;
    assert_eq!(after, b"one", "the kernel served its cached pages");
    assert_eq!(synced_ino, ino);
    m.unmount().await;
}

/// A sync whose target adds a directory tree lists and serves it, while a directory it leaves
/// as it is keeps its cached listing.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_adding_a_directory_tree_lists_it_and_keeps_unchanged_listings_warm() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup_path(&fs, "keep/k.txt").await.expect("k.txt");
            let keep = node_at(&instance, &fs, "keep").await;
            let warm = fs.served().listings.get(keep).expect("keep/ is listed");

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            let a = lookup_path(&fs, "new/a").await.expect("new/a");
            assert_eq!(listed(&fs, a).await, vec!["b.txt"]);
            let b = lookup(&fs, a, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, b, 64).await, b"bee");
            let kept = fs
                .served()
                .listings
                .get(keep)
                .expect("keep/ is still listed");
            assert!(Arc::ptr_eq(&warm, &kept), "keep/ was listed again");
        })
        .await;
}

/// A file committed on one branch is served from another branch's revision after a switch to
/// it, never from the committed copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_file_does_not_shadow_another_branch() {
    let instance = sync_instance().await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_branch_through(&instance, &fs, "other").await;
            switch_through(&instance, &fs, "other")
                .await
                .expect("switch to other");
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"committed", 0)
                .await
                .expect("write");
            commit_through(&instance, &fs, false)
                .await
                .expect("commit on other");

            switch_through(&instance, &fs, lore_revision::branch::DEFAULT_DEFAULT_NAME)
                .await
                .expect("switch back");

            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            assert_eq!(read(&fs, f, 64).await, b"two");
            assert!(!instance.write_dir().join("dir/f.txt").exists());
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A sync whose target adds a directory the mount made locally, with nothing local below it,
/// settles the directory as the revision's: its node is clean tracked content, its inode answers
/// from the revision rather than the host, and `write/` holds no host directory for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_adding_a_directory_made_locally_settles_it_as_the_revisions() {
    use lore_revision::fs::fuse::inode::InodeKind;
    use lore_revision::node::NodeExtFlags;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let made = fs
                .mkdir(&request(), ROOT_INO, b"new", 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            assert!(instance.write_dir().join("new").is_dir(), "write/new");

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            assert_eq!(
                lookup(&fs, ROOT_INO, "new").await,
                Ok(made),
                "the inode stays"
            );
            let a = lookup_path(&fs, "new/a").await.expect("new/a");
            assert_eq!(listed(&fs, a).await, vec!["b.txt"]);
            let node = fs_node(&instance, &fs, "new").await;
            assert!(
                !node.is_dirty() && !node.ext_flags().contains(NodeExtFlags::LocalContent),
                "new is the revision's"
            );
            assert!(
                matches!(
                    fs.inodes().get(made).expect("new's inode").kind,
                    InodeKind::Dir { local: false, .. }
                ),
                "new's inode answers from the revision"
            );
            assert!(
                !instance.write_dir().join("new").exists(),
                "write/new is gone"
            );
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A sync whose target lacks a tracked directory removes the host directory a write below it
/// left under `write/`, so a later MKDIR of the name succeeds and a remount does not bring the
/// directory back.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_removing_a_directory_removes_its_host_directory() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let gone = lookup_path(&fs, "gone").await.expect("gone");
            lookup(&fs, gone, "x.txt").await.expect("x.txt");
            create_with(&fs, gone, "tmp", b"t").await;
            fs.unlink(&request(), gone, b"tmp").await.expect("unlink");
            assert!(instance.write_dir().join("gone").is_dir());

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            assert_eq!(lookup_path(&fs, "gone").await, Err(Errno::ENOENT));
            assert!(
                !instance.write_dir().join("gone").exists(),
                "write/gone outlives its directory"
            );
            fs.mkdir(&request(), ROOT_INO, b"gone", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir of the removed name");
        })
        .await;
}

/// `lore sync` back and forth between two revisions 1,000 files apart on a mounted virtual clone:
/// the verify, the in-place apply, the directory settle and the deletion-list rewrite.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run on a quiet machine"]
async fn sync_of_a_thousand_changes_bench() {
    skip_without_fuse!();
    const FILES: usize = 1000;
    const RUNS: usize = 7;
    let names: Vec<String> = (0..FILES)
        .map(|i| format!("d{:02}/f{i:04}.txt", i % 20))
        .collect();
    let contents = |tag: &str| -> Vec<Vec<u8>> {
        (0..FILES)
            .map(|i| format!("{tag} {i}").into_bytes())
            .collect()
    };
    let (first, second) = (contents("first"), contents("second"));
    let side = |contents: &[Vec<u8>]| -> Vec<(String, Vec<u8>)> {
        names
            .iter()
            .cloned()
            .zip(contents.iter().cloned())
            .collect()
    };
    let (first, second) = (side(&first), side(&second));
    let (first, second) = (as_files(&first), as_files(&second));
    let instance = Instance::with_revisions(&[&first, &second]).await;
    let revisions = [
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    ];
    let m = instance.mount().await;
    let mut times = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let started = std::time::Instant::now();
        sync_through(&instance, &m.fs, revisions[run % 2], Default::default())
            .await
            .expect("the sync");
        times.push(started.elapsed());
    }
    m.unmount().await;
    times.sort();
    println!(
        "sync of {FILES} changes: min {:?}, median {:?}, max {:?}",
        times[0],
        times[RUNS / 2],
        times[RUNS - 1]
    );
}

/// `side` as the fixture's file list.
fn as_files(side: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    side.iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect()
}

/// A swap hook that runs `step` with the provider at the first `phase` it sees, and passes every
/// phase.
fn at_first(
    fs: &Arc<FuseFilesystem>,
    phase: lore_revision::fs::hold::SwapPhase,
    step: impl Fn(Arc<FuseFilesystem>) -> lore_revision::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> lore_revision::fs::hold::SwapHook {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let armed = AtomicBool::new(true);
    let weak = Arc::downgrade(fs);
    Arc::new(move |seen| {
        let ran = (seen == phase && armed.swap(false, Ordering::SeqCst))
            .then(|| weak.upgrade())
            .flatten()
            .map(&step);
        Box::pin(async move {
            if let Some(ran) = ran {
                ran.await;
            }
            true
        })
    })
}

/// A WRITE copy-up and a CREATE landing in a directory between the settle's compare of its
/// children and the settle leave it dirty: status and the provider diff report both files.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_landing_while_a_directory_settles_keeps_it_dirty() {
    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup_path(&fs, "dir").await.expect("dir");
            let f = lookup(&fs, dir, "f.txt").await.expect("f.txt");
            fs.set_swap_hook(at_first(&fs, SwapPhase::Settle, move |fs| {
                Box::pin(async move {
                    fs.write(&request(), f, 0, 0, b"ONE", 0)
                        .await
                        .expect("a write while dir settles");
                    let flags = (libc::O_CREAT | libc::O_RDWR) as u32;
                    fs.create(
                        &request(),
                        dir,
                        b"new.txt",
                        libc::S_IFREG | 0o644,
                        0o022,
                        flags,
                    )
                    .await
                    .expect("a create while dir settles");
                })
            }));

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            assert!(
                fs_node(&instance, &fs, "dir").await.is_dirty(),
                "dir is clean over its changes"
            );
            let changed = changed_paths(&instance, &fs).await;
            assert!(
                changed.contains(&"dir/f.txt".to_string())
                    && changed.contains(&"dir/new.txt".to_string()),
                "{changed:?}"
            );
        })
        .await;
}

/// A forced sync whose target removes a directory removes the files the served revision holds
/// there, edited or not, and keeps the files added locally: they are still served, still under
/// `write/`, and status reports them added.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_sync_keeps_untracked_files_below_a_removed_directory() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let gone = lookup_path(&fs, "gone").await.expect("gone");
            let x = lookup(&fs, gone, "x.txt").await.expect("x.txt");
            fs.write(&request(), x, 0, 0, b"X!", 0)
                .await
                .expect("write");
            create_with(&fs, gone, "new.txt", b"untracked").await;

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            assert_eq!(lookup_path(&fs, "gone/x.txt").await, Err(Errno::ENOENT));
            assert!(!instance.write_dir().join("gone/x.txt").exists());
            let new = lookup_path(&fs, "gone/new.txt")
                .await
                .expect("the added file stays");
            assert_eq!(read(&fs, new, 64).await, b"untracked");
            assert!(instance.write_dir().join("gone/new.txt").exists());
            let changed = changed_paths(&instance, &fs).await;
            assert!(
                changed.contains(&"gone/new.txt".to_string())
                    && !changed.contains(&"gone/x.txt".to_string()),
                "{changed:?}"
            );
        })
        .await;
}

/// A LOOKUP that read a local file before a forced sync overwrote it, while no inode held the
/// file, and makes its inode after: READ through that inode serves the target's bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lookup_during_a_forced_overwrite_serves_the_target() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup_path(&fs, "dir").await.expect("dir");
            let f = lookup(&fs, dir, "f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");
            let f_node = node_at(&instance, &fs, "dir/f.txt").await;
            forget_all(&fs, &[f]);

            let gating = Arc::new(AtomicBool::new(false));
            let (admitting, admitted) = tokio::sync::oneshot::channel::<()>();
            let admitting = std::sync::Mutex::new(Some(admitting));
            let (release, released) = std::sync::mpsc::channel::<()>();
            let released = std::sync::Mutex::new(released);
            let gate = gating.clone();
            fs.set_write_hook(Arc::new(move |step| {
                if let WriteStep::Admitting { node } = step
                    && node == f_node
                    && gate.swap(false, Ordering::SeqCst)
                {
                    if let Some(admitting) = admitting.lock().expect("admitting").take() {
                        let _ = admitting.send(());
                    }
                    let _ = released.lock().expect("released").recv();
                }
                true
            }));
            let admitted = Arc::new(tokio::sync::Mutex::new(Some(admitted)));
            let release = Arc::new(std::sync::Mutex::new(Some(release)));
            let lookups = Arc::new(tokio::sync::Mutex::new(None));
            let found = Arc::new(std::sync::Mutex::new(None));
            let modified = Arc::new(AtomicBool::new(true));
            let weak = Arc::downgrade(&fs);
            let looked_up = found.clone();
            fs.set_swap_hook(Arc::new(move |phase| {
                let fs = weak.upgrade();
                let (gating, admitted, release, lookups, found, modified) = (
                    gating.clone(),
                    admitted.clone(),
                    release.clone(),
                    lookups.clone(),
                    looked_up.clone(),
                    modified.clone(),
                );
                Box::pin(async move {
                    match phase {
                        SwapPhase::Modify if modified.swap(false, Ordering::SeqCst) => {
                            gating.store(true, Ordering::SeqCst);
                            let fs = fs.expect("the provider");
                            let lookup =
                                lore_base::lore_spawn!(
                                    async move { lookup(&fs, dir, "f.txt").await }
                                );
                            *lookups.lock().await = Some(lookup);
                            if let Some(admitted) = admitted.lock().await.take() {
                                let _ = admitted.await;
                            }
                        }
                        SwapPhase::Placed => {
                            if let Some(release) = release.lock().expect("release").take() {
                                let _ = release.send(());
                            }
                            if let Some(lookup) = lookups.lock().await.take() {
                                *found.lock().expect("found") =
                                    Some(lookup.await.expect("the lookup task"));
                            }
                        }
                        _ => {}
                    }
                    true
                })
            }));

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            let ino = found
                .lock()
                .expect("found")
                .take()
                .expect("the lookup ran during the apply")
                .expect("the lookup found f.txt");
            assert_eq!(read(&fs, ino, 64).await, b"one");
        })
        .await;
}

/// A forced sync's overwrite of a local file an inode holds forgets the inode's host file, and a
/// later copy-up records the host file it makes.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_overwrite_forgets_the_inodes_host_file() {
    use std::os::unix::fs::MetadataExt as _;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");
            let host_file = || fs.inodes().get(f).map(|entry| entry.host_file);
            let on_disk = || {
                std::fs::metadata(instance.write_dir().join("dir/f.txt"))
                    .expect("write/dir/f.txt")
                    .ino()
            };
            assert_eq!(host_file(), Some(on_disk()), "the copy-up's host file");

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");
            assert_eq!(
                host_file(),
                Some(0),
                "the overwritten inode keeps a host file"
            );

            fs.write(&request(), f, 0, 0, b"X", 0)
                .await
                .expect("a write after the overwrite");
            assert_eq!(
                host_file(),
                Some(on_disk()),
                "the later copy-up's host file"
            );
        })
        .await;
}

/// A host file a READ opens for a local file, before a forced sync overwrites the file, is never
/// cached after it: no descriptor of the removed host file stands for the inode, and the next
/// READ serves the target's bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_descriptor_opened_before_a_forced_overwrite_is_not_cached() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(sync_instance().await);
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");
            let armed = Arc::new(AtomicBool::new(false));
            let raced = Arc::new(AtomicBool::new(false));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (armed, raced, instance) = (armed.clone(), raced.clone(), instance.clone());
                move |step| {
                    if step != (WriteStep::DescriptorOpened { ino: f })
                        || !armed.swap(false, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let (instance, raced) = (instance.clone(), raced.clone());
                    let racing = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            sync_through(
                                &instance,
                                &fs,
                                first,
                                lore_revision::interface::LoreGlobalArgs {
                                    force: 1,
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("the forced sync");
                            let overwritten = fs
                                .inodes()
                                .get(f)
                                .is_some_and(|entry| !entry.kind.is_local_file());
                            raced.store(overwritten, Ordering::SeqCst);
                        },
                    ));
                    // The sync may wait in this worker's queue: it is handed to another worker
                    // first.
                    tokio::task::block_in_place(|| {
                        while !racing.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    true
                }
            }));

            // With no descriptor cached, the READ opens one.
            fs.close_local_file(f);
            armed.store(true, Ordering::SeqCst);
            read(&fs, f, 64).await;
            assert!(
                raced.load(Ordering::SeqCst),
                "inside the READ, the forced sync overwrote the file"
            );
            assert_eq!(
                fs.descriptor(f).err(),
                Some(Errno::ENOENT),
                "the overwritten host file is cached for the inode"
            );
            assert_eq!(read(&fs, f, 64).await, b"one");
        })
        .await;
}

const SCRIPT_BYTES: &[u8] = b"#!/bin/sh\necho run\n";

/// An instance whose second revision makes `bin/run.sh` executable and changes nothing else.
async fn mode_instance() -> Instance {
    Instance {
        fixture: fuse_fixture::released_executable(
            &[
                &[("bin/run.sh", SCRIPT_BYTES)],
                &[("bin/run.sh", SCRIPT_BYTES)],
            ],
            &["bin/run.sh"],
        )
        .await,
        external: TempDir::new("lore-fuse-write-mount-instance-"),
    }
}

/// Whether the file at `path` reads as executable through `fs`.
async fn executable_at(fs: &FuseFilesystem, path: &str) -> bool {
    let ino = lookup_path(fs, path).await.expect("the file");
    fs.getattr(&request(), ino)
        .await
        .expect("getattr")
        .attr
        .mode
        & 0o111
        != 0
}

/// A sync and a switch whose targets change only a file's executable bit complete: the mount
/// serves the target's mode, and status reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_whose_verify_sets_a_mode_completes() {
    const STEP: Duration = Duration::from_secs(30);
    let instance = mode_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            assert!(executable_at(&fs, "bin/run.sh").await);
            tokio::time::timeout(STEP, create_branch_through(&instance, &fs, "other"))
                .await
                .expect("the branch create completes");
            tokio::time::timeout(STEP, switch_through(&instance, &fs, "other"))
                .await
                .expect("the switch to other completes")
                .expect("switch to other");

            tokio::time::timeout(
                STEP,
                sync_through(&instance, &fs, first, Default::default()),
            )
            .await
            .expect("the sync completes")
            .expect("a sync changing only the mode");
            assert!(!executable_at(&fs, "bin/run.sh").await);
            let changed = tokio::time::timeout(STEP, changed_paths(&instance, &fs))
                .await
                .expect("the status after the sync completes");
            assert_eq!(changed, Vec::<String>::new());

            tokio::time::timeout(
                STEP,
                switch_through(&instance, &fs, lore_revision::branch::DEFAULT_DEFAULT_NAME),
            )
            .await
            .expect("the switch back completes")
            .expect("a switch changing only the mode");
            assert!(executable_at(&fs, "bin/run.sh").await);
            let changed = tokio::time::timeout(STEP, changed_paths(&instance, &fs))
                .await
                .expect("the status after the switch completes");
            assert_eq!(changed, Vec::<String>::new());
        })
        .await;
}

/// A RENAME moving a local file out of a directory a forced sync removes, landing as the sync
/// is about to detach the file, keeps the renamed file and its host file.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_racing_a_forced_removal_keeps_the_renamed_file() {
    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let gone = lookup_path(&fs, "gone").await.expect("gone");
            let x = lookup(&fs, gone, "x.txt").await.expect("x.txt");
            fs.write(&request(), x, 0, 0, b"X!", 0)
                .await
                .expect("write");
            let keep = lookup_path(&fs, "keep").await.expect("keep");
            fs.set_swap_hook(at_first(&fs, SwapPhase::Detach, move |fs| {
                Box::pin(async move {
                    fs.rename(&request(), gone, b"x.txt", keep, b"x.txt", 0)
                        .await
                        .expect("a rename out of gone/");
                })
            }));

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            let moved = lookup_path(&fs, "keep/x.txt")
                .await
                .expect("the renamed file stays");
            assert_eq!(read(&fs, moved, 64).await, b"X!");
            assert!(instance.write_dir().join("keep/x.txt").exists());
        })
        .await;
}

/// A forced sync overwriting a local file spelled in another case than the target's leaves no
/// entry of it under `write/`, so a remount serves the target's file.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_overwrite_of_a_case_variant_local_file_survives_a_remount() {
    let instance = Instance::with_revisions(&[
        &[("case/foo", b"first-foo")],
        &[("case/foo", b"second-foo")],
    ])
    .await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let case = lookup_path(&fs, "case").await.expect("case");
            lookup(&fs, case, "foo").await.expect("foo");
            fs.rename(&request(), case, b"foo", case, b"Foo", 0)
                .await
                .expect("a rename to another spelling");
            let foo = lookup(&fs, case, "Foo").await.expect("Foo");
            fs.write(&request(), foo, 0, 0, b"LOCAL", 0)
                .await
                .expect("write");
            assert!(instance.write_dir().join("case/Foo").exists());

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            assert_eq!(
                names(&instance.write_dir().join("case")),
                Vec::<String>::new()
            );
            let remounted = provider_at(&instance, first).await;
            let foo = lookup_path(&remounted, "case/foo").await.expect("foo");
            assert_eq!(read(&remounted, foo, 64).await, b"first-foo");
        })
        .await;
}

/// Files the target changes below a directory deleted locally are restored, with the directory,
/// as an ordinary sync restores them.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_the_target_changes_under_a_locally_deleted_directory_is_restored() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup_path(&fs, "dir").await.expect("dir");
            lookup(&fs, dir, "f.txt").await.expect("f.txt");
            lookup(&fs, dir, "g.txt").await.expect("g.txt");
            fs.unlink(&request(), dir, b"f.txt").await.expect("unlink");
            fs.unlink(&request(), dir, b"g.txt").await.expect("unlink");
            fs.rmdir(&request(), ROOT_INO, b"dir").await.expect("rmdir");

            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");

            let f = lookup_path(&fs, "dir/f.txt")
                .await
                .expect("f.txt is restored");
            assert_eq!(read(&fs, f, 64).await, b"one");
            let g = lookup_path(&fs, "dir/g.txt")
                .await
                .expect("g.txt is restored");
            assert_eq!(read(&fs, g, 64).await, b"g-one");
            assert_eq!(deleted(&instance), Vec::<String>::new());
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A WRITE through a file's inode, landing after a forced sync overwrote the file and before the
/// sync plans its invalidations, wins: READ through that inode serves the user's bytes, `write/`
/// holds them, and status shows the file modified.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_between_a_forced_overwrite_and_the_plan_is_served() {
    use lore_revision::fs::hold::SwapPhase;

    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let f = lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            fs.write(&request(), f, 0, 0, b"TWO!", 0)
                .await
                .expect("write");
            fs.set_swap_hook(at_first(&fs, SwapPhase::Settle, move |fs| {
                Box::pin(async move {
                    fs.write(&request(), f, 0, 0, b"USER", 0)
                        .await
                        .expect("a write after the overwrite");
                })
            }));

            sync_through(
                &instance,
                &fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("the forced sync");

            assert_eq!(read(&fs, f, 64).await, b"USER");
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/f.txt")).expect("write/dir/f.txt"),
                b"USER"
            );
            let changed = changed_paths(&instance, &fs).await;
            assert!(changed.contains(&"dir/f.txt".to_string()), "{changed:?}");
        })
        .await;
}

/// A file the user made at the name of a directory they deleted refuses a sync whose target
/// changes a file below that directory with `LocalModifications`, as applying the change needs the
/// directory back in place of the file. Nothing is applied, the file stays, and status is as it
/// was.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_at_a_locally_deleted_directorys_name_refuses_a_sync_changing_below_it() {
    let instance = sync_instance().await;
    let (first, second) = (
        instance.fixture.state.parent_self(),
        instance.fixture.state.revision(),
    );
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup_path(&fs, "dir").await.expect("dir");
            lookup(&fs, dir, "f.txt").await.expect("f.txt");
            lookup(&fs, dir, "g.txt").await.expect("g.txt");
            fs.unlink(&request(), dir, b"f.txt").await.expect("unlink");
            fs.unlink(&request(), dir, b"g.txt").await.expect("unlink");
            fs.rmdir(&request(), ROOT_INO, b"dir").await.expect("rmdir");
            create_with(&fs, ROOT_INO, "dir", b"mine").await;
            let before = status_of(&instance, &fs).await;

            let synced = sync_through(&instance, &fs, first, Default::default()).await;

            assert!(
                matches!(
                    synced,
                    Err(lore_revision::revision::sync::SyncError::LocalModifications(_))
                ),
                "{synced:?}"
            );
            assert_eq!(fs.served().revision(), second);
            assert_eq!(current_revision(&instance).await, second);
            let file = lookup_path(&fs, "dir").await.expect("dir");
            assert_eq!(read(&fs, file, 64).await, b"mine");
            assert_eq!(status_of(&instance, &fs).await, before);
        })
        .await;
}

/// A sync that rewrites a file the kernel looked up keeps nothing of the command after it ends:
/// the next command takes the repository's write token at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_sync_rewriting_a_looked_up_file_releases_the_write_token() {
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup_path(&fs, "dir/f.txt").await.expect("f.txt");
            sync_through(&instance, &fs, first, Default::default())
                .await
                .expect("the sync");
            let token = tokio::time::timeout(
                Duration::from_secs(10),
                lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path),
            )
            .await
            .expect("the write token is free after the sync");
            drop(token);
        })
        .await;
}

/// Creates branch `name` at the current revision through `fs`, answering its id.
async fn create_branch_id_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    name: &str,
) -> lore_revision::lore::BranchId {
    use lore_revision::repository::RepositoryWriteToken;

    let id = lore_revision::lore::BranchId::from(uuid::Uuid::now_v7());
    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            Box::pin(lore_revision::branch::create::create(
                context,
                &token,
                name.to_string(),
                Some(id),
                String::new(),
                false,
            )),
        )
        .await
        .expect("the branch is created");
    id
}

/// Runs `lore merge start <branch> --no-commit` through `fs`.
async fn merge_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    branch: lore_revision::lore::BranchId,
) -> Result<Hash, String> {
    use lore_revision::branch::merge;
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    let offline = Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    );
    LORE_CONTEXT
        .scope(
            offline,
            Box::pin(merge::merge_start(
                context,
                &token,
                branch,
                merge::MergeStartOptions {
                    message: String::new(),
                    link_messages: Default::default(),
                    no_commit: true,
                    scope: merge::MergeScope::MainOnly,
                    inherit_metadata: Default::default(),
                },
            )),
        )
        .await
        .map_err(|error| format!("{error:?}"))
}

/// Runs `lore revision cherry-pick <revision> --no-commit` through `fs`.
async fn cherry_pick_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    revision: Hash,
) -> Result<Hash, lore_revision::branch::merge::MergeError> {
    use lore_revision::repository::RepositoryWriteToken;
    use lore_revision::revision::cherry_pick;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    let offline = Arc::new(
        lore_revision::interface::ExecutionContext::new_client_with_user_id(
            lore_revision::interface::LoreGlobalArgs::default().set_offline(),
            lore_revision::relay::EventDispatcher::no_dispatch(),
            "test-user".to_string(),
        ),
    );
    LORE_CONTEXT
        .scope(
            offline,
            cherry_pick::cherry_pick_boxed(
                context,
                &token,
                revision,
                cherry_pick::CherryPickOptions {
                    message: String::new(),
                    no_commit: true,
                    inherit_metadata: Default::default(),
                },
            ),
        )
        .await
}

/// Runs `lore reset <paths>` through `fs`, purging untracked files where `purge`.
async fn reset_through(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    paths: &[&str],
    purge: bool,
) -> Result<(), String> {
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::repository::RepositoryWriteToken;

    let token = RepositoryWriteToken::acquire(&instance.fixture.path).await;
    let context = instance.command_context(fs, Some(token.share()));
    let paths = LoreArray::from_vec(
        paths
            .iter()
            .map(|path| LoreString::from(&instance.fixture.path.join(path)))
            .collect(),
    );
    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            Box::pin(lore_revision::file::reset::reset(
                context,
                &token,
                paths,
                lore_revision::interface::LoreString::default(),
                lore_revision::file::reset::ResetOptions {
                    purge,
                    single_node: false,
                },
            )),
        )
        .await
        .map_err(|error| format!("{error:?}"))
}

/// Status as each path's action and whether the working tree changed it past what is staged.
async fn status_of(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
) -> std::collections::BTreeMap<String, (lore_revision::interface::LoreFileAction, bool)> {
    staged_state::status_files(instance.command_context(fs, None), false).await
}

/// The bytes served at `path`.
async fn served_at(fs: &FuseFilesystem, path: &str) -> Vec<u8> {
    let ino = lookup_path(fs, path)
        .await
        .unwrap_or_else(|errno| panic!("{path}: {errno:?}"));
    read(fs, ino, 256).await
}

/// A reset of an edited file serves the revision through the inode a reader holds open, and
/// status reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn reset_of_an_edited_file_serves_the_revision_to_an_open_reader_and_status_is_clean() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA!", 0)
                .await
                .expect("write");
            assert_eq!(read(&fs, a, 64).await, b"ALPHA!");

            reset_through(&instance, &fs, &["a.txt"], false)
                .await
                .expect("the reset");

            assert_eq!(read(&fs, a, 64).await, b"alpha");
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A reset of an edited file to the mounted revision serves it from the store again: its node is
/// clean, tracked content, and `write/` holds nothing for it.
#[tokio::test(flavor = "multi_thread")]
async fn reset_of_an_edited_file_leaves_nothing_under_write() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA!", 0)
                .await
                .expect("write");

            reset_through(&instance, &fs, &["a.txt"], false)
                .await
                .expect("the reset");

            assert!(!instance.write_dir().join("a.txt").exists());
            let node = fs_node(&instance, &fs, "a.txt").await;
            assert!(!node.is_dirty(), "a.txt is dirty");
            assert!(
                !node
                    .ext_flags()
                    .contains(lore_revision::node::NodeExtFlags::LocalContent),
                "a.txt is local content"
            );
            assert_eq!(read(&fs, a, 64).await, b"alpha");
        })
        .await;
}

/// How a file made again over its deletion got there, before a reset.
#[derive(Clone, Copy, Debug)]
enum MadeAgain {
    /// Unlinked, then created.
    Created,
    /// Unlinked, then another file renamed onto it.
    RenamedOnto,
    /// Renamed away, edited, and renamed back: the revision's own node, over the deletion its
    /// rename away recorded.
    RenamedBack,
}

/// A reset of a tracked file the mount deleted and then made again, by a create, by a rename of
/// another file onto its path, or by renaming it away and back, serves the revision through the
/// inode a reader holds, and keeps serving it after a remount: the file stays in `write/`, over
/// the deletion the list records, and status reports nothing before or after.
#[tokio::test(flavor = "multi_thread")]
async fn reset_of_a_file_made_again_over_its_deletion_survives_a_remount() {
    for made in [
        MadeAgain::Created,
        MadeAgain::RenamedOnto,
        MadeAgain::RenamedBack,
    ] {
        let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
                match made {
                    MadeAgain::Created => {
                        fs.unlink(&request(), ROOT_INO, b"a.txt")
                            .await
                            .expect("unlink");
                        create_with(&fs, ROOT_INO, "a.txt", b"other").await;
                    }
                    MadeAgain::RenamedOnto => {
                        fs.unlink(&request(), ROOT_INO, b"a.txt")
                            .await
                            .expect("unlink");
                        create_with(&fs, ROOT_INO, "other.txt", b"other").await;
                        fs.rename(&request(), ROOT_INO, b"other.txt", ROOT_INO, b"a.txt", 0)
                            .await
                            .expect("rename onto a.txt");
                    }
                    MadeAgain::RenamedBack => {
                        fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"away.txt", 0)
                            .await
                            .expect("rename away");
                        fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                            .await
                            .expect("edit");
                        fs.rename(&request(), ROOT_INO, b"away.txt", ROOT_INO, b"a.txt", 0)
                            .await
                            .expect("rename back");
                    }
                }
                let held = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt again");

                reset_through(&instance, &fs, &["a.txt"], false)
                    .await
                    .expect("the reset");

                assert_eq!(read(&fs, held, 64).await, b"alpha", "{made:?}");
                assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
                drop(fs);

                let fs = provider(&instance).await;
                assert_eq!(served_at(&fs, "a.txt").await, b"alpha", "{made:?}");
                assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
            })
            .await;
    }
}

/// How a symlink came to stand at a tracked file's path, before a reset.
#[derive(Clone, Copy, Debug)]
enum LinkedOver {
    /// Unlinked, then a symlink made at the path, as `rm` then `ln -s` do.
    Unlinked,
    /// A symlink made beside it and renamed over it, as an atomic `ln -sf` does.
    RenamedOver,
}

/// A reset of a tracked file a symlink to another tracked file replaced takes the symlink away,
/// never writing through it: the path serves the revision's file as a regular file, after a
/// remount too, the other file is unchanged and stays tracked, no symlink stays under `write/`,
/// and status reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn reset_of_a_tracked_file_a_symlink_replaced_serves_the_revisions_file() {
    for how in [LinkedOver::Unlinked, LinkedOver::RenamedOver] {
        let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
                match how {
                    LinkedOver::Unlinked => {
                        fs.unlink(&request(), ROOT_INO, b"a.txt")
                            .await
                            .expect("unlink");
                        fs.symlink(&request(), ROOT_INO, b"a.txt", b"b.txt")
                            .await
                            .expect("symlink");
                    }
                    LinkedOver::RenamedOver => {
                        fs.symlink(&request(), ROOT_INO, b"a.txt.tmp", b"b.txt")
                            .await
                            .expect("symlink");
                        fs.rename(&request(), ROOT_INO, b"a.txt.tmp", ROOT_INO, b"a.txt", 0)
                            .await
                            .expect("rename over a.txt");
                    }
                }
                let kind = async |name: &str| {
                    let ino = lookup(&fs, ROOT_INO, name).await.expect(name);
                    let attr = fs.getattr(&request(), ino).await.expect("getattr").attr;
                    attr.mode & libc::S_IFMT
                };
                assert_eq!(kind("a.txt").await, libc::S_IFLNK, "{how:?}");

                reset_through(&instance, &fs, &["a.txt"], false)
                    .await
                    .expect("the reset");

                assert_eq!(kind("a.txt").await, libc::S_IFREG, "{how:?}");
                assert_eq!(served_at(&fs, "a.txt").await, b"alpha", "{how:?}");
                assert_eq!(served_at(&fs, "b.txt").await, b"bee", "{how:?}");
                assert!(!instance.write_dir().join("b.txt").exists(), "{how:?}");
                assert!(
                    std::fs::symlink_metadata(instance.write_dir().join("a.txt"))
                        .is_ok_and(|host| host.file_type().is_file()),
                    "{how:?}: a.txt stands under write/ as a file, over its deletion"
                );
                assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
                drop(fs);

                let fs = provider(&instance).await;
                assert_eq!(served_at(&fs, "a.txt").await, b"alpha", "{how:?}");
                assert_eq!(served_at(&fs, "b.txt").await, b"bee", "{how:?}");
                assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
            })
            .await;
    }
}

/// A tracked file replaced through the mount by a symlink to another tracked file reads, after a
/// reset, as the revision's regular file through the mount, and the other file is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn reset_of_a_tracked_file_a_symlink_replaced_through_the_mount_reads_the_revisions_file() {
    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || {
        std::fs::remove_file(root.join("a.txt")).expect("rm a.txt");
        std::os::unix::fs::symlink("b.txt", root.join("a.txt")).expect("ln -s b.txt a.txt");
        assert_eq!(std::fs::read(root.join("a.txt")).expect("a.txt"), b"bee");
    })
    .await;

    reset_through(&instance, &m.fs, &["a.txt"], false)
        .await
        .expect("the reset");

    let root = m.root.clone();
    let (regular, a, b) = blocking(move || {
        let a = root.join("a.txt");
        (
            std::fs::symlink_metadata(&a)
                .expect("a.txt")
                .file_type()
                .is_file(),
            std::fs::read(&a).expect("a.txt"),
            std::fs::read(root.join("b.txt")).expect("b.txt"),
        )
    })
    .await;
    assert!(regular, "a.txt is a regular file again");
    assert_eq!((a, b), (b"alpha".to_vec(), b"bee".to_vec()));
    assert_eq!(changed_paths(&instance, &m.fs).await, Vec::<String>::new());
    m.unmount().await;
}

/// An instance operation reads the deletion list again once a write has appended to it since
/// its last read: a file renamed away and back after the operation first read the list is the
/// revision's own node over a recorded deletion, so a restore of it writes it under `write/`
/// rather than serving it from the store, and it survives a remount.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_reads_the_deletion_list_again_once_a_write_appended_to_it() {
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::operation::FuseOperation;
    use lore_revision::util::path::RelativePath;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let revision_node = fs_node(&instance, &fs, "a.txt").await;
            let at = RelativePath::new_from_initial_path("a.txt").expect("path");
            let operation = FuseOperation::new(fs.clone());
            let repository = instance.fixture.repository.clone();
            // The first restore reads the list, which holds nothing.
            operation
                .write_node(repository.clone(), &revision_node, &at)
                .await
                .expect("the restore of a clean file");
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.rename(&request(), ROOT_INO, b"a.txt", ROOT_INO, b"away.txt", 0)
                .await
                .expect("rename away");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("edit");
            fs.rename(&request(), ROOT_INO, b"away.txt", ROOT_INO, b"a.txt", 0)
                .await
                .expect("rename back");

            operation
                .write_node(repository, &revision_node, &at)
                .await
                .expect("the restore over the deletion");
            drop(operation);
            assert_eq!(read(&fs, a, 64).await, b"alpha");
            assert!(
                instance.write_dir().join("a.txt").exists(),
                "a.txt stands under write/ over its deletion"
            );
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(served_at(&fs, "a.txt").await, b"alpha", "after a remount");
        })
        .await;
}

/// An instance operation that cannot read the deletion list takes every path as listed: a restore
/// of an edited file writes the revision's content under `write/` rather than serving it from the
/// store, which is right whatever the list holds.
#[tokio::test(flavor = "multi_thread")]
async fn an_operation_that_cannot_read_the_deletion_list_writes_a_restored_file() {
    use std::os::unix::fs::PermissionsExt as _;

    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::operation::FuseOperation;
    use lore_revision::util::path::RelativePath;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let revision_node = fs_node(&instance, &fs, "a.txt").await;
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("edit");
            let list = instance.external.path().join("deleted");
            let mode = |bits: u32| {
                std::fs::set_permissions(&list, std::fs::Permissions::from_mode(bits))
                    .expect("chmod the list");
            };
            mode(0o200);
            if std::fs::read(&list).is_ok() {
                mode(0o600);
                eprintln!("skipped: a list the process may not read reads anyway, as for root");
                return;
            }

            let restored = FuseOperation::new(fs.clone())
                .write_node(
                    instance.fixture.repository.clone(),
                    &revision_node,
                    &RelativePath::new_from_initial_path("a.txt").expect("path"),
                )
                .await;
            mode(0o600);
            restored.expect("the restore");
            assert_eq!(read(&fs, a, 64).await, b"alpha");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"alpha",
                "the revision's content is written under write/"
            );
        })
        .await;
}

/// A purging reset removes a file added locally and restores one deleted locally, and status
/// reports nothing.
#[tokio::test(flavor = "multi_thread")]
async fn reset_purge_removes_added_files_and_restores_deleted_ones() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("dir/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "new.txt", b"new").await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            lookup(&fs, dir, "b.txt").await.expect("b.txt");
            fs.unlink(&request(), dir, b"b.txt").await.expect("unlink");

            reset_through(&instance, &fs, &[""], true)
                .await
                .expect("the reset");

            assert_eq!(lookup(&fs, ROOT_INO, "new.txt").await, Err(Errno::ENOENT));
            assert!(!instance.write_dir().join("new.txt").exists());
            assert_eq!(served_at(&fs, "dir/b.txt").await, b"bee");
            assert_eq!(changed_paths(&instance, &fs).await, Vec::<String>::new());
        })
        .await;
}

/// A branch `feature` off `instance`'s revision whose commit rewrites `a.txt`, adds `new.txt` and
/// deletes `b.txt`, with the mount back on the default branch; answers the branch.
async fn feature_branch(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
) -> lore_revision::lore::BranchId {
    feature_branch_and_revision(instance, fs).await.0
}

/// [`feature_branch`], and the revision its commit stored.
async fn feature_branch_and_revision(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
) -> (lore_revision::lore::BranchId, Hash) {
    let feature = create_branch_id_through(instance, fs, "feature").await;
    switch_through(instance, fs, "feature")
        .await
        .expect("switch to feature");
    let a = lookup(fs, ROOT_INO, "a.txt").await.expect("a.txt");
    fs.write(&request(), a, 0, 0, b"ALPHA", 0)
        .await
        .expect("write");
    create_with(fs, ROOT_INO, "new.txt", b"new").await;
    lookup(fs, ROOT_INO, "b.txt").await.expect("b.txt");
    fs.unlink(&request(), ROOT_INO, b"b.txt")
        .await
        .expect("unlink");
    let revision = commit_through(instance, fs, false)
        .await
        .expect("commit on feature");
    switch_through(instance, fs, lore_revision::branch::DEFAULT_DEFAULT_NAME)
        .await
        .expect("switch back");
    (feature, revision)
}

/// A merge serves the incoming changes through the mount, and status shows them staged with no
/// change in the working tree past that.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_writes_incoming_changes_into_the_mount_and_status_shows_them_staged_not_modified()
{
    use lore_revision::interface::LoreFileAction;

    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = feature_branch(&instance, &fs).await;

            merge_through(&instance, &fs, feature)
                .await
                .expect("the merge");

            assert_eq!(served_at(&fs, "a.txt").await, b"ALPHA");
            assert_eq!(served_at(&fs, "new.txt").await, b"new");
            assert_eq!(lookup(&fs, ROOT_INO, "b.txt").await, Err(Errno::ENOENT));
            let status = status_of(&instance, &fs).await;
            assert_eq!(
                status.into_iter().collect::<Vec<_>>(),
                vec![
                    ("a.txt".to_string(), (LoreFileAction::Keep, false)),
                    ("b.txt".to_string(), (LoreFileAction::Delete, false)),
                    ("new.txt".to_string(), (LoreFileAction::Add, false)),
                ]
            );
        })
        .await;
}

/// The root as a readdir through the mount lists it, as the provider's READDIR answers it, and as
/// the operation's listing walks it.
///
/// The kernel serves a directory's listing from its cache (`CACHE_DIR`) from the second open on, so
/// a test that checks the cache lists the root twice before the change.
async fn root_listings(m: &Mounted) -> [Vec<String>; 3] {
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::operation::FuseOperation;

    let root = m.root.clone();
    let through_mount = blocking(move || names(&root)).await;
    let provider = listed(&m.fs, ROOT_INO).await;
    let mut operation = Vec::new();
    let mut listing = FuseOperation::new(m.fs.clone())
        .read_directory(&lore_revision::util::path::RelativePath::new())
        .await
        .expect("the operation's listing");
    while let Some(entry) = listing.next().await {
        operation.push(entry.expect("entry").name);
    }
    operation.sort();
    [through_mount, provider, operation]
}

fn expected_listings(names: &[&str]) -> [Vec<String>; 3] {
    let names: Vec<String> = names.iter().map(ToString::to_string).collect();
    [names.clone(), names.clone(), names]
}

/// A merge whose only change in the root adds a name the kernel never looked up shows it in a
/// listing through the mount whose root the kernel listed before, as the provider lists it.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_adding_only_a_new_name_lists_it_through_the_mount() {
    skip_without_fuse!();
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    let m = instance.mount().await;
    let feature = LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let feature = create_branch_id_through(&instance, &m.fs, "feature").await;
            switch_through(&instance, &m.fs, "feature")
                .await
                .expect("switch to feature");
            create_with(&m.fs, ROOT_INO, "theirs.txt", b"theirs").await;
            commit_through(&instance, &m.fs, false)
                .await
                .expect("commit on feature");
            switch_through(
                &instance,
                &m.fs,
                lore_revision::branch::DEFAULT_DEFAULT_NAME,
            )
            .await
            .expect("switch back");
            feature
        })
        .await;
    for _ in 0..2 {
        assert_eq!(root_listings(&m).await, expected_listings(&["a.txt"]));
    }

    merge_through(&instance, &m.fs, feature)
        .await
        .expect("the merge");
    let merged = root_listings(&m).await;
    commit_through(&instance, &m.fs, false)
        .await
        .expect("the merge commit");
    let committed = root_listings(&m).await;

    let after = expected_listings(&["a.txt", "theirs.txt"]);
    assert_eq!(
        (merged, committed),
        (after.clone(), after),
        "[through the mount, the provider's READDIR, the operation] after the merge, then after \
         its commit"
    );
    m.unmount().await;
}

/// Every namespace change an instance operation makes in a directory whose listing the kernel
/// cached shows in the next listing through the mount, as the provider lists it, a new name the
/// kernel never looked up too.
#[tokio::test(flavor = "multi_thread")]
async fn instance_namespace_changes_show_in_a_listing_the_kernel_cached() {
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::operation::FuseOperation;
    use lore_revision::util::path::RelativePath;

    skip_without_fuse!();
    let path = |at: &str| RelativePath::new_from_initial_path(at).expect("path");
    let mut stale = Vec::new();
    for (change, after) in [
        ("write", &["a.txt", "d", "new.txt"][..]),
        ("create_dir_all", &["a.txt", "d", "e"][..]),
        ("remove", &["d"][..]),
        ("rename", &["b.txt", "d"][..]),
        ("remove_recursive", &["a.txt"][..]),
    ] {
        let instance = Instance::new(&[("a.txt", b"alpha"), ("d/x.txt", b"x")]).await;
        let m = instance.mount().await;
        for _ in 0..2 {
            assert_eq!(root_listings(&m).await, expected_listings(&["a.txt", "d"]));
        }
        let operation = FuseOperation::new(m.fs.clone());
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                match change {
                    "write" => {
                        operation
                            .write_file(&path("new.txt"), bytes::Bytes::from_static(b"new"))
                            .await
                    }
                    "create_dir_all" => operation.create_dir_all(&path("e/f")).await,
                    "remove" => operation.remove(&path("a.txt")).await,
                    "rename" => operation.rename(&path("a.txt"), &path("b.txt")).await,
                    _ => operation.remove_recursive(&path("d")).await,
                }
                .unwrap_or_else(|error| panic!("{change}: {error}"));
            })
            .await;
        let [through_mount, provider, _] = root_listings(&m).await;
        assert_eq!(provider, after, "{change}: the provider lists the change");
        if through_mount != provider {
            stale.push((change, through_mount, provider));
        }
        m.unmount().await;
    }
    assert_eq!(
        stale,
        Vec::new(),
        "(change, listed through the mount, listed by the provider)"
    );
}

/// An instance write that changes a file the kernel holds drops what it cached of the file: a
/// descriptor held across the change reads the new content, and a stat shows the new mode. Each
/// change keeps the length, so only the inode's invalidation shows: content written, the file
/// made executable, and an edit restored to the revision.
#[tokio::test(flavor = "multi_thread")]
async fn instance_content_changes_reach_what_the_kernel_cached_of_the_file() {
    use std::os::unix::fs::FileExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::operation::FuseOperation;
    use lore_revision::util::path::RelativePath;

    skip_without_fuse!();
    let path = |at: &str| RelativePath::new_from_initial_path(at).expect("path");
    for change in ["write", "make_executable", "restore"] {
        let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
        let m = instance.mount().await;
        let revision_node = fs_node(&instance, &m.fs, "a.txt").await;
        let file = m.root.join("a.txt");
        let edited = change == "restore";
        let (held, mode) = blocking({
            let file = file.clone();
            move || {
                if edited {
                    std::fs::write(&file, b"ALPHA").expect("edit a.txt");
                }
                let held = std::fs::File::open(&file).expect("open a.txt");
                let mut cached = [0u8; 5];
                held.read_exact_at(&mut cached, 0).expect("read a.txt");
                let mode = std::fs::metadata(&file).expect("stat a.txt").permissions();
                (held, (cached, mode.mode()))
            }
        })
        .await;
        assert_eq!(
            &mode.0,
            if edited { b"ALPHA" } else { b"alpha" },
            "{change}"
        );
        assert_eq!(mode.1 & 0o111, 0, "{change}: a.txt is not executable");

        let operation = FuseOperation::new(m.fs.clone());
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                match change {
                    "write" => {
                        operation
                            .write_file(&path("a.txt"), bytes::Bytes::from_static(b"OMEGA"))
                            .await
                    }
                    "make_executable" => operation.make_executable(&path("a.txt"), true).await,
                    _ => operation
                        .write_node(
                            instance.fixture.repository.clone(),
                            &revision_node,
                            &path("a.txt"),
                        )
                        .await
                        .map(|_| ()),
                }
                .unwrap_or_else(|error| panic!("{change}: {error}"));
            })
            .await;
        drop(operation);

        let (read, mode) = blocking(move || {
            let mut read = [0u8; 5];
            held.read_exact_at(&mut read, 0).expect("read a.txt again");
            let mode = std::fs::metadata(&file).expect("stat a.txt").permissions();
            (read, mode.mode())
        })
        .await;
        match change {
            "write" => assert_eq!(&read, b"OMEGA", "{change}"),
            "make_executable" => assert_ne!(mode & 0o111, 0, "{change}: the new mode shows"),
            _ => assert_eq!(&read, b"alpha", "{change}"),
        }
        m.unmount().await;
    }
}

/// A remount while a merge is in progress keeps what the merge wrote: the incoming files from
/// `write/`, the deletion from the deletion list.
#[tokio::test(flavor = "multi_thread")]
async fn a_remount_in_the_middle_of_a_merge_keeps_what_the_merge_wrote() {
    let instance = Instance::new(&[("a.txt", b"alpha"), ("b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = feature_branch(&instance, &fs).await;
            merge_through(&instance, &fs, feature)
                .await
                .expect("the merge");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(served_at(&fs, "a.txt").await, b"ALPHA");
            assert_eq!(served_at(&fs, "new.txt").await, b"new");
            assert_eq!(lookup(&fs, ROOT_INO, "b.txt").await, Err(Errno::ENOENT));
        })
        .await;
}

/// A merge whose sides both change a text file serves it with conflict markers, beside the
/// base, mine and theirs copies.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_merge_serves_the_file_with_markers_and_its_side_files() {
    let instance = Instance::new(&[("a.txt", b"base\n")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let feature = create_branch_id_through(&instance, &fs, "feature").await;
            switch_through(&instance, &fs, "feature")
                .await
                .expect("switch to feature");
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"theirs\n", 0)
                .await
                .expect("write");
            commit_through(&instance, &fs, false)
                .await
                .expect("commit on feature");
            switch_through(&instance, &fs, lore_revision::branch::DEFAULT_DEFAULT_NAME)
                .await
                .expect("switch back");
            let a = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"mine\n", 0)
                .await
                .expect("write");
            commit_through(&instance, &fs, false)
                .await
                .expect("commit on main");

            let _ = merge_through(&instance, &fs, feature).await;

            let merged = String::from_utf8(served_at(&fs, "a.txt").await).expect("text");
            assert!(
                merged.contains("<<<<<<<") && merged.contains("mine") && merged.contains("theirs"),
                "{merged}"
            );
            assert_eq!(served_at(&fs, "a.txt~base").await, b"base\n");
            assert_eq!(served_at(&fs, "a.txt~mine").await, b"mine\n");
            assert_eq!(served_at(&fs, "a.txt~theirs").await, b"theirs\n");
        })
        .await;
}

/// An instance write held between finding nothing at its name and creating the file, while the
/// user creates that name and another in the same directory, leaves both of the user's names: the
/// instance's file takes the name the user made, with the instance's bytes, and the other keeps
/// the user's.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_write_racing_a_create_in_the_same_directory_leaves_both() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::operation::FuseOperation;

    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let armed = Arc::new(AtomicBool::new(true));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let armed = armed.clone();
                move |step| {
                    if step != (WriteStep::InstanceFoundNothing { parent: dir })
                        || !armed.swap(false, Ordering::SeqCst)
                    {
                        return true;
                    }
                    let Some(fs) = weak.upgrade() else {
                        return true;
                    };
                    let racing = runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let made = create_with(&fs, dir, "lore.txt", b"user's").await;
                            fs.forget(made, 1);
                            create_with(&fs, dir, "user.txt", b"user").await;
                        },
                    ));
                    // The creates may wait in this worker's queue: they are handed to another
                    // worker first.
                    tokio::task::block_in_place(|| {
                        while !racing.is_finished() {
                            std::thread::yield_now();
                        }
                    });
                    true
                }
            }));

            FuseOperation::new(fs.clone())
                .write_file(
                    &lore_revision::util::path::RelativePath::new_from_initial_path("dir/lore.txt")
                        .expect("path"),
                    bytes::Bytes::from_static(b"lore"),
                )
                .await
                .expect("the instance write");

            assert!(!armed.load(Ordering::SeqCst), "the creates raced the write");
            assert_eq!(served_at(&fs, "dir/lore.txt").await, b"lore");
            assert_eq!(served_at(&fs, "dir/user.txt").await, b"user");
            assert_eq!(
                listed(&fs, dir).await,
                vec!["a.txt", "lore.txt", "user.txt"]
            );
            assert_eq!(
                std::fs::read(instance.write_dir().join("dir/lore.txt"))
                    .expect("write/dir/lore.txt"),
                b"lore"
            );
        })
        .await;
}

/// A tree by path: `None` for a directory, the content for a file.
type Tree = std::collections::BTreeMap<String, Option<Vec<u8>>>;

/// The tree a walk through the mount at `root` finds, with ordinary system calls.
async fn walked(root: &Path) -> Tree {
    fn walk(root: &Path, dir: &Path, tree: &mut Tree) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dirent");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("below the root")
                .to_string_lossy()
                .into_owned();
            if entry.file_type().expect("file type").is_dir() {
                tree.insert(relative, None);
                walk(root, &path, tree);
            } else {
                tree.insert(relative, Some(std::fs::read(&path).expect("read")));
            }
        }
    }
    let root = root.to_path_buf();
    blocking(move || {
        let mut tree = Tree::new();
        walk(&root, &root, &mut tree);
        tree
    })
    .await
}

/// The tree `revision` holds, read through a read-only provider over it.
async fn revision_tree(instance: &Instance, revision: Hash) -> Tree {
    let state = state_at(instance, revision).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = FuseFilesystem::new(
                instance.fixture.repository.clone(),
                state,
                AttrContext::for_mounting_user(MAX_READ, BASE_TIME),
            )
            .await
            .expect("a provider over the revision");
            let mut tree = Tree::new();
            let mut pending = vec![(ROOT_INO, String::new())];
            while let Some((dir, prefix)) = pending.pop() {
                for name in listed(&fs, dir).await {
                    let ino = lookup(&fs, dir, &name).await.expect("a listed name");
                    let path = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    let mode = fs
                        .getattr(&request(), ino)
                        .await
                        .expect("getattr")
                        .attr
                        .mode;
                    if mode & libc::S_IFMT == libc::S_IFDIR {
                        tree.insert(path.clone(), None);
                        pending.push((ino, path));
                    } else {
                        tree.insert(path, Some(read(&fs, ino, 1 << 16).await));
                    }
                }
            }
            tree
        })
        .await
}

/// What status reports for a working tree `walked` over `revision`: each path the working tree
/// adds, modifies (kept, with new content) or deletes.
fn status_for(
    revision: &Tree,
    walked: &Tree,
) -> std::collections::BTreeMap<String, (lore_revision::interface::LoreFileAction, bool)> {
    use lore_revision::interface::LoreFileAction;

    let mut status = std::collections::BTreeMap::new();
    for (path, content) in walked {
        match revision.get(path) {
            None => {
                status.insert(path.clone(), (LoreFileAction::Add, true));
            }
            Some(committed) if committed != content => {
                status.insert(path.clone(), (LoreFileAction::Keep, true));
            }
            Some(_) => {}
        }
    }
    for path in revision.keys().filter(|path| !walked.contains_key(*path)) {
        status.insert(path.clone(), (LoreFileAction::Delete, true));
    }
    status
}

/// How the mount at `m` disagrees after `step` with `working`, what the steps wrote, and how
/// status disagrees with a walk of the mount over the instance's current revision.
async fn disagreements(
    instance: &Instance,
    m: &Mounted,
    working: &Tree,
    step: &str,
) -> Vec<String> {
    let mut found = Vec::new();
    let walk = walked(&m.root).await;
    if walk != *working {
        found.push(format!(
            "{step}: the mount serves {walk:?}, the steps wrote {working:?}"
        ));
    }
    let revision = revision_tree(instance, current_revision(instance).await).await;
    let expected = status_for(&revision, &walk);
    let status = status_of(instance, &m.fs).await;
    if status != expected {
        found.push(format!(
            "{step}: status reports {status:?}, the walk over the revision {expected:?}"
        ));
    }
    found
}

/// Status, stage, commit, sync and reset agree with what the mount serves: after a write, its
/// commit, an edit and a save through a temporary, a sync and a reset, run back to back, a walk
/// of the mount holds what the steps wrote and status reports exactly what the walk changes over
/// the current revision, and a remount over that revision agrees again.
#[tokio::test(flavor = "multi_thread")]
async fn status_after_write_commit_edit_sync_and_reset_matches_what_the_mount_serves() {
    skip_without_fuse!();
    let first: &[(&str, &[u8])] = &[
        ("a.txt", b"a-one"),
        ("dir/b.txt", b"bee"),
        ("dir/c.txt", b"c-one"),
        ("gone.txt", b"gone"),
        ("keep.txt", b"kept"),
    ];
    let second: &[(&str, &[u8])] = &[("dir/c.txt", b"c-two"), ("r2.txt", b"two")];
    let instance = Instance::with_revisions(&[first, second]).await;
    let first_revision = instance.fixture.state.parent_self();
    let mut working = revision_tree(&instance, current_revision(&instance).await).await;
    let mut found = Vec::new();
    let mut m = instance.mount().await;
    found.extend(disagreements(&instance, &m, &working, "mounted").await);

    let remount = async |m: Mounted, working: &Tree, step: &str, found: &mut Vec<String>| {
        m.unmount().await;
        let m = instance
            .mount_at(state_at(&instance, current_revision(&instance).await).await)
            .await;
        found.extend(disagreements(&instance, &m, working, &format!("{step}, remounted")).await);
        m
    };

    let root = m.root.clone();
    blocking(move || {
        std::fs::write(root.join("a.txt"), b"A-local").expect("rewrite a.txt");
        std::fs::write(root.join("dir/new.txt"), b"new").expect("create dir/new.txt");
        std::fs::create_dir(root.join("sub")).expect("mkdir sub");
        std::fs::write(root.join("sub/deep.txt"), b"deep").expect("create sub/deep.txt");
        std::fs::remove_file(root.join("gone.txt")).expect("unlink gone.txt");
    })
    .await;
    working.insert("a.txt".into(), Some(b"A-local".to_vec()));
    working.insert("dir/new.txt".into(), Some(b"new".to_vec()));
    working.insert("sub".into(), None);
    working.insert("sub/deep.txt".into(), Some(b"deep".to_vec()));
    working.remove("gone.txt");
    found.extend(disagreements(&instance, &m, &working, "write").await);
    m = remount(m, &working, "write", &mut found).await;

    commit_through(&instance, &m.fs, false)
        .await
        .expect("the commit");
    found.extend(disagreements(&instance, &m, &working, "commit").await);
    // Status agrees with whatever revision is current, so only this shows the commit stored
    // everything the mount serves.
    let committed = revision_tree(&instance, current_revision(&instance).await).await;
    let walk = walked(&m.root).await;
    if committed != walk {
        found.push(format!(
            "commit: the committed revision holds {committed:?}, the mount serves {walk:?}"
        ));
    }

    let root = m.root.clone();
    blocking(move || {
        std::fs::write(root.join("dir/b.txt"), b"B-edit").expect("edit dir/b.txt");
        std::fs::write(root.join("keep.txt.tmp"), b"saved").expect("write the temporary");
        std::fs::rename(root.join("keep.txt.tmp"), root.join("keep.txt"))
            .expect("save keep.txt over its temporary");
    })
    .await;
    working.insert("dir/b.txt".into(), Some(b"B-edit".to_vec()));
    working.insert("keep.txt".into(), Some(b"saved".to_vec()));
    found.extend(disagreements(&instance, &m, &working, "edit after the commit").await);
    m = remount(m, &working, "edit after the commit", &mut found).await;

    sync_through(&instance, &m.fs, first_revision, Default::default())
        .await
        .expect("the sync");
    let synced = current_revision(&instance).await;
    if synced != first_revision {
        found.push(format!(
            "sync: the anchor stands on {synced}, not the target {first_revision}"
        ));
    }
    working = revision_tree(&instance, first_revision).await;
    working.insert("dir/b.txt".into(), Some(b"B-edit".to_vec()));
    working.insert("keep.txt".into(), Some(b"saved".to_vec()));
    found.extend(disagreements(&instance, &m, &working, "sync").await);
    m = remount(m, &working, "sync", &mut found).await;

    reset_through(&instance, &m.fs, &["dir/b.txt", "keep.txt"], false)
        .await
        .expect("the reset");
    working.insert("dir/b.txt".into(), Some(b"bee".to_vec()));
    working.insert("keep.txt".into(), Some(b"kept".to_vec()));
    found.extend(disagreements(&instance, &m, &working, "reset").await);
    m = remount(m, &working, "reset", &mut found).await;

    m.unmount().await;
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// A file the kernel held when a commit read it, forgotten and looked up again before the flip,
/// takes the same inode number with a new incarnation, and a write through it moves its write
/// generation back to what the commit read. The flip compares the incarnation and leaves the file
/// dirty: the write is served and status reports it.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_through_an_inode_forgotten_and_found_again_during_a_commit_is_kept() {
    use std::sync::Mutex;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let read_generation = fs.write_generation(ino).expect("held");
            // (generation at the read, after the forget, the inode found again, after its write)
            type Window = (Option<u64>, Option<u64>, u64, u64);
            let window: Arc<Mutex<Option<Window>>> = Arc::default();
            fs.set_commit_hook(commit_step(&fs, {
                let window = window.clone();
                move |fs| {
                    let window = window.clone();
                    async move {
                        fs.forget(ino, 1);
                        let forgotten = fs.write_generation(ino);
                        let again = lookup(&fs, ROOT_INO, "a.txt").await.expect("found again");
                        let fresh = fs.write_generation(again);
                        fs.write(&request(), again, 0, 0, b"WRITE", 0)
                            .await
                            .expect("a write through the new inode");
                        *window.lock().expect("window") = Some((
                            Some(read_generation),
                            forgotten,
                            again,
                            fs.write_generation(again).expect("held again"),
                        ));
                        assert_eq!(fresh, Some(0), "a fresh entry");
                    }
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            let (read_generation, forgotten, again, written) =
                window.lock().expect("window").expect("the hook ran");
            assert_eq!(forgotten, None, "the forget took the entry out");
            assert_eq!(again, ino, "the same inode number");
            assert_eq!(
                read_generation,
                Some(written),
                "the generation moved back to the commit's read"
            );
            assert_eq!(read(&fs, ino, 64).await, b"WRITE");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"WRITE"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a.txt"]);
        })
        .await;
}

/// What a request does in the settle window of [`settle_retry`].
#[derive(Clone, Copy, Debug)]
enum SettleRequest {
    Read,
    Getattr,
    Write,
}

/// Commits a local file no inode held at the commit's read, looks it up again once the commit has
/// classified it, before its settle, and closes that inode's descriptor, as an eviction would; then
/// runs `request` on the inode while the settle holds its host file aside. Answers whether the
/// request began waiting for the settle there, what it answered, and the inode.
async fn settle_retry(
    instance: &Instance,
    fs: &Arc<FuseFilesystem>,
    request_kind: SettleRequest,
) -> (bool, Result<Vec<u8>, Errno>, u64) {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let ino = lookup(fs, ROOT_INO, "a.txt").await.expect("a.txt");
    fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
        .await
        .expect("the edit the commit stores");
    fs.forget(ino, 1);
    assert_eq!(
        fs.write_generation(ino),
        None,
        "no inode at the commit's read"
    );
    let held = Arc::new(AtomicU64::new(0));
    let classified = AtomicBool::new(true);
    let armed = AtomicBool::new(true);
    let waited = Arc::new(AtomicBool::new(false));
    type Answer = tokio::task::JoinHandle<Result<Vec<u8>, Errno>>;
    let answered: Arc<Mutex<Option<Answer>>> = Arc::default();
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let (held, waited, answered) = (held.clone(), waited.clone(), answered.clone());
        move |step| {
            if let WriteStep::Classified = step
                && classified.swap(false, Ordering::SeqCst)
                && let Some(fs) = weak.upgrade()
            {
                let (found_tx, found_rx) = std::sync::mpsc::channel::<u64>();
                let task_fs = fs.clone();
                runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        let again = lookup(&task_fs, ROOT_INO, "a.txt").await;
                        let _ = found_tx.send(again.expect("found again"));
                    },
                ));
                let again = tokio::task::block_in_place(|| {
                    found_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("the lookup once classified")
                });
                fs.close_local_file(again);
                held.store(again, Ordering::SeqCst);
            }
            if let WriteStep::Settle { aside: true, .. } = step
                && armed.swap(false, Ordering::SeqCst)
                && let Some(fs) = weak.upgrade()
            {
                let ino = held.load(Ordering::SeqCst);
                let begun = fs.settle_waits_begun();
                let task_fs = fs.clone();
                let task = runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        match request_kind {
                            SettleRequest::Read => {
                                let mut out = vec![0u8; 64];
                                match task_fs.read(&request(), ino, 0, 0, &mut out, true).await {
                                    Ok(read) => {
                                        out.truncate(read);
                                        Ok(out)
                                    }
                                    Err(ReadFault::Errno(errno)) => Err(errno),
                                    Err(ReadFault::WouldFetch) => Err(Errno::EAGAIN),
                                }
                            }
                            SettleRequest::Getattr => task_fs
                                .getattr(&request(), ino)
                                .await
                                .map(|reply| reply.attr.size.to_string().into_bytes()),
                            SettleRequest::Write => task_fs
                                .write(&request(), ino, 0, 0, b"W", 0)
                                .await
                                .map(|wrote| wrote.to_string().into_bytes()),
                        }
                    },
                ));
                *answered.lock().expect("answered") = Some(task);
                tokio::task::block_in_place(|| {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while fs.settle_waits_begun() == begun && std::time::Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                });
                waited.store(fs.settle_waits_begun() > begun, Ordering::SeqCst);
            }
            true
        }
    }));

    commit_through(instance, fs, false).await.expect("commit");
    let task = answered
        .lock()
        .expect("answered")
        .take()
        .expect("the request ran");
    let answer = task.await.expect("the request task");
    (
        waited.load(Ordering::SeqCst),
        answer,
        held.load(Ordering::SeqCst),
    )
}

/// A READ of a local file whose host file a commit's settle holds aside waits for the settle,
/// reached on an inode looked up before it, and then reads the committed content from the store.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_meeting_a_host_file_held_aside_waits_and_reads_the_committed_content() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (waited, answer, _) = settle_retry(&instance, &fs, SettleRequest::Read).await;
            assert!(waited, "the READ waited for the settle");
            assert_eq!(answer, Ok(b"ALPHA".to_vec()));
        })
        .await;
}

/// A GETATTR of a local file whose host file a commit's settle holds aside waits for the settle
/// and answers the committed file's attributes.
#[tokio::test(flavor = "multi_thread")]
async fn a_getattr_meeting_a_host_file_held_aside_waits_and_answers() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (waited, answer, _) = settle_retry(&instance, &fs, SettleRequest::Getattr).await;
            assert!(waited, "the GETATTR waited for the settle");
            assert_eq!(answer, Ok(b"5".to_vec()));
        })
        .await;
}

/// A WRITE to a local file whose host file a commit's settle holds aside waits for the settle,
/// then copies the committed file up and lands: the file is local and dirty with the write.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_meeting_a_host_file_held_aside_waits_and_lands() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let (waited, answer, held) = settle_retry(&instance, &fs, SettleRequest::Write).await;
            assert!(waited, "the WRITE waited for the settle");
            assert_eq!(answer, Ok(b"1".to_vec()));
            assert_eq!(read(&fs, held, 64).await, b"WLPHA");
            assert_eq!(
                std::fs::read(instance.write_dir().join("a.txt")).expect("write/a.txt"),
                b"WLPHA"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a.txt"]);
        })
        .await;
}

/// A READ that read a file as local before a commit flipped it, and opens its host file after
/// the flip removed it, reads the committed content: the flip turned the inode store-served.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_opening_a_host_file_a_flip_removed_reads_the_committed_content() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            fs.write(&request(), ino, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            fs.forget(ino, 1);
            let ino = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt again");
            // As an eviction would: the READ opens the host file by its path.
            fs.close_local_file(ino);
            let armed = Arc::new(AtomicBool::new(true));
            let (resolved_tx, resolved_rx) = mpsc::channel::<()>();
            let (committed_tx, committed_rx) = mpsc::channel::<()>();
            let (resolved_tx, committed_rx) = (Mutex::new(resolved_tx), Mutex::new(committed_rx));
            fs.set_write_hook(Arc::new({
                let armed = armed.clone();
                move |step| {
                    if let WriteStep::PathResolved { ino: opened } = step
                        && opened == ino
                        && armed.swap(false, Ordering::SeqCst)
                    {
                        let _ = resolved_tx.lock().expect("resolved").send(());
                        tokio::task::block_in_place(|| {
                            committed_rx
                                .lock()
                                .expect("committed")
                                .recv_timeout(Duration::from_secs(10))
                                .expect("the commit released the READ");
                        });
                    }
                    true
                }
            }));
            let reader = {
                let fs = fs.clone();
                lore_base::lore_spawn!(async move {
                    let mut out = vec![0u8; 64];
                    fs.read(&request(), ino, 0, 0, &mut out, true)
                        .await
                        .map(|read| {
                            out.truncate(read);
                            out
                        })
                        .map_err(|fault| match fault {
                            ReadFault::Errno(errno) => format!("{errno:?}"),
                            ReadFault::WouldFetch => "WouldFetch".to_string(),
                        })
                })
            };
            resolved_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the READ resolved its host path as local");
            let (flipped_before, _) = fs.commit_outcomes();
            commit_through(&instance, &fs, false).await.expect("commit");
            let (flipped_after, _) = fs.commit_outcomes();
            assert_eq!(
                flipped_after,
                flipped_before + 1,
                "the commit flipped the file"
            );
            assert!(
                !instance.write_dir().join("a.txt").exists(),
                "the flip removed the host file"
            );
            assert!(!reader.is_finished(), "the READ waited out the commit");
            committed_tx.send(()).expect("release the READ");
            let answer = reader.await.expect("the READ task");
            assert_eq!(answer, Ok(b"ALPHA".to_vec()));
        })
        .await;
}

/// A CREATE whose directory a commit settles clean before its attach marks the directory dirty
/// again before it attaches, under the served chain rule: status reports the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_attaching_after_a_commit_settled_its_directory_marks_it_dirty_again() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let d_node = node_at(&instance, &fs, "d").await;
            let armed = AtomicBool::new(true);
            let settled_clean = Arc::new(Mutex::new(None::<bool>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (instance, settled_clean) = (instance.clone(), settled_clean.clone());
                move |step| {
                    if let WriteStep::Attaching { dir } = step
                        && dir == d_node
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        let instance = instance.clone();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                commit_through(&instance, &fs, false).await.expect("commit");
                                let clean = !fs_node(&instance, &fs, "d").await.is_dirty();
                                let _ = done_tx.send(clean);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *settled_clean.lock().expect("settled") =
                                done_rx.recv_timeout(Duration::from_secs(30)).ok();
                        });
                    }
                    true
                }
            }));

            create_with(&fs, d, "new.txt", b"new").await;

            assert_eq!(
                *settled_clean.lock().expect("settled"),
                Some(true),
                "the commit settled d clean inside the CREATE"
            );
            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over its new file"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/new.txt"]);
        })
        .await;
}

/// A CREATE landing in a directory a commit is settling, after the settle found no dirty child
/// and before it sets the directory clean, leaves the directory dirty: status reports the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_landing_while_a_commit_settles_its_directory_keeps_it_dirty() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("d/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let d_node = node_at(&instance, &fs, "d").await;
            let armed = AtomicBool::new(true);
            let created = Arc::new(Mutex::new(None::<bool>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let created = created.clone();
                move |step| {
                    if let WriteStep::DirectorySettling { node } = step
                        && node == d_node
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let made = create_with(&fs, d, "new.txt", b"new").await;
                                let _ = done_tx.send(made != 0);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *created.lock().expect("created") =
                                done_rx.recv_timeout(Duration::from_secs(30)).ok();
                        });
                    }
                    true
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                *created.lock().expect("created"),
                Some(true),
                "the CREATE landed while d settled"
            );
            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over its new file"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/new.txt"]);
        })
        .await;
}

/// An UNLINK landing in a directory a commit is settling, after the settle marked the
/// directory's clear pending and before it sets the directory clean, refuses the clear: the
/// unlink's served chain rule meets the pending clear and marks the directory again, so the
/// directory stays dirty and status reports the deletion.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_landing_while_a_commit_settles_its_directory_keeps_it_dirty() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("d/a.txt", b"alpha"), ("d/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            lookup(&fs, d, "b.txt").await.expect("b.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let d_node = node_at(&instance, &fs, "d").await;
            let armed = AtomicBool::new(true);
            let unlinked = Arc::new(Mutex::new(None::<bool>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let unlinked = unlinked.clone();
                move |step| {
                    if let WriteStep::DirectorySettling { node } = step
                        && node == d_node
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let removed = fs.unlink(&request(), d, b"b.txt").await;
                                let _ = done_tx.send(removed.is_ok());
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *unlinked.lock().expect("unlinked") =
                                done_rx.recv_timeout(Duration::from_secs(30)).ok();
                        });
                    }
                    true
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                *unlinked.lock().expect("unlinked"),
                Some(true),
                "the UNLINK landed while d settled"
            );
            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over its deletion"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/b.txt"]);
        })
        .await;
}

/// H15: a removal from a directory between the commit's read and its settle. The committed
/// directory holds the removed file and the State does not, so the settle keeps the directory
/// dirty and status reports the removal: an UNLINK reports a deletion, a RENAME away a deletion
/// and an add.
#[tokio::test(flavor = "multi_thread")]
async fn a_removal_during_a_commit_keeps_its_directory_dirty() {
    for rename in [false, true] {
        let instance = Instance::new(&[("d/a.txt", b"alpha"), ("d/b.txt", b"bee")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
                let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
                lookup(&fs, d, "b.txt").await.expect("b.txt");
                fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                    .await
                    .expect("the edit the commit stores");
                fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                    if rename {
                        fs.rename(&request(), d, b"b.txt", ROOT_INO, b"c.txt", 0)
                            .await
                            .expect("a rename away during the commit");
                    } else {
                        fs.unlink(&request(), d, b"b.txt")
                            .await
                            .expect("an unlink during the commit");
                    }
                }));

                let revision = commit_through(&instance, &fs, false).await.expect("commit");

                assert_eq!(
                    revision_content(&instance, revision, "d/b.txt").await,
                    Some(b"bee".to_vec())
                );
                assert!(
                    fs_node(&instance, &fs, "d").await.is_dirty(),
                    "d is clean over its removal (rename: {rename})"
                );
                let expected: &[&str] = if rename {
                    &["c.txt", "d/b.txt"]
                } else {
                    &["d/b.txt"]
                };
                assert_eq!(changed_paths(&instance, &fs).await, expected);
            })
            .await;
    }
}

/// A directory removed while a commit settles a directory below it: the settle of the removed
/// directory, found through the pass's index, finds it gone, looks for its path again by name,
/// finds nothing there and records the path deleted, ahead of the line the RMDIR appended while
/// the commit read; `status --scan` reports the committed file deleted, and a plain status the
/// removed directory alone, as on an ordinary clone.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_removed_while_a_commit_settles_below_it_is_recorded_deleted() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("a/x.txt", b"x")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let a = lookup(&fs, ROOT_INO, "a").await.expect("a");
            let mkdir = async |parent: u64, name: &[u8]| {
                fs.mkdir(&request(), parent, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(a, b"p").await;
            let q = mkdir(p, b"q").await;
            create_with(&fs, q, "f", b"eff").await;
            let q_node = node_at(&instance, &fs, "a/p/q").await;
            let armed = AtomicBool::new(true);
            let removed = Arc::new(std::sync::Mutex::new(None::<bool>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let removed = removed.clone();
                move |step| {
                    if let WriteStep::DirectorySettling { node } = step
                        && node == q_node
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let gone = fs.unlink(&request(), q, b"f").await.is_ok()
                                    && fs.rmdir(&request(), p, b"q").await.is_ok()
                                    && fs.rmdir(&request(), a, b"p").await.is_ok();
                                let _ = done_tx.send(gone);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *removed.lock().expect("removed") =
                                done_rx.recv_timeout(Duration::from_secs(30)).ok();
                        });
                    }
                    true
                }
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(*removed.lock().expect("removed"), Some(true));
            assert_eq!(
                revision_content(&instance, revision, "a/p/q/f").await,
                Some(b"eff".to_vec())
            );
            let listed = deleted(&instance);
            assert_eq!(
                listed.iter().filter(|line| *line == "a/p").count(),
                2,
                "the settle and the RMDIR each record a/p: {listed:?}"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["a/p"]);
            let scanned: Vec<String> =
                staged_state::status_files(instance.command_context(&fs, None), true)
                    .await
                    .into_keys()
                    .collect();
            assert_eq!(scanned, vec!["a/p", "a/p/q", "a/p/q/f"]);
        })
        .await;
}

/// A settle whose index read a directory at a path that moved away meanwhile, another directory
/// moved there, settles the other directory in turn: two local directories renamed while the
/// commit settles one below them. The directory now at the committed path no longer counts as
/// added and keeps its own children, so it stays dirty; the one moved away stays dirty too.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_moved_onto_a_settled_path_after_the_index_read_it_is_settled_in_turn() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |parent: u64, name: &[u8]| {
                fs.mkdir(&request(), parent, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(ROOT_INO, b"p").await;
            let sub = mkdir(p, b"sub").await;
            let f = create_with(&fs, sub, "f", b"eff").await;
            let q = mkdir(ROOT_INO, b"q").await;
            let x = create_with(&fs, q, "x", b"ex").await;
            // Written after the commit's read, so neither file flips: both directories stay local
            // subtrees, which a rename moves.
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                for ino in [f, x] {
                    fs.write(&request(), ino, 0, 0, b"!", 0)
                        .await
                        .expect("a write after the read");
                }
            }));
            let sub_node = node_at(&instance, &fs, "p/sub").await;
            let q_node = node_at(&instance, &fs, "q").await;
            let armed = AtomicBool::new(true);
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new(move |step| {
                if let WriteStep::DirectorySettling { node } = step
                    && node == sub_node
                    && armed.swap(false, Ordering::SeqCst)
                    && let Some(fs) = weak.upgrade()
                {
                    let (done_tx, done_rx) = mpsc::channel::<bool>();
                    runtime.spawn(LORE_CONTEXT.scope(
                        fuse_fixture::setup_test_execution(),
                        async move {
                            let moved = fs
                                .rename(&request(), ROOT_INO, b"p", ROOT_INO, b"r", 0)
                                .await
                                .is_ok()
                                && fs
                                    .rename(&request(), ROOT_INO, b"q", ROOT_INO, b"p", 0)
                                    .await
                                    .is_ok();
                            let _ = done_tx.send(moved);
                        },
                    ));
                    tokio::task::block_in_place(|| {
                        assert_eq!(
                            done_rx.recv_timeout(Duration::from_secs(30)).ok(),
                            Some(true),
                            "p moves to r and q onto p"
                        );
                    });
                }
                true
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "p/sub/f").await,
                Some(b"eff".to_vec())
            );
            assert_eq!(node_at(&instance, &fs, "p").await, q_node);
            let p = fs_node(&instance, &fs, "p").await;
            assert!(p.is_dirty(), "q's directory at p is clean with p's address");
            assert!(
                !p.is_dirty_add(),
                "q's directory at the committed p still counts as added"
            );
            assert!(fs_node(&instance, &fs, "r").await.is_dirty(), "r is clean");
        })
        .await;
}

/// A settle that finds a different directory at its path on every try, two local directories
/// swapped there before each, gives up with the directory last found unsettled: the settle is
/// not clean, so the commit's settle stays owed and an added file's later UNLINK records its
/// deletion.
#[tokio::test(flavor = "multi_thread")]
async fn a_settle_out_of_tries_leaves_the_settle_owed() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |parent: u64, name: &[u8]| {
                fs.mkdir(&request(), parent, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(ROOT_INO, b"p").await;
            let sub = mkdir(p, b"sub").await;
            let f = create_with(&fs, sub, "f", b"eff").await;
            let q = mkdir(ROOT_INO, b"q").await;
            let x = create_with(&fs, q, "x", b"ex").await;
            // Written after the commit's read, so both directories stay local subtrees.
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                for ino in [f, x] {
                    fs.write(&request(), ino, 0, 0, b"!", 0)
                        .await
                        .expect("a write after the read");
                }
            }));
            let sub_node = node_at(&instance, &fs, "p/sub").await;
            let armed = AtomicBool::new(true);
            let swaps = Arc::new(AtomicUsize::new(0));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            // Each swap parks the directory at p as `x<n>` and brings back the one parked last.
            let swap = move |fs: Arc<FuseFilesystem>, n: usize| {
                let (done_tx, done_rx) = mpsc::channel::<bool>();
                runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        let (parked, back) = (format!("x{n}"), format!("x{}", n.wrapping_sub(1)));
                        let back = if n == 0 { "q".to_string() } else { back };
                        let swapped = fs
                            .rename(&request(), ROOT_INO, b"p", ROOT_INO, parked.as_bytes(), 0)
                            .await
                            .is_ok()
                            && fs
                                .rename(&request(), ROOT_INO, back.as_bytes(), ROOT_INO, b"p", 0)
                                .await
                                .is_ok();
                        let _ = done_tx.send(swapped);
                    },
                ));
                tokio::task::block_in_place(|| {
                    assert_eq!(
                        done_rx.recv_timeout(Duration::from_secs(30)).ok(),
                        Some(true),
                        "swap {n} of the directories at p"
                    );
                });
            };
            fs.set_write_hook(Arc::new({
                let swaps = swaps.clone();
                move |step| {
                    let fire = match step {
                        WriteStep::DirectorySettling { node } => {
                            node == sub_node && armed.swap(false, Ordering::SeqCst)
                        }
                        WriteStep::SettleFound { .. } => true,
                        _ => false,
                    };
                    if fire && let Some(fs) = weak.upgrade() {
                        swap(fs, swaps.fetch_add(1, Ordering::SeqCst));
                    }
                    true
                }
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(
                swaps.load(Ordering::SeqCst) > 64,
                "the settle stopped trying after {} swaps",
                swaps.load(Ordering::SeqCst)
            );
            create_with(&fs, ROOT_INO, "g.txt", b"gee").await;
            fs.unlink(&request(), ROOT_INO, b"g.txt")
                .await
                .expect("unlink");
            assert!(
                deleted(&instance).iter().any(|line| line == "g.txt"),
                "the settle out of tries released its debt: {:?}",
                deleted(&instance)
            );
        })
        .await;
}

/// A case-only respell of a clean tracked file during a commit's upload, in a directory the
/// commit settles: the rename copies the file up, so the settle finds a dirty child and keeps
/// the directory dirty, and status still reports the new spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_respell_of_a_clean_file_during_a_commit_stays_reported() {
    let instance = Instance::new(&[("d/a.txt", b"alpha"), ("d/c.txt", b"see")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            lookup(&fs, d, "a.txt").await.expect("a.txt");
            let c = lookup(&fs, d, "c.txt").await.expect("c.txt");
            fs.write(&request(), c, 0, 0, b"SEE", 0)
                .await
                .expect("the edit the commit stores");
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                fs.rename(&request(), d, b"a.txt", d, b"A.txt", 0)
                    .await
                    .expect("the respell");
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over the respelled file"
            );
            let changed = changed_paths(&instance, &fs).await;
            assert!(
                changed.iter().any(|path| path == "d/A.txt"),
                "status misses the respell: {changed:?}"
            );
        })
        .await;
}

/// A case-only respell of a clean tracked directory during a commit's upload: the respell
/// dirties the directory, so the commit's settles leave it dirty and status still reports the
/// new spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_respell_of_a_clean_directory_during_a_commit_stays_reported() {
    let instance = Instance::new(&[("d/a.txt", b"alpha"), ("e/b.txt", b"bee")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            lookup(&fs, ROOT_INO, "d").await.expect("d");
            let b = lookup_path(&fs, "e/b.txt").await.expect("e/b.txt");
            fs.write(&request(), b, 0, 0, b"BEE", 0)
                .await
                .expect("the edit the commit stores");
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                fs.rename(&request(), ROOT_INO, b"d", ROOT_INO, b"D", 0)
                    .await
                    .expect("the respell");
            }));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(
                fs_node(&instance, &fs, "D").await.is_dirty(),
                "the respelled directory is clean"
            );
            assert!(
                !changed_paths(&instance, &fs).await.is_empty(),
                "status misses the respell"
            );
        })
        .await;
}

/// H19: a directory renamed away during a commit's upload, and an empty one renamed onto its
/// path. The settle finds the empty directory at the committed path, whose children are not the
/// committed ones, so it keeps it dirty: status reports the file moved, not duplicated.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_renamed_onto_a_committed_path_during_a_commit_stays_dirty() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |name: &[u8]| {
                fs.mkdir(&request(), ROOT_INO, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(b"p").await;
            create_with(&fs, p, "x", b"one").await;
            mkdir(b"q").await;
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                fs.rename(&request(), ROOT_INO, b"p", ROOT_INO, b"r", 0)
                    .await
                    .expect("p moves away");
                fs.rename(&request(), ROOT_INO, b"q", ROOT_INO, b"p", 0)
                    .await
                    .expect("q moves onto p");
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "p/x").await,
                Some(b"one".to_vec())
            );
            assert!(
                fs_node(&instance, &fs, "p").await.is_dirty(),
                "the empty directory at p is clean with p's committed address"
            );
            let changed = changed_paths(&instance, &fs).await;
            assert!(
                changed.iter().any(|path| path == "p/x")
                    && changed.iter().any(|path| path == "r/x"),
                "status misses the move of p/x to r/x: {changed:?}"
            );
        })
        .await;
}

/// H19: two directories holding the same names swapped during a commit's upload. Each now stands
/// at the other's committed path with its own content, so each settle finds its children's
/// content not the committed directory's and keeps it dirty: status reports both files changed.
#[tokio::test(flavor = "multi_thread")]
async fn two_directories_swapped_during_a_commit_stay_dirty() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |name: &[u8]| {
                fs.mkdir(&request(), ROOT_INO, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(b"p").await;
            create_with(&fs, p, "x", b"one").await;
            let q = mkdir(b"q").await;
            create_with(&fs, q, "x", b"two").await;
            fs.set_commit_hook(commit_step(&fs, |fs| async move {
                for (from, to) in [(&b"p"[..], &b"t"[..]), (b"q", b"p"), (b"t", b"q")] {
                    fs.rename(&request(), ROOT_INO, from, ROOT_INO, to, 0)
                        .await
                        .expect("a swap step");
                }
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "p/x").await,
                Some(b"one".to_vec())
            );
            for dir in ["p", "q"] {
                assert!(
                    fs_node(&instance, &fs, dir).await.is_dirty(),
                    "{dir} is clean with the other directory's committed address"
                );
            }
            assert_eq!(changed_paths(&instance, &fs).await, vec!["p/x", "q/x"]);
            let p = lookup(&fs, ROOT_INO, "p").await.expect("p");
            let x = lookup(&fs, p, "x").await.expect("p/x");
            assert_eq!(read(&fs, x, 64).await, b"two");
        })
        .await;
}

/// H25: two directories holding the same names swapped during a commit's upload, their files
/// forgotten before the read and looked up again after the swap, so each file's read path now
/// holds the other node's inode. A flip turns an inode only where it stands for the node it
/// flips, so neither file takes the other's committed content: each serves its own bytes from
/// its own host file, and status reports both.
#[tokio::test(flavor = "multi_thread")]
async fn a_flip_leaves_an_inode_standing_for_another_node_alone() {
    swapped_during_a_commit(true).await;
}

/// [`a_flip_leaves_an_inode_standing_for_another_node_alone`] with no lookup after the swap, so
/// no inode holds either file: the host file at each file's read path is the other node's, and a
/// flip takes it aside and removes it only where the path still leads to the node it flips.
#[tokio::test(flavor = "multi_thread")]
async fn a_flip_of_an_unheld_file_leaves_another_nodes_host_file_alone() {
    swapped_during_a_commit(false).await;
}

/// Two local directories holding `x` swapped during a commit's upload, their files forgotten
/// before the read and, where `looked_up`, looked up again after the swap: each keeps its own
/// bytes and host file, and status reports both.
async fn swapped_during_a_commit(looked_up: bool) {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |name: &[u8]| {
                fs.mkdir(&request(), ROOT_INO, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            let p = mkdir(b"p").await;
            let one = create_with(&fs, p, "x", b"one").await;
            let q = mkdir(b"q").await;
            let two = create_with(&fs, q, "x", b"two").await;
            fs.forget(one, 1);
            fs.forget(two, 1);
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                for (from, to) in [(&b"p"[..], &b"t"[..]), (b"q", b"p"), (b"t", b"q")] {
                    fs.rename(&request(), ROOT_INO, from, ROOT_INO, to, 0)
                        .await
                        .expect("a swap step");
                }
                if looked_up {
                    for path in ["p/x", "q/x"] {
                        lookup_path(&fs, path)
                            .await
                            .expect("a lookup after the swap");
                    }
                }
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "p/x").await,
                Some(b"one".to_vec())
            );
            for (path, held) in [("p/x", &b"two"[..]), ("q/x", b"one")] {
                let ino = lookup_path(&fs, path).await.expect(path);
                assert_eq!(
                    read(&fs, ino, 64).await,
                    held,
                    "{path} serves another's bytes"
                );
                assert_eq!(
                    std::fs::read(instance.write_dir().join(path))
                        .ok()
                        .as_deref(),
                    Some(held),
                    "write/{path}"
                );
            }
            assert_eq!(changed_paths(&instance, &fs).await, vec!["p/x", "q/x"]);
        })
        .await;
}

/// A commit of a file three tracked directories deep, none of them handed, settles every
/// directory above the file: none stays dirty over committed content.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_settles_every_directory_above_a_stored_file() {
    let instance = Instance::new(&[("d/e/f/old.txt", b"old"), ("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let e = lookup(&fs, d, "e").await.expect("e");
            let f = lookup(&fs, e, "f").await.expect("f");
            create_with(&fs, f, "new.txt", b"new").await;
            for path in ["d/e/f", "d/e", "d"] {
                assert!(
                    fs_node(&instance, &fs, path).await.is_dirty(),
                    "'{path}' is clean before the commit"
                );
            }

            commit_through(&instance, &fs, false).await.expect("commit");

            for path in ["d/e/f", "d/e", "d"] {
                assert!(
                    !fs_node(&instance, &fs, path).await.is_dirty(),
                    "'{path}' is dirty after the commit"
                );
            }
        })
        .await;
}

/// H20: an empty directory and nested empty ones a commit adds are handed to its settle, which
/// settles them clean and lets their host directories go. An RMDIR after the commit's operation
/// ended then records each deletion, so a remount keeps them deleted.
#[tokio::test(flavor = "multi_thread")]
async fn empty_directories_a_commit_adds_are_settled_and_their_removals_recorded() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let mkdir = async |parent: u64, name: &[u8]| {
                fs.mkdir(&request(), parent, name, libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir")
                    .ino
            };
            mkdir(ROOT_INO, b"e").await;
            let n = mkdir(ROOT_INO, b"n").await;
            mkdir(n, b"m").await;

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            for dir in ["e", "n", "n/m"] {
                assert!(
                    committed_node(&instance, revision, dir)
                        .await
                        .is_directory(),
                    "the revision misses {dir}"
                );
                assert!(
                    !fs_node(&instance, &fs, dir).await.is_dirty(),
                    "{dir} is dirty after the commit"
                );
                assert!(
                    !instance.write_dir().join(dir).exists(),
                    "write/{dir} stays after the commit"
                );
            }
            fs.rmdir(&request(), n, b"m").await.expect("rmdir n/m");
            fs.rmdir(&request(), ROOT_INO, b"e").await.expect("rmdir e");
            let listed = deleted(&instance);
            for dir in ["e", "n/m"] {
                assert!(
                    listed.iter().any(|line| line == dir),
                    "the RMDIR of {dir} went unrecorded: {listed:?}"
                );
            }
            let again = provider_at(&instance, revision).await;
            assert!(lookup(&again, ROOT_INO, "e").await.is_err(), "e is back");
            let n = lookup(&again, ROOT_INO, "n").await.expect("n");
            assert!(lookup(&again, n, "m").await.is_err(), "n/m is back");
        })
        .await;
}

/// H20: an empty directory added and staged, then removed before the commit, when no commit
/// reads content: the RMDIR of an added directory records nothing, and the commit stores the
/// directory. The commit hands it to its settle, whose pre-record finds no directory at its path
/// and records the deletion, so status reports it and a remount keeps it deleted.
#[tokio::test(flavor = "multi_thread")]
async fn an_added_directory_removed_between_the_stage_and_the_commit_is_recorded_deleted() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            stage_through(&instance, &fs).await.expect("stage");
            fs.rmdir(&request(), ROOT_INO, b"e").await.expect("rmdir");
            assert!(deleted(&instance).is_empty(), "{:?}", deleted(&instance));

            let revision = commit_staged(&instance, &fs).await.expect("commit");

            assert!(
                committed_node(&instance, revision, "e")
                    .await
                    .is_directory()
            );
            assert!(
                deleted(&instance).iter().any(|line| line == "e"),
                "the deletion list misses e: {:?}",
                deleted(&instance)
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["e"]);
            let again = provider_at(&instance, revision).await;
            assert!(lookup(&again, ROOT_INO, "e").await.is_err(), "e is back");
        })
        .await;
}

/// H20: [`an_added_directory_removed_between_the_stage_and_the_commit_is_recorded_deleted`] with
/// the commit's reconcile refused, as a commit dropped before its settle: its pre-record alone
/// records the directory's deletion. The settle stays owed, so the UNLINK of a file the commit
/// stored, still marked added as no settle reached it, records its deletion too, and a remount
/// keeps both deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_dropped_before_its_settle_has_recorded_and_keeps_recording() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            create_with(&fs, ROOT_INO, "f.txt", b"eff").await;
            stage_through(&instance, &fs).await.expect("stage");
            fs.rmdir(&request(), ROOT_INO, b"e").await.expect("rmdir");
            fs.set_write_hook(Arc::new(|step| step != WriteStep::Reconcile));

            let revision = commit_staged(&instance, &fs).await.expect("commit");

            assert_eq!(
                revision_content(&instance, revision, "f.txt").await,
                Some(b"eff".to_vec())
            );
            assert!(
                deleted(&instance).iter().any(|line| line == "e"),
                "the pre-record misses e: {:?}",
                deleted(&instance)
            );
            assert!(fs_node(&instance, &fs, "f.txt").await.is_dirty_add());
            fs.unlink(&request(), ROOT_INO, b"f.txt")
                .await
                .expect("unlink");
            assert!(
                deleted(&instance).iter().any(|line| line == "f.txt"),
                "the UNLINK after the dropped settle went unrecorded: {:?}",
                deleted(&instance)
            );
            let again = provider_at(&instance, revision).await;
            for name in ["e", "f.txt"] {
                assert!(
                    lookup(&again, ROOT_INO, name).await.is_err(),
                    "{name} is back"
                );
            }
        })
        .await;
}

/// A commit whose branch moves on while it reads fails at `store_latest`: nothing became
/// current, so the settle it owed is released, and an added file's UNLINK afterwards records
/// nothing, as before the commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_failing_to_store_its_branch_latest_owes_no_settle() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            create_with(&fs, ROOT_INO, "f.txt", b"eff").await;
            stage_through(&instance, &fs).await.expect("stage");
            // The commit's token, which another committer's store needs as well.
            let token =
                lore_revision::repository::RepositoryWriteToken::acquire(&instance.fixture.path)
                    .await;
            let repository = instance.writing(&token);
            let (_, branch) = lore_revision::instance::load_current_anchor_boxed(&repository)
                .await
                .expect("the current anchor");
            // The tip the commit compares against, as it loads it.
            let tip = lore_revision::branch::load_latest(repository.clone(), branch)
                .await
                .unwrap_or_default();
            let armed = AtomicBool::new(true);
            let moved = Arc::new(AtomicBool::new(false));
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let moved = moved.clone();
                move |step| {
                    if step == WriteStep::ContentServing && armed.swap(false, Ordering::SeqCst) {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        let repository = repository.clone();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let stored = lore_revision::branch::store_latest(
                                    repository,
                                    branch,
                                    tip,
                                    Hash::from_u64(0xfeed),
                                    lore_revision::branch::BranchLatestStatus::Divergent,
                                )
                                .await;
                                let _ = done_tx.send(stored.is_ok());
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            let stored = done_rx.recv_timeout(Duration::from_secs(30)).ok();
                            moved.store(stored == Some(true), Ordering::SeqCst);
                        });
                    }
                    true
                }
            }));

            let failed = commit_staged_holding(&instance, &fs, &token).await;

            assert!(moved.load(Ordering::SeqCst), "the branch did not move on");
            let error = failed.expect_err("the commit stored over a moved branch");
            assert!(
                error.contains("advanced by another instance"),
                "the commit failed otherwise: {error}"
            );
            create_with(&fs, ROOT_INO, "g.txt", b"gee").await;
            fs.unlink(&request(), ROOT_INO, b"g.txt")
                .await
                .expect("unlink");
            assert!(
                deleted(&instance).is_empty(),
                "the failed commit left its settle owed: {:?}",
                deleted(&instance)
            );
        })
        .await;
}

/// A file or directory made and renamed over a tracked one of its kind takes over the tracked
/// path, so it no longer counts as added: its later removal records the path deleted, and a
/// remount keeps it deleted rather than bringing back the revision's.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_renamed_over_a_tracked_one_records_its_removal() {
    for dir in [false, true] {
        let instance = Instance::new(&[("t.txt", b"tracked"), ("d/only.txt", b"o")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                let (from, to) = if dir {
                    let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
                    fs.unlink(&request(), d, b"only.txt")
                        .await
                        .expect("d is emptied");
                    fs.mkdir(&request(), ROOT_INO, b"made", libc::S_IFDIR | 0o755, 0o022)
                        .await
                        .expect("mkdir");
                    ("made", "d")
                } else {
                    create_with(&fs, ROOT_INO, "made", b"made").await;
                    ("made", "t.txt")
                };
                fs.rename(
                    &request(),
                    ROOT_INO,
                    from.as_bytes(),
                    ROOT_INO,
                    to.as_bytes(),
                    0,
                )
                .await
                .expect("the rename over the tracked node");
                assert!(
                    !fs_node(&instance, &fs, to).await.is_dirty_add(),
                    "{to} still counts as added"
                );
                assert!(
                    !deleted(&instance).iter().any(|line| line == from),
                    "the added {from} was recorded deleted: {:?}",
                    deleted(&instance)
                );
                if dir {
                    fs.rmdir(&request(), ROOT_INO, to.as_bytes())
                        .await
                        .expect("rmdir");
                } else {
                    fs.unlink(&request(), ROOT_INO, to.as_bytes())
                        .await
                        .expect("unlink");
                }
                assert!(
                    deleted(&instance).iter().any(|line| line == to),
                    "the removal of {to} went unrecorded: {:?}",
                    deleted(&instance)
                );
                let again = provider_at(&instance, instance.fixture.state.revision()).await;
                assert!(lookup(&again, ROOT_INO, to).await.is_err(), "{to} is back");
            })
            .await;
    }
}

/// H20: an UNLINK or RENAME away of a file a commit added, landing during the commit and
/// deciding whether to record only after the commit's settle completed and released its debt.
/// The removal sampled the debt before its State step, so it records the deletion: its line
/// follows the one the settle's rewrite kept for the file it found gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_removal_racing_the_release_of_a_commits_settle_records_its_deletion() {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    for rename in [false, true] {
        let instance = Instance::new(&[("keep.txt", b"k")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                create_with(&fs, ROOT_INO, "f.txt", b"eff").await;
                let f_node = node_at(&instance, &fs, "f.txt").await;
                // The removal parks past its State step until the commit has ended.
                let (parked_tx, parked_rx) = mpsc::channel::<()>();
                let (go_tx, go_rx) = mpsc::channel::<()>();
                let parked_tx = Mutex::new(Some(parked_tx));
                let go_rx = Mutex::new(go_rx);
                fs.set_write_hook(Arc::new(move |step| {
                    let removed = match step {
                        WriteStep::Detached { node } => node == f_node,
                        WriteStep::NodeMoved { .. } => true,
                        _ => false,
                    };
                    if removed && let Some(parked) = parked_tx.lock().expect("parked").take() {
                        let _ = parked.send(());
                        tokio::task::block_in_place(|| {
                            let _ = go_rx
                                .lock()
                                .expect("go")
                                .recv_timeout(Duration::from_secs(30));
                        });
                    }
                    true
                }));
                let parked_rx = Arc::new(Mutex::new(parked_rx));
                let removal: Arc<Mutex<Option<tokio::task::JoinHandle<bool>>>> = Arc::default();
                fs.set_commit_hook(commit_step(&fs, {
                    let removal = removal.clone();
                    move |fs| {
                        let (parked_rx, removal) = (parked_rx.clone(), removal.clone());
                        async move {
                            let removing = tokio::spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    if rename {
                                        fs.rename(
                                            &request(),
                                            ROOT_INO,
                                            b"f.txt",
                                            ROOT_INO,
                                            b"g.txt",
                                            0,
                                        )
                                        .await
                                        .is_ok()
                                    } else {
                                        fs.unlink(&request(), ROOT_INO, b"f.txt").await.is_ok()
                                    }
                                },
                            ));
                            *removal.lock().expect("removal") = Some(removing);
                            tokio::task::block_in_place(|| {
                                let _ = parked_rx
                                    .lock()
                                    .expect("parked")
                                    .recv_timeout(Duration::from_secs(30));
                            });
                        }
                    }
                }));

                commit_through(&instance, &fs, false).await.expect("commit");

                let count = |listed: Vec<String>| listed.iter().filter(|l| *l == "f.txt").count();
                assert_eq!(
                    count(deleted(&instance)),
                    1,
                    "the settle's rewrite keeps f.txt (rename: {rename})"
                );
                go_tx.send(()).expect("the removal waits");
                let removing = removal
                    .lock()
                    .expect("removal")
                    .take()
                    .expect("the removal ran");
                assert!(
                    removing.await.expect("the removal task"),
                    "the removal failed"
                );
                assert_eq!(
                    count(deleted(&instance)),
                    2,
                    "the removal recorded nothing once the settle released (rename: {rename})"
                );
            })
            .await;
    }
}

/// H20: an added directory a file is created in during the commit: its settle keeps it dirty,
/// and it no longer counts as added, so once the files go its RMDIR records the deletion and a
/// remount keeps it deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_its_settle_kept_dirty_records_its_removal() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = fs
                .mkdir(&request(), ROOT_INO, b"d", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, d, "a.txt", b"alpha").await;
            fs.set_commit_hook(commit_step(&fs, move |fs| async move {
                create_with(&fs, d, "b.txt", b"bee").await;
            }));

            let revision = commit_through(&instance, &fs, false).await.expect("commit");

            let node = fs_node(&instance, &fs, "d").await;
            assert!(node.is_dirty(), "d is clean over b.txt");
            assert!(!node.is_dirty_add(), "d still counts as added");
            for name in [&b"a.txt"[..], b"b.txt"] {
                fs.unlink(&request(), d, name).await.expect("unlink");
            }
            fs.rmdir(&request(), ROOT_INO, b"d").await.expect("rmdir");
            assert!(
                deleted(&instance).iter().any(|line| line == "d"),
                "the RMDIR went unrecorded: {:?}",
                deleted(&instance)
            );
            let again = provider_at(&instance, revision).await;
            assert!(lookup(&again, ROOT_INO, "d").await.is_err(), "d is back");
        })
        .await;
}

/// H20: an added empty directory staged, then replaced by a file of its name before the commit:
/// the commit stores the directory and hands it to its settle, whose pre-record finds a file at
/// the path and records the directory's deletion. The file is left as it stands, added, which a
/// forced sync must not delete; a remount serves it.
#[tokio::test(flavor = "multi_thread")]
async fn a_handed_directory_path_now_holding_a_file_is_recorded_and_the_file_kept() {
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            stage_through(&instance, &fs).await.expect("stage");
            fs.rmdir(&request(), ROOT_INO, b"e").await.expect("rmdir");
            create_with(&fs, ROOT_INO, "e", b"a file").await;

            let revision = commit_staged(&instance, &fs).await.expect("commit");

            assert!(
                committed_node(&instance, revision, "e")
                    .await
                    .is_directory()
            );
            let node = fs_node(&instance, &fs, "e").await;
            assert!(
                node.is_file() && node.is_dirty_add(),
                "the file at e lost its add"
            );
            assert!(
                deleted(&instance).iter().any(|line| line == "e"),
                "the deletion list misses e: {:?}",
                deleted(&instance)
            );
            let again = provider_at(&instance, revision).await;
            let e = lookup(&again, ROOT_INO, "e").await.expect("e");
            assert_eq!(read(&again, e, 64).await, b"a file");
        })
        .await;
}

/// H20: a commit stopped after its settles, before its deletion-list rewrite, as one cancelled
/// there: the empty directory it settled keeps its host directory, which only the rewrite lets
/// go. The settle stays owed, so an added file's UNLINK afterwards records its deletion.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_stopped_before_its_rewrite_keeps_its_host_directories_and_its_debt() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            fs.set_write_hook(Arc::new(|step| step != WriteStep::DirectoriesSettled));

            commit_through(&instance, &fs, false).await.expect("commit");

            assert!(!fs_node(&instance, &fs, "e").await.is_dirty(), "e is dirty");
            assert!(
                instance.write_dir().join("e").is_dir(),
                "write/e went before the rewrite"
            );
            create_with(&fs, ROOT_INO, "g.txt", b"gee").await;
            fs.unlink(&request(), ROOT_INO, b"g.txt")
                .await
                .expect("unlink");
            assert!(
                deleted(&instance).iter().any(|line| line == "g.txt"),
                "the stopped commit left no settle owed: {:?}",
                deleted(&instance)
            );
        })
        .await;
}

/// H20: an RMDIR and an MKDIR at the path of an empty directory a commit settled, landing after
/// its settles and before its host directories go: the deferred removal finds another node at
/// the path and leaves the new directory's host directory alone, whether or not an inode is live
/// there when it comes.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_made_again_before_the_deferred_host_removal_keeps_its_host_directory() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    for live in [true, false] {
        let instance = Instance::new(&[("keep.txt", b"k")]).await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let fs = provider(&instance).await;
                fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                    .await
                    .expect("mkdir");
                let armed = AtomicBool::new(true);
                let weak = Arc::downgrade(&fs);
                let runtime = tokio::runtime::Handle::current();
                fs.set_write_hook(Arc::new(move |step| {
                    if step == WriteStep::DirectoriesSettled
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<bool>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let mut again = fs.rmdir(&request(), ROOT_INO, b"e").await.is_ok();
                                match fs
                                    .mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                                    .await
                                {
                                    Ok(made) if !live => fs.forget(made.ino, 1),
                                    Ok(_) => {}
                                    Err(_) => again = false,
                                }
                                let _ = done_tx.send(again);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            assert_eq!(
                                done_rx.recv_timeout(Duration::from_secs(30)).ok(),
                                Some(true),
                                "e is made again"
                            );
                        });
                    }
                    true
                }));

                commit_through(&instance, &fs, false).await.expect("commit");

                assert!(
                    fs_node(&instance, &fs, "e").await.is_dirty_add(),
                    "e is not the new directory"
                );
                assert!(
                    instance.write_dir().join("e").is_dir(),
                    "the new e lost its host directory (live inode: {live})"
                );
            })
            .await;
    }
}

/// Runs a commit to its end inside the first write step `at` accepts, and answers, once that step
/// has run, whether the commit left each of `dirs` clean (`None` where the commit did not end).
fn commit_inside_step(
    instance: &Arc<Instance>,
    fs: &Arc<FuseFilesystem>,
    dirs: &'static [&'static str],
    at: impl Fn(lore_revision::fs::fuse::filesystem::WriteStep) -> bool + Send + Sync + 'static,
) -> Arc<std::sync::Mutex<Option<Vec<bool>>>> {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let armed = AtomicBool::new(true);
    let settled: Arc<std::sync::Mutex<Option<Vec<bool>>>> = Arc::default();
    let weak = Arc::downgrade(fs);
    let runtime = tokio::runtime::Handle::current();
    fs.set_write_hook(Arc::new({
        let (instance, settled) = (instance.clone(), settled.clone());
        move |step| {
            if at(step)
                && armed.swap(false, Ordering::SeqCst)
                && let Some(fs) = weak.upgrade()
            {
                let (done_tx, done_rx) = std::sync::mpsc::channel::<Vec<bool>>();
                let instance = instance.clone();
                runtime.spawn(LORE_CONTEXT.scope(
                    fuse_fixture::setup_test_execution(),
                    async move {
                        commit_through(&instance, &fs, false).await.expect("commit");
                        let mut clean = Vec::new();
                        for dir in dirs {
                            clean.push(!fs_node(&instance, &fs, dir).await.is_dirty());
                        }
                        let _ = done_tx.send(clean);
                    },
                ));
                tokio::task::block_in_place(|| {
                    *settled.lock().expect("settled") =
                        done_rx.recv_timeout(Duration::from_secs(30)).ok();
                });
            }
            true
        }
    }));
    settled
}

/// A copy-up whose directory a commit settles clean before its change of the file marks the
/// directory dirty again before it changes the file, under the served chain rule: status
/// reports the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_up_after_a_commit_settled_its_directory_marks_it_dirty_again() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/a.txt", b"alpha"), ("d/b.txt", b"bee")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let b = lookup(&fs, d, "b.txt").await.expect("b.txt");
            let settled =
                commit_inside_step(&instance, &fs, &["d"], |step| step == WriteStep::CopyUp);

            fs.write(&request(), b, 0, 0, b"BEE", 0)
                .await
                .expect("the write that copies b.txt up");

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![true]),
                "the commit settled d clean inside the copy-up"
            );
            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over its copied-up file"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/b.txt"]);
        })
        .await;
}

/// A RENAME whose target directory a commit settles clean before the rename moves the node
/// leaves it dirty: status reports the move. The source directory holds the copied-up file
/// throughout, so no commit settles it clean.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_after_a_commit_settled_its_target_directory_marks_it_dirty_again() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/b.txt", b"bee"), ("e/c.txt", b"sea")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let e = lookup(&fs, ROOT_INO, "e").await.expect("e");
            let c = lookup(&fs, e, "c.txt").await.expect("c.txt");
            fs.write(&request(), c, 0, 0, b"SEA", 0)
                .await
                .expect("the edit the commit stores");
            lookup(&fs, d, "b.txt").await.expect("b.txt");
            let settled =
                commit_inside_step(&instance, &fs, &["e"], |step| step == WriteStep::Rename);

            fs.rename(&request(), d, b"b.txt", e, b"b.txt", 0)
                .await
                .expect("the rename");

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![true]),
                "the commit settled e clean inside the rename"
            );
            assert!(
                fs_node(&instance, &fs, "e").await.is_dirty(),
                "e is clean over the file moved in"
            );
            assert!(fs_node(&instance, &fs, "d").await.is_dirty());
            assert_eq!(
                changed_paths(&instance, &fs).await,
                vec!["d/b.txt", "e/b.txt"]
            );
        })
        .await;
}

/// An UNLINK whose directory a commit settles clean before the unlink detaches the node leaves
/// it dirty: status reports the deletion.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_after_a_commit_settled_its_directory_marks_it_dirty_again() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/a.txt", b"alpha"), ("d/b.txt", b"bee")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let b_node = node_at(&instance, &fs, "d/b.txt").await;
            let settled = commit_inside_step(&instance, &fs, &["d"], move |step| {
                step == WriteStep::Detaching { node: b_node }
            });

            fs.unlink(&request(), d, b"b.txt")
                .await
                .expect("the unlink");

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![true]),
                "the commit settled d clean inside the unlink"
            );
            assert!(
                fs_node(&instance, &fs, "d").await.is_dirty(),
                "d is clean over its deleted file"
            );
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/b.txt"]);
        })
        .await;
}

/// A RENAME whose source a commit flips to the store after the RENAME resolved it local, before
/// its host move, is resolved again and moves the committed file.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_whose_source_a_commit_flips_before_its_host_move_resolves_it_again() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/a.txt", b"alpha")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            let settled = commit_inside_step(&instance, &fs, &[], move |step| {
                step == WriteStep::RenameTargetBuilt { dir: d }
            });

            let renamed = fs.rename(&request(), d, b"a.txt", d, b"b.txt", 0).await;

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![]),
                "the commit ran inside the rename"
            );
            assert_eq!(
                fs.commit_outcomes().0,
                1,
                "the commit flipped a.txt inside the rename"
            );
            renamed.expect("the rename");
            let b = lookup(&fs, d, "b.txt").await.expect("b.txt");
            assert_eq!(read(&fs, b, 64).await, b"ALPHA");
            assert!(lookup(&fs, d, "a.txt").await.is_err());
        })
        .await;
}

/// A RENAME of an empty local directory that a commit's settle clears between the RENAME's host
/// move and its State step refuses with `EXDEV` and moves the host directory back. The deferred
/// removal found no host directory at the path, so the rollback leaves an empty `write/e` under
/// the clean tracked `e`, which a remount keeps as the tracked directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_refused_after_its_host_move_brings_its_host_directory_back() {
    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::node::NodeExtFlags;

    let instance = Arc::new(Instance::new(&[("keep.txt", b"k")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            let settled =
                commit_inside_step(&instance, &fs, &["e"], |step| step == WriteStep::Rename);

            let renamed = fs
                .rename(&request(), ROOT_INO, b"e", ROOT_INO, b"e2", 0)
                .await;

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![true]),
                "the commit settled e inside the RENAME"
            );
            assert_eq!(renamed, Err(Errno::EXDEV));
            let write_dir = instance.write_dir();
            assert!(write_dir.join("e").is_dir(), "write/e did not come back");
            assert!(!write_dir.join("e2").exists(), "write/e2 stayed");
            let e = fs_node(&instance, &fs, "e").await;
            assert!(
                !e.is_dirty() && !e.ext_flags().contains(NodeExtFlags::LocalContent),
                "e is not the revision's"
            );
            let revision = current_revision(&instance).await;
            drop(fs);
            let again = provider_at(&instance, revision).await;
            assert_eq!(listed(&again, ROOT_INO).await, vec!["e", "keep.txt"]);
        })
        .await;
}

/// A RENAME of an empty local directory whose host directory a commit's settle removes after the
/// RENAME resolved it, before its host move, resolves it again and answers `EXDEV`, on which `mv`
/// copies, rather than `ENOENT` for a directory that stands.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_of_a_directory_whose_host_directory_a_commit_removes_resolves_it_again() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("keep.txt", b"k")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            fs.mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir");
            let settled = commit_inside_step(&instance, &fs, &["e"], |step| {
                step == WriteStep::RenameTargetBuilt { dir: ROOT_INO }
            });

            let renamed = fs
                .rename(&request(), ROOT_INO, b"e", ROOT_INO, b"e2", 0)
                .await;

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![true]),
                "the commit settled e inside the RENAME"
            );
            assert!(
                !instance.write_dir().join("e").exists(),
                "the commit left write/e"
            );
            assert_eq!(renamed, Err(Errno::EXDEV));
            assert!(lookup(&fs, ROOT_INO, "e").await.is_ok(), "e is gone");
            assert!(lookup(&fs, ROOT_INO, "e2").await.is_err(), "e2 stands");
        })
        .await;
}

/// A RENAME of a local directory whose file a commit flips to the store, and which the commit's
/// settle then clears, after the RENAME resolved it local and before its State step, refuses with
/// `EXDEV` and moves the host directory back. Moved, the directory would carry a tracked file its
/// `write/` entry no longer holds to a path the revision lacks, and a remount would lose the file.
/// On `EXDEV`, `mv` copies instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_of_a_directory_a_commit_settles_after_its_resolution_refuses() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    const WAIT: Duration = Duration::from_secs(30);
    let instance = Instance::new(&[("keep.txt", b"k")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let e = fs
                .mkdir(&request(), ROOT_INO, b"e", libc::S_IFDIR | 0o755, 0o022)
                .await
                .expect("mkdir")
                .ino;
            create_with(&fs, e, "x.txt", b"ex").await;
            // The RENAME, resolved, waits at its host move for the commit's settles; the commit
            // waits after its settles for the RENAME's end.
            let (window_tx, window_rx) = mpsc::channel::<()>();
            let (settled_tx, settled_rx) = mpsc::channel::<()>();
            let (renamed_tx, renamed_rx) = mpsc::channel::<()>();
            let (settled_rx, renamed_rx) = (Mutex::new(settled_rx), Mutex::new(renamed_rx));
            let (rename_armed, settle_armed) = (AtomicBool::new(true), AtomicBool::new(true));
            fs.set_write_hook(Arc::new(move |step| {
                match step {
                    WriteStep::RenameTargetBuilt { dir }
                        if dir == ROOT_INO && rename_armed.swap(false, Ordering::SeqCst) =>
                    {
                        let _ = window_tx.send(());
                        tokio::task::block_in_place(|| {
                            settled_rx
                                .lock()
                                .expect("settled")
                                .recv_timeout(WAIT)
                                .expect("the commit's settles");
                        });
                    }
                    WriteStep::DirectoriesSettled if settle_armed.swap(false, Ordering::SeqCst) => {
                        let _ = settled_tx.send(());
                        tokio::task::block_in_place(|| {
                            renamed_rx
                                .lock()
                                .expect("renamed")
                                .recv_timeout(WAIT)
                                .expect("the RENAME's end");
                        });
                    }
                    _ => {}
                }
                true
            }));
            let rename = tokio::spawn(LORE_CONTEXT.scope(fuse_fixture::setup_test_execution(), {
                let fs = fs.clone();
                async move {
                    let renamed = fs
                        .rename(&request(), ROOT_INO, b"e", ROOT_INO, b"e2", 0)
                        .await;
                    let _ = renamed_tx.send(());
                    renamed
                }
            }));
            tokio::task::block_in_place(|| {
                window_rx.recv_timeout(WAIT).expect("the RENAME resolved e");
            });

            let revision = commit_through(&instance, &fs, false).await.expect("commit");
            let renamed = rename.await.expect("the RENAME's task");

            assert_eq!(
                fs.commit_outcomes().0,
                1,
                "the commit flipped e/x.txt inside the RENAME"
            );
            drop(fs);
            let again = provider_at(&instance, revision).await;
            let at = if renamed.is_ok() {
                "e2/x.txt"
            } else {
                "e/x.txt"
            };
            assert_eq!(
                served_at(&again, at).await,
                b"ex",
                "the remount lost the file (RENAME: {renamed:?})"
            );
            assert_eq!(renamed, Err(Errno::EXDEV));
        })
        .await;
}

/// An UNLINK whose file a commit flips to the store while the UNLINK opens the file's host file
/// sets nothing aside and removes the committed file.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlink_whose_file_a_commit_flips_while_it_opens_the_host_file_removes_it() {
    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Arc::new(Instance::new(&[("d/a.txt", b"alpha"), ("d/b.txt", b"bee")]).await);
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let d = lookup(&fs, ROOT_INO, "d").await.expect("d");
            let a = lookup(&fs, d, "a.txt").await.expect("a.txt");
            fs.write(&request(), a, 0, 0, b"ALPHA", 0)
                .await
                .expect("the edit the commit stores");
            fs.close_local_file(a);
            let settled = commit_inside_step(&instance, &fs, &[], move |step| {
                step == WriteStep::DescriptorOpened { ino: a }
            });

            let unlinked = fs.unlink(&request(), d, b"a.txt").await;

            assert_eq!(
                *settled.lock().expect("settled"),
                Some(vec![]),
                "the commit ran inside the unlink"
            );
            assert_eq!(
                fs.commit_outcomes().0,
                1,
                "the commit flipped a.txt inside the unlink"
            );
            unlinked.expect("the unlink");
            assert!(lookup(&fs, d, "a.txt").await.is_err());
            assert_eq!(changed_paths(&instance, &fs).await, vec!["d/a.txt"]);
        })
        .await;
}

/// FSYNC of a tracked file has nothing to sync and answers on the worker; FSYNC of a local file
/// answers `WouldFetch` there, and its flush off the worker syncs the host file and succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn fsync_answers_a_tracked_file_on_the_worker_and_flushes_a_local_one_off_it() {
    let instance = Instance::new(&[("a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let tracked = lookup(&fs, ROOT_INO, "a.txt").await.expect("a.txt");
            assert!(
                matches!(fs.fsync_local(&request(), tracked, 0, false).await, Ok(())),
                "a tracked file syncs on the worker"
            );
            assert!(
                !instance.write_dir().join("a.txt").exists(),
                "FSYNC copies nothing up"
            );
            let local = create_with(&fs, ROOT_INO, "new.txt", b"new").await;
            assert!(
                matches!(
                    fs.fsync_local(&request(), local, 0, true).await,
                    Err(ReadFault::WouldFetch)
                ),
                "a local file's flush goes off the worker"
            );
            for datasync in [true, false] {
                fs.fetch(
                    &request(),
                    Fetch::Flush {
                        ino: local,
                        datasync,
                    },
                )
                .await
                .expect("the flush");
            }
        })
        .await;
}

/// An instance write of more than one chunk reaches the provider as one WRITE per chunk, each at
/// its own offset, and the file holds every byte.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_write_larger_than_a_chunk_writes_every_chunk_at_its_offset() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use bytes::Bytes;
    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::operation::FuseOperation;

    const CHUNK: usize = 1 << 20;
    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let writes = Arc::new(AtomicUsize::new(0));
            fs.set_write_hook(Arc::new({
                let writes = writes.clone();
                move |step| {
                    if let WriteStep::HostWrite { .. } = step {
                        writes.fetch_add(1, Ordering::SeqCst);
                    }
                    true
                }
            }));
            let content = pattern(2 * CHUNK + CHUNK / 2, 11);
            let op = FuseOperation::new(fs.clone());
            op.write_file(
                &lore_revision::util::path::RelativePath::new_from_initial_path("big.bin")
                    .expect("a path"),
                Bytes::from(content.clone()),
            )
            .await
            .expect("the instance write");
            drop(op);

            assert_eq!(writes.load(Ordering::SeqCst), 3, "one WRITE per chunk");
            let big = lookup(&fs, ROOT_INO, "big.bin").await.expect("big.bin");
            assert!(read(&fs, big, content.len() + 1).await == content);
            assert!(
                std::fs::read(instance.write_dir().join("big.bin")).expect("write/big.bin")
                    == content
            );
        })
        .await;
}

/// An instance write whose missing directory another writer makes meanwhile, after the write
/// found nothing there and before it made the directory, goes on into that directory: the file
/// lands in it, and both are there after a remount.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_write_whose_directory_another_writer_made_meanwhile_goes_on_into_it() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::operation::FuseOperation;

    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let armed = AtomicBool::new(true);
            let made = Arc::new(Mutex::new(None::<Result<u64, Errno>>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let made = made.clone();
                move |step| {
                    if step == (WriteStep::HostPathBuilt { parent: ROOT_INO })
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let made = fs.mkdir(&request(), ROOT_INO, b"x", 0o755, 0o022).await;
                                let _ = done_tx.send(made.map(|entry| entry.ino));
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *made.lock().expect("made") =
                                done_rx.recv_timeout(Duration::from_secs(10)).ok();
                        });
                    }
                    true
                }
            }));

            let op = FuseOperation::new(fs.clone());
            let written = op
                .write_file(
                    &lore_revision::util::path::RelativePath::new_from_initial_path("x/new.txt")
                        .expect("a path"),
                    bytes::Bytes::from_static(b"new"),
                )
                .await;
            drop(op);

            let made = made
                .lock()
                .expect("made")
                .expect("the MKDIR ran inside the write");
            let x = made.expect("the MKDIR made x");
            written.expect("the instance write");
            assert_eq!(lookup(&fs, ROOT_INO, "x").await, Ok(x), "x is the MKDIR's");
            assert_eq!(served_at(&fs, "x/new.txt").await, b"new");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["keep.txt", "x"]);
            assert_eq!(served_at(&fs, "x/new.txt").await, b"new");
        })
        .await;
}

/// A WRITE copying up a tracked file whose host path it built just before a case-only RENAME of
/// the directory moved and respelled it builds the path again: the copy lands below the new
/// spelling, the write succeeds, and the file is there after a remount.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_up_racing_a_rename_of_its_directory_lands_below_the_new_name() {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::fuse::filesystem::WriteStep;

    let instance = Instance::new(&[("dir/a.txt", b"alpha")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let dir = lookup(&fs, ROOT_INO, "dir").await.expect("dir");
            let a = lookup(&fs, dir, "a.txt").await.expect("a.txt");
            let armed = AtomicBool::new(true);
            let renamed = Arc::new(Mutex::new(None::<Result<(), Errno>>));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let renamed = renamed.clone();
                move |step| {
                    if let WriteStep::CopyUpPathBuilt { parent } = step
                        && parent == dir
                        && armed.swap(false, Ordering::SeqCst)
                        && let Some(fs) = weak.upgrade()
                    {
                        let (done_tx, done_rx) = mpsc::channel::<Result<(), Errno>>();
                        runtime.spawn(LORE_CONTEXT.scope(
                            fuse_fixture::setup_test_execution(),
                            async move {
                                let result = fs
                                    .rename(&request(), ROOT_INO, b"dir", ROOT_INO, b"Dir", 0)
                                    .await;
                                let _ = done_tx.send(result);
                            },
                        ));
                        tokio::task::block_in_place(|| {
                            *renamed.lock().expect("renamed") =
                                done_rx.recv_timeout(Duration::from_secs(10)).ok();
                        });
                    }
                    true
                }
            }));

            let written = fs.write(&request(), a, 0, 0, b"ALPHA", 0).await;

            assert_eq!(
                *renamed.lock().expect("renamed"),
                Some(Ok(())),
                "the directory's rename ran inside the copy-up"
            );
            assert_eq!(written, Ok(5), "the write");
            let write_dir = instance.write_dir();
            assert_eq!(
                std::fs::read(write_dir.join("Dir/a.txt")).expect("write/Dir/a.txt"),
                b"ALPHA"
            );
            assert!(!write_dir.join("dir").exists(), "write/dir came back");
            drop(fs);

            let fs = provider(&instance).await;
            assert_eq!(listed(&fs, ROOT_INO).await, vec!["Dir"]);
            assert_eq!(served_at(&fs, "Dir/a.txt").await, b"ALPHA");
        })
        .await;
}

/// A forced sync that overwrites a file edited through the mount, and removes another, has the
/// kernel drop what it cached of both: a descriptor held across the sync reads the target's
/// content, and the removed file is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_sync_over_files_edited_through_the_mount_drops_what_the_kernel_cached() {
    use std::os::unix::fs::FileExt;

    skip_without_fuse!();
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    let m = instance.mount().await;
    let root = m.root.clone();
    // The edit is as long as the target's content, so only an inode invalidation shows.
    let held = blocking({
        let root = root.clone();
        move || {
            std::fs::write(root.join("dir/f.txt"), b"TWO").expect("edit f.txt");
            std::fs::write(root.join("gone/x.txt"), b"X-EDIT").expect("edit x.txt");
            assert_eq!(
                std::fs::read(root.join("gone/x.txt")).expect("read x.txt"),
                b"X-EDIT"
            );
            let held = std::fs::File::open(root.join("dir/f.txt")).expect("open f.txt");
            let mut cached = [0u8; 3];
            held.read_exact_at(&mut cached, 0).expect("read f.txt");
            assert_eq!(&cached, b"TWO");
            held
        }
    })
    .await;

    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            sync_through(
                &instance,
                &m.fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("the forced sync");

    blocking(move || {
        let mut read = [0u8; 3];
        held.read_exact_at(&mut read, 0).expect("read f.txt again");
        assert_eq!(
            &read, b"one",
            "the held descriptor served the overwritten edit"
        );
        assert!(
            std::fs::metadata(root.join("gone/x.txt")).is_err(),
            "the removed file is still there"
        );
    })
    .await;
    m.unmount().await;
}

/// A forced sync whose target adds a file where one made through the mount stands, a change the
/// plan does not count as a rewrite, has the kernel drop what it cached of the local file: a
/// descriptor held across the sync reads the target's content.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_sync_adding_a_file_over_a_local_one_drops_what_the_kernel_cached() {
    use std::os::unix::fs::FileExt;

    skip_without_fuse!();
    let instance = sync_instance().await;
    let first = instance.fixture.state.parent_self();
    let m = instance.mount().await;
    let root = m.root.clone();
    let held = blocking({
        let root = root.clone();
        move || {
            std::fs::create_dir_all(root.join("new/a")).expect("mkdir new/a");
            std::fs::write(root.join("new/a/b.txt"), b"BEE").expect("make b.txt");
            let held = std::fs::File::open(root.join("new/a/b.txt")).expect("open b.txt");
            let mut cached = [0u8; 3];
            held.read_exact_at(&mut cached, 0).expect("read b.txt");
            assert_eq!(&cached, b"BEE");
            held
        }
    })
    .await;

    LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            sync_through(
                &instance,
                &m.fs,
                first,
                lore_revision::interface::LoreGlobalArgs {
                    force: 1,
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("the forced sync");

    blocking(move || {
        let mut read = [0u8; 3];
        held.read_exact_at(&mut read, 0).expect("read b.txt again");
        assert_eq!(
            &read, b"bee",
            "the held descriptor served the overwritten local file"
        );
        assert_eq!(
            std::fs::read(root.join("new/a/b.txt")).expect("read b.txt by name"),
            b"bee"
        );
    })
    .await;
    m.unmount().await;
}

/// Instance writes into a directory none of them finds, as a merge writes the side files of its
/// conflicts below a directory the local side deleted, all land: a write whose create of the
/// directory finds the host directory another has just made waits for that create's inode before
/// it looks, rather than answering `ENOTDIR` or making an inode the create then replaces.
#[tokio::test(flavor = "multi_thread")]
async fn two_instance_writes_racing_to_make_their_directory_both_land() {
    instance_writes_racing_to_make_their_directory(2).await;
}

/// [`two_instance_writes_racing_to_make_their_directory_both_land`] with eight writes, whose
/// creates that lose the race hold the name's claim briefly beside the one that wins.
#[tokio::test(flavor = "multi_thread")]
async fn eight_instance_writes_racing_to_make_their_directory_all_land() {
    instance_writes_racing_to_make_their_directory(8).await;
}

/// Writes `new/w<n>.txt` with each of `writes` instance writes at once. The first is held between
/// making the host directory `new` and attaching its node until each other write has finished or
/// waits for a create of `new`, then 50 ms longer, so a write that looked without waiting would
/// find nothing in every round. Every file is served, and served again by a fresh provider.
async fn instance_writes_racing_to_make_their_directory(writes: usize) {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use lore_revision::fs::filesystem_provider::InstanceOperation as _;
    use lore_revision::fs::fuse::filesystem::WriteStep;
    use lore_revision::fs::fuse::operation::FuseOperation;
    use lore_revision::util::path::RelativePath;

    type Writes = Arc<Mutex<Vec<tokio::task::JoinHandle<Result<(), String>>>>>;
    const WAIT: Duration = Duration::from_secs(10);

    fn file(n: usize) -> String {
        format!("new/w{n}.txt")
    }
    fn content(n: usize) -> Vec<u8> {
        format!("content {n}").into_bytes()
    }
    async fn write(fs: Arc<FuseFilesystem>, n: usize) -> Result<(), String> {
        let path = RelativePath::new_from_initial_path(file(n)).expect("path");
        FuseOperation::new(fs)
            .write_file(&path, bytes::Bytes::from(content(n)))
            .await
            .map_err(|error| error.to_string())
    }

    let instance = Instance::new(&[("keep.txt", b"keep")]).await;
    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            let fs = provider(&instance).await;
            let others: Writes = Arc::default();
            // Set where the first write's hold timed out, so the window did not run as meant.
            let missed = Arc::new(AtomicBool::new(false));
            let weak = Arc::downgrade(&fs);
            let runtime = tokio::runtime::Handle::current();
            fs.set_write_hook(Arc::new({
                let (others, missed) = (others.clone(), missed.clone());
                let armed = AtomicBool::new(true);
                let (done_tx, done_rx) = mpsc::channel::<()>();
                let (done_tx, done_rx) = (Mutex::new(done_tx), Mutex::new(done_rx));
                move |step| {
                    // The first write's host directory made, before its node is attached.
                    if let WriteStep::HostEntryMade { parent } = step
                        && parent == ROOT_INO
                        && armed.swap(false, Ordering::SeqCst)
                    {
                        let Some(fs) = weak.upgrade() else {
                            return true;
                        };
                        for n in 1..writes {
                            let (fs, done) = (fs.clone(), done_tx.lock().expect("done").clone());
                            let other = runtime.spawn(LORE_CONTEXT.scope(
                                fuse_fixture::setup_test_execution(),
                                async move {
                                    let written = write(fs, n).await;
                                    let _ = done.send(());
                                    written
                                },
                            ));
                            others.lock().expect("others").push(other);
                        }
                        tokio::task::block_in_place(|| {
                            let done = done_rx.lock().expect("done");
                            for _ in 1..writes {
                                if done.recv_timeout(WAIT).is_err() {
                                    missed.store(true, Ordering::SeqCst);
                                    break;
                                }
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        });
                    }
                    if let WriteStep::AwaitingCreate { parent } = step
                        && parent == ROOT_INO
                    {
                        let _ = done_tx.lock().expect("done").send(());
                    }
                    true
                }
            }));

            let first = write(fs.clone(), 0).await;
            let others = std::mem::take(&mut *others.lock().expect("others"));
            assert_eq!(others.len(), writes - 1, "the other writes ran");
            let mut written = Vec::new();
            for other in others {
                written.push(
                    tokio::time::timeout(WAIT, other)
                        .await
                        .expect("a write finished")
                        .expect("a write's task"),
                );
            }
            assert!(
                !missed.load(Ordering::SeqCst),
                "each other write finished or waited for the first"
            );
            first.expect("the first write");
            for (n, written) in (1..).zip(written) {
                written.unwrap_or_else(|error| panic!("write {n}: {error}"));
            }
            for n in 0..writes {
                assert_eq!(served_at(&fs, &file(n)).await, content(n));
            }
            drop(fs);
            let fs = provider(&instance).await;
            for n in 0..writes {
                assert_eq!(
                    served_at(&fs, &file(n)).await,
                    content(n),
                    "after a remount"
                );
            }
        })
        .await;
}
