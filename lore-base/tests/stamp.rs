// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore-stamp` against a real artifact: a copy of this test binary, which links `lore-base` as
//! every Lore binary does.
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use lore_base::test_util::TempDir;
use lore_base::version::LORE_LIBRARY_VERSION;
use lore_base::version::LORE_LIBRARY_VERSION_CSTR;
use lore_base::version::version_name;

/// Prints the versions this binary reports. The tests below run it inside a copy of this binary.
#[test]
#[ignore = "run inside a copy of this binary"]
fn report_version() {
    println!("version={}", *LORE_LIBRARY_VERSION);
    println!("cstr={}", LORE_LIBRARY_VERSION_CSTR.to_string_lossy());
}

fn stamp(build: &str, files: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lore-stamp"))
        .arg("--build")
        .arg(build)
        .args(files)
        .output()
        .expect("run lore-stamp")
}

fn copy_of_this_binary(dir: &TempDir) -> PathBuf {
    let copy = dir.child(&format!("copy{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().expect("locate this binary"), &copy)
        .expect("copy this binary");
    copy
}

/// The version and the C string version a copy of this binary reports.
///
/// On arm64 macOS the copy only runs if `lore-stamp` signed it again after stamping it.
fn reported_versions(binary: &Path) -> (String, String) {
    let output = Command::new(binary)
        .args(["report_version", "--exact", "--ignored", "--nocapture"])
        .output()
        .expect("run the copy of this binary");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    let value = |key: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} line in {stdout}"))
            .to_owned()
    };
    (value("version="), value("cstr="))
}

#[test]
fn a_stamped_binary_reports_the_stamped_version() {
    let dir = TempDir::new("lore-stamp-binary-");
    let binary = copy_of_this_binary(&dir);
    let local = version_name("local");
    assert_eq!(reported_versions(&binary), (local.clone(), local));

    let output = stamp("e2e.1", &[&binary]);
    assert!(output.status.success(), "{output:?}");
    let stamped = version_name("e2e.1");
    assert_eq!(reported_versions(&binary), (stamped.clone(), stamped));

    let output = stamp("2", &[&binary]);
    assert!(output.status.success(), "{output:?}");
    let restamped = version_name("2");
    assert_eq!(reported_versions(&binary), (restamped.clone(), restamped));
}

#[test]
fn a_batch_holding_a_file_without_a_slot_changes_no_file() {
    let dir = TempDir::new("lore-stamp-batch-");
    let binary = copy_of_this_binary(&dir);
    let unstampable = dir.child("unstampable");
    std::fs::write(&unstampable, b"no version slot here").expect("write a file without a slot");
    let before = std::fs::read(&binary).expect("read the copy of this binary");

    let output = stamp("e2e.1", &[&binary, &unstampable]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        std::fs::read(&binary).expect("read the copy of this binary") == before,
        "the binary was changed by a batch that failed"
    );
}
