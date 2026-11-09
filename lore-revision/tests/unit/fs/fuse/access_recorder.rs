// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Access recording (milestone 2, decision 3): the recorder, and the first-access paths the FUSE
//! provider records, no kernel mount. See `lore-fuse/README.md`, "Access recording".

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::test_util::TempDir;
use lore_fuse::codec::dirent::DirentWriter;
use lore_fuse::filesystem::Filesystem;
use lore_revision::fs::fuse::access_recorder::AccessRecorder;
use lore_revision::fs::fuse::access_recorder::RECORD_DIR_ENV;
use lore_revision::fs::fuse::attr::AttrContext;
use lore_revision::fs::fuse::filesystem::FuseFilesystem;
use lore_revision::fs::fuse::inode::ROOT_INO;
use lore_revision::node::NodeBlock;
use lore_revision::node::ROOT_NODE;
use lore_revision::repository::RepositoryContext;
use lore_revision::state::State;
use lore_revision::state::StateNodeChildrenIterator;

use crate::fs::fuse::fuse_fixture;
use crate::fs::fuse::fuse_test_util::req;

#[test]
fn from_env_is_none_when_the_variable_is_unset() {
    assert!(
        std::env::var_os(RECORD_DIR_ENV).is_none(),
        "test isolation requires this unset"
    );
    assert!(AccessRecorder::from_env("test-instance").is_none());
}

#[tokio::test]
async fn start_writes_recorded_lines_and_flushes_on_drop() {
    let dir = TempDir::new("lore-fuse-access-recorder-unit-");
    let path = dir.child("recorded.txt");
    let recorder = AccessRecorder::start(&path).expect("recorder starts");

    recorder.record("/".to_string());
    recorder.record("a.txt".to_string());
    drop(recorder);

    let deadline = std::time::Instant::now() + WRITER_WAIT;
    loop {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        if content.lines().count() >= 2 {
            assert_eq!(content.lines().collect::<Vec<_>>(), vec!["/", "a.txt"]);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer did not flush in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How long a test waits for the recorder's writer before calling it stuck.
const WRITER_WAIT: Duration = Duration::from_secs(60);

const BASE_TIME: u64 = 1_700_000_000_000;
const NO_HANDLE: u64 = 0;

fn attr_context() -> AttrContext {
    AttrContext {
        uid: 1000,
        gid: 1000,
        blksize: 1 << 17,
        base_time: BASE_TIME,
    }
}

async fn provider(repository: Arc<RepositoryContext>, state: Arc<State>) -> Arc<FuseFilesystem> {
    lore_base::runtime::LORE_CONTEXT
        .scope(
            fuse_fixture::setup_test_execution(),
            FuseFilesystem::new(repository, state, attr_context()),
        )
        .await
        .expect("provider construction")
}

async fn mounted(files: &[(&str, &[u8])]) -> (fuse_fixture::FuseFixture, Arc<FuseFilesystem>) {
    let fixture = fuse_fixture::committed(files).await;
    let fs = provider(fixture.repository.clone(), fixture.state.clone()).await;
    (fixture, fs)
}

/// Starts recording on `fs` to a fresh file under a fresh temporary directory, returned so the
/// caller can read it; the directory is removed when it drops.
fn recording(fs: &FuseFilesystem) -> (TempDir, PathBuf) {
    let dir = TempDir::new("lore-fuse-access-recorder-");
    let path = dir.child("recorded.txt");
    let recorder = AccessRecorder::start(&path).expect("access recorder starts");
    fs.start_access_recording(recorder);
    (dir, path)
}

/// The recorded paths of `lines`, each without the block hints a directory carries after a tab.
fn paths(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line.split('\t').next().unwrap_or_default())
        .collect()
}

/// The index of every node block holding a child of the root of `fixture`, ascending, spaced.
async fn root_child_blocks(fixture: &fuse_fixture::FuseFixture) -> String {
    let mut children = StateNodeChildrenIterator::new(
        fixture.state.clone(),
        fixture.repository.clone(),
        ROOT_NODE,
    )
    .await
    .expect("root children open");
    let mut blocks = Vec::new();
    while let Some((id, _)) = children.next().await.expect("root child reads") {
        blocks.push(NodeBlock::index(id));
    }
    blocks.sort_unstable();
    blocks.dedup();
    let spelled: Vec<String> = blocks.iter().map(usize::to_string).collect();
    spelled.join(" ")
}

fn read_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Polls the recorded file until it has at least `expected` lines or `timeout` elapses.
async fn wait_for_lines(path: &Path, expected: usize, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = read_lines(path);
        if lines.len() >= expected || Instant::now() >= deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Polls the recorded file until a line records `expected` or `timeout` elapses.
async fn wait_for_line(path: &Path, expected: &str, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = read_lines(path);
        if paths(&lines).contains(&expected) || Instant::now() >= deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn read(fs: &FuseFilesystem, ino: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let n = read_into(fs, ino, &mut out).await;
    out.truncate(n);
    out
}

/// Reads into a caller-owned buffer, for callers that must not allocate one per call.
async fn read_into(fs: &FuseFilesystem, ino: u64, out: &mut [u8]) -> usize {
    // `ReadFault` carries no `Debug`, so `.expect()` cannot be used here.
    fs.read(&req(), ino, 0, 0, out, false)
        .await
        .unwrap_or_else(|_| panic!("read failed"))
}

#[tokio::test]
async fn reads_and_lookups_record_each_path_once_in_order() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello"), ("b.txt", b"world")]).await;
    let (_dir, path) = recording(&fs);

    let a = fs
        .lookup(&req(), ROOT_INO, b"a.txt")
        .await
        .expect("lookup a");
    let b = fs
        .lookup(&req(), ROOT_INO, b"b.txt")
        .await
        .expect("lookup b");
    read(&fs, a.ino, 5).await;
    read(&fs, b.ino, 5).await;
    // A second read of an already-recorded file must not add another line.
    read(&fs, a.ino, 5).await;

    let lines = wait_for_lines(&path, 3, WRITER_WAIT).await;
    assert_eq!(paths(&lines), vec!["/", "a.txt", "b.txt"]);
}

#[tokio::test]
async fn negative_lookup_records_its_parent_directory() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello")]).await;
    let (_dir, path) = recording(&fs);

    let _ = fs.lookup(&req(), ROOT_INO, b"missing.txt").await;

    let lines = wait_for_lines(&path, 1, WRITER_WAIT).await;
    assert_eq!(paths(&lines), vec!["/"]);
}

#[tokio::test]
async fn a_lookup_of_local_content_records_its_parent_directory() {
    let fixture = fuse_fixture::committed(&[("dir/a.txt", b"hello")]).await;
    let external = TempDir::new("lore-fuse-access-recorder-local-");
    let fs = lore_base::runtime::LORE_CONTEXT
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
        .expect("writable provider construction");
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup dir");
    fs.create(
        &req(),
        dir.ino,
        b"new.o",
        libc::S_IFREG | 0o644,
        0o022,
        (libc::O_CREAT | libc::O_RDWR) as u32,
    )
    .await
    .expect("create");
    let (_dir, path) = recording(&fs);

    fs.lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup dir");
    fs.lookup(&req(), dir.ino, b"new.o")
        .await
        .expect("lookup new.o");

    let lines = wait_for_lines(&path, 2, WRITER_WAIT).await;
    assert_eq!(paths(&lines), vec!["/", "dir/"]);
}

/// What the mount's writer added, a file and a directory under `write/`, is not recorded when it
/// is read or listed: the revision holds nothing a prefetch could fetch for it. A file the
/// revision holds still is.
#[tokio::test]
async fn files_and_directories_the_writer_added_are_not_recorded() {
    let fixture = fuse_fixture::committed(&[("dir/a.txt", b"hello")]).await;
    let external = TempDir::new("lore-fuse-access-recorder-local-");
    let fs = lore_base::runtime::LORE_CONTEXT
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
        .expect("writable provider construction");
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup dir");
    let (_dir, path) = recording(&fs);

    let created = fs
        .create(
            &req(),
            dir.ino,
            b"new.o",
            libc::S_IFREG | 0o644,
            0o022,
            (libc::O_CREAT | libc::O_RDWR) as u32,
        )
        .await
        .expect("create");
    fs.write(&req(), created.entry.ino, created.open.fh, 0, b"object", 0)
        .await
        .expect("write");
    assert_eq!(read(&fs, created.entry.ino, 6).await, b"object".to_vec());
    let made = fs
        .mkdir(&req(), ROOT_INO, b"Intermediate", 0o755, 0o022)
        .await
        .expect("mkdir");
    let mut buffer = vec![0u8; 4096];
    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdir(&req(), made.ino, NO_HANDLE, 0, &mut writer)
        .await
        .expect("readdir Intermediate");
    let a = fs
        .lookup(&req(), dir.ino, b"a.txt")
        .await
        .expect("lookup a.txt");
    read(&fs, a.ino, 5).await;

    // The recorder writes in order: once the last read is recorded, every earlier access is.
    let lines = wait_for_line(&path, "dir/a.txt", WRITER_WAIT).await;
    let recorded = paths(&lines);
    assert!(
        recorded.contains(&"dir/a.txt"),
        "the revision's file is recorded: {recorded:?}"
    );
    assert!(
        !recorded
            .iter()
            .any(|path| path.ends_with("new.o") || path.starts_with("Intermediate")),
        "nothing the writer added is recorded: {recorded:?}"
    );
}

/// The worker's local forms record as the fetching forms do: a LOOKUP its parent, a READDIR or
/// READDIRPLUS the directory it lists.
#[tokio::test]
async fn local_lookups_and_directory_reads_record_the_directories_they_answer_from() {
    let (_fixture, fs) = mounted(&[
        ("a.txt", b"hello"),
        ("dir/b.txt", b"bee"),
        ("plus/c.txt", b"sea"),
    ])
    .await;
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup dir");
    let plus = fs
        .lookup(&req(), ROOT_INO, b"plus")
        .await
        .expect("lookup plus");
    fs.lookup(&req(), dir.ino, b"b.txt")
        .await
        .expect("lookup b.txt");
    fs.lookup(&req(), plus.ino, b"c.txt")
        .await
        .expect("lookup c.txt");
    let (_dir, path) = recording(&fs);
    let mut buffer = vec![0u8; 4096];

    assert!(fs.lookup_local(&req(), ROOT_INO, b"a.txt").await.is_ok());
    let mut writer = DirentWriter::new(&mut buffer);
    assert!(
        fs.readdir_local(&req(), dir.ino, NO_HANDLE, 0, &mut writer)
            .await
            .is_ok()
    );
    let mut writer = DirentWriter::new(&mut buffer);
    assert!(
        fs.readdirplus_local(&req(), plus.ino, NO_HANDLE, 0, &mut writer)
            .await
            .is_ok()
    );

    let lines = wait_for_lines(&path, 3, WRITER_WAIT).await;
    assert_eq!(paths(&lines), vec!["/", "dir/", "plus/"]);
}

/// A READDIR or READDIRPLUS the dispatcher defers builds the listing and records the directory.
#[tokio::test]
async fn directory_reads_that_build_their_listing_record_the_directory() {
    let (_fixture, fs) = mounted(&[("dir/b.txt", b"bee"), ("plus/c.txt", b"sea")]).await;
    let dir = fs
        .lookup(&req(), ROOT_INO, b"dir")
        .await
        .expect("lookup dir");
    let plus = fs
        .lookup(&req(), ROOT_INO, b"plus")
        .await
        .expect("lookup plus");
    let (_dir, path) = recording(&fs);
    let mut buffer = vec![0u8; 4096];

    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdir(&req(), dir.ino, NO_HANDLE, 0, &mut writer)
        .await
        .expect("readdir dir");
    let mut writer = DirentWriter::new(&mut buffer);
    fs.readdirplus(&req(), plus.ino, NO_HANDLE, 0, &mut writer)
        .await
        .expect("readdirplus plus");

    let lines = wait_for_lines(&path, 2, WRITER_WAIT).await;
    assert_eq!(paths(&lines), vec!["dir/", "plus/"]);
}

/// A directory a listing answers from is recorded with the index of every node block holding
/// one of its children, after a tab.
#[tokio::test]
async fn a_recorded_directory_names_the_blocks_holding_its_children() {
    let (fixture, fs) = mounted(&[("a.txt", b"hello"), ("dir/b.txt", b"bee")]).await;
    let (_dir, path) = recording(&fs);

    fs.lookup(&req(), ROOT_INO, b"a.txt")
        .await
        .expect("lookup a");

    let lines = wait_for_lines(&path, 1, WRITER_WAIT).await;
    let blocks = root_child_blocks(&fixture).await;
    assert_eq!(lines, vec![format!("/\t{blocks}")]);
}

#[tokio::test]
async fn nothing_recorded_when_recording_is_off() {
    let (_fixture, fs) = mounted(&[("a.txt", b"hello"), ("dir/c.txt", b"c")]).await;

    let a = fs
        .lookup(&req(), ROOT_INO, b"a.txt")
        .await
        .expect("lookup a");
    read(&fs, a.ino, 5).await;
    let _ = fs.lookup(&req(), ROOT_INO, b"missing.txt").await;

    assert!(
        !fs.inodes()
            .get(a.ino)
            .expect("a.txt inode")
            .access_recorded
            .load(Ordering::Relaxed),
        "the off switch must not touch the file's gate"
    );
    assert!(
        !fs.inodes()
            .get(ROOT_INO)
            .expect("root inode")
            .access_recorded
            .load(Ordering::Relaxed),
        "the off switch must not touch the directory's gate"
    );
}

// ---------------------------------------------------------------------------------------------
// Zero allocations when recording is off
// ---------------------------------------------------------------------------------------------

struct CountingOff;
impl Drop for CountingOff {
    fn drop(&mut self) {
        lore_base::allocator::set_allocation_counting(false);
    }
}

fn counting() -> CountingOff {
    lore_base::allocator::set_allocation_counting(true);
    CountingOff
}

fn allocations() -> u64 {
    lore_base::allocator::thread_allocation_count()
}

#[tokio::test(flavor = "current_thread")]
async fn access_recording_off_allocates_nothing_on_read_and_lookup() {
    // Fragmented, so the warm-up READ keeps the leaf the counted READ is served from.
    let content = crate::fs::fuse::fuse_test_util::incompressible(1 << 20, 1);
    let (_fixture, fs) = mounted(&[("a.bin", &content)]).await;
    let file = fs
        .lookup(&req(), ROOT_INO, b"a.bin")
        .await
        .expect("warm-up lookup");
    // A buffer reused across READs, allocated before the counted section: a freshly allocated
    // one per call would count against the read path, not against access recording.
    let mut buffer = [0u8; 5];
    read_into(&fs, file.ino, &mut buffer).await;

    let _counting = counting();

    let before = allocations();
    fs.lookup(&req(), ROOT_INO, b"a.bin").await.expect("lookup");
    assert_eq!(allocations(), before, "LOOKUP hit");

    let before = allocations();
    read_into(&fs, file.ino, &mut buffer).await;
    assert_eq!(allocations(), before, "READ");
}
