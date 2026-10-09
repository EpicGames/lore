// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use lore_base::test_util::TempDir;
use rbe_lore::digest::EMPTY_SHA256;
use rbe_lore::workspace::*;

#[test]
fn only_lowercase_sha256_hex_is_accepted() {
    assert!(is_sha256_hex(EMPTY_SHA256));
    assert!(!is_sha256_hex(&EMPTY_SHA256.to_uppercase()));
    assert!(!is_sha256_hex(&EMPTY_SHA256[..63]));
    assert!(!is_sha256_hex(""));
}

#[test]
fn a_hash_is_trusted_only_for_content_of_the_recorded_size() {
    let p = Path::new("f");
    let hex = EMPTY_SHA256.to_string();
    let five = "5".to_string();
    assert_eq!(classify(p, None, None, 5).unwrap(), Recorded::Missing);
    assert_eq!(
        classify(p, Some(&hex), Some(&five), 5).unwrap(),
        Recorded::Valid(hex.clone())
    );
    assert_eq!(
        classify(p, Some(&hex), Some(&five), 6).unwrap(),
        Recorded::Stale
    );
    assert_eq!(classify(p, Some(&hex), None, 5).unwrap(), Recorded::Stale);
    assert_eq!(
        classify(p, Some(&hex), Some(&"five".to_string()), 5).unwrap(),
        Recorded::Stale
    );
    assert!(classify(p, Some(&"not hex".to_string()), Some(&five), 5).is_err());
}

#[test]
fn checkout_files_skips_the_lore_directory_and_symlinks() {
    let dir = TempDir::new("rbe-ws-checkout-files-");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".lore/store")).unwrap();
    std::fs::create_dir_all(root.join("src/.lore")).unwrap();
    std::fs::write(root.join(".lore/store/x"), b"x").unwrap();
    std::fs::write(root.join("src/.lore/kept"), b"k").unwrap();
    std::fs::write(root.join("src/a.cc"), b"a").unwrap();
    std::fs::write(root.join("BUILD"), b"b").unwrap();
    std::os::unix::fs::symlink(root.join("BUILD"), root.join("link")).unwrap();

    let files = checkout_files(root).unwrap();
    let rel: Vec<_> = files
        .iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(rel, ["BUILD", "src/.lore/kept", "src/a.cc"]);
}
