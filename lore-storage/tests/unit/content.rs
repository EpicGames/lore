// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::types::Address;
use lore_base::types::Hash;
use lore_storage::content::ContentSource;
use lore_storage::error::StorageError;

fn test_address(seed: u64) -> Address {
    Address {
        hash: Hash::from_u64(seed),
        ..Default::default()
    }
}

/// Virtual content names no host path.
#[test]
fn virtual_immutable_has_no_file_path() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    assert!(source.file_path().is_none());
}

/// This crate does not open virtual content.
#[tokio::test]
async fn virtual_immutable_open_once_is_unsupported() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    match source.open_once().await {
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::Unsupported),
        Ok(_) => panic!("virtual content cannot be opened directly"),
    }
}

/// Virtual immutable content displays as the address it names.
#[test]
fn virtual_immutable_display_names_address() {
    let address = test_address(7);
    let source = ContentSource::virtual_immutable(address, 42);
    assert_eq!(format!("{source}"), format!("immutable:{address}"));
}

/// Virtual immutable content answers its declared size, which settles a comparison by size
/// without reading it.
#[tokio::test]
async fn virtual_immutable_size_is_its_declared_size() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    assert_eq!(source.size().await.expect("a declared size"), 42);
}

/// A file's size is its length, and a path that holds nothing has no size.
#[tokio::test]
async fn a_file_size_is_its_length() {
    let file = lore_base::test_util::TempFile::with_contents("content-size-", b"twelve bytes");
    let missing = lore_base::test_util::TempDir::new("content-size-missing-");

    assert_eq!(
        ContentSource::file(file.path())
            .size()
            .await
            .expect("a file's size"),
        12
    );
    assert!(
        ContentSource::file(&missing.child("absent"))
            .size()
            .await
            .is_err()
    );
}

/// Opening or reading virtual content fails at once as an argument error, with no back-off.
#[tokio::test]
async fn virtual_immutable_open_and_read_all_are_invalid_arguments() {
    let source = ContentSource::virtual_immutable(test_address(1), 42);
    let started = std::time::Instant::now();
    match source.open().await {
        Err(err) => assert!(matches!(err, StorageError::InvalidArguments(_)), "{err:?}"),
        Ok(_) => panic!("virtual content cannot be opened by this crate"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "the open waited on a back-off"
    );
    let err = source
        .read_all()
        .await
        .expect_err("virtual content cannot be read by this crate");
    assert!(matches!(err, StorageError::InvalidArguments(_)), "{err:?}");
}
