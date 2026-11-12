// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Local behaviour of a mounted FUSE instance, driven through the kernel with ordinary syscalls:
//! reads, listings, names resolved as spelled, read-only refusals, swaps under an open file,
//! stable inode numbers, and lookup accounting at unmount.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Read as _;
use std::io::Seek as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_fuse::skip_without_fuse;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::fs::fuse::notifications::Invalidations;
use lore_revision::fs::mount_manager::MountManager;
use lore_revision::fs::mount_manager_state::MountManagerState;
use lore_revision::lore::RepositoryId;
use lore_revision::repository::RepositoryContext;
use lore_revision::repository::RepositoryWriteToken;
use lore_revision::repository::VfsType;
use lore_revision::repository::load_repository_config_from_dot_dir;
use lore_revision::revision::sync::SyncOptions;
use lore_revision::state::State;
use lore_storage::FlushOptions;

use crate::fuse_service::*;

/// A FUSE instance synced to a commit of `files`, with its anchor stored so it can remount.
struct Mounted {
    path: PathBuf,
    /// The instance's context, holding no write token.
    repository: Arc<RepositoryContext>,
    mutable: Arc<dyn lore_storage::MutableStore>,
    state: Arc<State>,
    manager: Arc<MountManager>,
    twin: Twin,
    // Dropped in declaration order: unmount before the directories go.
    _guard: MountGuard,
    _dirs: Vec<TempDir>,
}

impl Mounted {
    async fn new(files: &[(&str, &[u8])]) -> Self {
        let (store_dir, store) = canonical_dir("lore-fuse-int-store-");
        let (dir, path) = canonical_dir("lore-fuse-int-");
        let guard = MountGuard(path.clone());
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());
        let (repository, token) = Box::pin(create_instance(
            &path,
            repository_id,
            shared_store_config(&store, VfsType::Fuse),
        ))
        .await;
        let twin = Box::pin(Twin::new(&store, repository_id)).await;
        let state = Box::pin(twin.commit(files)).await;
        Box::pin(sync_to(&repository, State::new(), state.clone())).await;
        lore_revision::instance::store_current_anchor(&repository, state.revision())
            .await
            .expect("anchor stores");
        repository
            .flush(FlushOptions::SYNC_DATA)
            .await
            .expect("flushes");
        drop(token);
        let mutable = repository
            .try_mutable_store_arc()
            .expect("the created instance holds a write token");
        Self {
            path,
            // The mount records its writes in the staged State under the instance's write
            // token, which a context holding a sibling of it would keep from it.
            repository: Arc::new(repository.without_command_locks()),
            mutable,
            state,
            manager: MountManagerState::mount_manager().expect("manager is initialised"),
            twin,
            _guard: guard,
            _dirs: vec![dir, store_dir],
        }
    }

    /// Commits `files` over the twin's tree and syncs the mount to it, as a relayed sync does:
    /// under the instance's write token, through the mount's provider.
    async fn sync(&mut self, files: &[(&str, &[u8])]) {
        let target = Box::pin(self.twin.commit(files)).await;
        let token = RepositoryWriteToken::acquire(&self.path).await;
        let filesystem = self
            .manager
            .filesystem_provider(&self.path)
            .expect("the mount's provider");
        let repository = Arc::new(
            RepositoryContext::new(
                default_repository_creation_args(
                    self.repository.immutable_store(),
                    self.mutable.clone(),
                )
                .with_path(&self.path)
                .with_id(self.repository.id)
                .with_instance_id(self.repository.instance_id)
                .with_filesystem_provider(filesystem),
            )
            .with_write_token(token),
        );
        Box::pin(sync_to(&repository, self.state.clone(), target.clone())).await;
        self.state = target;
    }

    async fn remount(&self) {
        let dot_lore = dot_lore_of(&self.repository);
        let config = load_repository_config_from_dot_dir(&dot_lore).expect("config loads");
        self.manager
            .fuse()
            .remount_instance(
                self.repository.clone(),
                &instance_metadata(&self.repository, &self.path),
                &dot_lore,
                &config,
            )
            .await
            .expect("remount succeeds");
    }

    /// Unmounts and, with debug assertions, checks the kernel's lookups were all accounted for.
    async fn unmount(&self) {
        let mount = self.manager.fuse().mounted(&self.path).expect("mounted");
        self.manager
            .fuse()
            .unmount(&self.path, false)
            .await
            .expect("an idle mount unmounts");
        assert!(!is_mounted(&self.path), "the mount is gone after unmount");
        if cfg!(debug_assertions) {
            assert_eq!(mount.leaked_lookups(), Some(Vec::new()), "leaked lookups");
        }
        let late = mount
            .filesystem
            .notifications()
            .expect("the mount's notification thread")
            .send(Invalidations {
                entries: Vec::new(),
                inodes: vec![ROOT_INO],
            })
            .await;
        assert_eq!(
            late.inodes,
            [ROOT_INO],
            "unmount ended the notification thread"
        );
    }
}

async fn sync_to(repository: &Arc<RepositoryContext>, from: Arc<State>, to: Arc<State>) {
    let operation = repository
        .file_system()
        .begin_operation()
        .await
        .expect("the FUSE provider begins an operation");
    lore_revision::fs::realize::realize_state(
        repository.clone(),
        repository.clone(),
        operation,
        from,
        to,
        SyncOptions::default(),
    )
    .await
    .expect("syncing into the FUSE instance succeeds");
}

/// Runs `test` under the service's execution context, holding `SERIAL`. Boxed: the test
/// futures are large.
fn in_service(test: impl Future<Output = ()>) -> Pin<Box<impl Future<Output = ()>>> {
    Box::pin(async {
        let _serial = SERIAL.lock().await;
        let execution = service_execution().await;
        LORE_CONTEXT.scope(execution, test).await;
    })
}

/// Asserts no FUSE mount of this binary's kind is left in the mount table.
fn assert_no_lore_mounts() {
    let table = std::fs::read_to_string("/proc/self/mountinfo").expect("mountinfo reads");
    let left: Vec<&str> = table
        .lines()
        .filter(|line| line.contains(" - fuse.lore ") && line.contains("lore-fuse-int-"))
        .collect();
    assert!(left.is_empty(), "mounts left behind: {left:#?}");
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Dir,
    File,
}

/// Every path under `root`, relative to it, with its kind and inode number, by a blocking walk.
async fn walk(root: &Path) -> BTreeMap<String, (Kind, u64)> {
    let root = root.to_path_buf();
    lore_base::lore_spawn_blocking!(move || {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("directory lists") {
                let entry = entry.expect("entry reads");
                let full = entry.path();
                let meta = std::fs::symlink_metadata(&full).expect("entry stats");
                let relative = full
                    .strip_prefix(&root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                let kind = if meta.is_dir() {
                    pending.push(full);
                    Kind::Dir
                } else {
                    Kind::File
                };
                found.insert(relative, (kind, meta.ino()));
            }
        }
        found
    })
    .await
    .expect("walk task completes")
}

/// The kinds a commit of `files` produces, directories included.
fn expected_tree(files: &[(&str, &[u8])]) -> BTreeMap<String, Kind> {
    let mut tree = BTreeMap::new();
    for (name, _) in files {
        let mut prefix = PathBuf::new();
        let components: Vec<_> = Path::new(name).components().collect();
        for (index, component) in components.iter().enumerate() {
            prefix.push(component);
            let kind = if index + 1 == components.len() {
                Kind::File
            } else {
                Kind::Dir
            };
            tree.insert(prefix.to_string_lossy().into_owned(), kind);
        }
    }
    tree
}

/// A deterministic byte pattern that differs at every offset modulo a prime, so a misplaced
/// chunk cannot compare equal.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 251) as u8 ^ seed.wrapping_mul(31))
        .collect()
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    lore_base::lore_spawn_blocking!(work)
        .await
        .expect("blocking task completes")
}

fn errno_of<T: std::fmt::Debug>(result: std::io::Result<T>) -> Option<i32> {
    result.err().and_then(|error| error.raw_os_error())
}

fn status_errno(status: libc::c_int) -> Option<i32> {
    if status == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
}

fn c_path(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("no interior nul")
}

#[tokio::test(flavor = "multi_thread")]
async fn every_committed_file_reads_back_byte_exact() {
    skip_without_fuse!();
    in_service(async {
        // Around and across the 1 MiB largest READ, plus an empty file.
        let big = pattern(3 * (1 << 20) + 17, 1);
        let edge = pattern(1 << 20, 2);
        let small = pattern(4097, 3);
        let files: Vec<(&str, &[u8])> = vec![
            ("empty.txt", b""),
            ("small.bin", &small),
            ("dir/edge.bin", &edge),
            ("dir/deeper/big.bin", &big),
            ("dir/deeper/tiny.txt", b"t"),
        ];
        let mounted = Mounted::new(&files).await;
        for (name, contents) in &files {
            let read = read_file(mounted.path.join(name)).await;
            assert_eq!(read.len(), contents.len(), "{name} length");
            assert!(read == *contents, "{name} differs from its committed bytes");
        }
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

#[tokio::test(flavor = "multi_thread")]
async fn recursive_listing_matches_committed_tree() {
    skip_without_fuse!();
    in_service(async {
        let files: Vec<(&str, &[u8])> = vec![
            ("a.txt", b"a"),
            ("b/c.txt", b"c"),
            ("b/d/e.txt", b"e"),
            ("b/d/f/g.txt", b"g"),
            ("h/i.txt", b"i"),
            ("h/j.txt", b"j"),
        ];
        let mounted = Mounted::new(&files).await;
        let listed: BTreeMap<String, Kind> = walk(&mounted.path)
            .await
            .into_iter()
            .map(|(path, (kind, _))| (path, kind))
            .collect();
        assert_eq!(listed, expected_tree(&files));
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

/// Answers the errno of looking up `path` through the mount, `None` where it is found.
async fn lookup_errno(path: PathBuf) -> Option<i32> {
    errno_of(blocking(move || std::fs::metadata(&path)).await)
}

/// Answers the errno of creating a file at `path` through the mount, `None` where it is made.
async fn create_errno(path: PathBuf) -> Option<i32> {
    errno_of(
        blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
        })
        .await,
    )
}

/// The mount answers a name only in the spelling it holds, as an ordinary Linux clone does:
/// another spelling is absent, and creating one is refused, since a repository holds one
/// spelling of a name.
#[tokio::test(flavor = "multi_thread")]
async fn case_variant_paths_answer_absent_and_cannot_be_created() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[("Dir/File.txt", b"case")]).await;
        assert_eq!(read_file(mounted.path.join("Dir/File.txt")).await, b"case");
        for spelling in [
            "dir",
            "dir/file.txt",
            "DIR/FILE.TXT",
            "dIr/fIlE.tXt",
            "Dir/file.txt",
        ] {
            assert_eq!(
                lookup_errno(mounted.path.join(spelling)).await,
                Some(libc::ENOENT),
                "{spelling} resolved"
            );
        }

        assert_eq!(
            create_errno(mounted.path.join("Dir/FILE.TXT")).await,
            Some(libc::EEXIST),
            "a file spelled as File.txt in another case was created"
        );
        let dir = mounted.path.join("dir");
        assert_eq!(
            errno_of(blocking(move || std::fs::create_dir(&dir)).await),
            Some(libc::EEXIST),
            "a directory spelled as Dir in another case was made"
        );
        // Neither the lookups nor the refused creates rename the committed entries.
        assert_eq!(list_names(&mounted.path).await, vec!["Dir"]);
        assert_eq!(
            list_names(&mounted.path.join("Dir")).await,
            vec!["File.txt"]
        );
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

/// A name beyond ASCII also resolves only as spelled, and collides with another as `to_lowercase`
/// folds the two, the rule the State hashes names by.
#[tokio::test(flavor = "multi_thread")]
async fn unicode_names_resolve_as_spelled_and_collide_as_they_fold() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[
            ("ÅNGSTRÖM/ÉCOLE.md", b"accents"),
            ("ΟΔΥΣΣΕΥΣ-1.txt", b"sigma"),
        ])
        .await;
        assert_eq!(
            read_file(mounted.path.join("ÅNGSTRÖM/ÉCOLE.md")).await,
            b"accents"
        );
        assert_eq!(
            read_file(mounted.path.join("ΟΔΥΣΣΕΥΣ-1.txt")).await,
            b"sigma"
        );
        for spelling in [
            "ångström/école.md",
            "ÅNGSTRÖM/école.MD",
            "οδυσσευς-1.txt",
            "Οδυσσευς-1.TXT",
        ] {
            assert_eq!(
                lookup_errno(mounted.path.join(spelling)).await,
                Some(libc::ENOENT),
                "{spelling} resolved"
            );
        }

        // `to_lowercase` folds a word-final capital sigma to `ς`, where a per-char fold gives
        // `σ`. Before `-` the sigma is final, so the name spelled with a medial `σ` there folds
        // to another name, and only that one can be made beside the held one.
        for spelling in ["ÅNGSTRÖM/école.MD", "οδυσσευς-1.txt"] {
            assert_eq!(
                create_errno(mounted.path.join(spelling)).await,
                Some(libc::EEXIST),
                "{spelling} was created beside the name it folds to"
            );
        }
        assert_eq!(
            create_errno(mounted.path.join("οδυσσευσ-1.txt")).await,
            None,
            "a name folding to no held one was refused"
        );
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

/// A name looked up again once the unmount's drain has emptied the table, as a desktop's file
/// monitor does on a create, is invalidated again before the unmount goes on, so it finds no
/// lookup left.
#[tokio::test(flavor = "multi_thread")]
async fn unmount_drains_a_lookup_made_after_its_invalidations() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[("file.txt", b"tracked")]).await;
        let path = mounted.path.join("file.txt");
        assert_eq!(read_file(path.clone()).await, b"tracked");
        let (reached, mut emptied) = tokio::sync::mpsc::unbounded_channel();
        let resume = Arc::new(tokio::sync::Notify::new());
        let look_up = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), emptied.recv())
                .await
                .expect("the drain empties the table")
                .expect("the drain reports it");
            blocking(move || std::fs::metadata(path))
                .await
                .expect("the file is looked up after the invalidations");
            resume.notify_one();
            while emptied.recv().await.is_some() {
                resume.notify_one();
            }
        };
        tokio::join!(
            lore_revision::test_util::hold_unmount_drains(
                reached,
                resume.clone(),
                mounted.unmount()
            ),
            look_up
        );
    })
    .await;
    assert_no_lore_mounts();
}

#[tokio::test(flavor = "multi_thread")]
async fn create_chmod_rename_truncate_and_unlink_succeed() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[("file.txt", b"tracked"), ("dir/inner.txt", b"i")]).await;
        let root = mounted.path.clone();
        let outcomes: Vec<(&str, Option<i32>)> = blocking(move || {
            let file = root.join("file.txt");
            let moved = root.join("dir/moved.txt");
            let moved_c = c_path(&moved);
            let create = errno_of(std::fs::File::create(root.join("new.txt")));
            let chmod = errno_of(std::fs::set_permissions(
                &file,
                std::fs::Permissions::from_mode(0o600),
            ));
            // Writes through the mount, as a build makes them.
            #[allow(clippy::disallowed_methods)]
            let rename = errno_of(std::fs::rename(&file, &moved));
            // SAFETY: `moved_c` is a valid nul-terminated path.
            let truncate = status_errno(unsafe { libc::truncate(moved_c.as_ptr(), 0) });
            #[allow(clippy::disallowed_methods)]
            let unlink = errno_of(std::fs::remove_file(root.join("dir/inner.txt")));
            vec![
                ("create", create),
                ("chmod", chmod),
                ("rename", rename),
                ("truncate", truncate),
                ("unlink", unlink),
            ]
        })
        .await;
        for (operation, errno) in outcomes {
            assert_eq!(errno, None, "{operation}");
        }
        assert_eq!(read_file(mounted.path.join("dir/moved.txt")).await, b"");
        assert_eq!(list_names(&mounted.path).await, vec!["dir", "new.txt"]);
        assert_eq!(
            list_names(&mounted.path.join("dir")).await,
            vec!["moved.txt"]
        );
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

/// Copies `from` to a new file `to` by `copy_file_range(2)` alone; answers the bytes copied.
fn copy_file_range_all(from: &Path, to: &Path) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd as _;
    let source = std::fs::File::open(from)?;
    let target = std::fs::File::create(to)?;
    let len = source.metadata()?.len();
    let mut copied = 0;
    while copied < len {
        let remaining = usize::try_from(len - copied).unwrap_or(usize::MAX);
        // SAFETY: both descriptors are open, and null offsets use and advance their positions.
        let n = unsafe {
            libc::copy_file_range(
                source.as_raw_fd(),
                std::ptr::null_mut(),
                target.as_raw_fd(),
                std::ptr::null_mut(),
                remaining,
                0,
            )
        };
        match n {
            0 => break,
            n if n < 0 => return Err(std::io::Error::last_os_error()),
            n => copied += n as u64,
        }
    }
    Ok(copied)
}

/// `cp` tries `copy_file_range` first. The kernel copies by splice, or answers `EOPNOTSUPP` for
/// an xattr write or `O_TMPFILE`, only once the mount answers these with `ENOSYS`.
#[tokio::test(flavor = "multi_thread")]
async fn cp_and_copy_file_range_copy_and_xattr_writes_and_tmpfile_are_unsupported() {
    skip_without_fuse!();
    in_service(async {
        let content = pattern(300_000, 7);
        let mounted =
            Mounted::new(&[("src.bin", content.as_slice()), ("dir/keep.txt", b"k")]).await;
        let root = mounted.path.clone();
        let (cp, copied, setxattr, removexattr, tmpfile) = blocking(move || {
            let source = root.join("src.bin");
            let cp = std::process::Command::new("cp")
                .arg(&source)
                .arg(root.join("cp.bin"))
                .output();
            let copied = copy_file_range_all(&source, &root.join("dir/range.bin"));
            let source_c = c_path(&source);
            // SAFETY: the path and name are nul-terminated and the value is 1 byte long.
            let setxattr = status_errno(unsafe {
                libc::setxattr(
                    source_c.as_ptr(),
                    c"user.lore-test".as_ptr(),
                    c"v".as_ptr().cast(),
                    1,
                    0,
                )
            });
            // SAFETY: the path and name are nul-terminated.
            let removexattr = status_errno(unsafe {
                libc::removexattr(source_c.as_ptr(), c"user.lore-test".as_ptr())
            });
            let dir_c = c_path(&root.join("dir"));
            // SAFETY: the path is nul-terminated; a descriptor returned is closed at once.
            let fd = unsafe { libc::open(dir_c.as_ptr(), libc::O_TMPFILE | libc::O_RDWR, 0o600) };
            let tmpfile = if fd >= 0 {
                // SAFETY: `fd` was just opened.
                unsafe { libc::close(fd) };
                None
            } else {
                std::io::Error::last_os_error().raw_os_error()
            };
            (cp, copied, setxattr, removexattr, tmpfile)
        })
        .await;
        let cp = cp.expect("cp runs");
        assert!(cp.status.success(), "cp inside the mount fails: {cp:?}");
        assert_eq!(copied.ok(), Some(content.len() as u64), "copy_file_range");
        assert!(read_file(mounted.path.join("cp.bin")).await == content);
        assert!(read_file(mounted.path.join("dir/range.bin")).await == content);
        assert_eq!(setxattr, Some(libc::EOPNOTSUPP), "setxattr");
        assert_eq!(removexattr, Some(libc::EOPNOTSUPP), "removexattr");
        assert_eq!(tmpfile, Some(libc::EOPNOTSUPP), "O_TMPFILE");
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

/// Creates files through `root`, an open descriptor of a mount's root, until a create fails;
/// answers the names created.
fn create_until_refused(root: std::fs::File) -> Vec<String> {
    use std::os::fd::AsRawFd as _;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut created = Vec::new();
    while std::time::Instant::now() < deadline {
        let name = format!("c{:06}.txt", created.len());
        let name_c = CString::new(name.as_bytes()).expect("no interior nul");
        // SAFETY: `root` is an open directory and `name_c` is nul-terminated.
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            break;
        }
        // SAFETY: `fd` was just opened.
        unsafe { libc::close(fd) };
        created.push(name);
    }
    created
}

/// A lazy unmount records every namespace change the mount answered, those it answered while
/// detaching included.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_unmount_records_every_change_the_mount_answered() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[("keep.txt", b"k")]).await;
        let path = mounted.path.clone();
        let root = blocking(move || std::fs::File::open(path))
            .await
            .expect("the mount's root opens");
        let creator = std::thread::spawn(move || create_until_refused(root));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        mounted
            .manager
            .fuse()
            .unmount(&mounted.path, true)
            .await
            .expect("a lazy unmount");
        let created = blocking(move || creator.join().expect("the creator")).await;
        assert!(!created.is_empty(), "the creator created nothing");

        mounted.remount().await;
        let listed: std::collections::BTreeSet<String> =
            list_names(&mounted.path).await.into_iter().collect();
        let missing: Vec<&String> = created
            .iter()
            .filter(|name| !listed.contains(*name))
            .collect();
        assert!(
            missing.is_empty(),
            "{} of {} created files were not recorded, first {:?}",
            missing.len(),
            created.len(),
            missing.first()
        );
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_with_open_file_serves_the_new_bytes_through_the_same_inode() {
    skip_without_fuse!();
    in_service(async {
        let mut mounted = Mounted::new(&[("doc.txt", b"old bytes"), ("keep.txt", b"k")]).await;
        let doc = mounted.path.join("doc.txt");
        let open_path = doc.clone();
        let mut open = blocking(move || std::fs::File::open(&open_path))
            .await
            .expect("the file opens");

        mounted
            .sync(&[("doc.txt", b"new, longer bytes"), ("keep.txt", b"k")])
            .await;

        let (open, old, links) = blocking(move || {
            let mut old = Vec::new();
            open.rewind().expect("seeks");
            // Through the fd opened before the sync, not a fresh open.
            #[allow(clippy::verbose_file_reads)]
            open.read_to_end(&mut old).expect("the open fd reads");
            let links = open.metadata().expect("fstat").nlink();
            (open, old, links)
        })
        .await;
        assert_eq!(
            old, b"new, longer bytes",
            "an fd opened before the sync reads the file's new bytes, as on ext4"
        );
        assert_eq!(links, 1, "the file keeps its inode");
        assert_eq!(read_file(doc).await, b"new, longer bytes");
        blocking(move || drop(open)).await;
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

#[tokio::test(flavor = "multi_thread")]
async fn inode_numbers_stable_across_unmount_and_remount() {
    skip_without_fuse!();
    in_service(async {
        let mounted = Mounted::new(&[
            ("top.txt", b"top"),
            ("a/one.txt", b"1"),
            ("a/b/two.txt", b"2"),
            ("A2/Mixed.Case", b"m"),
        ])
        .await;
        let before = walk(&mounted.path).await;
        assert_eq!(before.len(), 7);
        mounted.unmount().await;
        mounted.remount().await;
        let after = walk(&mounted.path).await;
        assert_eq!(before, after, "every path keeps its inode number");
        mounted.unmount().await;
    })
    .await;
    assert_no_lore_mounts();
}

// Only an unmount with debug assertions drains the kernel's lookups first; without it they stay
// uncounted, since the kernel sends no FORGET once the superblock dies.
#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn nlookup_all_zero_after_unmount() {
    skip_without_fuse!();
    in_service(async {
        let mounted =
            Mounted::new(&[("x.txt", b"x"), ("d/y.txt", b"y"), ("d/e/z.txt", b"z")]).await;
        let walked = walk(&mounted.path).await;
        assert_eq!(walked.len(), 5);
        let mount = mounted
            .manager
            .fuse()
            .mounted(&mounted.path)
            .expect("mounted");
        assert!(
            mount.filesystem.inodes().check_all_zero().is_err(),
            "the walk holds lookups while mounted"
        );
        mounted.unmount().await;
        assert_eq!(mount.filesystem.inodes().check_all_zero(), Ok(()));
    })
    .await;
    assert_no_lore_mounts();
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn unmount_after_heavy_ls_reports_no_leaked_lookups() {
    skip_without_fuse!();
    in_service(async {
        let names: Vec<String> = (0..20)
            .flat_map(|d| {
                (0..25).flat_map(move |f| {
                    [
                        format!("dir{d:02}/file{f:02}.txt"),
                        format!("dir{d:02}/Sub/deep{f:02}.bin"),
                    ]
                })
            })
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), &b"x"[..]))
            .collect();
        let mounted = Mounted::new(&files).await;
        let root = mounted.path.clone();
        let listing = blocking(move || {
            std::process::Command::new("ls")
                .args(["-laR"])
                .arg(&root)
                .output()
        })
        .await
        .expect("ls runs");
        assert!(listing.status.success(), "ls -R fails: {listing:?}");
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(text.contains("deep24.bin") && text.contains("dir19"));

        let mount = mounted
            .manager
            .fuse()
            .mounted(&mounted.path)
            .expect("mounted");
        assert_eq!(
            mount.leaked_lookups(),
            None,
            "nothing is checked before unmount"
        );
        mounted.unmount().await;
        assert_eq!(mount.leaked_lookups(), Some(Vec::new()));
    })
    .await;
    assert_no_lore_mounts();
}
