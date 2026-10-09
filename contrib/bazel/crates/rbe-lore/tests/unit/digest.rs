// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use lore_base::test_util::TempDir;
use rbe_lore::digest::*;
use rbe_proto::reapi::Digest;

#[test]
fn the_empty_blob_constant_is_the_hash_of_nothing() {
    assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    assert!(is_empty_digest(&of(b"")));
    assert!(is_empty_blob(EMPTY_SHA256, 0));
}

/// Size and hash both have to match. A zero-length file whose hash says otherwise is
/// corruption, not the empty blob, and treating it as always-present would hide that.
#[test]
fn a_wrong_size_or_hash_is_not_the_empty_blob() {
    assert!(!is_empty_blob(EMPTY_SHA256, 1));
    assert!(!is_empty_blob("00", 0));
    assert!(!is_empty_digest(&of(b"x")));
}

#[tokio::test]
async fn the_streaming_file_digest_agrees_with_the_in_memory_one() {
    // Spans the read buffer, because a digest that only works for content smaller than one
    // chunk would pass every small test and corrupt every real output.
    for size in [
        0usize,
        1,
        HASH_CHUNK_BYTES - 1,
        HASH_CHUNK_BYTES,
        HASH_CHUNK_BYTES + 1,
    ] {
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let dir = TempDir::new(&format!("rbe-digest-test-{size}-"));
        let path = dir.child("content");
        tokio::fs::write(&path, &content).await.unwrap();
        assert_eq!(of_file(&path).await.unwrap(), of(&content), "size {size}");
    }
}

#[tokio::test]
async fn digesting_a_missing_file_is_an_error_rather_than_the_empty_digest() {
    let dir = TempDir::new("rbe-digest-test-absent-");
    assert!(of_file(&dir.child("absent")).await.is_err());
}

#[test]
fn key_of_carries_hash_and_size() {
    let d = of(b"hello");
    assert_eq!(key_of(&d), (d.hash.clone(), 5));
}

#[test]
fn fmt_truncates_without_panicking_on_a_short_hash() {
    assert_eq!(
        fmt(&Digest {
            hash: "abc".into(),
            size_bytes: 1
        }),
        "abc/1"
    );
    assert_eq!(fmt(&of(b"hello")).len(), 12 + 2);
}
