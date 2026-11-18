use std::alloc::Layout;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CStr;
use std::sync::Arc;

use bytes::Bytes;
use lore_error_set::WrapInternal;
use lore_storage::ContentSource;
use lore_storage::content::RangedRead;

use crate::fs::filesystem_provider::FileInfo;
use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::api_interface::SwfsInterfaceError;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_Deleted;
use crate::fs::swfs::api_interface::swfs_api::SWFSNodeModFlag_UpdateName;
use crate::fs::swfs::api_interface::swfs_api::swfs_u32;
use crate::fs::swfs::api_interface::swfs_api::swfs_u64;
use crate::fs::swfs::api_interface::swfs_api::swfs_ubyte;
use crate::fs::swfs::file::DIRECTORY_ATTRIBUTE;
use crate::fs::swfs::paths::SwfsPath;
use crate::util::path::RelativePath;

/// SWFS's buffers for reading a frozen file are aligned to, and hold a whole number of, blocks of
/// this size.
pub const FROZEN_READ_ALIGNMENT: usize = 1 << 16;

/// The range SWFS reads to cover `length` bytes from `offset`, as its start and length:
/// `swfsReadFrozenFile` takes an offset and a length that are whole blocks, so the range is
/// widened to the blocks it touches.
pub fn frozen_read_range(offset: u64, length: usize) -> (u64, usize) {
    let block = FROZEN_READ_ALIGNMENT as u64;
    let start = offset - offset % block;
    let end = (offset + length as u64).next_multiple_of(block);
    (start, (end - start) as usize)
}

/// A file in the frozen set, copied out of the buffer SWFS filled so it outlives the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenFile {
    /// The SWFS-side path, backslash separated.
    pub path: String,
    /// `SWFSNodeModFlag_*` OR'd together.
    pub mod_flags: swfs_u32,
    pub file_index: swfs_u32,
    /// As of the freeze.
    pub size: swfs_u64,
    pub time_create: swfs_u64,
    pub time_write: swfs_u64,
    pub is_directory: bool,
}

/// 100ns ticks from 1601, where SWFS's file times start, to the Unix epoch.
const UNIX_EPOCH_AS_FILE_TIME: swfs_u64 = 116_444_736_000_000_000;

impl FrozenFile {
    /// The file's name on disk, which may spell the revision's name in another case.
    pub fn name(&self) -> &str {
        self.path.rsplit('\\').next().unwrap_or(&self.path)
    }

    /// What the file was as of the freeze. SWFS holds no executable bit, so none is reported.
    pub fn file_info(&self) -> FileInfo {
        if self.is_directory {
            return FileInfo::Directory;
        }
        FileInfo::File {
            executable: None,
            size: self.size,
            // Milliseconds since the Unix epoch, as `FileInfo` holds them.
            mtime: self.time_write.saturating_sub(UNIX_EPOCH_AS_FILE_TIME) / 10_000,
        }
    }
}

/// Receives the file list of `swfsGetFrozenFileList` through its two-call `SWFSDataCBFn` protocol:
/// the first call (null `start_buff`) asks for a buffer of `num_bytes`, and the second carries that
/// buffer back filled with the entries, each an `SWFSFile` followed by its inline path.
#[derive(Default)]
pub struct FrozenListCollector {
    // `u64` elements give the buffer the alignment `SWFSFile` needs.
    buffer: Vec<u64>,
    files: Vec<FrozenFile>,
}

impl FrozenListCollector {
    /// One call of the `SWFSDataCBFn` protocol. Returns the buffer SWFS should fill, or null when
    /// there is nothing more to hand out. `out_num_buff_bytes` may be null, as it is on the second
    /// call.
    ///
    /// # Safety
    /// A non-null `start_buff` must be the head of a linked list of valid `SWFSFile`, each with a
    /// valid NUL-terminated `path`, as SWFS passes it. A non-null `out_num_buff_bytes` must be
    /// writable.
    pub unsafe fn on_callback(
        &mut self,
        start_buff: *mut swfs_ubyte,
        num_bytes: usize,
        out_num_buff_bytes: *mut usize,
    ) -> *mut swfs_ubyte {
        if start_buff.is_null() {
            self.buffer = vec![0; num_bytes.div_ceil(size_of::<u64>())];
            if !out_num_buff_bytes.is_null() {
                unsafe { *out_num_buff_bytes = self.buffer.len() * size_of::<u64>() };
            }
            return if self.buffer.is_empty() {
                std::ptr::null_mut()
            } else {
                self.buffer.as_mut_ptr().cast()
            };
        }

        // Entries are packed one after another, so they are not aligned for `SWFSFile`.
        let mut file = start_buff.cast::<SWFSFile>().cast_const();
        while !file.is_null() {
            let entry = unsafe { file.read_unaligned() };
            self.files.push(FrozenFile {
                path: unsafe { CStr::from_ptr(entry.path) }
                    .to_string_lossy()
                    .into_owned(),
                mod_flags: entry.mod_flags,
                file_index: entry.file_index,
                size: entry.finfo.size,
                time_create: entry.finfo.time_create,
                time_write: entry.finfo.time_write,
                is_directory: entry.finfo.attrs & DIRECTORY_ATTRIBUTE != 0,
            });
            file = entry.next;
        }
        if !out_num_buff_bytes.is_null() {
            unsafe { *out_num_buff_bytes = 0 };
        }
        std::ptr::null_mut()
    }

    pub fn into_files(self) -> Vec<FrozenFile> {
        self.files
    }
}

/// The most a [`FrozenReadBuffer`] keeps allocated between reads: the windows the frozen
/// snapshot is read in, the chunker's and `copy_file`'s, `2 * FRAGMENT_SIZE_THRESHOLD`, and a
/// block at either end. A larger read, as of a whole file, is released once copied out.
pub const FROZEN_READ_RETAINED: usize =
    2 * lore_base::types::FRAGMENT_SIZE_THRESHOLD + 2 * FROZEN_READ_ALIGNMENT;

/// Receives the content of `swfsReadFrozenFile` through the same two-call protocol: the first
/// call asks for a buffer for the range read, which must be aligned and a whole number of blocks,
/// and the second carries it back filled.
///
/// One buffer serves read after read: the allocation is kept while it covers what a read asks
/// for, and grown, zeroed, only where it does not. Only the first `read` bytes SWFS reports it
/// filled, of this read's request, are ever copied out, so what an earlier read left in the
/// allocation is never exposed.
pub struct FrozenReadBuffer {
    buffer: *mut u8,
    capacity: usize,
    /// What the current read asked for, rounded up to whole blocks.
    requested: usize,
    filled: bool,
}

// The buffer is owned here alone and only touched through `&mut self`.
unsafe impl Send for FrozenReadBuffer {}

impl Default for FrozenReadBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FrozenReadBuffer {
    fn drop(&mut self) {
        self.release();
    }
}

impl FrozenReadBuffer {
    /// A buffer that holds no allocation yet.
    pub const fn new() -> Self {
        Self {
            buffer: std::ptr::null_mut(),
            capacity: 0,
            requested: 0,
            filled: false,
        }
    }

    /// Runs one frozen read: `read` makes it, given this buffer to publish to SWFS's callback, and
    /// answers the bytes SWFS reports it read; then fills `into` with the bytes from `skip`, as
    /// [`Self::copy_into`] answers, and releases a buffer larger than [`FROZEN_READ_RETAINED`].
    /// Nothing of an earlier read counts as filled, so a read SWFS answers without filling the
    /// buffer, as for a file with no bytes of its own, has no content.
    pub fn read(
        &mut self,
        read: impl FnOnce(*mut FrozenReadBuffer) -> usize,
        skip: usize,
        into: &mut [u8],
    ) -> Option<bool> {
        self.filled = false;
        self.requested = 0;
        let count = read(std::ptr::from_mut(self));
        let copied = self.copy_into(count, skip, into);
        self.retain_at_most(FROZEN_READ_RETAINED);
        copied
    }

    /// Releases the allocation where it is larger than `retained`, so the buffer kept between
    /// reads stays bounded.
    #[lore_macro::test_pub]
    fn retain_at_most(&mut self, retained: usize) {
        if self.capacity > retained {
            self.release();
        }
    }

    fn layout(capacity: usize) -> Layout {
        Layout::from_size_align(capacity, FROZEN_READ_ALIGNMENT)
            .expect("a block-multiple capacity with a power-of-two alignment")
    }

    fn release(&mut self) {
        if !self.buffer.is_null() {
            unsafe { std::alloc::dealloc(self.buffer, Self::layout(self.capacity)) };
            self.buffer = std::ptr::null_mut();
            self.capacity = 0;
        }
    }

    /// One call of the `SWFSDataCBFn` protocol. `out_num_buff_bytes` is null on the second call.
    ///
    /// # Safety
    /// A non-null `out_num_buff_bytes` must be writable.
    pub unsafe fn on_callback(
        &mut self,
        start_buff: *mut swfs_ubyte,
        num_bytes: usize,
        out_num_buff_bytes: *mut usize,
    ) -> *mut swfs_ubyte {
        if start_buff.is_null() {
            self.filled = false;
            // A file of no bytes still gets a block, so that null can mean no buffer.
            let requested = num_bytes.max(1).next_multiple_of(FROZEN_READ_ALIGNMENT);
            if self.buffer.is_null() || self.capacity < requested {
                self.release();
                self.buffer = unsafe { std::alloc::alloc_zeroed(Self::layout(requested)) };
                self.capacity = if self.buffer.is_null() { 0 } else { requested };
            }
            self.requested = if self.buffer.is_null() { 0 } else { requested };
            if !out_num_buff_bytes.is_null() {
                unsafe { *out_num_buff_bytes = self.requested };
            }
            return self.buffer;
        }
        self.filled = true;
        if !out_num_buff_bytes.is_null() {
            unsafe { *out_num_buff_bytes = 0 };
        }
        std::ptr::null_mut()
    }

    /// Fills `into` with the bytes from `skip` of the first `read` SWFS put in the buffer: `None`
    /// if it never filled one, and `Some(false)` if it read too few to fill `into`.
    #[lore_macro::test_pub]
    fn copy_into(&self, read: usize, skip: usize, into: &mut [u8]) -> Option<bool> {
        if !self.filled || self.buffer.is_null() {
            return None;
        }
        let available = read.min(self.requested);
        let Some(end) = skip.checked_add(into.len()).filter(|end| *end <= available) else {
            return Some(false);
        };
        let filled = unsafe { std::slice::from_raw_parts(self.buffer, available) };
        into.copy_from_slice(&filled[skip..end]);
        Some(true)
    }
}

/// Where the frozen files of a mount come from: SWFS itself, or a stand-in in a test.
pub trait FrozenReader: Send + Sync {
    /// The files frozen with the mount.
    fn files(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError>;

    /// Fills `into` with the content of the frozen file at the SWFS-side `path` from `offset`,
    /// answering `false` where SWFS has none to give. Content shorter than `into` is an error.
    fn read_range(
        &self,
        path: &str,
        offset: u64,
        into: &mut [u8],
    ) -> Result<bool, SwfsInterfaceError>;

    /// The files SWFS holds modified, deletions included, which name where a move came from. None
    /// where the reader cannot list them.
    fn modified(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        Ok(Vec::new())
    }
}

/// SWFS's own frozen files.
pub struct SwfsFrozenReader(pub Arc<SwfsInterface>);

impl FrozenReader for SwfsFrozenReader {
    fn files(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        self.0.frozen_files()
    }

    fn read_range(
        &self,
        path: &str,
        offset: u64,
        into: &mut [u8],
    ) -> Result<bool, SwfsInterfaceError> {
        self.0.read_frozen_range(path, offset, into)
    }

    fn modified(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        self.0.modified_files()
    }
}

struct NoFrozenFiles;

impl FrozenReader for NoFrozenFiles {
    fn files(&self) -> Result<Vec<FrozenFile>, SwfsInterfaceError> {
        Ok(Vec::new())
    }

    fn read_range(
        &self,
        _path: &str,
        _offset: u64,
        _into: &mut [u8],
    ) -> Result<bool, SwfsInterfaceError> {
        Ok(false)
    }
}

/// The frozen files of a mount as one operation sees them, listed once when the operation began.
///
/// What SWFS holds frozen is fixed at the freeze, so the list does not change under the operation,
/// and one lookup answers what a call into SWFS per read would.
pub struct FrozenSnapshot {
    /// By lowercase node path, since the files are matched with case folded.
    files: HashMap<String, FrozenFile>,
    /// The lowercase node paths of the files directly in each directory, by the directory's own.
    children: HashMap<String, Vec<String>>,
    /// Where each moved file came from, by the lowercase node path it moved to, as the modified
    /// list pairs them ([`moves_in`]).
    origins: HashMap<String, String>,
    /// The lowercase node paths the modified list names as renamed (`UpdateName`), deletions left
    /// out. The frozen list carries no such flag.
    renamed: HashSet<String>,
    reader: Arc<dyn FrozenReader>,
    /// Whether SWFS listed the frozen files, so that a path the snapshot lacks is one SWFS holds
    /// no change to.
    captured: bool,
}

impl FrozenSnapshot {
    /// Lists the frozen files through `reader`. Call once the freeze has taken hold.
    pub fn capture(reader: Arc<dyn FrozenReader>) -> Result<Self, SwfsInterfaceError> {
        let mut files = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for file in reader.files()? {
            let node_path = SwfsPath(&file.path)
                .node_path()
                .internal("Parsing a frozen file path")?;
            // SWFS lists the root too. Kept, it would be a child of itself with no name, which a
            // listing of the root then shows as an entry, and a walk as a file at the root's path.
            if node_path.0.is_empty() {
                continue;
            }
            let key = node_path.0.as_lowercase_str().to_owned();
            children
                .entry(node_path.0.parent_path().as_lowercase_str().to_owned())
                .or_default()
                .push(key.clone());
            files.insert(key, file);
        }
        let modified = reader.modified()?;
        let renamed = modified
            .iter()
            .filter(|file| {
                file.mod_flags & SWFSNodeModFlag_UpdateName != 0
                    && file.mod_flags & SWFSNodeModFlag_Deleted == 0
            })
            .filter_map(|file| {
                Some(
                    SwfsPath(&file.path)
                        .node_path()
                        .ok()?
                        .0
                        .as_lowercase_str()
                        .to_owned(),
                )
            })
            .collect();
        let origins = moves_in(&modified)
            .into_iter()
            .map(|(from, to)| (to.to_lowercase(), from))
            .collect();
        Ok(Self {
            files,
            children,
            reader,
            origins,
            renamed,
            captured: true,
        })
    }

    /// A snapshot of nothing, for an operation begun without a freeze, as in a test: every path
    /// then has no frozen answer.
    pub fn empty() -> Self {
        Self {
            files: HashMap::new(),
            children: HashMap::new(),
            reader: Arc::new(NoFrozenFiles),
            origins: HashMap::new(),
            renamed: HashSet::new(),
            captured: false,
        }
    }

    /// Whether SWFS listed the frozen files: a snapshot of nothing for want of a listing cannot
    /// say that a path it lacks holds no change.
    pub fn is_captured(&self) -> bool {
        self.captured
    }

    /// Where the file at `path` was moved from, as the node path of the deletion SWFS lists for the
    /// move.
    pub fn origin_of(&self, path: &RelativePath) -> Option<&str> {
        self.origins
            .get(path.as_lowercase_str())
            .map(String::as_str)
    }

    /// Whether the modified list names the node at `path`, a node path in any spelling, as renamed.
    pub fn lists_renamed(&self, path: &str) -> bool {
        self.renamed.contains(&path.to_lowercase())
    }

    /// Whether the modified list names `path`, or a directory above it, as renamed.
    pub fn renamed_at_or_above(&self, path: &RelativePath) -> bool {
        let mut at = path.clone();
        while !at.is_empty() {
            if self.renamed.contains(at.as_lowercase_str()) {
                return true;
            }
            at = at.parent_path();
        }
        false
    }

    /// The files, directories left out, SWFS froze at and below `path`.
    pub fn files_below(&self, path: &RelativePath) -> Vec<RelativePath> {
        self.files
            .values()
            .filter(|file| !file.is_directory)
            .filter_map(|file| {
                let node = SwfsPath(&file.path).node_path().ok()?.0;
                crate::fs::swfs::paths::at_or_below(&node, path).then_some(node)
            })
            .collect()
    }

    /// Where `path` was moved from, where a directory above it was: SWFS lists the moved directory
    /// under its new path and the files in it not at all, so the revision holds them at the same
    /// place below the directory's origin.
    pub fn moved_from(&self, path: &RelativePath) -> Option<RelativePath> {
        let mut ancestor = path.parent_path();
        while !ancestor.is_empty() {
            if let Some(origin) = self.origin_of(&ancestor) {
                let rest = path.as_str().get(ancestor.as_str().len() + 1..)?;
                let moved = if origin.is_empty() {
                    rest.to_owned()
                } else {
                    format!("{origin}/{rest}")
                };
                return RelativePath::new_from_initial_path(&moved).ok();
            }
            ancestor = ancestor.parent_path();
        }
        None
    }

    pub fn get(&self, path: &RelativePath) -> Option<&FrozenFile> {
        self.files.get(path.as_lowercase_str())
    }

    /// What SWFS froze, at most `limit` of it, each as its SWFS path and modification flags.
    pub fn sample(&self, limit: usize) -> Vec<(String, u32)> {
        self.files
            .values()
            .take(limit)
            .map(|file| (file.path.clone(), file.mod_flags))
            .collect()
    }

    /// How many files SWFS froze, directories left out: every file it holds changed.
    pub fn file_count(&self) -> usize {
        self.files
            .values()
            .filter(|file| !file.is_directory)
            .count()
    }

    /// The frozen files directly in the directory at `dir`.
    pub fn children_of(&self, dir: &RelativePath) -> impl Iterator<Item = &FrozenFile> {
        self.children
            .get(dir.as_lowercase_str())
            .into_iter()
            .flatten()
            .filter_map(|key| self.files.get(key))
    }

    /// The content of the frozen file at `path`, read from SWFS a range at a time as it is read,
    /// or `None` where it is not listed, is a directory, or SWFS has none to give.
    ///
    /// SWFS is read for no more of the file than each read asks for, so a file of any size costs
    /// no more memory than the window it is read in.
    pub fn content(
        &self,
        path: &RelativePath,
    ) -> Result<Option<ContentSource<'static>>, SwfsInterfaceError> {
        let Some(file) = self.get(path) else {
            return Ok(None);
        };
        if file.is_directory {
            return Ok(None);
        }
        // A file of no bytes has nothing to read and SWFS gives it no buffer.
        if file.size == 0 {
            return Ok(Some(ContentSource::memory(Bytes::new())));
        }
        // A file changed only in name or attributes has no bytes of its own for SWFS to give, which
        // a read of its first byte tells.
        if !self.reader.read_range(&file.path, 0, &mut [0])? {
            return Ok(None);
        }
        let range = FrozenRange {
            reader: self.reader.clone(),
            path: file.path.clone(),
        };
        Ok(Some(ContentSource::ranged(Arc::new(range), file.size)))
    }
}

/// The moves a modified list names, as (origin, destination) node paths: a move lists the node under
/// its new path with `UpdateName`, and its origin as a deletion with the same `file_index`.
pub fn moves_in(listed: &[FrozenFile]) -> Vec<(String, String)> {
    let mut origins: HashMap<swfs_u32, String> = HashMap::new();
    for file in listed {
        if file.mod_flags & SWFSNodeModFlag_Deleted != 0
            && file.file_index != 0
            && let Ok(path) = SwfsPath(&file.path).node_path()
        {
            origins.insert(file.file_index, path.0.as_str().to_owned());
        }
    }
    let mut moves = Vec::new();
    for file in listed {
        if file.mod_flags & SWFSNodeModFlag_Deleted == 0
            && file.mod_flags & SWFSNodeModFlag_UpdateName != 0
            && let Some(from) = origins.get(&file.file_index)
            && let Ok(to) = SwfsPath(&file.path).node_path()
        {
            moves.push((from.clone(), to.0.as_str().to_owned()));
        }
    }
    moves
}

/// The content of one frozen file, which SWFS is read for a range at a time.
struct FrozenRange {
    reader: Arc<dyn FrozenReader>,
    /// The SWFS-side path.
    path: String,
}

impl RangedRead for FrozenRange {
    fn read_range<'a>(
        &'a self,
        offset: u64,
        into: &'a mut [u8],
    ) -> futures::future::BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            match self.reader.read_range(&self.path, offset, into) {
                Ok(true) => Ok(()),
                Ok(false) => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("SWFS holds no frozen content for {}", self.path),
                )),
                Err(error) => Err(std::io::Error::other(error.to_string())),
            }
        })
    }
}

#[cfg(feature = "test-util")]
pub mod test_util {
    use super::FrozenReadBuffer;

    impl FrozenReadBuffer {
        /// How much the buffer holds allocated.
        pub fn capacity(&self) -> usize {
            self.capacity
        }
    }
}
