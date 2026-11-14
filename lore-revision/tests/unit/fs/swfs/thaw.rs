// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::ffi::CString;
use std::ffi::c_int;

use lore_revision::fs::filesystem_provider::FileInfo;
use lore_revision::fs::swfs::api_interface::THAW_WAIT_MS;
use lore_revision::fs::swfs::api_interface::Thaw;
use lore_revision::fs::swfs::api_interface::thaw_leftover;
use lore_revision::fs::swfs::api_interface::thaw_or_keep;
use lore_revision::fs::swfs::file::SwfsFile;
use lore_revision::fs::swfs::file::SwfsFileArray;
use parking_lot::Mutex;

/// Thaws a frozen mount, with a thaw that `finishes` or not: the outcome, and the waits it asked
/// SWFS for.
fn thaw_finishing(finishes: bool, kept: &Mutex<Option<SwfsFileArray>>) -> (Thaw, Vec<c_int>) {
    let mut waits = Vec::new();
    let thaw = thaw_or_keep(
        true,
        SwfsFileArray::empty(),
        |_, wait_ms| {
            waits.push(wait_ms);
            finishes
        },
        kept,
    );
    (thaw, waits)
}

#[test]
fn a_thaw_asks_swfs_once_with_the_whole_wait() {
    let kept = Mutex::new(None);
    let (thaw, waits) = thaw_finishing(true, &kept);
    assert_eq!(thaw, Thaw::Thawed);
    assert_eq!(waits, [THAW_WAIT_MS]);
    assert!(kept.lock().is_none());
}

#[test]
fn a_thaw_that_doesnt_finish_is_kept_for_the_next_command() {
    let kept = Mutex::new(None);
    let (thaw, waits) = thaw_finishing(false, &kept);
    assert_eq!(thaw, Thaw::StillFrozen);
    assert_eq!(waits, [THAW_WAIT_MS], "SWFS is asked once, not again");
    assert!(kept.lock().is_some(), "the thaw is kept, with its files");
}

#[test]
fn a_mount_that_isnt_frozen_isnt_thawed() {
    let kept = Mutex::new(None);
    let mut attempts = 0;
    let thaw = thaw_or_keep(
        false,
        SwfsFileArray::empty(),
        |_, _| {
            attempts += 1;
            true
        },
        &kept,
    );
    assert_eq!(thaw, Thaw::NotFrozen);
    assert_eq!(attempts, 0, "SWFS isn't asked");
    assert!(
        kept.lock().is_none(),
        "nothing is kept to wedge the next command"
    );
}

/// A list of one file, which unlike an empty one has an address of its own to tell it by.
fn one_file() -> SwfsFileArray {
    let file = SwfsFile::new(
        CString::new("written.txt").expect("name"),
        &FileInfo::Directory,
        None,
    )
    .expect("file");
    SwfsFileArray::new(vec![file])
}

#[test]
fn a_leftover_freeze_is_ended_with_the_files_of_the_thaw_kept_for_it() {
    let kept = Mutex::new(Some(one_file()));
    let kept_files = kept.lock().as_ref().expect("kept").array_ptr();
    let mut applied = Vec::new();
    let thaw = thaw_leftover(
        true,
        |files, _| {
            applied.push(files.array_ptr());
            true
        },
        &kept,
    );
    assert_eq!(thaw, Thaw::Thawed);
    assert_eq!(
        applied,
        [kept_files],
        "the kept thaw's own files are applied"
    );
    assert!(kept.lock().is_none(), "the kept thaw is finished");
}

#[test]
fn a_leftover_freeze_with_no_thaw_kept_is_ended_naming_no_files() {
    let kept = Mutex::new(None);
    let mut applied = Vec::new();
    let thaw = thaw_leftover(
        true,
        |files, _| {
            applied.push(files.array_ptr());
            true
        },
        &kept,
    );
    assert_eq!(thaw, Thaw::Thawed);
    assert_eq!(applied, [std::ptr::null()], "SWFS is told of no files");
}

#[test]
fn a_kept_thaw_whose_freeze_is_gone_is_dropped() {
    let kept = Mutex::new(Some(one_file()));
    let thaw = thaw_leftover(false, |_, _| true, &kept);
    assert_eq!(thaw, Thaw::NotFrozen);
    assert!(
        kept.lock().is_none(),
        "a thaw with no freeze to end can't hold the next command back"
    );
}

#[test]
fn a_kept_thaw_that_fails_again_keeps_its_files() {
    let kept = Mutex::new(Some(one_file()));
    let kept_files = kept.lock().as_ref().expect("kept").array_ptr();
    let thaw = thaw_leftover(true, |_, _| false, &kept);
    assert_eq!(thaw, Thaw::StillFrozen);
    assert_eq!(
        kept.lock().as_ref().map(SwfsFileArray::array_ptr),
        Some(kept_files),
        "the same files are kept for the command after"
    );
}
