// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Temporary directories and files for the tests.
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
        let inner = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .unwrap_or_else(|error| panic!("Failed to create a temp directory: {error}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_the_directory_on_drop() {
        let path = {
            let temp = TempDir::new("lore-test-util-drop-");
            std::fs::write(temp.child("file.txt"), b"contents").expect("write");
            temp.path().to_path_buf()
        };
        assert_eq!(
            path.exists(),
            keep_test_data(),
            "{path:?} should be gone once the TempDir is dropped, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
        );
    }

    #[test]
    fn removes_the_directory_while_unwinding() {
        // Removal has to happen on the failing path too, not just when a test
        // returns normally.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        let inner = std::sync::Arc::clone(&captured);
        let result = std::panic::catch_unwind(move || {
            let temp = TempDir::new("lore-test-util-panic-");
            *inner.lock().expect("lock") = temp.path().to_path_buf();
            panic!("as a failing assertion would");
        });
        assert!(result.is_err(), "the closure was supposed to panic");
        let path = captured.lock().expect("lock").clone();
        assert_eq!(
            path.exists(),
            keep_test_data(),
            "{path:?} should be gone after unwinding, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
        );
    }

    #[test]
    fn temp_file_holds_what_was_written() {
        let file = TempFile::with_contents("lore-test-util-file-", b"scratch content");
        assert_eq!(
            std::fs::read(file.path()).expect("read back"),
            b"scratch content",
            "the file must hold exactly what it was created with"
        );
    }

    #[test]
    fn temp_file_goes_on_drop() {
        let path = {
            let file = TempFile::with_contents("lore-test-util-file-drop-", b"x");
            file.path().to_path_buf()
        };
        assert_eq!(
            path.exists(),
            keep_test_data(),
            "{path:?} should be gone once the TempFile is dropped, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
        );
    }

    #[test]
    fn temp_file_goes_while_unwinding() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        let inner = std::sync::Arc::clone(&captured);
        let result = std::panic::catch_unwind(move || {
            let file = TempFile::with_contents("lore-test-util-file-panic-", b"x");
            *inner.lock().expect("lock") = file.path().to_path_buf();
            panic!("as a failing assertion would");
        });
        assert!(result.is_err(), "the closure was supposed to panic");
        let path = captured.lock().expect("lock").clone();
        assert_eq!(
            path.exists(),
            keep_test_data(),
            "{path:?} should be gone after unwinding, \
             unless {KEEP_TEST_DATA_VAR} asked for it to stay"
        );
    }

    /// The one test that covers the `keep` branch of both `Drop` impls.
    ///
    /// It needs a child process: `keep_test_data` caches its answer in a
    /// `OnceLock`, so the variable cannot be flipped part-way through a run. The
    /// child creates a guard and records where it put it; the parent checks that
    /// the path outlived the child, and removes it.
    #[test]
    fn keep_test_data_leaves_them_on_disk() {
        const RECORD_VAR: &str = "LORE_TEST_UTIL_RECORD_TO";
        const TEST_NAME: &str = "test_util::tests::keep_test_data_leaves_them_on_disk";

        if let Some(record_to) = std::env::var_os(RECORD_VAR) {
            let dir = TempDir::new("lore-test-util-kept-dir-");
            let file = TempFile::with_contents("lore-test-util-kept-file-", b"kept");
            let record = format!("{}\n{}", dir.path().display(), file.path().display());
            std::fs::write(record_to, record).expect("record the paths");
            return;
        }

        let record = TempDir::new("lore-test-util-record-");
        let record_to = record.child("paths");
        let status = std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(KEEP_TEST_DATA_VAR, "1")
            .env(RECORD_VAR, &record_to)
            .status()
            .expect("run the child");
        assert!(status.success(), "the child run should have passed");

        let recorded = std::fs::read_to_string(&record_to).expect("read the recorded paths");
        let mut lines = recorded.lines();
        let kept_dir = PathBuf::from(lines.next().expect("a directory path"));
        let kept_file = PathBuf::from(lines.next().expect("a file path"));

        assert!(
            kept_dir.is_dir(),
            "{kept_dir:?} should have survived the child, which had {KEEP_TEST_DATA_VAR} set"
        );
        assert!(
            kept_file.is_file(),
            "{kept_file:?} should have survived the child, which had {KEEP_TEST_DATA_VAR} set"
        );

        // Nothing owns these now: the child deliberately let them go.
        let _ = std::fs::remove_dir_all(&kept_dir);
        let _ = std::fs::remove_file(&kept_file);
    }

    #[test]
    fn names_do_not_collide() {
        let first = TempDir::new("lore-test-util-unique-");
        let second = TempDir::new("lore-test-util-unique-");
        assert_ne!(
            first.path(),
            second.path(),
            "two directories asked for with the same prefix must not collide"
        );
    }
}
