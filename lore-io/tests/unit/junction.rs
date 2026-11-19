// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::test_util::TempDir;
use lore_io::junction::create_junction;

/// A junction made to a directory reads back through `std::fs::read_link` as that directory, is a
/// name surrogate that `symlink_metadata` reports as a link, and leads to the directory's files.
#[test]
fn a_junction_is_made_and_read_back() {
    let dir = TempDir::new("lore-io-junction-");
    let target = dir.join("target");
    std::fs::create_dir(&target).expect("create the target");
    std::fs::write(target.join("file"), b"file").expect("write a file in it");
    let junction = dir.join("junction");

    create_junction(&target, &junction).expect("make the junction");

    assert!(
        std::fs::symlink_metadata(&junction)
            .expect("stat the junction")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&junction).expect("read the junction"),
        target
    );
    assert_eq!(
        std::fs::read(junction.join("file")).expect("read through the junction"),
        b"file"
    );
}

/// A junction to a relative path is refused, and leaves nothing at its path.
#[test]
fn a_junction_to_a_relative_path_is_refused() {
    let dir = TempDir::new("lore-io-junction-relative-");
    let junction = dir.join("junction");

    let made = create_junction(std::path::Path::new("target"), &junction);

    assert_eq!(
        made.map_err(|err| err.kind()),
        Err(std::io::ErrorKind::InvalidInput)
    );
    assert!(std::fs::symlink_metadata(&junction).is_err());
}

/// A junction is refused where anything is at its path, which it leaves as it was.
#[test]
fn a_junction_over_an_existing_path_is_refused() {
    let dir = TempDir::new("lore-io-junction-existing-");
    let target = dir.join("target");
    std::fs::create_dir(&target).expect("create the target");
    let junction = dir.join("junction");
    std::fs::create_dir(&junction).expect("create a directory at the junction's path");
    std::fs::write(junction.join("kept"), b"kept").expect("write a file in it");

    let made = create_junction(&target, &junction);

    assert_eq!(
        made.map_err(|err| err.kind()),
        Err(std::io::ErrorKind::AlreadyExists)
    );
    assert!(junction.join("kept").is_file());
}
