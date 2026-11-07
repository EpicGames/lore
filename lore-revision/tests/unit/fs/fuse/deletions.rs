// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures write and tear the list on disk directly; what these test is how it reads back.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;

use lore_base::test_util::TempDir;
use lore_revision::fs::fuse::deletions::DELETION_LIST;
use lore_revision::fs::fuse::deletions::DeletionList;
use lore_revision::fs::fuse::deletions::REWRITE_SUFFIX;
use lore_revision::fs::fuse::deletions::exact_path;
use lore_revision::fs::fuse::deletions::test_util::drains_waited;
use lore_revision::fs::fuse::deletions::whole_lines;
use lore_revision::util::path::RelativePath;

fn path(text: &str) -> RelativePath {
    RelativePath::new_from_initial_path(text).expect("a valid relative path")
}

#[test]
fn append_then_read_round_trips_and_rewrite_replaces() {
    let dir = TempDir::new("lore-fuse-deletions-");
    let file = dir.path().join(DELETION_LIST);
    let list = DeletionList::open(&file).expect("the list opens where absent");
    assert_eq!(list.read().expect("read"), Vec::<RelativePath>::new());

    list.append(&path("a.txt")).expect("append");
    list.append(&path("dir/b c.txt")).expect("append");
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("dir/b c.txt")]
    );
    assert_eq!(
        DeletionList::open(&file)
            .expect("reopen")
            .read()
            .expect("read"),
        vec![path("a.txt"), path("dir/b c.txt")],
        "a reopened list reads what was appended"
    );

    let refused = list.append(&path("line\nfeed"));
    assert_eq!(
        refused.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::InvalidInput),
        "a path holding a line feed cannot be one line"
    );

    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .expect("open for a torn write")
        .write_all(b"torn")
        .expect("torn write");
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("dir/b c.txt")],
        "a final line without its line feed is not read"
    );
    let list = DeletionList::open(&file).expect("reopen over a torn line");
    list.append(&path("g.txt"))
        .expect("append after the torn line");
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("dir/b c.txt"), path("g.txt")],
        "opening drops a torn final line, so the next append starts a line"
    );

    let covered = list.length().expect("the length the rewrite covers");
    list.rewrite(&[path("c.txt"), path("d/e.txt")], covered)
        .expect("rewrite");
    assert_eq!(
        list.read().expect("read"),
        vec![path("c.txt"), path("d/e.txt")]
    );
    list.append(&path("f.txt"))
        .expect("append after the rewrite");
    assert_eq!(
        list.read().expect("read"),
        vec![path("c.txt"), path("d/e.txt"), path("f.txt")],
        "the kept descriptor appends to the rewritten list"
    );
}

/// A line that is not UTF-8 names no path: a read skips it and keeps the lines around it.
#[test]
fn a_line_that_is_not_utf8_is_skipped() {
    let dir = TempDir::new("lore-fuse-deletions-utf8-");
    let file = dir.path().join(DELETION_LIST);
    std::fs::write(&file, b"a.txt\nnot-\xff-utf8\nb.txt\n").expect("a list holding bad bytes");
    let list = DeletionList::open(&file).expect("open");
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("b.txt")]
    );
}

/// The empty root path would be an empty line, which a read skips: append refuses it and
/// writes nothing.
#[test]
fn append_refuses_the_empty_path() {
    let dir = TempDir::new("lore-fuse-deletions-empty-");
    let file = dir.path().join(DELETION_LIST);
    let list = DeletionList::open(&file).expect("open");
    assert_eq!(
        list.append(&exact("")).map_err(|error| error.kind()),
        Err(std::io::ErrorKind::InvalidInput)
    );
    assert_eq!(
        std::fs::read(&file).expect("the list"),
        b"",
        "nothing written"
    );
}

/// How many appended paths the list at `file` holds past its first `covered` bytes that
/// `read` lacks.
fn only_carried(file: &std::path::Path, covered: u64, read: &[RelativePath]) -> usize {
    let listed = std::fs::read(file).expect("read the list");
    let past = usize::try_from(covered)
        .ok()
        .and_then(|covered| listed.get(covered..))
        .unwrap_or_default();
    // An appended path is `t<thread>/p<index>`; a line cut at `covered` is not one.
    let mut unread: std::collections::BTreeSet<&str> = whole_lines(past)
        .split(|&byte| byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter(|line| line.starts_with('t'))
        .collect();
    if unread.is_empty() {
        return 0;
    }
    for path in read {
        unread.remove(path.as_str());
    }
    unread.len()
}

/// A length taken while an append is under way may end inside its line: tmpfs grows a file
/// page by page as a write copies in. Whether the read after that length saw the line whole
/// or only its start, the rewrite keeps the line whole: its path stays listed, and no tail of
/// it is listed as a path of its own.
#[test]
fn a_rewrite_whose_length_ends_inside_a_line_keeps_the_line_whole() {
    const CUT: &str = "dir/le.txt";

    let dir = TempDir::new("lore-fuse-deletions-cut-");
    let appended = vec!["a.txt".to_owned(), CUT.to_owned()];
    let mut wrong = Vec::new();
    // The line `append` writes for `CUT` is a line feed, `CUT`, then a line feed.
    for into_line in 1..=CUT.len() {
        for read_whole in [true, false] {
            let file = dir.path().join(format!("{into_line}-{read_whole}"));
            let list = DeletionList::open(&file).expect("open");
            list.append(&path("a.txt")).expect("append");
            list.append(&path(CUT)).expect("append");
            let end = list.length().expect("length");
            let covered = end - (CUT.len() + 1 - into_line) as u64;
            let read = if read_whole {
                vec![path("a.txt"), path(CUT)]
            } else {
                vec![path("a.txt")]
            };
            list.rewrite(&read, covered).expect("rewrite");
            let mut listed: Vec<String> = list
                .read()
                .expect("read")
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect();
            listed.sort_unstable();
            listed.dedup();
            if listed != appended {
                wrong.push((into_line, read_whole, listed));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "(bytes into the line, the read saw it whole, listed after the rewrite): {wrong:?}"
    );
}

/// A rewrite that cannot read the list it replaces fails and points the kept descriptor
/// back at that list, which stays on disk: the next append lands there, beside what it held.
#[test]
fn a_failed_rewrite_appends_to_the_list_on_disk_again() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("lore-fuse-deletions-restore-");
    let file = dir.path().join(DELETION_LIST);
    let list = DeletionList::open(&file).expect("open");
    list.append(&path("a.txt")).expect("append");
    let covered = list.length().expect("length");
    let mode = |mode| {
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).expect("mode");
    };
    mode(0o200);
    if std::fs::read(&file).is_ok() {
        mode(0o600);
        eprintln!("skipped: a list the process may not read reads anyway, as for root");
        return;
    }

    let failed = list.rewrite(&[path("b.txt")], covered);
    list.append(&path("c.txt"))
        .expect("append after the failed rewrite");
    mode(0o600);

    assert_eq!(
        failed.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::PermissionDenied),
        "the rewrite fails reading the list"
    );
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("c.txt")],
        "the list on disk keeps its paths and takes the next append"
    );
}

/// A rewrite that fails replacing the list, once it has pointed the kept descriptor at the new
/// one, points it back at the list on disk and carries there what the new one took past the
/// rewritten paths: here the line the first carry copied, so it is listed twice, which the
/// mount's dedup absorbs. The next append lands on the list on disk.
#[test]
fn a_rewrite_failing_its_rename_carries_the_new_lists_lines_back() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("lore-fuse-deletions-carry-back-");
    let file = dir.path().join(DELETION_LIST);
    let list = DeletionList::open(&file).expect("open");
    list.append(&path("a.txt")).expect("append");
    let covered = list.length().expect("length");
    list.append(&path("b.txt"))
        .expect("append past the covered length");
    // The new list stands already, so it is rewritten in place; the directory refuses the
    // rename.
    std::fs::write(
        dir.path().join(format!("{DELETION_LIST}{REWRITE_SUFFIX}")),
        b"",
    )
    .expect("the new list");
    let mode = |mode| {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).expect("mode");
    };
    mode(0o500);
    let probe = dir.path().join("probe");
    if std::fs::write(&probe, b"").is_ok() {
        mode(0o700);
        eprintln!("skipped: a directory the process may not write takes a file, as for root");
        return;
    }

    let failed = list.rewrite(&[path("x.txt")], covered);
    list.append(&path("c.txt"))
        .expect("append after the failed rewrite");
    mode(0o700);

    assert_eq!(
        failed.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::PermissionDenied),
        "the rewrite fails renaming the new list"
    );
    assert_eq!(
        list.read().expect("read"),
        vec![path("a.txt"), path("b.txt"), path("b.txt"), path("c.txt")],
        "the list on disk takes back the new one's line and the next append"
    );
}

/// Appenders racing rewrites lose nothing and list nothing they did not append. Each appender
/// appends `APPENDS` new paths of its own unpaced, then goes on paced, one append in
/// `NEW_EVERY` a new path and the rest repeating its first ones, until the rewrites have seen
/// every window: an append past a rewrite's read, one during the rewrite, and one under way on
/// the list a rewrite replaces when its drain begins. A new path past a read is one the read
/// missed, so that window stays open however late the rewrites start, where with repeats alone
/// only the first pass could show it; the repeats keep the list, which every rewrite reads
/// whole, from growing with each append.
#[test]
fn an_append_racing_a_rewrite_is_kept() {
    use std::collections::BTreeSet;
    use std::os::fd::AsRawFd as _;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    const THREADS: usize = 4;
    const APPENDS: usize = 5000;
    const NEW_EVERY: usize = 8;
    const MIN_REWRITES: usize = 50;
    // A bound on a hang, not a timing: the windows are seen long before on an idle machine.
    const DEADLINE: Duration = Duration::from_secs(30);

    let dir = TempDir::new("lore-fuse-deletions-race-");
    let file = dir.path().join(DELETION_LIST);
    let list = Arc::new(DeletionList::open(&file).expect("open"));
    let stop = Arc::new(AtomicBool::new(false));
    let appenders: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (list, stop) = (list.clone(), stop.clone());
            std::thread::spawn(move || {
                // Appends made, and new paths among them: `t<thread>/p<index>` below `new`.
                let (mut index, mut new) = (0, 0);
                while index < APPENDS || !stop.load(Ordering::SeqCst) {
                    let appended = if index < APPENDS || index % NEW_EVERY == 0 {
                        new += 1;
                        new - 1
                    } else {
                        index % APPENDS
                    };
                    list.append(&path(&format!("t{thread}/p{appended}")))
                        .expect("append");
                    index += 1;
                    if index >= APPENDS {
                        // Paced, so the rewrites get the CPU even on one core.
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
                new
            })
        })
        .collect();
    let started = Instant::now();
    let deadline = started + DEADLINE;
    let mut rewrites = 0;
    // Appends past a rewrite's covered length that its read missed: only its carry keeps them.
    let mut carried = 0;
    // Rewrites an append was counted during.
    let mut overlapped = 0;
    // Counted per thread: this test's rewrites run on this thread.
    let waited = drains_waited();
    let drained = || drains_waited() - waited;
    while (rewrites < MIN_REWRITES || carried == 0 || overlapped == 0 || drained() == 0)
        && Instant::now() < deadline
    {
        let covered = list.length().expect("the length the rewrite covers");
        let mut paths = list.read().expect("read");
        paths.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        paths.dedup_by(|a, b| a.as_str() == b.as_str());
        // Appenders land lines past the read before the rewrite carries them.
        std::thread::sleep(Duration::from_millis(1));
        carried += only_carried(&file, covered, &paths);
        let before = list.changes();
        list.rewrite(&paths, covered).expect("rewrite");
        // The rewrite moves the count once itself.
        if list.changes() - before > 1 {
            overlapped += 1;
        }
        rewrites += 1;
    }
    stop.store(true, Ordering::SeqCst);
    // Paths each appender appended: `t<thread>/p<index>` for every index below its count.
    let appended: Vec<usize> = appenders
        .into_iter()
        .map(|thread| thread.join().expect("an appender"))
        .collect();
    eprintln!(
        "{rewrites} rewrites in {:?}: {carried} appends carried, {overlapped} rewrites \
         overlapped, {} drains waited",
        started.elapsed(),
        drained()
    );
    assert!(
        carried > 0,
        "no append landed past a covered length after the read, in {rewrites} rewrites \
         ({overlapped} overlapped by an append)"
    );
    assert!(
        overlapped > 0,
        "no append was counted during a rewrite, in {rewrites} rewrites ({carried} carried)"
    );
    assert!(
        drained() > 0,
        "no rewrite's drain found an append under way, in {rewrites} rewrites"
    );

    let listed: BTreeSet<String> = list
        .read()
        .expect("read")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    let missing: Vec<String> = appended
        .iter()
        .enumerate()
        .flat_map(|(thread, &count)| (0..count).map(move |index| format!("t{thread}/p{index}")))
        .filter(|path| !listed.contains(path))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} appends lost across {rewrites} rewrites, first {:?}",
        missing.len(),
        appended.iter().sum::<usize>(),
        missing.first()
    );
    let extra: Vec<&String> = listed
        .iter()
        .filter(|listed| {
            !listed
                .strip_prefix('t')
                .and_then(|rest| rest.split_once("/p"))
                .is_some_and(|(thread, index)| {
                    let count = thread.parse::<usize>().ok().and_then(|t| appended.get(t));
                    count.is_some_and(|&count| index.parse::<usize>().is_ok_and(|i| i < count))
                })
        })
        .collect();
    assert!(
        extra.is_empty(),
        "{} listed paths were never appended, first {:?}",
        extra.len(),
        extra.first()
    );
    // SAFETY: `F_GETFD` reads the flags of a descriptor the list keeps open.
    let flags = unsafe { libc::fcntl(list.file.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "the kept descriptor stays close-on-exec"
    );
}

/// A rewrite finishes while appends never pause: its drain waits only for the appends that
/// began on the list it replaces, not for a moment with none under way.
#[test]
fn a_rewrite_finishes_while_appends_never_pause() {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    const THREADS: usize = 8;
    const DISTINCT: usize = 5000;
    // A bound on a hang, not a timing: the rewrite finishes in milliseconds on an idle machine,
    // and takes longer only on one this test shares with others, where it must not fail.
    const DEADLINE: Duration = Duration::from_secs(30);

    let dir = TempDir::new("lore-fuse-deletions-steady-");
    let list = Arc::new(DeletionList::open(&dir.path().join(DELETION_LIST)).expect("open"));
    let stop = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicUsize::new(0));
    let appenders: Vec<_> = (0..THREADS)
        .map(|thread| {
            let (list, stop, started) = (list.clone(), stop.clone(), started.clone());
            let distinct: Vec<RelativePath> = (0..DISTINCT)
                .map(|index| path(&format!("t{thread}/p{index}")))
                .collect();
            let filler = path(&format!("t{thread}/filler"));
            std::thread::spawn(move || {
                started.fetch_add(1, Ordering::SeqCst);
                for appended in &distinct {
                    list.append(appended).expect("append");
                }
                while !stop.load(Ordering::Relaxed) {
                    list.append(&filler).expect("append");
                }
            })
        })
        .collect();
    while started.load(Ordering::SeqCst) < THREADS {
        std::thread::yield_now();
    }

    let (finished, took) = std::sync::mpsc::channel();
    let rewriter = {
        let list = list.clone();
        std::thread::spawn(move || {
            let begun = Instant::now();
            let covered = list.length().expect("the length the rewrite covers");
            let paths = list.read().expect("read");
            list.rewrite(&paths, covered).expect("rewrite");
            let _ = finished.send(begun.elapsed());
        })
    };
    let elapsed = took.recv_timeout(DEADLINE);
    stop.store(true, Ordering::Relaxed);
    for thread in appenders {
        thread.join().expect("an appender");
    }
    rewriter.join().expect("the rewrite");
    let elapsed = elapsed.unwrap_or_else(|_| {
        panic!("a rewrite under appends that never pause finishes within {DEADLINE:?}")
    });
    eprintln!("a rewrite under {THREADS} appenders that never pause: {elapsed:?}");

    let listed: BTreeSet<String> = list
        .read()
        .expect("read")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    let missing = (0..THREADS)
        .flat_map(|thread| (0..DISTINCT).map(move |index| format!("t{thread}/p{index}")))
        .filter(|appended| !listed.contains(appended))
        .count();
    assert_eq!(missing, 0, "no append is lost to the rewrite");
}

#[test]
fn the_change_count_moves_once_an_append_or_a_rewrite_is_written() {
    let dir = TempDir::new("lore-fuse-deletions-changes-");
    let list = DeletionList::open(&dir.path().join(DELETION_LIST)).expect("open");
    let opened = list.changes();
    list.append(&path("a.txt")).expect("append");
    let appended = list.changes();
    assert_ne!(appended, opened, "an append moves the count");
    list.read().expect("read");
    assert_eq!(list.changes(), appended, "a read leaves it");
    let covered = list.length().expect("length");
    list.rewrite(&[path("b.txt")], covered).expect("rewrite");
    assert_ne!(list.changes(), appended, "a rewrite moves the count");
}

fn exact(text: &str) -> RelativePath {
    exact_path(text)
}

#[test]
fn every_path_append_takes_reads_back_unchanged() {
    const TRICKY: [&str; 6] = ["a:b.txt", "x\\y", "/lead", "a//b", "./a", "dir/b c.txt"];

    let dir = TempDir::new("lore-fuse-deletions-exact-");
    let list = DeletionList::open(&dir.path().join(DELETION_LIST)).expect("open");
    for text in TRICKY {
        list.append(&exact(text)).expect("append");
    }
    let read: Vec<String> = list
        .read()
        .expect("read")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(read, TRICKY, "what append writes reads back unchanged");

    let written = dir.path().join("written");
    let lines: String = TRICKY.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&written, lines).expect("a list written as lines");
    let read: Vec<String> = DeletionList::open(&written)
        .expect("open")
        .read()
        .expect("read")
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(read, TRICKY, "each line reads back as the path it holds");

    let list = DeletionList::open(&written).expect("open");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&written)
        .expect("open for a short write")
        .write_all(b"torn")
        .expect("a short write");
    list.append(&exact("next"))
        .expect("append after a torn write");
    assert!(
        list.read()
            .expect("read")
            .iter()
            .any(|path| path.as_str() == "next"),
        "an append after a torn line is a line of its own"
    );
}
