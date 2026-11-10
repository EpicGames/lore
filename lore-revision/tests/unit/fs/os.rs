// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Fixtures build filesystem state directly; what these test is how the primitives read it.
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::FragmentFlags;
use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::filesystem_provider::FilesystemProvider;
use lore_revision::fs::filesystem_provider::InstanceOperation;
use lore_revision::fs::filesystem_provider::InstanceOperationImpl;
use lore_revision::fs::os::*;
use lore_revision::node::Node;
use lore_revision::node::NodeFileMode;
use lore_revision::repository::RepositoryContext;
use lore_revision::util::path::RelativePath;

use crate::fs::filesystem_provider::setup_test_execution;
use crate::helper::case_insensitive;
use crate::repository::test_helpers::file_node;
use crate::repository::test_helpers::store_fragment;
use crate::repository::test_helpers::stored_as_list;
use crate::repository::test_helpers::working_tree_repository;

fn temp_dir() -> lore_base::test_util::TempDir {
    lore_base::test_util::TempDir::new("lore-fs-os-test-")
}

/// An operation rooted at `root`, which is what a caller names its paths against.
async fn os_operation(root: &Path) -> Arc<InstanceOperationImpl> {
    FilesystemProvider::begin_operation(&OsFilesystem::new(root))
        .await
        .expect("beginning an operation over the OS filesystem")
}

fn relative(path: &str) -> RelativePath {
    RelativePath::new_from_initial_path(path).expect("relative path")
}

/// An empty directory of its own on a filesystem other than the one holding `beside`, or
/// `None` where the machine offers no second one. `/dev/shm` is the tmpfs a Linux system
/// mounts apart from the one temporary directories come from.
///
/// Named per call, the tests sharing a process and running at the same time, and per process,
/// a previous run having left one behind where it failed. The caller removes it: it lies
/// outside the temporary directory that would have.
#[cfg(target_os = "linux")]
fn second_filesystem_directory(beside: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    static TAKEN: AtomicUsize = AtomicUsize::new(0);

    let shm = Path::new("/dev/shm");
    if std::fs::metadata(shm).ok()?.dev() == std::fs::metadata(beside).ok()?.dev() {
        return None;
    }
    let directory = shm.join(format!(
        "lore-fs-os-test-{}-{}",
        std::process::id(),
        TAKEN.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir(&directory).ok()?;
    Some(directory)
}

/// What the lookup answers where the platform has one, and `None` where it
/// has not — macOS can say nothing about a case variation short of the
/// directory read this exists to avoid, so every expectation collapses to that
/// there.
fn verdict(held: bool) -> Option<bool> {
    cfg!(any(target_os = "linux", target_family = "windows")).then_some(held)
}

#[tokio::test]
async fn list_path_yields_a_directory_listing() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("child"), b"data").expect("write child");

    let PathListingResult::Directory { mut listing } =
        list_path(dir.path().to_path_buf()).await.expect("listing")
    else {
        panic!("a directory must list");
    };
    let mut names = Vec::new();
    while let Some(entry) = listing.next().await {
        if let Some(item) = file_list_item(entry).expect("entry name") {
            names.push(item.name);
        }
    }
    assert_eq!(names, vec!["child".to_string()]);
}

/// A link is not a kind the repository tracks, and the listing describes what a name holds
/// rather than what it points at, so one naming a file is skipped as the link it is.
#[cfg(target_family = "unix")]
#[tokio::test]
async fn a_listing_skips_a_link_to_a_file_it_would_otherwise_track() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("target"), b"data").expect("write target");
    std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("link"))
        .expect("link the target");

    let PathListingResult::Directory { mut listing } =
        list_path(dir.path().to_path_buf()).await.expect("listing")
    else {
        panic!("a directory must list");
    };
    let mut names = Vec::new();
    while let Some(entry) = listing.next().await {
        if let Some(item) = file_list_item(entry).expect("entry name") {
            names.push(item.name);
        }
    }
    assert_eq!(names, vec!["target".to_string()]);
}

/// A name a listing can yield that has no text spelling: bytes that are not UTF-8 on unix, an
/// unpaired surrogate on Windows.
#[cfg(target_family = "unix")]
fn name_that_is_not_text() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt as _;

    std::ffi::OsString::from_vec(vec![0xff])
}

#[cfg(target_family = "windows")]
fn name_that_is_not_text() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt as _;

    std::ffi::OsString::from_wide(&[0xd800])
}

/// A name that is not text is reported, and named lossily in the report, rather than passed
/// over: it hashes to a node the tree does not hold, so nothing can be done with it that is
/// not a guess.
///
/// The entry is assembled rather than read off a disk so this runs wherever the crate builds.
/// A filesystem enforcing UTF-8 -- ZFS with `utf8only=on`, APFS -- refuses to create such a
/// name at all, which leaves
/// [`crate::fs::filesystem_provider::a_name_that_is_not_text_is_reported`], the
/// end-to-end cover over a real listing, with nothing to list there.
#[test]
fn a_listed_name_that_is_not_text_is_reported() {
    let dir = temp_dir();
    let path = dir.path().join("named");
    std::fs::write(&path, b"data").expect("write file");
    // A real file's metadata: an entry carrying none, or naming neither a file nor a
    // directory, is passed over before its name is read.
    let metadata = std::fs::metadata(&path).expect("read metadata");

    let listed = file_list_item(Ok(lore_io::DirEntry {
        file_name: name_that_is_not_text(),
        metadata: Some(lore_io::Stat::from(&metadata)),
        is_symlink: false,
    }));

    let Err(error) = listed else {
        panic!("a name with no text spelling must be reported, not listed");
    };
    assert!(
        error.to_string().contains('\u{fffd}'),
        "the report names the entry lossily, not by a spelling it does not have: {error}"
    );
}

#[tokio::test]
async fn list_path_describes_a_file_by_its_own_name() {
    let dir = temp_dir();
    let path = dir.path().join("lonely.txt");
    std::fs::write(&path, b"data").expect("write file");

    let PathListingResult::File { item } = list_path(path).await.expect("listing") else {
        panic!("a file must be described, not listed");
    };
    assert_eq!(item.name, "lonely.txt");
    assert_eq!(item.metadata.len, 4);
}

#[tokio::test]
async fn list_path_reports_a_missing_path() {
    let dir = temp_dir();
    assert!(
        list_path(dir.path().join("absent"))
            .await
            .expect("listing")
            .is_not_found(),
        "a missing path is neither a file nor a directory"
    );
}

/// The distinction the whole thing rests on: this answers for the case
/// variation asked about, where `Path::exists` answers for the file whatever
/// it is called. A case-insensitive filesystem finds the file under either name,
/// and must still say no to the one it does not hold.
#[tokio::test]
async fn a_name_is_held_only_in_the_case_variation_on_disk() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    std::fs::write(dir.path().join("Test.file"), b"").expect("write file");

    assert_eq!(
        operation.holds_name_exactly(&relative("Test.file")).await,
        verdict(true)
    );
    assert_eq!(
        operation.holds_name_exactly(&relative("test.FILE")).await,
        verdict(false),
        "a case variation the filesystem does not hold is not a match, whether or not it would find the file"
    );
    assert_eq!(
        operation.holds_name_exactly(&relative("other.file")).await,
        verdict(false)
    );
    assert_eq!(
        operation.holds_name_exactly(&relative("Test.file.")).await,
        Some(false),
        "no node may be named with a trailing dot, so every platform holds it in no spelling"
    );
    assert_eq!(
        operation
            .holds_name_exactly(&relative("absent/Test.file"))
            .await,
        verdict(false),
        "a missing directory holds nothing"
    );
}

/// A directory is what most components resolve to, and the lookup has to
/// answer for one as readily as for a file.
#[tokio::test]
async fn a_directory_name_is_held() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;
    std::fs::create_dir(dir.path().join("Assets")).expect("create dir");

    assert_eq!(
        operation.holds_name_exactly(&relative("Assets")).await,
        verdict(true)
    );
}

#[tokio::test]
async fn names_answers_with_the_case_variation_it_was_given() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
    let operation = os_operation(dir.path()).await;

    assert_eq!(
        operation
            .names_folding_to(&relative(""), "Test.file")
            .await
            .expect("a name the filesystem holds must resolve"),
        vec!["Test.file".to_string()]
    );
}

/// The reason the member exists: a caller holding a name in one case needs the one the
/// filesystem kept. The directory is read and the names folded rather than looked up, so the
/// case variation on disk is reported whether or not the filesystem would itself have found
/// the file under the one asked about.
#[tokio::test]
async fn names_answers_with_the_stored_case_variation() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
    let operation = os_operation(dir.path()).await;

    assert_eq!(
        operation
            .names_folding_to(&relative(""), "test.FILE")
            .await
            .expect("a case variation must resolve"),
        vec!["Test.file".to_string()]
    );
}

#[tokio::test]
async fn names_reports_a_name_that_is_not_there_in_any_case() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
    let operation = os_operation(dir.path()).await;

    assert!(
        operation
            .names_folding_to(&relative(""), "other.file")
            .await
            .expect("reading the directory must succeed")
            .is_empty(),
        "a name no case variation of which is there must not resolve"
    );
}

/// Win32 trims trailing dots and spaces from a path before it looks it up, so asking about a
/// name with one can be answered about the neighbouring name. That is a different file, and
/// must not be reported as a case variation of the name asked about.
#[tokio::test]
async fn names_does_not_answer_with_a_neighbouring_name() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("Test.file"), b"").expect("write file");
    let operation = os_operation(dir.path()).await;

    assert!(
        operation
            .names_folding_to(&relative(""), "Test.file.")
            .await
            .expect("reading the directory must succeed")
            .is_empty(),
        "a trailing dot names a file that is not there"
    );
}

/// A link is not a spelling the repository holds, so it is not reported as one even where its
/// name is the only thing that folds to the one asked about.
#[tokio::test]
#[cfg(target_family = "unix")]
async fn names_leaves_out_a_link() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("target"), b"").expect("write target");
    std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("Test.file"))
        .expect("create link");
    let operation = os_operation(dir.path()).await;

    assert!(
        operation
            .names_folding_to(&relative(""), "test.file")
            .await
            .expect("reading the directory must succeed")
            .is_empty(),
        "a link must not be reported as a case variation"
    );
}

/// Every variation comes back, the exact one among them: resolving the ambiguity is the
/// caller's to do, and one that has to tell a collision from a resolution needs to see both.
/// Only a case-sensitive filesystem can hold the two files this needs.
#[tokio::test]
async fn names_reports_every_case_variation_that_coexists() {
    let dir = temp_dir();
    if case_insensitive(dir.path()) {
        return;
    }
    std::fs::write(dir.path().join("Test.file"), b"").expect("write Test.file");
    std::fs::write(dir.path().join("test.file"), b"").expect("write test.file");
    let operation = os_operation(dir.path()).await;

    for asked in ["TEST.FILE", "test.file"] {
        let mut found = operation
            .names_folding_to(&relative(""), asked)
            .await
            .expect("the variations must resolve");
        found.sort();
        assert_eq!(
            found,
            vec!["Test.file".to_string(), "test.file".to_string()],
            "asking about {asked} must report every case variation, not pick one"
        );
    }
}

/// A file lands on the name even where the name is taken, replacing what was there.
#[tokio::test]
async fn rename_replaces_the_file_at_the_destination() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("from"), b"carried").expect("write source");
    std::fs::write(dir.path().join("to"), b"replaced").expect("write destination");
    let operation = os_operation(dir.path()).await;

    operation
        .rename(&relative("from"), &relative("to"))
        .await
        .expect("the move must land");

    assert_eq!(
        b"carried".to_vec(),
        std::fs::read(dir.path().join("to")).expect("read destination")
    );
    assert!(!dir.path().join("from").exists(), "the source must be gone");
}

/// Text is diffable and an opaque format is not, read through the content the operation names
/// at the path.
#[tokio::test]
async fn infer_is_diffable_reads_the_content_at_the_path() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("text"), b"one\ntwo\n").expect("write text");
    std::fs::write(dir.path().join("opaque"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR")
        .expect("write opaque");
    let operation = os_operation(dir.path()).await;

    assert!(
        operation
            .infer_is_diffable(&relative("text"))
            .await
            .expect("reading the content must succeed")
    );
    assert!(
        !operation
            .infer_is_diffable(&relative("opaque"))
            .await
            .expect("reading the content must succeed")
    );
}

/// Content that is not there is not diffable, there being nothing to diff.
#[tokio::test]
async fn infer_is_diffable_reports_content_that_is_not_there() {
    let dir = temp_dir();
    let operation = os_operation(dir.path()).await;

    assert!(
        !operation
            .infer_is_diffable(&relative("absent"))
            .await
            .expect("an unreadable path is an answer, not a failure")
    );
}

/// Conflict markers are read from the content the operation names at the path, and a path
/// holding no file holds none.
#[tokio::test]
async fn infer_is_conflicted_reads_the_content_at_the_path() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            std::fs::write(
                dir.path().join("marked"),
                b"clean line\n<<<<<<< ours\nmore\n",
            )
            .expect("write marked");
            std::fs::write(dir.path().join("clean"), b"one\ntwo\n").expect("write clean");
            let operation = os_operation(dir.path()).await;

            assert!(
                operation
                    .infer_is_conflicted(&relative("marked"))
                    .await
                    .expect("reading the content must succeed")
            );
            assert!(
                !operation
                    .infer_is_conflicted(&relative("clean"))
                    .await
                    .expect("reading the content must succeed")
            );
            assert!(
                !operation
                    .infer_is_conflicted(&relative("absent"))
                    .await
                    .expect("a path holding no file is an answer, not a failure")
            );
        })
        .await;
}

/// A file is not moved onto a directory: the rename is refused as unsupported and both keep what
/// they hold.
#[tokio::test]
async fn rename_refuses_a_file_onto_a_directory() {
    let dir = temp_dir();
    let file = dir.path().join("file");
    let directory = dir.path().join("directory");
    std::fs::write(&file, b"kept").expect("write the file");
    std::fs::create_dir(&directory).expect("create the directory");
    std::fs::write(directory.join("entry"), b"kept").expect("write the entry");

    let refused = rename_unifying(&file, &directory)
        .await
        .expect_err("a file is not moved onto a directory");

    assert_eq!(std::io::ErrorKind::Unsupported, refused.kind());
    assert_eq!(Some(b"kept".to_vec()), std::fs::read(&file).ok());
    assert_eq!(
        Some(b"kept".to_vec()),
        std::fs::read(directory.join("entry")).ok()
    );
}

/// A directory is not moved onto a file on Unix, whose rename refuses it: the rename is refused as
/// unsupported and both keep what they hold.
#[tokio::test]
#[cfg(unix)]
async fn rename_refuses_a_directory_onto_a_file() {
    let dir = temp_dir();
    let file = dir.path().join("file");
    let directory = dir.path().join("directory");
    std::fs::write(&file, b"kept").expect("write the file");
    std::fs::create_dir(&directory).expect("create the directory");
    std::fs::write(directory.join("entry"), b"kept").expect("write the entry");

    let refused = rename_unifying(&directory, &file)
        .await
        .expect_err("a directory is not moved onto a file");

    assert_eq!(std::io::ErrorKind::Unsupported, refused.kind());
    assert_eq!(Some(b"kept".to_vec()), std::fs::read(&file).ok());
    assert_eq!(
        Some(b"kept".to_vec()),
        std::fs::read(directory.join("entry")).ok()
    );
}

/// A move between two filesystems is no rename, so the content is copied and the source
/// unlinked, whether or not the destination is already taken. The source is reached through a
/// link, the two paths an operation names lying under one root.
#[tokio::test]
#[cfg(target_os = "linux")]
async fn rename_carries_a_file_across_filesystems() {
    for occupied in [false, true] {
        let dir = temp_dir();
        let Some(elsewhere) = second_filesystem_directory(dir.path()) else {
            return;
        };
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("elsewhere")).expect("link");
        std::fs::write(elsewhere.join("from"), b"carried").expect("write source");
        if occupied {
            std::fs::write(dir.path().join("to"), b"replaced").expect("write destination");
        }
        let operation = os_operation(dir.path()).await;

        let moved = operation
            .rename(&relative("elsewhere/from"), &relative("to"))
            .await;

        let landed = std::fs::read(dir.path().join("to")).ok();
        let source_left = elsewhere.join("from").exists();
        let _ = std::fs::remove_dir_all(&elsewhere);

        moved.expect("the move must land");
        assert_eq!(
            Some(b"carried".to_vec()),
            landed,
            "the destination must hold the source, occupied {occupied}"
        );
        assert!(!source_left, "the source must be gone, occupied {occupied}");
    }
}

/// A directory crosses a filesystem the same way, its children carried one at a time into a
/// destination created to take them.
#[tokio::test]
#[cfg(target_os = "linux")]
async fn rename_carries_a_directory_across_filesystems() {
    let dir = temp_dir();
    let Some(elsewhere) = second_filesystem_directory(dir.path()) else {
        return;
    };
    std::os::unix::fs::symlink(&elsewhere, dir.path().join("elsewhere")).expect("link");
    std::fs::create_dir_all(elsewhere.join("from").join("nested")).expect("create source");
    std::fs::write(
        elsewhere.join("from").join("nested").join("leaf"),
        b"carried",
    )
    .expect("write nested child");
    let operation = os_operation(dir.path()).await;

    let moved = operation
        .rename(&relative("elsewhere/from"), &relative("to"))
        .await;

    let landed = std::fs::read(dir.path().join("to").join("nested").join("leaf")).ok();
    let source_left = elsewhere.join("from").exists();
    let _ = std::fs::remove_dir_all(&elsewhere);

    moved.expect("the move must land");
    assert_eq!(Some(b"carried".to_vec()), landed);
    assert!(!source_left, "the source must be gone");
}

/// A directory whose name is taken hands its children over one at a time, leaving what the
/// destination already held beside them, and a child whose name is taken too is merged under
/// the same rules rather than refused.
#[tokio::test]
async fn rename_merges_a_directory_into_the_one_at_the_destination() {
    let dir = temp_dir();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    std::fs::create_dir_all(from.join("shared")).expect("create source");
    std::fs::create_dir_all(to.join("shared")).expect("create destination");
    std::fs::write(from.join("carried"), b"").expect("write child");
    std::fs::write(to.join("held"), b"").expect("write held child");
    std::fs::write(from.join("shared").join("nested"), b"").expect("write nested child");
    std::fs::write(to.join("shared").join("kept"), b"").expect("write nested held child");
    let operation = os_operation(dir.path()).await;

    operation
        .rename(&relative("from"), &relative("to"))
        .await
        .expect("the merge must land");

    assert!(to.join("carried").exists());
    assert!(to.join("held").exists());
    assert!(to.join("shared").join("nested").exists());
    assert!(to.join("shared").join("kept").exists());
    assert!(!from.exists(), "the source must be gone");
}

/// A file and a directory are not two spellings of one thing, so there is no move that leaves
/// one of them.
#[tokio::test]
async fn rename_refuses_a_destination_of_another_kind() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("from"), b"").expect("write source");
    std::fs::create_dir(dir.path().join("to")).expect("create destination");
    let operation = os_operation(dir.path()).await;

    operation
        .rename(&relative("from"), &relative("to"))
        .await
        .expect_err("a file must not land on a directory");

    assert!(dir.path().join("from").is_file(), "the source must survive");
    assert!(
        dir.path().join("to").is_dir(),
        "the destination must survive"
    );
}

/// A second path to `name` in `dir` naming the same entry: the name in another case where the file
/// system folds case, and the name through `.` where it does not.
fn same_entry(dir: &Path, name: &str) -> PathBuf {
    if case_insensitive(dir) {
        dir.join(name.to_uppercase())
    } else {
        dir.join(".").join(name)
    }
}

/// Two paths name one entry only where they lead to the same file, symbolic links followed: a
/// link to it does, another spelling does where the file system folds case, and another file and a
/// path naming nothing never do. A first path naming nothing, one through a file included, is
/// answered as naming nothing.
#[tokio::test]
async fn names_one_entry_only_for_paths_to_the_same_file() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("file"), b"").expect("write the file");
    std::fs::write(dir.path().join("other"), b"").expect("write the other file");
    let folds_case = case_insensitive(dir.path());
    #[cfg(target_family = "unix")]
    let linked = std::os::unix::fs::symlink("file", dir.path().join("link")).is_ok();
    #[cfg(target_family = "windows")]
    let linked = std::os::windows::fs::symlink_file("file", dir.path().join("link")).is_ok();
    let operation = os_operation(dir.path()).await;

    let mut cases = vec![
        ("file", "file", Some(true)),
        ("file", "FILE", Some(folds_case)),
        ("file", "other", Some(false)),
        ("file", "missing", Some(false)),
        ("file", "other/child", Some(false)),
        ("missing", "file", None),
        ("other/child", "file", None),
    ];
    if linked {
        cases.push(("file", "link", Some(true)));
    }
    for (from, to, one) in cases {
        assert_eq!(
            operation
                .names_one_entry(&relative(from), &relative(to))
                .await
                .expect("an answer"),
            one,
            "{from} and {to}"
        );
    }
}

/// A rename holds no unification, which it boxes only where the rename is refused: its future is
/// smaller than the unification's, which would otherwise make every rename as large.
#[test]
fn a_rename_holds_no_unification() {
    let (from, to) = (Path::new("from"), Path::new("to"));
    let rename = rename_unifying(from, to);
    let unification = unify_refused_rename(from, to, std::io::ErrorKind::Other.into());
    assert!(
        size_of_val(&rename) < size_of_val(&unification),
        "the rename holds {} bytes, the unification {}",
        size_of_val(&rename),
        size_of_val(&unification)
    );
}

/// A refused rename of a file to a path naming its own entry, as a case-only rename does on a file
/// system that folds case, removes nothing: the refusal is answered and the file keeps what it
/// holds.
#[tokio::test]
async fn a_refused_rename_of_a_file_to_its_own_entry_keeps_it() {
    let dir = temp_dir();
    let file = dir.path().join("file");
    std::fs::write(&file, b"kept").expect("write the file");

    let refused = unify_refused_rename(
        &file,
        &same_entry(dir.path(), "file"),
        std::io::ErrorKind::PermissionDenied.into(),
    )
    .await
    .expect_err("an entry is not unified with itself");

    assert_eq!(std::io::ErrorKind::PermissionDenied, refused.kind());
    assert_eq!(Some(b"kept".to_vec()), std::fs::read(&file).ok());
}

/// A refused rename of a directory to a path naming its own entry does not enter it as its own
/// destination: the refusal is answered and the directory keeps its children.
#[tokio::test]
async fn a_refused_rename_of_a_directory_to_its_own_entry_keeps_it() {
    let dir = temp_dir();
    let directory = dir.path().join("directory");
    std::fs::create_dir(&directory).expect("create the directory");
    std::fs::write(directory.join("entry"), b"kept").expect("write the entry");

    let refused = unify_refused_rename(
        &directory,
        &same_entry(dir.path(), "directory"),
        std::io::ErrorKind::PermissionDenied.into(),
    )
    .await
    .expect_err("an entry is not unified with itself");

    assert_eq!(std::io::ErrorKind::PermissionDenied, refused.kind());
    assert_eq!(
        Some(b"kept".to_vec()),
        std::fs::read(directory.join("entry")).ok()
    );
}

/// The content each fragment of a list [`stored_node`] stores holds, in bytes.
const LEAF_LEN: usize = 16 * 1024;

/// A file node holding `content`, stored as one fragment or, where `listed` asks for it, as a
/// list of fragments, and carrying the executable bit where `executable` asks for it.
///
/// The two shapes reach the file differently: one fragment is written whole and its write
/// captures the file's metadata, and a list streams into the file and captures none.
async fn stored_node(
    repository: &Arc<RepositoryContext>,
    content: &[u8],
    listed: bool,
    executable: bool,
) -> Node {
    let mut node = if listed {
        stored_as_list(repository, content, LEAF_LEN).await
    } else {
        let mut node = file_node(content);
        node.address = store_fragment(
            repository,
            content,
            FragmentFlags::empty(),
            content.len() as u64,
        )
        .await;
        node
    };
    if executable {
        node.mode = NodeFileMode::Executable.bits();
    }
    node
}

/// A written node answers what a stat of the file it left reads, whichever shape the content
/// is stored in, and on Unix the file carries the executable bit the node does.
#[tokio::test]
async fn a_written_node_answers_what_the_file_holds() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            let repository = working_tree_repository(dir.path()).await;
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation over the working tree");
            let content: Vec<u8> = (0..40 * 1024).map(|index| (index % 251) as u8).collect();

            for (name, listed, executable) in [
                ("whole", false, false),
                ("whole.sh", false, true),
                ("listed", true, false),
                ("listed.sh", true, true),
            ] {
                let node = stored_node(&repository, &content, listed, executable).await;
                let info = operation
                    .write_node(repository.clone(), &node, &relative(name))
                    .await
                    .expect("writing the node");

                let path = dir.path().join(name);
                assert_eq!(
                    content,
                    std::fs::read(&path).expect("read the file"),
                    "{name} holds the node's content"
                );
                assert_eq!(
                    FileInfo::from_metadata(&std::fs::metadata(&path).expect("stat the file")),
                    info,
                    "{name} answers what a stat reads"
                );
                if cfg!(unix) {
                    assert_eq!(
                        Some(executable),
                        info.executable(),
                        "{name} carries the node's executable bit"
                    );
                }
            }
        })
        .await;
}

/// A node written below directories that are not there creates them, one level or several
/// deep, whichever shape the content is stored in, and a node written into a directory that is
/// there lands beside what it holds.
#[tokio::test]
async fn a_node_written_below_missing_directories_creates_them() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            let repository = working_tree_repository(dir.path()).await;
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation over the working tree");
            let content: Vec<u8> = (0..40 * 1024).map(|index| (index % 251) as u8).collect();

            for (name, listed) in [
                ("whole/file", false),
                ("listed/file", true),
                ("whole/deeper/below/file", false),
                ("listed/deeper/below/file", true),
                ("whole/beside", false),
                ("listed/beside", true),
            ] {
                let node = stored_node(&repository, &content, listed, false).await;
                let info = operation
                    .write_node(repository.clone(), &node, &relative(name))
                    .await
                    .unwrap_or_else(|err| panic!("writing {name}: {err}"));

                let path = dir.path().join(name);
                assert_eq!(
                    content,
                    std::fs::read(&path).expect("read the file"),
                    "{name} holds the node's content"
                );
                assert_eq!(
                    FileInfo::from_metadata(&std::fs::metadata(&path).expect("stat the file")),
                    info,
                    "{name} answers what a stat reads"
                );
            }
            for directory in ["whole", "listed"] {
                let mut names: Vec<_> = std::fs::read_dir(dir.path().join(directory))
                    .expect("list the directory")
                    .map(|entry| entry.expect("an entry").file_name())
                    .collect();
                names.sort();
                assert_eq!(
                    names,
                    ["beside", "deeper", "file"],
                    "{directory} holds what was written and nothing else"
                );
            }
        })
        .await;
}

/// A node written where a file stands in for the directory above it fails at once, and leaves
/// that file as it was.
#[tokio::test]
async fn a_node_written_below_a_file_fails() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            let repository = working_tree_repository(dir.path()).await;
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation over the working tree");
            let blocker = dir.path().join("blocker");
            std::fs::write(&blocker, b"kept").expect("write the blocking file");
            let content: Vec<u8> = (0..40 * 1024).map(|index| (index % 251) as u8).collect();

            for listed in [false, true] {
                let node = stored_node(&repository, &content, listed, false).await;
                let path = relative("blocker/file");
                let write = operation.write_node(repository.clone(), &node, &path);
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), write)
                    .await
                    .expect("the write fails without waiting out the retries");

                assert!(result.is_err(), "a node cannot land below a file");
                assert_eq!(
                    std::fs::read(&blocker).expect("read the blocking file"),
                    b"kept",
                    "the file is left as it was"
                );
            }
        })
        .await;
}

/// A node without the executable bit clears it from the file it overwrites, whose mode the
/// write keeps.
#[cfg(unix)]
#[tokio::test]
async fn a_written_node_clears_an_executable_bit_it_does_not_carry() {
    use std::os::unix::fs::PermissionsExt;

    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            let repository = working_tree_repository(dir.path()).await;
            let operation = repository
                .file_system()
                .begin_operation()
                .await
                .expect("beginning an operation over the working tree");
            let path = dir.path().join("tool");
            std::fs::write(&path, b"#!/bin/sh\n").expect("write the file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("set the file's mode");

            let node = stored_node(&repository, b"plain content", false, false).await;
            let info = operation
                .write_node(repository.clone(), &node, &relative("tool"))
                .await
                .expect("writing the node");

            let mode = std::fs::metadata(&path)
                .expect("stat the file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644, "the bit is cleared for everyone");
            assert_eq!(Some(false), info.executable());
        })
        .await;
}

/// An operation creates, writes and removes nothing at a path that reaches anywhere but an entry
/// below its root on some platform: one that steps up, one that opens with a Windows drive, which
/// on this platform names a place inside the root, and one that names the root itself.
#[tokio::test]
async fn an_operation_touches_nothing_at_a_path_that_leaves_its_root() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let dir = temp_dir();
            let root = dir.path().join("root");
            std::fs::create_dir(&root).expect("create the root");
            let operation = os_operation(&root).await;

            let up = RelativePath::new().join("..").join("escape");
            let drive = RelativePath::new().join("Q:").join("planted");
            for (path, lands) in [(&up, dir.path().join("escape")), (&drive, root.join("Q:"))] {
                assert!(
                    operation
                        .create_dir_all(path)
                        .await
                        .is_err_and(|err| err.is_invalid_path()),
                    "{path} must be refused"
                );
                assert!(
                    operation
                        .write_file(path, bytes::Bytes::from_static(b"planted"))
                        .await
                        .is_err_and(|err| err.is_invalid_path()),
                    "{path} must be refused"
                );
                assert!(!lands.exists(), "nothing lands at {}", lands.display());
            }

            let victim = dir.path().join("victim");
            std::fs::write(&victim, b"kept").expect("write the file outside the root");
            let kept = root.join("kept");
            std::fs::write(&kept, b"kept").expect("write the file in the root");
            let up = RelativePath::new().join("..").join("victim");
            let itself = RelativePath::new().join(".");
            assert!(
                operation
                    .remove(&up)
                    .await
                    .is_err_and(|err| err.is_invalid_path()),
                "{up} must be refused"
            );
            assert!(
                operation
                    .remove_recursive(&itself)
                    .await
                    .is_err_and(|err| err.is_invalid_path()),
                "{itself} must be refused"
            );
            for file in [&victim, &kept] {
                assert_eq!(
                    std::fs::read(file).expect("read the file"),
                    b"kept",
                    "{} is left as it was",
                    file.display()
                );
            }
        })
        .await;
}
