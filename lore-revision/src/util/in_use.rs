// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reissuing a file operation that fails while something has the files in use.

use std::fmt;
use std::io;
use std::path::Path;

use lore_base::lore_debug;

/// First wait before a file operation that failed because something had the files in use is
/// reissued, in milliseconds.
#[lore_macro::test_pub]
const IN_USE_RETRY_START_MILLIS: u64 = 50;

/// Longest wait the backoff grows to, in milliseconds.
#[lore_macro::test_pub]
const IN_USE_RETRY_MAXIMUM_MILLIS: u64 = 2_000;

/// Waits before an operation that keeps failing in use is reported. With the schedule above this
/// waits about fifteen seconds, longer than the ten seconds a finished command in the same process
/// keeps its stores open by default.
#[lore_macro::test_pub]
const IN_USE_RETRY_LIMIT: usize = 12;

/// Whether `error` says that something has the files in use, so the operation can succeed once
/// it lets go: another Lore command reading them, in this process or another, or a program the
/// user has open in them.
#[lore_macro::test_pub]
pub(crate) fn is_in_use(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ResourceBusy || is_in_use_for_platform(error)
}

/// Whether a failed rename to `to` is worth reissuing: the files were in use and
/// the target is still free. Windows reports a rename onto a path that is taken as access denied,
/// the same error a file open in the directory gives, and no wait frees that path.
#[lore_macro::test_pub]
pub(crate) fn rename_is_retryable<'a>(
    error: &io::Error,
    to: &'a Path,
) -> impl Future<Output = bool> + use<'a> {
    let in_use = is_in_use(error);
    async move { in_use && is_free(to).await }
}

/// Whether nothing is at `path`, linked or not. A path that cannot be checked counts as taken.
pub(crate) async fn is_free(path: &Path) -> bool {
    matches!(
        lore_io::IoDriver::global().symlink_stat(path).await,
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

/// Windows refuses to rename a directory with a file open in it as access denied, and to delete
/// a file another handle shares no deletion with as a sharing violation.
#[cfg(target_family = "windows")]
fn is_in_use_for_platform(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32
                || code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
                || code == windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32
    )
}

/// No other kernel refuses a rename or delete because a file is open.
#[cfg(not(target_family = "windows"))]
fn is_in_use_for_platform(_error: &io::Error) -> bool {
    false
}

/// Runs `operation`, reissuing it with backoff for as long as it fails with an error `retryable`
/// accepts, up to [`IN_USE_RETRY_LIMIT`] times. Any other failure is reported at once.
///
/// Takes plain closures returning futures rather than `AsyncFn`s, whose higher-ranked lifetimes
/// defeat `Send` inference for the callers that spawn these commands.
#[lore_macro::test_pub]
pub(crate) async fn retry_while_in_use<T, E, F, O, R, RO>(
    action: &str,
    retryable: R,
    mut operation: F,
) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut() -> O,
    O: Future<Output = Result<T, E>>,
    R: Fn(&E) -> RO,
    RO: Future<Output = bool>,
{
    let mut retry = crate::util::time::retry(
        IN_USE_RETRY_START_MILLIS,
        IN_USE_RETRY_MAXIMUM_MILLIS,
        IN_USE_RETRY_LIMIT,
    );
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error).await && retry.wait().await => {
                lore_debug!("Retrying to {action}, which failed in use: {error}");
            }
            Err(error) => return Err(error),
        }
    }
}
