// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The files, symlinks and directories under a writable provider's `write/` directory, read
//! once at mount. Each directory's entries are held together, sorted by the hash of their
//! folded names, one entry per folded name, so a path resolves by one binary search per
//! component. Names are copied from the directory stream into one buffer, and each directory
//! is opened relative to its parent's descriptor, so the scan allocates only as its buffers
//! grow.

use std::ffi::CStr;
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::NonNull;

use crate::hash::hash_string;
use crate::lore_warn;
use crate::node::validate_node_name_for_store;

/// What an entry under `write/` is.
#[lore_macro::test_pub]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalKind {
    File,
    Dir,
    Symlink,
}

/// One entry under `write/`.
#[lore_macro::test_pub]
#[derive(Clone, Debug)]
pub(crate) struct LocalEntry {
    /// The name's range in the tree's name buffer, which holds a NUL after each name.
    name: Range<u32>,
    /// The hash of the folded name, as a node's `name_hash`.
    pub(crate) name_hash: u64,
    pub(crate) kind: LocalKind,
    /// The file's length or the symlink target's; zero for a directory.
    pub(crate) size: u64,
    /// Whether a file carries the owner's execute bit.
    pub(crate) executable: bool,
    /// A directory's entries in the tree; `None` for a directory that could not be read, and
    /// for a file or symlink.
    children: Option<Range<u32>>,
}

/// What `write/` holds.
#[lore_macro::test_pub]
#[derive(Default)]
pub(crate) struct LocalTree {
    names: String,
    entries: Vec<LocalEntry>,
    top: Range<u32>,
}

impl LocalTree {
    /// Reads the tree under `root`, empty where `root` does not exist. Skips, with a warning,
    /// an entry whose name is not UTF-8 or no node can store, an entry whose name folds to that
    /// of an entry before
    /// it in byte order, and a directory the process may not read, with what it holds. Skips
    /// entries that are neither files, directories nor symlinks. Blocking.
    #[lore_macro::test_pub]
    pub(crate) fn scan(root: &Path) -> io::Result<Self> {
        let mut tree = Self::default();
        let root = CString::new(root.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut top = match HostDir::open(&root) {
            Ok(top) => top,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(tree),
            Err(error) => return Err(error),
        };
        tree.top = tree.read_dir(&mut top)?;
        let mut open = vec![(top, tree.top.clone())];
        while let Some((dir, pending)) = open.last_mut() {
            let Some(index) = pending.find(|&index| tree.entry(index).kind == LocalKind::Dir)
            else {
                open.pop();
                continue;
            };
            let mut below = match dir.open_dir(tree.name_cstr(index)) {
                Ok(below) => below,
                Err(error) if skippable(&error) => {
                    lore_warn!(
                        "FUSE rebuild: skipping the directory {} under the write directory: {error}",
                        tree.name(tree.entry(index))
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            let children = tree.read_dir(&mut below)?;
            tree.entries[index as usize].children = Some(children.clone());
            open.push((below, children));
        }
        Ok(tree)
    }

    /// The entries at the top of `write/`.
    #[lore_macro::test_pub]
    pub(crate) fn top(&self) -> &[LocalEntry] {
        self.slice(&self.top)
    }

    /// The entries of the directory `entry`; none for a file, a symlink or a directory that
    /// could not be read.
    pub(crate) fn children(&self, entry: &LocalEntry) -> &[LocalEntry] {
        entry
            .children
            .as_ref()
            .map_or(&[], |children| self.slice(children))
    }

    /// Whether the scan read `entry`: false for a directory it could not read.
    pub(crate) fn is_read(entry: &LocalEntry) -> bool {
        entry.kind != LocalKind::Dir || entry.children.is_some()
    }

    /// The entry at repository-relative `path`, matched by folded name; `None` where the tree
    /// holds none there.
    pub(crate) fn find(&self, path: &str) -> Option<&LocalEntry> {
        let mut entries = self.top();
        let mut found = None;
        for name in path.split('/') {
            let hash = hash_string(name);
            let index = entries
                .binary_search_by_key(&hash, |entry| entry.name_hash)
                .ok()?;
            let entry = &entries[index];
            entries = self.children(entry);
            found = Some(entry);
        }
        found.filter(|entry| Self::is_read(entry))
    }

    /// The name of `entry`, which belongs to this tree.
    #[lore_macro::test_pub]
    pub(crate) fn name(&self, entry: &LocalEntry) -> &str {
        &self.names[entry.name.start as usize..entry.name.end as usize]
    }

    fn entry(&self, index: u32) -> &LocalEntry {
        &self.entries[index as usize]
    }

    fn slice(&self, range: &Range<u32>) -> &[LocalEntry] {
        &self.entries[range.start as usize..range.end as usize]
    }

    fn name_cstr(&self, index: u32) -> &CStr {
        let name = &self.entry(index).name;
        CStr::from_bytes_with_nul(&self.names.as_bytes()[name.start as usize..=name.end as usize])
            .expect("each name in the buffer is followed by its NUL")
    }

    /// Records the entries of `dir`, sorted by folded name with the first of each kept, and
    /// answers their range.
    fn read_dir(&mut self, dir: &mut HostDir) -> io::Result<Range<u32>> {
        let start = self.entries.len();
        while let Some(entry) = dir.next()? {
            let Ok(name) = entry.name.to_str() else {
                lore_warn!(
                    "FUSE rebuild: skipping {:?} under the write directory: not UTF-8",
                    entry.name
                );
                continue;
            };
            if let Err(error) = validate_node_name_for_store(name) {
                lore_warn!("FUSE rebuild: skipping {name:?} under the write directory: {error}");
                continue;
            }
            let stat = match entry.stat() {
                Ok(stat) => stat,
                Err(error) if skippable(&error) => {
                    lore_warn!("FUSE rebuild: skipping {name} under the write directory: {error}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            let kind = match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => LocalKind::File,
                libc::S_IFDIR => LocalKind::Dir,
                libc::S_IFLNK => LocalKind::Symlink,
                _ => continue,
            };
            let size = match kind {
                LocalKind::Dir => 0,
                LocalKind::File | LocalKind::Symlink => {
                    u64::try_from(stat.st_size).unwrap_or_default()
                }
            };
            let name_start = offset(self.names.len())?;
            self.names.push_str(name);
            let name_end = offset(self.names.len())?;
            self.names.push('\0');
            self.entries.push(LocalEntry {
                name: name_start..name_end,
                name_hash: hash_string(name),
                kind,
                size,
                executable: kind == LocalKind::File && stat.st_mode & libc::S_IXUSR != 0,
                children: None,
            });
        }
        let names = self.names.as_bytes();
        let span = |entry: &LocalEntry| &names[entry.name.start as usize..entry.name.end as usize];
        self.entries[start..].sort_unstable_by(|a, b| {
            a.name_hash
                .cmp(&b.name_hash)
                .then_with(|| span(a).cmp(span(b)))
        });
        let mut kept = start;
        for index in start..self.entries.len() {
            if kept > start && self.entries[kept - 1].name_hash == self.entries[index].name_hash {
                lore_warn!(
                    "FUSE rebuild: skipping {} under the write directory: its name folds to that of {}",
                    self.name(&self.entries[index]),
                    self.name(&self.entries[kept - 1])
                );
                continue;
            }
            self.entries.swap(kept, index);
            kept += 1;
        }
        self.entries.truncate(kept);
        Ok(offset(start)?..offset(kept)?)
    }
}

/// Whether the scan skips an entry `error` is about rather than failing: one gone since it was
/// listed, or one the process may not read.
fn skippable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    )
}

/// `value` as a 32-bit offset; a write directory holding 4 GiB of names or 4 Gi entries is
/// refused.
fn offset(value: usize) -> io::Result<u32> {
    u32::try_from(value).map_err(|error| io::Error::new(io::ErrorKind::FileTooLarge, error))
}

/// An open directory stream, closed on drop.
struct HostDir(NonNull<libc::DIR>);

impl HostDir {
    fn open(path: &CStr) -> io::Result<Self> {
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        Self::from_fd(fd)
    }

    /// The stream over `fd`, an open directory descriptor or the `-1` of a failed open, which
    /// it takes ownership of.
    fn from_fd(fd: libc::c_int) -> io::Result<Self> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is an open descriptor this call owns.
        if let Some(dir) = NonNull::new(unsafe { libc::fdopendir(fd) }) {
            return Ok(Self(dir));
        }
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `fd` is still this call's to close.
        unsafe { libc::close(fd) };
        Err(error)
    }

    fn fd(&self) -> libc::c_int {
        // SAFETY: the stream is open for the life of `self`.
        unsafe { libc::dirfd(self.0.as_ptr()) }
    }

    /// The directory `name` names in this one, never through a symlink.
    fn open_dir(&self, name: &CStr) -> io::Result<HostDir> {
        // SAFETY: `name` is NUL-terminated and the stream is open.
        let fd = unsafe {
            libc::openat(
                self.fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        HostDir::from_fd(fd)
    }

    /// The next entry other than `.` and `..`; `None` at the end.
    fn next(&mut self) -> io::Result<Option<HostEntry<'_>>> {
        loop {
            // SAFETY: `errno` is this thread's; `readdir` leaves it unchanged at the end.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: the stream is open. The entry stays valid until the next call on the
            // stream, which the borrow of `self` the result holds excludes.
            let entry = unsafe { libc::readdir(self.0.as_ptr()) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) => Ok(None),
                    _ => Err(error),
                };
            }
            // SAFETY: `d_name` is NUL-terminated within the entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name != c"." && name != c".." {
                return Ok(Some(HostEntry { dir: self, name }));
            }
        }
    }
}

impl Drop for HostDir {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed only here.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

/// One entry of a [`HostDir`], valid until the stream's next entry is read.
struct HostEntry<'a> {
    dir: &'a HostDir,
    name: &'a CStr,
}

impl HostEntry<'_> {
    /// The entry's own status, a symlink's rather than its target's.
    fn stat(&self) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` names an entry of the open directory `dir`, and `stat` has room for
        // what `fstatat` writes.
        let result = unsafe {
            libc::fstatat(
                self.dir.fd(),
                self.name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstatat` succeeded, so it filled `stat`.
        Ok(unsafe { stat.assume_init() })
    }
}
