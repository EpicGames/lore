// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use lore_base::fs::win_path::*;

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Pad `prefix` with filler bytes until appending `suffix` produces a
/// path longer than `MAX_PATH`, so the prefix branch of
/// `to_extended_wide` is exercised.
fn long_path(prefix: &str, suffix: &str) -> String {
    let mut out = String::from(prefix);
    while out.len() + suffix.len() < 270 {
        out.push('a');
    }
    out.push_str(suffix);
    out
}

#[test]
fn drive_letter_path() {
    assert_eq!(
        to_extended_wide(Path::new(r"C:\foo\bar")),
        wide(r"C:\foo\bar"),
        "short drive-letter path is returned unchanged",
    );
    let long = long_path(r"C:\foo\", r"\bar");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&format!(r"\\?\{long}")),
        "long drive-letter path gets the verbatim prefix",
    );
}

#[test]
fn forward_slashes() {
    assert_eq!(
        to_extended_wide(Path::new("Z:/devel/temp/file.txt")),
        wide("Z:/devel/temp/file.txt"),
        "short path with forward slashes is returned unchanged",
    );
    let long = long_path("Z:/devel/", "/file.txt");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&format!(r"\\?\{}", long.replace('/', r"\"))),
        "long path with forward slashes is prefixed and normalised",
    );
}

#[test]
fn mixed_separators() {
    assert_eq!(
        to_extended_wide(Path::new(r"Z:/devel/temp\dir\file.uasset")),
        wide(r"Z:/devel/temp\dir\file.uasset"),
        "short path with mixed separators is returned unchanged",
    );
    let long = long_path(r"Z:/devel/temp\dir/", r"\file.uasset");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&format!(r"\\?\{}", long.replace('/', r"\"))),
        "long path with mixed separators is prefixed and normalised",
    );
}

#[test]
fn already_prefixed_path() {
    assert_eq!(
        to_extended_wide(Path::new(r"\\?\C:\foo")),
        wide(r"\\?\C:\foo"),
        "short \\?\\ path is returned unchanged",
    );
    let long = long_path(r"\\?\C:\foo\", r"\bar");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&long),
        "long \\?\\ path passes through without a second prefix",
    );
}

#[test]
fn device_prefixed_path() {
    assert_eq!(
        to_extended_wide(Path::new(r"\\.\PhysicalDrive0")),
        wide(r"\\.\PhysicalDrive0"),
        "short \\.\\ path is returned unchanged",
    );
    let long = long_path(r"\\.\PhysicalDrive0\", r"\bar");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&long),
        "long \\.\\ path passes through without being rewritten",
    );
}

#[test]
fn unc_path() {
    assert_eq!(
        to_extended_wide(Path::new(r"\\server\share\file")),
        wide(r"\\server\share\file"),
        "short UNC path is returned unchanged",
    );
    let long = long_path(r"\\server\share\", r"\file");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&format!(r"\\?\UNC\{}", &long[2..])),
        "long UNC path gets the \\?\\UNC\\ prefix",
    );
}

#[test]
fn relative_path() {
    assert_eq!(
        to_extended_wide(Path::new(r"foo\bar")),
        wide(r"foo\bar"),
        "short relative path is returned unchanged",
    );
    let long = long_path(r"foo\", r"\bar");
    assert_eq!(
        to_extended_wide(Path::new(&long)),
        wide(&long),
        "long relative path is left alone (no prefix injected)",
    );
}
