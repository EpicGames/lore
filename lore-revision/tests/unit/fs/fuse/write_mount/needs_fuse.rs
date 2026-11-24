// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use super::*;

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
    let path = m.root.join("linked.elf");

    blocking(move || write_through_a_shared_mapping(&path)).await;
    assert!(
        std::fs::read(instance.write_dir().join("linked.elf")).expect("the host file")
            == pattern(MAPPING_LEN, MAPPING_SEED),
        "the mapping's pages reached the host file"
    );
    m.unmount().await;
}

/// The length of the file [`shared_mapping_writer`] writes.
const MAPPING_LEN: usize = (256 << 10) + 123;

/// The seed of the [`pattern`] [`shared_mapping_writer`] writes.
const MAPPING_SEED: u8 = 11;

/// Names the file [`shared_mapping_writer`] writes.
const MAPPING_PATH_VAR: &str = "LORE_FUSE_WRITE_MOUNT_MAPPING_PATH";

/// The test a writer process runs.
const MAPPING_WRITER_TEST: &str = "fs::fuse::write_mount::needs_fuse::shared_mapping_writer";

/// Starts this test binary again as a writer that writes `path` through a shared mapping, and
/// waits for it to pass. In this process, a fault on a mapping of a mount it serves would wait
/// for a WRITE its own session serves, holding the mapping's lock, and a `fork` elsewhere in the
/// process holds the process's memory map until it has that lock, so the session's threads
/// could not take a page fault to serve the WRITE.
fn write_through_a_shared_mapping(path: &Path) {
    let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", MAPPING_WRITER_TEST, "--ignored", "--nocapture"])
        .env(MAPPING_PATH_VAR, path)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the writer must run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the writer failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The writer process [`write_through_a_shared_mapping`] starts: creates the file
/// [`MAPPING_PATH_VAR`] names, writes [`MAPPING_LEN`] bytes of the [`pattern`] of [`MAPPING_SEED`]
/// through a shared mapping of it, and reads them back. Run without that variable, it does
/// nothing.
#[test]
#[ignore = "the writer process write_through_a_shared_mapping starts, not a test of its own"]
fn shared_mapping_writer() {
    let Some(path) = std::env::var_os(MAPPING_PATH_VAR) else {
        return;
    };
    let data = pattern(MAPPING_LEN, MAPPING_SEED);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("create output");
    file.set_len(MAPPING_LEN as u64).expect("size output");
    // SAFETY: a fresh shared mapping of `MAPPING_LEN` bytes of an open file.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            MAPPING_LEN,
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
    // SAFETY: the mapping is `MAPPING_LEN` bytes, readable and writable, until the munmap below.
    let view = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), MAPPING_LEN) };
    view.copy_from_slice(&data);
    // SAFETY: `mapped` is the live mapping of `MAPPING_LEN` bytes.
    assert_eq!(
        unsafe { libc::msync(mapped, MAPPING_LEN, libc::MS_SYNC) },
        0
    );
    // SAFETY: as above; the view is not used past this point.
    assert_eq!(unsafe { libc::munmap(mapped, MAPPING_LEN) }, 0);
    drop(file);
    assert!(std::fs::read(&path).expect("read back") == data);
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

    reset_through(&instance, &m.fs, &["a.txt"], None, false)
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

    reset_through(&instance, &m.fs, &["dir/b.txt", "keep.txt"], None, false)
        .await
        .expect("the reset");
    working.insert("dir/b.txt".into(), Some(b"bee".to_vec()));
    working.insert("keep.txt".into(), Some(b"kept".to_vec()));
    found.extend(disagreements(&instance, &m, &working, "reset").await);
    m = remount(m, &working, "reset", &mut found).await;

    m.unmount().await;
    assert!(found.is_empty(), "{}", found.join("\n"));
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

/// `files` by path, with each directory above them, as [`walked`] holds a tree.
fn tree_holding(files: &std::collections::BTreeMap<String, Vec<u8>>) -> Tree {
    let mut tree = Tree::new();
    for (path, content) in files {
        let mut parent = path.as_str();
        while let Some((above, _)) = parent.rsplit_once('/') {
            tree.insert(above.to_string(), None);
            parent = above;
        }
        tree.insert(path.clone(), Some(content.clone()));
    }
    tree
}

/// A merge onto a FUSE instance of the branch whose latest revision makes each of
/// [`RENAMES_BELOW`], into a branch from its parent that added a file, serves the tree the merged
/// revision holds: only the spellings it holds, each file with its content, and nothing below a
/// directory's other spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_across_a_move_below_a_case_only_rename_serves_only_the_merged_spellings() {
    use crate::revision::instance::Edit;
    use crate::revision::instance::RENAMES_BELOW;
    use crate::revision::instance::renamed_all;

    skip_without_fuse!();
    const TREE: [&str; 4] = ["Dir/a.txt", "Dir/b.txt", "Dir/Sub/c.txt", "Top.txt"];
    let files: Vec<(&str, &[u8])> = TREE.iter().map(|path| (*path, path.as_bytes())).collect();
    for (edits, renames) in &RENAMES_BELOW {
        let moved: Vec<(&str, &str)> = edits
            .iter()
            .map(|edit| match *edit {
                Edit::Move(from, to) => (from, to),
                other => panic!("{other:?} is not a move"),
            })
            .collect();
        let instance = Instance::with_moves(&files, &moved).await;
        let (_, main) =
            lore_revision::instance::load_current_anchor_boxed(&instance.fixture.repository)
                .await
                .expect("the current anchor");
        let m = instance.mount().await;
        LORE_CONTEXT
            .scope(fuse_fixture::setup_test_execution(), async {
                let parent = instance.fixture.state.parent_self();
                sync_through(&instance, &m.fs, parent, Default::default())
                    .await
                    .expect("sync to the parent");
                create_branch_id_through(&instance, &m.fs, "target").await;
                switch_through(&instance, &m.fs, "target")
                    .await
                    .expect("switch to target");
                let root = m.root.clone();
                blocking(move || std::fs::write(root.join("main.txt"), b"main").expect("write"))
                    .await;
                commit_through(&instance, &m.fs, false)
                    .await
                    .expect("commit on target");
            })
            .await;

        merge_through(&instance, &m.fs, main)
            .await
            .expect("the merge");
        let merged = commit_through(&instance, &m.fs, false)
            .await
            .expect("the merge commit");

        let files: std::collections::BTreeMap<String, Vec<u8>> = TREE
            .iter()
            .map(|path| (path.to_string(), path.as_bytes().to_vec()))
            .collect();
        let mut expected = renamed_all(&files, renames);
        expected.insert("main.txt".to_string(), b"main".to_vec());
        let expected = tree_holding(&expected);
        assert_eq!(walked(&m.root).await, expected, "{edits:?}: served");
        assert_eq!(
            revision_tree(&instance, merged).await,
            expected,
            "{edits:?}: merged"
        );
        m.unmount().await;
    }
}

/// A directory renamed in case through a mount commits at its new spelling under the case handling
/// that renames, and the mount keeps serving it so.
#[tokio::test(flavor = "multi_thread")]
async fn a_directory_renamed_in_case_through_the_mount_commits_at_its_new_spelling() {
    skip_without_fuse!();
    let instance = Instance::new(&[
        ("Dir/a.txt", b"Dir/a.txt"),
        ("Dir/Sub/c.txt", b"Dir/Sub/c.txt"),
        ("Top.txt", b"Top.txt"),
    ])
    .await;
    let m = instance.mount().await;
    let root = m.root.clone();
    blocking(move || std::fs::rename(root.join("Dir"), root.join("dir")).expect("rename")).await;

    LORE_CONTEXT
        .scope(fuse_fixture::setup_test_execution(), async {
            stage_case_through(
                &instance,
                &m.fs,
                lore_revision::stage::StageCaseChange::Rename,
            )
            .await
            .expect("stage");
            let committed = commit_staged(&instance, &m.fs).await.expect("commit");
            assert_eq!(
                revision_paths(&instance, committed).await,
                ["Top.txt", "dir", "dir/Sub", "dir/Sub/c.txt", "dir/a.txt"]
            );
        })
        .await;
    let root = m.root.clone();
    assert_eq!(blocking(move || names(&root)).await, ["Top.txt", "dir"]);
    m.unmount().await;
}
