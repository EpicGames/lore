// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;

use lore_base::test_util::TempDir;
use rbe_lore::digest;
use rbe_worker::*;

/// An output staged under its content key is what a later action links instead of fetching,
/// so it has to land under the right name, read-only, and as the same inode.
#[test]
fn adopting_an_output_stages_it_read_only_under_its_key() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("rbe-worker-test-adopt-");
    let output = dir.join("out.o");
    std::fs::write(&output, b"object code").unwrap();
    let d = digest::of(b"object code");

    let staging = Staging::new(dir.join("staged"));
    staging.adopt(&[(d.clone(), output.clone())]).unwrap();

    let staged = staging.path_for(&d.hash, false);
    let meta = std::fs::metadata(&staged).unwrap();
    assert_eq!(meta.ino(), std::fs::metadata(&output).unwrap().ino());
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);
    // Adopting the same content again is a no-op rather than an error.
    staging.adopt(&[(d, output)]).unwrap();
}

#[test]
fn laying_out_creates_parents_before_children_and_links_files() {
    let dir = TempDir::new("rbe-worker-test-layout-");
    std::fs::create_dir_all(dir.join("root")).unwrap();
    let staged = dir.join("staged-file");
    std::fs::write(&staged, b"x").unwrap();
    let root = dir.join("root");
    lay_out(
        &[root.join("a"), root.join("a/b")],
        &[(root.join("a/link"), "b".into())],
        &[(staged, root.join("a/b/f"), digest::of(b"x"))],
    )
    .unwrap();
    assert_eq!(std::fs::read(root.join("a/b/f")).unwrap(), b"x");
    assert_eq!(
        std::fs::read_link(root.join("a/link")).unwrap(),
        PathBuf::from("b")
    );
}
