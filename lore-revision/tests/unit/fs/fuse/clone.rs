// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Cloning a virtual clone's external directory: what cloning takes.

use std::path::Path;
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_io::clone::Kind;
use lore_revision::fs::filesystem_provider::CloneAnswer;
use lore_revision::fs::filesystem_provider::CloneRun;
use lore_revision::fs::fuse::clone::answer;
use lore_revision::fs::fuse::clone::clone;
use lore_revision::fs::fuse::clone::visit;
use lore_revision::fs::walk::Progress;
use lore_revision::fs::walk::Visit;

use super::fuse_fixture::setup_test_execution;
use super::fuse_fixture::test_file_write;

fn visited(root: &Path, relative: &str, kind: Kind) -> Visit {
    visit(&root.join(relative), Path::new(relative), kind)
}

/// Cloning takes the Lore directory, its records with it, but what cloning writes for the new
/// instance and what belongs to the running source, the local files but Lore directories and nested
/// repositories among them, and the deletion list; nothing else in the external directory.
#[test]
fn a_clone_takes_the_lore_directory_the_local_files_and_the_deletion_list() {
    let root = TempDir::new("fuse-clone-visit-");
    std::fs::create_dir_all(root.path().join("write/nested/.lore")).expect("a nested repository");
    std::fs::create_dir_all(root.path().join("write/plain")).expect("a directory");
    let root = root.path();
    let cases = [
        (".lore", Kind::Directory, Visit::EachEntry),
        (".lore/config", Kind::File, Visit::Clone),
        (".lore/records", Kind::Directory, Visit::EachEntry),
        (".lore/records/00", Kind::File, Visit::Clone),
        (".lore/records/3f", Kind::File, Visit::Clone),
        (".lore/instance", Kind::File, Visit::Skip),
        (".lore/clone-in-progress", Kind::File, Visit::Skip),
        (".lore/records/no-store", Kind::File, Visit::Skip),
        (".lore/records/3f.tmp", Kind::File, Visit::Skip),
        (".lore/lock", Kind::File, Visit::Skip),
        ("write", Kind::Directory, Visit::EachEntry),
        ("write/a.txt", Kind::File, Visit::Clone),
        ("write/plain", Kind::Directory, Visit::Clone),
        ("write/plain/b.txt", Kind::File, Visit::Clone),
        ("write/.lore", Kind::Directory, Visit::Skip),
        ("write/plain/.urc", Kind::Directory, Visit::Skip),
        ("write/nested", Kind::Directory, Visit::Skip),
        ("deleted", Kind::File, Visit::Clone),
        ("deleted.new", Kind::File, Visit::Skip),
        ("write-staging", Kind::Directory, Visit::Skip),
        ("write", Kind::File, Visit::Skip),
        ("other", Kind::File, Visit::Skip),
    ];
    for (relative, kind, expected) in cases {
        assert_eq!(visited(root, relative, kind), expected, "{relative}");
    }
}

/// An external directory as a mount leaves it.
fn external_directory(root: &Path) -> std::path::PathBuf {
    let external = root.join("source");
    for (relative, contents) in [
        (".lore/config", b"config" as &[u8]),
        (".lore/instance", b"0123456789abcdef"),
        (".lore/lock", b""),
        (".lore/records/00", b"records"),
        (".lore/records/3f", b"times"),
        ("write/local.txt", b"local"),
        ("write/dir/inner.txt", b"inner"),
        ("write/dir/nested/.lore/instance", b"nested"),
        ("write-staging/1", b"staged"),
        ("deleted", b"gone.txt\nhalf"),
        ("deleted.new", b"rewrite"),
    ] {
        let path = external.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        test_file_write(&path, contents);
    }
    external
}

/// Clones the external directory at `external` to `destination`, holding a Lore directory as the
/// instance clone writes it first. Where the volume cannot clone, the answer measures `files`
/// files to copy.
async fn clone_external(
    external: &Path,
    destination: &Path,
    files: u64,
) -> lore_revision::fs::walk::CloneStats {
    std::fs::create_dir_all(destination.join(".lore")).expect("the new instance's Lore directory");
    test_file_write(&destination.join(".lore/instance"), b"fedcba9876543210");
    let answer = answer(external).await.expect("answer");
    assert!(
        !matches!(answer, CloneAnswer::Unsupported { .. }),
        "{answer:?}"
    );
    if let CloneAnswer::CopiesBytes { size, .. } = &answer {
        assert_eq!(size.file_count, files, "the files cloning takes");
    }
    let run = CloneRun {
        progress: Arc::new(Progress::default()),
        hold: Arc::new(()),
    };
    clone(external, destination, &answer, true, &run)
        .await
        .expect("clone")
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// Cloning takes the Lore directory, its records with it, the local files and the deletion list as
/// they are, and leaves out the source's instance file, its lock, the writes under way, a
/// rewrite of the list and a nested repository.
#[tokio::test]
async fn a_clone_of_an_external_directory_takes_its_local_changes() {
    LORE_CONTEXT
        .scope(setup_test_execution(), async {
            let root = TempDir::new("fuse-clone-external-");
            let external = external_directory(root.path());
            let destination = root.path().join("clone");
            // The config, two records shards, two local files and the list.
            clone_external(&external, &destination, 6).await;

            for relative in [
                ".lore/config",
                ".lore/records/00",
                ".lore/records/3f",
                "write/local.txt",
                "write/dir/inner.txt",
                "deleted",
            ] {
                assert_eq!(
                    read(&destination.join(relative)),
                    read(&external.join(relative)),
                    "{relative}"
                );
            }
            assert_eq!(
                read(&destination.join(".lore/instance")),
                b"fedcba9876543210"
            );
            for left_out in [
                ".lore/lock",
                "write/dir/nested",
                "write-staging",
                "deleted.new",
            ] {
                assert!(!destination.join(left_out).exists(), "{left_out}");
            }
        })
        .await;
}
