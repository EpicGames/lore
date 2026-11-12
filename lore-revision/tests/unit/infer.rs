// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;

use lore_revision::infer::ConflictMarkers;
use lore_revision::infer::SCAN_WINDOW;
use lore_revision::infer::infer_is_conflicted;
use lore_revision::infer::infer_is_diffable_by_slice;
use lore_revision::infer::infer_is_upackage_by_slice;
use lore_revision::infer::infer_is_utf8_by_slice;
use lore_storage::ContentSource;

/// A file holding `contents`, alive for as long as the returned directory is.
#[allow(clippy::disallowed_methods)] // A test fixture in its own temporary directory.
fn file_holding(contents: &[u8]) -> (lore_base::test_util::TempDir, PathBuf) {
    let dir = lore_base::test_util::TempDir::new("lore-infer-test-");
    let path = dir.path().join("scanned");
    std::fs::write(&path, contents).expect("write scanned file");
    (dir, path)
}

#[test]
fn is_utf8() {
    let one_sparkle_heart = vec![240, 159, 146, 150];
    assert!(
        infer_is_utf8_by_slice(&one_sparkle_heart),
        "One sparkle heart is UTF-8"
    );

    let two_sparkle_hearts = vec![240, 159, 146, 150, 240, 159, 146, 150];
    assert!(
        infer_is_utf8_by_slice(&two_sparkle_hearts),
        "Two sparkle hearts are UTF-8"
    );

    let two_sparkle_hearts_truncated = vec![240, 159, 146, 150, 240, 159, 146];
    assert!(
        !infer_is_utf8_by_slice(&two_sparkle_hearts_truncated),
        "Two sparkle hearts with the last one truncated does not count as UTF-8"
    );

    let three_sparkle_heart_invalid = vec![240, 159, 146, 150, 240, 159, 240, 159, 146, 150];
    assert!(
        !infer_is_utf8_by_slice(&three_sparkle_heart_invalid),
        "Three sparkle hearts with the middle one being invalid does not count as UTF-8"
    );
}

#[test]
fn non_diffable_utf16_le_bom() {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("Hello\nWorld\n".encode_utf16().flat_map(u16::to_le_bytes));
    assert!(
        !infer_is_diffable_by_slice(&bytes),
        "UTF-16 LE BOM must be non-diffable so merge falls into the binary-conflict path that preserves bytes"
    );
}

#[test]
fn non_diffable_utf16_be_bom() {
    let mut bytes = vec![0xFE, 0xFF];
    bytes.extend("Hello\nWorld\n".encode_utf16().flat_map(u16::to_be_bytes));
    assert!(
        !infer_is_diffable_by_slice(&bytes),
        "UTF-16 BE BOM must be non-diffable so merge falls into the binary-conflict path that preserves bytes"
    );
}

#[test]
fn upackage_tag_is_detected() {
    let mut bytes = vec![0x9E, 0x2A, 0x83, 0xC1];
    bytes.extend_from_slice(&[0u8; 16]);
    assert!(
        infer_is_upackage_by_slice(&bytes),
        "An Unreal package file tag must be detected"
    );

    let mut bad_tag = vec![0x9E, 0x2A, 0x83, 0xC2];
    bad_tag.extend_from_slice(&[0u8; 16]);
    assert!(
        !infer_is_upackage_by_slice(&bad_tag),
        "A buffer that only shares the first three tag bytes is not an Unreal package"
    );
}

#[test]
fn upackage_swapped_tag_is_detected() {
    let mut bytes = vec![0xC1, 0x83, 0x2A, 0x9E];
    bytes.extend_from_slice(&[0u8; 16]);
    assert!(
        infer_is_upackage_by_slice(&bytes),
        "A byte swapped Unreal package file tag must be detected"
    );

    let mut bad_tag = vec![0xC1, 0x83, 0x2A, 0x9F];
    bad_tag.extend_from_slice(&[0u8; 16]);
    assert!(
        !infer_is_upackage_by_slice(&bad_tag),
        "A buffer that only shares the first three swapped tag bytes is not an Unreal package"
    );
}

#[tokio::test]
async fn marker_within_one_window_is_conflicted() {
    let (_dir, path) = file_holding(b"clean line\n<<<<<<< ours\nmore\n");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Present
    );
}

/// The marker's own bytes straddle the window boundary: three of them end the first window
/// and the rest open the second, so only the carry joining them sees a marker at all.
#[tokio::test]
async fn marker_split_across_a_window_boundary_is_conflicted() {
    let mut contents = vec![b'a'; SCAN_WINDOW - 4];
    contents.push(b'\n');
    contents.extend_from_slice(b"<<<<<<< ours\n");

    let (_dir, path) = file_holding(&contents);
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Present
    );
}

/// A marker line ending `\r\n` with the carriage return the last byte of one window and the
/// newline the first of the next. The line is only stripped after the carry joins them, so
/// this is what says the strip happens on the joined line rather than per window.
#[tokio::test]
async fn crlf_split_across_a_window_boundary_is_conflicted() {
    let marker = b"<<<<<<< ours";
    let mut contents = marker.to_vec();
    contents.extend(std::iter::repeat_n(b' ', SCAN_WINDOW - 1 - marker.len()));
    contents.extend_from_slice(b"\r\n");
    assert_eq!(contents[SCAN_WINDOW - 1], b'\r');
    assert_eq!(contents[SCAN_WINDOW], b'\n');

    let (_dir, path) = file_holding(&contents);
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Present
    );
}

/// The last line of a file with no trailing newline never reaches the in-window scan, only
/// the carry left over once the windows run out.
#[tokio::test]
async fn marker_on_an_unterminated_final_line_is_conflicted() {
    let (_dir, path) = file_holding(b"clean line\n>>>>>>> theirs");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Present
    );
}

#[tokio::test]
async fn unterminated_final_line_without_a_marker_is_clean() {
    let (_dir, path) = file_holding(b"clean line\nalso clean");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Absent
    );
}

/// A UTF-16 file is read whole rather than scanned in windows, so one larger than a window
/// exercises the second read the head does not cover.
#[tokio::test]
async fn utf16_larger_than_one_window_is_conflicted() {
    let text = format!("{}\n<<<<<<< ours\n", "a".repeat(SCAN_WINDOW));
    let mut contents = vec![0xFF, 0xFE];
    contents.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    assert!(contents.len() > SCAN_WINDOW);

    let (_dir, path) = file_holding(&contents);
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Present
    );
}

/// An empty file has no window to scan and no carry to check, and must end rather than wait
/// for a read that never comes.
#[tokio::test]
async fn empty_file_is_clean() {
    let (_dir, path) = file_holding(b"");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Absent
    );
}

/// A line that is not text ends the scan, so a marker after one is never reached. This is
/// the line reader's behaviour that the windowed scan replaced, kept deliberately.
#[tokio::test]
async fn a_line_that_is_not_utf8_ends_the_scan() {
    let (_dir, path) = file_holding(b"clean line\n\xC3\x28 broken\n<<<<<<< ours\n");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::Absent
    );
}

#[tokio::test]
async fn a_missing_file_holds_no_file() {
    let dir = lore_base::test_util::TempDir::new("lore-infer-test-");
    let path = dir.path().join("absent");
    assert_eq!(
        infer_is_conflicted(&ContentSource::file(&path))
            .await
            .unwrap(),
        ConflictMarkers::NoFile
    );
}
