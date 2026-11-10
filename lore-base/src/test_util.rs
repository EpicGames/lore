// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Temporary directories and files for the tests, and [`file_identity`], which tells a file written
//! in place from one replaced.
//!
//! [`TempDir`], and [`TempFile`] for a single file, are the only way a test asks
//! for scratch space on disk. Names carry a random suffix, so a test is isolated
//! from every other test and from a second run on the same machine, and removal
//! happens in `Drop`, so it happens whether the test passes, fails an assertion,
//! or returns early.
//!
//! Hand-building a path under [`std::env::temp_dir`] gives up both of those, and
//! removing a directory on the last line of a test body gives up the second: a
//! failing assertion panics before reaching that line.
//!
//! `LORE_KEEP_TEST_DATA=1` keeps them instead of removing them. The prefix is
//! all there is to go on when picking one out afterwards, so make it name the
//! test where several in a module would otherwise share one.
//!
//! Behind the `test-util` feature, which the crates whose tests need it enable
//! from their dev-dependencies, so none of this reaches a normal build.

use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The environment variable that keeps test directories on disk.
pub const KEEP_TEST_DATA_VAR: &str = "LORE_KEEP_TEST_DATA";

/// Whether this process should leave its test directories behind.
///
/// Read once. The variable is read from `Drop`, where there is nothing useful to
/// do about a mid-run change, and a test that set or cleared it partway through
/// would otherwise have some directories removed and others kept.
pub fn keep_test_data() -> bool {
    static KEEP: OnceLock<bool> = OnceLock::new();
    // Unset and empty both mean "clean up"; anything else, including the `0` a
    // shell script might pass, means keep. Being generous costs a stale
    // directory; being strict costs the evidence someone set the variable to
    // collect.
    *KEEP
        .get_or_init(|| std::env::var_os(KEEP_TEST_DATA_VAR).is_some_and(|value| !value.is_empty()))
}

/// The canonical form of `path`, without a Windows verbatim prefix.
///
/// Canonicalizing resolves the symlink the system temp directory is on macOS, so
/// a path a test built compares equal to one the code under test reports back.
/// On Windows it also returns the `\\?\` form, which is not the spelling the
/// rest of the system produces: APIs that treat a verbatim path as
/// already-normalized behave differently on one, and a test holding one is
/// testing a path shape it would never otherwise see. Only the prefix is
/// dropped; a UNC or long path keeps it, since removing it there changes which
/// file the path names.
fn canonical_without_verbatim(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("Failed to canonicalize {path:?}: {error}"));
    #[cfg(target_family = "windows")]
    {
        if let Some(text) = canonical.to_str()
            && let Some(stripped) = text.strip_prefix(r"\\?\")
            // `\\?\UNC\server\share` needs the prefix to stay a UNC path, and a
            // drive path is the only shape that reads the same without it.
            && stripped.len() >= 2
            && stripped.as_bytes()[1] == b':'
        {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

/// A uniquely named directory under the system temp directory, removed when this
/// value is dropped.
///
/// Hold it for as long as the paths under it are in use: the directory goes when
/// the last owner drops it, so binding it to `_` removes it immediately, while
/// binding it to a name starting with `_` keeps it for the rest of the scope.
#[derive(Debug)]
pub struct TempDir {
    /// `None` only after `LORE_KEEP_TEST_DATA` has taken the directory out of
    /// the guard's hands in `Drop`.
    inner: Option<tempfile::TempDir>,
    /// Resolved once at construction rather than through `inner` on every call:
    /// `path()` has to keep working after `Drop` has taken `inner`.
    path: PathBuf,
}

impl TempDir {
    /// Creates a directory named `<prefix><random>` under the system temp
    /// directory.
    ///
    /// Give `prefix` the test's or module's name, ending in `-`: it is what
    /// identifies the directory under `LORE_KEEP_TEST_DATA`, and what someone
    /// looking at a stale directory has to work back from.
    pub fn new(prefix: &str) -> Self {
        Self::created(tempfile::Builder::new().prefix(prefix).tempdir())
    }

    /// Creates a directory named `<prefix><random>` under `parent`, for a test that needs its
    /// scratch space on a particular volume.
    pub fn new_in(parent: &Path, prefix: &str) -> Self {
        Self::created(tempfile::Builder::new().prefix(prefix).tempdir_in(parent))
    }

    fn created(made: std::io::Result<tempfile::TempDir>) -> Self {
        let inner =
            made.unwrap_or_else(|error| panic!("Failed to create a temp directory: {error}"));
        // The guard still removes the directory by its own path; both name the
        // same directory.
        let path = canonical_without_verbatim(inner.path());
        Self {
            inner: Some(inner),
            path,
        }
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path to `name` inside the directory. Nothing is created.
    ///
    /// For a test that wants several named files under one guard; [`TempFile`]
    /// covers a single file on its own.
    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if keep_test_data()
            && let Some(inner) = self.inner.take()
        {
            // `keep` consumes the guard without removing anything, which is the
            // whole mechanism; dropping it instead would delete the directory.
            let _ = inner.keep();
        }
        // Otherwise `inner` drops here and removes the tree, ignoring errors.
        // Silence is deliberate: this runs while unwinding from a failed
        // assertion as often as from a passing test, and panicking in a drop
        // during a panic aborts the process, which would replace the assertion
        // message with a far less useful one.
    }
}

/// A uniquely named file under the system temp directory, removed when this
/// value is dropped.
///
/// For the tests that need a file rather than a tree — a payload to upload, a
/// certificate to point a config at.
#[derive(Debug)]
pub struct TempFile {
    inner: Option<tempfile::NamedTempFile>,
    path: PathBuf,
}

impl TempFile {
    /// Creates a file named `<prefix><random>` holding `contents`.
    pub fn with_contents(prefix: &str, contents: &[u8]) -> Self {
        use std::io::Write as _;

        let mut inner = tempfile::Builder::new()
            .prefix(prefix)
            .tempfile()
            .unwrap_or_else(|error| panic!("Failed to create a temp file: {error}"));
        // Written through the handle that is already open. Opening the same path
        // a second time would depend on the share mode of the first open, which
        // is a Windows question worth not having.
        inner
            .as_file_mut()
            .write_all(contents)
            .unwrap_or_else(|error| panic!("Failed to write {:?}: {error}", inner.path()));
        let path = canonical_without_verbatim(inner.path());
        Self {
            inner: Some(inner),
            path,
        }
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if keep_test_data()
            && let Some(inner) = self.inner.take()
        {
            // Errors are ignored: this drop runs while unwinding from a failed
            // assertion as often as from a passing test, and panicking there
            // aborts the process, which would replace the assertion message
            // with a far less useful one.
            let _ = inner.keep();
        }
    }
}

/// The identity of the file at `path`: its device and inode on Unix, its volume serial number and
/// file index on Windows. A file written in place keeps its identity, and a file renamed over
/// `path` brings its own.
pub fn file_identity(path: &Path) -> (u64, u64) {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::metadata(path)
            .unwrap_or_else(|error| panic!("Failed to stat {path:?}: {error}"));
        (metadata.dev(), metadata.ino())
    }
    #[cfg(target_family = "windows")]
    {
        use std::os::windows::io::AsRawHandle as _;

        use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

        let file = std::fs::File::open(path)
            .unwrap_or_else(|error| panic!("Failed to open {path:?}: {error}"));
        // SAFETY: all zeroes is a valid `BY_HANDLE_FILE_INFORMATION`, which the call writes through
        // a handle open for the call.
        let (queried, info) = unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            let queried = GetFileInformationByHandle(file.as_raw_handle(), &raw mut info);
            (queried, info)
        };
        assert_ne!(
            queried,
            0,
            "Failed to query {path:?}: {}",
            std::io::Error::last_os_error()
        );
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }
}
