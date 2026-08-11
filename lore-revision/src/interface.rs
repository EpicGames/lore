// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::borrow::Cow;
use std::fmt::Debug;
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::AtomicBool;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use lore_base::runtime::runtime_shutdown_timeout;
use lore_base::text::TextNotUtf8;
use lore_base::text::ValidateText;
use lore_base::types::BranchPoint;
pub use lore_credential::user_info;
pub use lore_transport::drop_connections;
use serde::Deserialize;
use serde::Serialize;
use serde::de;
use serde::ser::SerializeSeq;
use tokio::sync::Mutex;
use zerocopy::IntoBytes;

use crate::change::FileAction;
use crate::event::LoreBytes;
pub use crate::event::LoreEvent;
pub use crate::logging::LoreLogLevel;
use crate::lore::Address;
use crate::lore::BranchId;
use crate::lore::Context;
use crate::lore::Hash;
use crate::relay::EventDispatcher;
use crate::revision::ResolveSearchLocation;
use crate::util::path::RelativePath;
use crate::util::serde::u8_as_bool;

/// A block of raw bytes described by a pointer and a length.
///
/// Owns its payload: [`Self::from_bytes`] copies into a fresh allocation and
/// `Drop` frees it, so a value carried in an event stays valid without the
/// producer having to outlive the dispatch. An empty block is a NULL pointer
/// with length 0.
#[repr(C)]
#[derive(Debug)]
pub struct LoreBinary {
    /// Pointer to the start of the byte block.
    pub payload: *const std::ffi::c_void,
    /// Number of bytes in the block.
    pub length: usize,
}

unsafe impl Send for LoreBinary {}
unsafe impl Sync for LoreBinary {}

impl LoreBinary {
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn as_bytes(&self) -> &[u8] {
        if self.is_empty() {
            &[]
        } else {
            // SAFETY: a non-empty block points at `length` initialized bytes owned by
            // this value, allocated in `from_bytes` and freed in `Drop`.
            unsafe { std::slice::from_raw_parts(self.payload.cast::<u8>(), self.length) }
        }
    }

    /// Build an owning `LoreBinary` from raw bytes, copied into a freshly
    /// allocated buffer that `Drop` frees with the matching layout.
    pub fn from_bytes(source: &[u8]) -> Self {
        if source.is_empty() {
            return Self::default();
        }
        // SAFETY: the layout is non-zero-sized and matches the one `free` uses;
        // the copy fills exactly the bytes just allocated.
        unsafe {
            let length = source.len();
            let layout = std::alloc::Layout::from_size_align_unchecked(length, 1);
            let buffer = std::alloc::alloc(layout);
            std::ptr::copy_nonoverlapping(source.as_ptr(), buffer, length);
            LoreBinary {
                payload: buffer.cast::<std::ffi::c_void>(),
                length,
            }
        }
    }

    fn free(&mut self) {
        if !self.payload.is_null() && self.length > 0 {
            // SAFETY: the layout matches the one `from_bytes` allocated with.
            unsafe {
                let layout = std::alloc::Layout::from_size_align_unchecked(self.length, 1);
                std::alloc::dealloc(self.payload as *mut u8, layout);
            }
        }
        self.payload = std::ptr::null();
        self.length = 0;
    }
}

impl Default for LoreBinary {
    fn default() -> Self {
        LoreBinary {
            payload: std::ptr::null(),
            length: 0,
        }
    }
}

impl Clone for LoreBinary {
    fn clone(&self) -> Self {
        Self::from_bytes(self.as_bytes())
    }
}

impl Drop for LoreBinary {
    fn drop(&mut self) {
        self.free();
    }
}

impl PartialEq for LoreBinary {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

/// Base64 text for a self-describing format, raw bytes for the rest.
///
/// A format that writes bytes as text has to be told which text: JSON would
/// otherwise render a block as one number per byte, which costs about four
/// characters for each byte carried. The split is the same one
/// [`lore_base::types::serialize_hex`] makes for the identifiers, in base64
/// rather than hex because a block has no length bound to keep it short.
///
/// The text path allocates about a third again the payload and cannot stream:
/// `serialize_str` wants one contiguous `&str`, so the encoding has to be
/// complete before it is handed over. Only JSON pays it.
impl Serialize for LoreBinary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&BASE64.encode(self.as_bytes()))
        } else {
            serializer.serialize_bytes(self.as_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for LoreBinary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let text = String::deserialize(deserializer)?;
            let value = BASE64.decode(text.as_bytes()).map_err(de::Error::custom)?;
            Ok(LoreBinary::from_bytes(&value))
        } else {
            let value: Vec<u8> = serde_bytes::deserialize(deserializer)?;
            Ok(LoreBinary::from_bytes(&value))
        }
    }
}

/// A string described by a pointer to its character data and a length, holding
/// text as a sequence of bytes.
///
/// The text is UTF-8 by convention, but the bytes are never validated on
/// construction: a string carrying any other encoding is accepted here and
/// rejected by whichever verb needs to read it as text. The length field counts
/// the bytes before the trailing NUL. An empty string is a NULL pointer with
/// length 0, and a length of 0 means the string is empty.
#[repr(C)]
pub struct LoreString {
    /// Pointer to the start of the character data.
    pub string: *const std::ffi::c_char,
    /// Number of bytes in the string, not counting any trailing terminator.
    pub length: usize,
}

unsafe impl Send for LoreString {}
unsafe impl Sync for LoreString {}

impl std::fmt::Debug for LoreString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", String::from_utf8_lossy(self.as_bytes())))
    }
}

impl LoreString {
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn len(&self) -> usize {
        self.length
    }

    /// The text as `&str`, assuming it is valid UTF-8.
    ///
    /// Sound for arguments reaching a command handler, because the entry point
    /// checks every text field a call carries before dispatching it. Not sound
    /// for a string built by [`Self::from_bytes`] outside that path, which
    /// accepts any byte sequence — read those through [`Self::as_bytes`].
    pub fn as_str(&self) -> &str {
        if !self.is_empty() {
            unsafe {
                std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                    self.string.cast::<u8>(),
                    self.length,
                ))
            }
        } else {
            ""
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        if self.is_empty() {
            &[]
        } else {
            // SAFETY: a non-empty string points to `length` initialized bytes that outlive this
            // borrow, per the FFI contract and the `from_bytes` / `from_str` constructors.
            unsafe { std::slice::from_raw_parts(self.string.cast::<u8>(), self.length) }
        }
    }

    pub fn from_path(source: impl AsRef<Path>) -> Self {
        let source = source.as_ref().display().to_string();
        Self::from_str(source.as_str())
    }

    /// Build an owning `LoreString` from raw bytes, copied into a freshly
    /// allocated NUL-terminated buffer. The bytes need not be valid UTF-8;
    /// `Drop` frees the buffer with the matching layout.
    pub fn from_bytes(source: &[u8]) -> Self {
        unsafe {
            let length = source.len();
            let layout = std::alloc::Layout::from_size_align_unchecked(length + 1, 1);
            let buffer = std::alloc::alloc(layout);
            std::ptr::copy_nonoverlapping(source.as_ptr(), buffer, length);
            *buffer.add(length) = 0;
            LoreString {
                string: buffer as *const std::os::raw::c_char,
                length,
            }
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(source: &str) -> Self {
        Self::from_bytes(source.as_bytes())
    }

    fn free(&mut self) {
        if !self.string.is_null() {
            unsafe {
                let layout = std::alloc::Layout::from_size_align_unchecked(self.length + 1, 1);
                std::alloc::dealloc(self.string as *mut u8, layout);
            }
            self.string = std::ptr::null();
        }
        self.length = 0;
    }
}

impl Default for LoreString {
    fn default() -> Self {
        LoreString {
            string: core::ptr::null(),
            length: 0usize,
        }
    }
}

impl Clone for LoreString {
    /// Copies the raw bytes, like [`Self::clone_from`]. Cloning must not read
    /// the text as `&str`: every call clones its arguments before anything has
    /// checked them, so this runs on whatever the caller passed in.
    fn clone(&self) -> Self {
        Self::from_bytes(self.as_bytes())
    }

    fn clone_from(&mut self, source: &Self) {
        self.free();

        unsafe {
            let length = source.len();
            let layout = std::alloc::Layout::from_size_align_unchecked(length + 1, 1);
            let buffer = std::alloc::alloc(layout);
            std::ptr::copy_nonoverlapping(source.string.cast::<u8>(), buffer, length);
            *buffer.add(length) = 0;
            self.string = buffer as *const std::os::raw::c_char;
            self.length = length;
        }
    }
}

impl PartialEq for LoreString {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Display for LoreString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(self.as_bytes()))
    }
}

impl Drop for LoreString {
    fn drop(&mut self) {
        self.free();
    }
}

impl AsRef<str> for LoreString {
    /// Carries [`LoreString::as_str`]'s assumption without showing it at the
    /// call site.
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<LoreString> for Option<String> {
    fn from(value: LoreString) -> Self {
        if !value.is_empty() {
            Some(value.to_string())
        } else {
            None
        }
    }
}

impl From<&LoreString> for Option<String> {
    fn from(value: &LoreString) -> Self {
        if !value.is_empty() {
            Some(value.to_string())
        } else {
            None
        }
    }
}

impl<'a> From<&'a LoreString> for Option<&'a str> {
    /// Carries [`LoreString::as_str`]'s assumption without showing it at the
    /// call site.
    fn from(value: &'a LoreString) -> Self {
        if !value.is_empty() {
            Some(value.as_str())
        } else {
            None
        }
    }
}

impl From<String> for LoreString {
    fn from(value: String) -> Self {
        LoreString::from_str(value.as_str())
    }
}

impl From<&String> for LoreString {
    fn from(value: &String) -> Self {
        LoreString::from_str(value.as_str())
    }
}

impl From<&str> for LoreString {
    fn from(value: &str) -> Self {
        LoreString::from_str(value)
    }
}

impl From<Option<String>> for LoreString {
    fn from(value: Option<String>) -> Self {
        value
            .as_deref()
            .map(LoreString::from_str)
            .unwrap_or_default()
    }
}

impl From<Option<&String>> for LoreString {
    fn from(value: Option<&String>) -> Self {
        value
            .map(|value| LoreString::from_str(value.as_str()))
            .unwrap_or_default()
    }
}

impl From<&Option<String>> for LoreString {
    fn from(value: &Option<String>) -> Self {
        value
            .as_deref()
            .map(LoreString::from_str)
            .unwrap_or_default()
    }
}

impl From<Option<&str>> for LoreString {
    fn from(value: Option<&str>) -> Self {
        value.map(LoreString::from_str).unwrap_or_default()
    }
}

impl From<&Path> for LoreString {
    fn from(value: &Path) -> Self {
        LoreString::from_path(value)
    }
}

impl From<PathBuf> for LoreString {
    fn from(value: PathBuf) -> Self {
        LoreString::from_path(value.as_path())
    }
}

impl From<&PathBuf> for LoreString {
    fn from(value: &PathBuf) -> Self {
        LoreString::from_path(value.as_path())
    }
}

impl From<&RelativePath> for LoreString {
    fn from(value: &RelativePath) -> Self {
        LoreString::from_str(value.as_str())
    }
}

impl From<RelativePath> for LoreString {
    fn from(value: RelativePath) -> Self {
        LoreString::from_str(value.as_str())
    }
}

/// Serializes as a string, failing on bytes that are not UTF-8.
///
/// Serialization is how a command reaches the Lore service, so substituting
/// replacement characters here would let the service accept text the in-process
/// path rejects, storing a mangled name instead of reporting a bad argument.
/// Failing keeps both paths refusing the same input.
impl Serialize for LoreString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let text = std::str::from_utf8(self.as_bytes()).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(text)
    }
}

impl<'de> Deserialize<'de> for LoreString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: String = Deserialize::deserialize(deserializer)?;
        Ok(LoreString::from_str(&value))
    }
}

impl ValidateText for LoreString {
    fn validate_text(&self) -> Result<(), TextNotUtf8> {
        match std::str::from_utf8(self.as_bytes()) {
            Ok(_) => Ok(()),
            Err(_) => Err(TextNotUtf8::here()),
        }
    }
}

impl<T: ValidateText> ValidateText for LoreArray<T> {
    fn validate_text(&self) -> Result<(), TextNotUtf8> {
        for (index, item) in self.as_slice().iter().enumerate() {
            if let Err(error) = item.validate_text() {
                return Err(error.at(index));
            }
        }
        Ok(())
    }
}

lore_base::carries_no_text!(LoreBinary, LoreBytes, LoreMetadataType);

impl ValidateText for LoreGlobalArgs {
    fn validate_text(&self) -> Result<(), TextNotUtf8> {
        self.repository_path
            .validate_text()
            .map_err(|error| error.inside("repository_path"))
            .and_then(|()| {
                self.working_directory
                    .validate_text()
                    .map_err(|error| error.inside("working_directory"))
            })
            .and_then(|()| {
                self.correlation_id
                    .validate_text()
                    .map_err(|error| error.inside("correlation_id"))
            })
            .and_then(|()| {
                self.identity
                    .validate_text()
                    .map_err(|error| error.inside("identity"))
            })
    }
}

/// A contiguous array of elements described by a pointer and a count.
/// Holds zero or more values of the element type laid out one after another.
#[repr(C)]
#[derive(PartialEq)]
pub struct LoreArray<T> {
    /// Pointer to the first element.
    ptr: *const T,
    /// Number of elements in the array.
    count: usize,
}

unsafe impl<T: Send> Send for LoreArray<T> {}
unsafe impl<T: Sync> Sync for LoreArray<T> {}

impl<T> Default for LoreArray<T> {
    fn default() -> Self {
        Self {
            ptr: std::ptr::null(),
            count: 0,
        }
    }
}

impl<T> Debug for LoreArray<T>
where
    T: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:?}", self.as_slice()))
    }
}

impl<T> LoreArray<T> {
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: ptr is always valid, either from a clone, or from `from_vec`
        unsafe {
            if !self.ptr.is_null() && self.count > 0 {
                std::slice::from_raw_parts(self.ptr, self.count)
            } else {
                &[]
            }
        }
    }

    /// Moves the strings from the vec in the string array
    pub fn from_vec(vec: Vec<T>) -> Self {
        let target = LoreArray::<T>::new(vec.len());

        // SAFETY: target is created to the same count as the vec and we're going to initialise
        // every element.
        unsafe {
            let to_slice = std::slice::from_raw_parts_mut(target.ptr.cast_mut(), target.count);

            for (from, to) in vec.into_iter().zip(to_slice.iter_mut()) {
                // Needed to ensure drop is not called on *to, which is uninitialised right now
                std::ptr::write(to, from);
            }
        }

        target
    }

    pub fn is_empty(&self) -> bool {
        self.ptr.is_null() || self.count == 0
    }

    pub fn len(&self) -> usize {
        self.count
    }

    fn new(count: usize) -> Self {
        let layout =
            std::alloc::Layout::array::<T>(count).expect("layout overflow in LoreArray<T>::new");
        unsafe {
            let ptr = std::alloc::alloc(layout).cast::<T>();
            if ptr.is_null() {
                panic!("unable to alloc for LoreArray<T>::new");
            }

            Self { ptr, count }
        }
    }
}

impl<T: Clone> Clone for LoreArray<T> {
    fn clone(&self) -> Self {
        if self.is_empty() {
            return Self {
                ptr: std::ptr::null(),
                count: 0,
            };
        }
        unsafe {
            let mut clone = Self::new(self.count);

            // Deep clone the contained strings
            let from_slice = std::slice::from_raw_parts(self.ptr, self.count);
            let to_slice = std::slice::from_raw_parts_mut(clone.ptr.cast_mut(), self.count);

            for (from, to) in from_slice.iter().zip(to_slice.iter_mut()) {
                // Needed to ensure drop is not called on *to, which is uninitialised right now
                std::ptr::write(to, from.clone());
            }

            clone.count = self.count;

            clone
        }
    }
}

impl<T> Drop for LoreArray<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.count > 0 {
            unsafe {
                let items = std::ptr::slice_from_raw_parts_mut(self.ptr.cast_mut(), self.count);
                std::ptr::drop_in_place(items);
                let layout = std::alloc::Layout::array::<T>(self.count)
                    .expect("layout overflow in LoreArray<T>::drop");
                std::alloc::dealloc(self.ptr as *mut u8, layout);
            }
            self.ptr = std::ptr::null();
            self.count = 0;
        }
    }
}

impl<T> Serialize for LoreArray<T>
where
    T: serde::Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for value in self.as_slice() {
            seq.serialize_element(value)?;
        }
        seq.end()
    }
}

impl<'de, T> Deserialize<'de> for LoreArray<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: Vec<T> = Deserialize::deserialize(deserializer)?;
        Ok(LoreArray::from_vec(value))
    }
}

/// Selects which configuration sources are loaded. The values are flags
/// that can be combined.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Default)]
pub enum LoreLoadConfig {
    /// Load no configuration from any source.
    Disable = 0,
    /// Load configuration from the repository.
    Repository = 1,
    /// Load configuration from the user's home location.
    Home = 2,
    /// Load configuration from the environment.
    Environment = 4,
    /// Load configuration from all sources.
    #[default]
    Default = 7,
}

/// Common options shared by repository operations.
#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoreGlobalArgs {
    /// Repository path
    pub repository_path: LoreString,
    /// Directory that relative paths in this call are resolved against. Set it
    /// when a call may be executed by another process, such as the Lore
    /// service, whose own working directory is unrelated to the caller's. When
    /// empty, relative paths resolve against the working directory of the
    /// process performing the call.
    pub working_directory: LoreString,
    /// Correlation ID
    pub correlation_id: LoreString,
    /// Identity to use
    pub identity: LoreString,
    /// Force the operation if possible
    pub force: u8,
    /// Run operation without connecting to server
    pub offline: u8,
    /// Use only local data
    pub local: u8,
    /// Use only remote data
    pub remote: u8,
    /// Dry run mode, only report what would have been changed and perform no changes to local file system
    pub dry_run: u8,
    /// Avoid recording last access timestamps in the data stores
    pub no_atime: u8,
    /// Maximum number of parallel connections for bulk data transfer
    pub max_connections: u32,
    /// Search limit when iterating revisions
    pub search_limit: u32,
    /// Allow matching to the nearest matching revision when a perfect match is not available
    pub search_nearest: u8,
    /// Prevent the automatic incremental/step GC for this operation; it otherwise runs in the background on write operations. `repository gc` always runs a full pass regardless
    pub no_gc: u8,
    /// Use in-memory stores instead of file-backed stores. No store data is
    /// read from or written to the .urc/immutable/ and .urc/mutable/ directories.
    pub in_memory: u8,
    /// Maximum number of files being processed in parallel
    pub file_count_limit: u64,
    /// Maximum total size of all files being processed in parallel
    pub file_size_limit: u64,
    /// Maximum number of parallel compression tasks
    pub compress_task_limit: u64,
    /// Keep store references alive after a repository call completes to avoid
    /// repeated store open/close cycles for consecutive API calls in the same process.
    pub store_keep_alive: u8,
    /// Duration in seconds to keep store references alive. Only used when
    /// `store_keep_alive` is set. 0 means use the default (10 seconds).
    pub store_keep_alive_seconds: u64,
    /// Force sync data to storage media during store flush
    pub sync_data: u8,
    /// Cache fragment payloads fetched from remote in the local store. Without
    /// this only state fragments and fragments flagged for local cache priority
    /// are retained
    pub cache: u8,
}

impl LoreGlobalArgs {
    pub fn repository_path(&self) -> &str {
        self.repository_path.as_str()
    }

    pub fn working_directory(&self) -> Option<&str> {
        (&self.working_directory).into()
    }

    pub fn identity(&self) -> Option<&str> {
        (&self.identity).into()
    }

    pub fn force(&self) -> bool {
        self.force != 0
    }

    pub fn offline(&self) -> bool {
        self.offline != 0
    }

    pub fn set_offline(self) -> Self {
        let mut globals = self;
        globals.offline = 1;
        globals
    }

    pub fn local(&self) -> bool {
        self.local != 0
    }

    /// True when the operation should avoid the server: either explicit
    /// offline mode or local-only data mode.
    pub fn offline_or_local(&self) -> bool {
        self.offline() || self.local()
    }

    pub fn remote(&self) -> bool {
        self.remote != 0
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run != 0
    }

    pub fn atime(&self) -> bool {
        self.no_atime == 0
    }

    pub fn search_limit(&self) -> Option<usize> {
        if self.search_limit > 0 {
            Some(self.search_limit as usize)
        } else {
            None
        }
    }

    pub fn search_location(&self) -> ResolveSearchLocation {
        if self.local > 0 || self.offline > 0 {
            ResolveSearchLocation::Local
        } else if self.remote > 0 {
            ResolveSearchLocation::Remote
        } else {
            ResolveSearchLocation::RemoteOrLocal
        }
    }

    pub fn search_nearest(&self) -> bool {
        self.search_nearest != 0
    }

    pub fn no_gc(&self) -> bool {
        self.no_gc != 0
    }

    pub fn in_memory(&self) -> bool {
        self.in_memory != 0
    }

    pub fn sync_data(&self) -> bool {
        self.sync_data != 0
    }

    pub fn cache(&self) -> bool {
        self.cache != 0
    }

    /// Returns the store keep-alive duration if enabled.
    /// When `store_keep_alive` is not set, returns `None`.
    /// When set with `store_keep_alive_seconds` of 0, uses the default duration.
    pub fn store_keep_alive_duration(&self) -> Option<std::time::Duration> {
        if self.store_keep_alive == 0 {
            return None;
        }
        let seconds = if self.store_keep_alive_seconds == 0 {
            default_store_keep_alive_seconds()
        } else {
            self.store_keep_alive_seconds
        };
        Some(std::time::Duration::from_secs(seconds))
    }
}

/// Default duration in seconds to keep store references alive between consecutive
/// API calls when store keep-alive is enabled but no explicit duration is set.
const fn default_store_keep_alive_seconds() -> u64 {
    10
}

pub type LoreEventCallback = Option<Box<dyn Fn(&LoreEvent) + Send + Sync>>;

/// A callback function paired with a caller-supplied context value, used to
/// receive events.
///
/// The callback does not run inside the lore_* call that configured it. It runs
/// on a thread the library manages, one of a pool of worker threads, not the
/// calling thread.
///
/// The event pointer, and everything it points to, is valid only until the
/// callback returns. Copy any data you need to keep, and do not use the event
/// pointer after the callback returns.
///
/// Events for a single call arrive one at a time. Two concurrent asynchronous
/// calls that share one configuration can run the callback at the same time, so
/// a shared callback must be safe to call from more than one thread at once. A
/// callback that blocks delays the library's other work and can stall other
/// in-flight calls. Do long or blocking work on your own thread and return from
/// the callback promptly.
#[repr(C)]
pub struct LoreEventCallbackConfig {
    /// Caller-supplied value passed back to the callback on each call.
    pub user_context: u64,
    /// Function invoked for each event, or none to receive no events.
    pub func: Option<unsafe extern "C" fn(event: &LoreEvent, user_context: u64)>,
}

static EXECUTION_INITIALIZER: Once = Once::new();

fn execution_initialize() {
    EXECUTION_INITIALIZER.call_once(|| {
        #[cfg(debug_assertions)]
        {
            std::panic::set_hook(Box::new(|info| {
                eprintln!("panic: {info}");
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("{bt}");
            }));
        }

        #[cfg(target_family = "unix")]
        unsafe {
            use libc::RLIMIT_NOFILE;
            use libc::getrlimit;
            use libc::rlim_t;
            use libc::rlimit;
            use libc::setrlimit;

            let mut rlimit = rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if getrlimit(RLIMIT_NOFILE, &mut rlimit) == 0 {
                let desired_limit: rlim_t = 65536 * 2;
                if rlimit.rlim_cur < desired_limit {
                    rlimit.rlim_cur = desired_limit;
                    if rlimit.rlim_cur > rlimit.rlim_max {
                        rlimit.rlim_cur = rlimit.rlim_max;
                    }
                    setrlimit(RLIMIT_NOFILE, &rlimit);
                }
            }
        }

        let _ = install_crypto_provider();
    });
}

#[derive(PartialEq, Debug)]
pub enum ExecutionMode {
    Client,
    Server,
}

pub struct ExecutionContext {
    globals: LoreGlobalArgs,
    pub dispatcher: EventDispatcher,
    pub log_level: LoreLogLevel,
    user_id: Mutex<String>,
    pub failure: AtomicBool,
    mode: ExecutionMode,
    caller_state: Option<Arc<dyn Any + Send + Sync>>,
}

impl ExecutionContext {
    fn new(
        mut globals: LoreGlobalArgs,
        mut dispatcher: EventDispatcher,
        user_id: String,
        mode: ExecutionMode,
    ) -> Self {
        execution_initialize();
        lore_storage::concurrency::configure(globals.file_count_limit as usize);
        lore_storage::concurrency::configure_compress_limiter(globals.compress_task_limit as usize);

        // Ensure we have a consistent correlation ID
        if globals.correlation_id.is_empty() {
            globals.correlation_id = uuid::Uuid::new_v4().to_string().into();
        }
        dispatcher.correlation_id = globals.correlation_id.to_string();

        Self {
            globals,
            dispatcher,
            log_level: LoreLogLevel::Debug,
            user_id: Mutex::new(user_id),
            mode,
            ..Default::default()
        }
    }

    pub fn new_client(globals: LoreGlobalArgs, dispatcher: EventDispatcher) -> Self {
        Self::new(
            globals,
            dispatcher,
            String::default(),
            ExecutionMode::Client,
        )
    }

    pub fn new_client_with_user_id(
        globals: LoreGlobalArgs,
        dispatcher: EventDispatcher,
        user_id: String,
    ) -> Self {
        Self::new(globals, dispatcher, user_id, ExecutionMode::Client)
    }

    pub fn new_server(
        globals: LoreGlobalArgs,
        dispatcher: EventDispatcher,
        user_id: String,
    ) -> Self {
        Self::new(globals, dispatcher, user_id, ExecutionMode::Server)
    }

    pub fn globals(&self) -> &LoreGlobalArgs {
        &self.globals
    }

    pub fn is_client(&self) -> bool {
        self.mode == ExecutionMode::Client
    }

    pub fn is_server(&self) -> bool {
        self.mode == ExecutionMode::Server
    }

    pub fn set_caller_state(&mut self, state: Arc<dyn Any + Send + Sync>) {
        self.caller_state = Some(state);
    }

    pub fn caller_state(&self) -> Option<&Arc<dyn Any + Send + Sync>> {
        self.caller_state.as_ref()
    }
}

impl Default for ExecutionContext {
    fn default() -> Self {
        execution_initialize();

        ExecutionContext {
            globals: LoreGlobalArgs::default(),
            dispatcher: EventDispatcher::default(),
            log_level: LoreLogLevel::Error,
            user_id: Mutex::default(),
            failure: AtomicBool::default(),
            mode: ExecutionMode::Client,
            caller_state: None,
        }
    }
}

impl ExecutionContext {
    pub async fn user_id(&self) -> String {
        self.user_id.lock().await.clone()
    }

    pub async fn set_user_id(&self, id: &str) {
        *self.user_id.lock().await = id.to_string();
    }
}

fn install_crypto_provider() -> Result<(), String> {
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .map_err(|current| {
            format!("Trying to install default crypto provider, but one is already installed? (current: {current:?})")
        })
}

/// Error codes returned across the FFI boundary.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(i32)]
#[derive(Eq, PartialEq)]
pub enum LoreError {
    /// The arguments supplied to the operation were invalid.
    InvalidArguments = 1,
    /// A content-addressable object could not be found in any store.
    AddressNotFound = 2,
    /// A file path could not be resolved to a tracked node or found in the file system.
    FileNotFound = 3,
    /// A payload blob could not be found with the associated hash.
    PayloadNotFound = 4,
    /// The backing store is overloaded; the caller should retry later.
    SlowDown = 5,
    /// A blob exceeded a size limit enforced by the caller or the protocol.
    /// Discriminant matches the error code of the underlying `Oversized` struct
    /// in `lore-base` so callers see a single consistent code.
    Oversized = 26,

    // Legacy error categories (transitional, will be removed)
    /// A requested item was not found.
    NotFound = 101,
    /// An item that was being created already exists.
    AlreadyExists = 102,
    /// A connection could not be established or was lost.
    Connection = 103,

    /// An internal error occurred.
    Internal = -1,
}

/// A metadata value, tagged by the kind of value it holds.
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C, u32)]
#[derive(Clone, Debug, PartialEq)]
pub enum LoreMetadata {
    /// An address value.
    Address(Address) = LoreMetadataType::Address as u32,
    /// A boolean value, stored as a byte; any non-zero value is true.
    Boolean(u8) = LoreMetadataType::Boolean as u32,
    /// A context value.
    Context(Context) = LoreMetadataType::Context as u32,
    /// A hash value.
    Hash(Hash) = LoreMetadataType::Hash as u32,
    /// An unsigned integer value.
    Numeric(u64) = LoreMetadataType::Numeric as u32,
    /// A string value.
    String(LoreString) = LoreMetadataType::String as u32,
    /// A block of raw bytes.
    Binary(LoreBinary) = LoreMetadataType::Binary as u32,
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// The kind of value held by a metadata entry.
///
/// This is both the tag a caller passes across the API and the tag written into
/// a stored metadata buffer — the same type, so the two cannot drift apart.
///
/// There is deliberately no zero value: a zero-initialized field has not chosen
/// a type and must not be passed as one.
#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreMetadataType {
    /// A content address: 48 bytes, the 32-byte hash followed by the 16-byte
    /// context.
    Address = 1,
    /// A boolean: exactly one byte, where any non-zero value is true.
    Boolean = 2,
    /// A context identifier: 16 raw bytes.
    Context = 3,
    /// A content hash: 32 raw bytes.
    Hash = 4,
    /// An unsigned 64-bit integer: 8 bytes, little-endian.
    Numeric = 5,
    /// Text: UTF-8 bytes, not terminated.
    String = 6,
    /// Raw bytes, stored exactly as supplied and of any length.
    Binary = 255,
}

/// Adjacent tagging (`{"tagName": …, "data": …}`) for self-describing formats,
/// external tagging for the rest.
///
/// The derive cannot express both, and one representation will not do: adjacent
/// tagging needs `deserialize_identifier`, which the binary format used between
/// a client and the service does not implement, while switching everything to
/// external tagging would change the JSON that existing clients already read.
/// The split is the same one [`crate::lore::Address`] makes.
mod metadata_repr {
    use serde::Deserialize;
    use serde::Serialize;

    use super::*;

    pub(super) const ADDRESS: (u32, &str) = (0, "address");
    pub(super) const BOOLEAN: (u32, &str) = (1, "boolean");
    pub(super) const BINARY: (u32, &str) = (2, "binary");
    pub(super) const CONTEXT: (u32, &str) = (3, "context");
    pub(super) const HASH: (u32, &str) = (4, "hash");
    pub(super) const NUMERIC: (u32, &str) = (5, "numeric");
    pub(super) const STRING: (u32, &str) = (6, "string");

    pub(super) fn emit<S, T>(
        serializer: S,
        variant: (u32, &'static str),
        value: &T,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
        T: Serialize + ?Sized,
    {
        let (index, name) = variant;
        if serializer.is_human_readable() {
            use serde::ser::SerializeStruct;
            let mut tagged = serializer.serialize_struct("LoreMetadata", 2)?;
            tagged.serialize_field("tagName", name)?;
            tagged.serialize_field("data", value)?;
            tagged.end()
        } else {
            serializer.serialize_newtype_variant("LoreMetadata", index, name, value)
        }
    }

    /// Mirrors [`LoreMetadata`]'s variants so the derive can do the reading.
    ///
    /// The order here is not [`LoreMetadata`]'s and need not be: what matters is
    /// that it matches the indices the constants above carry, since the external
    /// form is read by position. Move a variant in one and the other has to move
    /// with it, or a value is written under one kind and read back as another.
    #[derive(Deserialize)]
    #[serde(tag = "tagName", content = "data", rename_all = "camelCase")]
    pub(super) enum Tagged {
        Address(Address),
        Boolean(#[serde(with = "u8_as_bool")] u8),
        Binary(LoreBinary),
        Context(Context),
        Hash(Hash),
        Numeric(u64),
        String(LoreString),
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) enum External {
        Address(Address),
        Boolean(#[serde(with = "u8_as_bool")] u8),
        Binary(LoreBinary),
        Context(Context),
        Hash(Hash),
        Numeric(u64),
        String(LoreString),
    }

    impl From<Tagged> for LoreMetadata {
        fn from(value: Tagged) -> Self {
            match value {
                Tagged::Address(inner) => LoreMetadata::Address(inner),
                Tagged::Boolean(inner) => LoreMetadata::Boolean(inner),
                Tagged::Binary(inner) => LoreMetadata::Binary(inner),
                Tagged::Context(inner) => LoreMetadata::Context(inner),
                Tagged::Hash(inner) => LoreMetadata::Hash(inner),
                Tagged::Numeric(inner) => LoreMetadata::Numeric(inner),
                Tagged::String(inner) => LoreMetadata::String(inner),
            }
        }
    }

    impl From<External> for LoreMetadata {
        fn from(value: External) -> Self {
            match value {
                External::Address(inner) => LoreMetadata::Address(inner),
                External::Boolean(inner) => LoreMetadata::Boolean(inner),
                External::Binary(inner) => LoreMetadata::Binary(inner),
                External::Context(inner) => LoreMetadata::Context(inner),
                External::Hash(inner) => LoreMetadata::Hash(inner),
                External::Numeric(inner) => LoreMetadata::Numeric(inner),
                External::String(inner) => LoreMetadata::String(inner),
            }
        }
    }
}

impl Serialize for LoreMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use metadata_repr::*;
        match self {
            LoreMetadata::Address(value) => emit(serializer, ADDRESS, value),
            LoreMetadata::Boolean(value) => emit(serializer, BOOLEAN, &(*value != 0)),
            LoreMetadata::Binary(value) => emit(serializer, BINARY, value),
            LoreMetadata::Context(value) => emit(serializer, CONTEXT, value),
            LoreMetadata::Hash(value) => emit(serializer, HASH, value),
            LoreMetadata::Numeric(value) => emit(serializer, NUMERIC, value),
            LoreMetadata::String(value) => emit(serializer, STRING, value),
        }
    }
}

impl<'de> Deserialize<'de> for LoreMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            metadata_repr::Tagged::deserialize(deserializer).map(LoreMetadata::from)
        } else {
            metadata_repr::External::deserialize(deserializer).map(LoreMetadata::from)
        }
    }
}

impl LoreMetadata {
    /// The value's stored byte form and the tag it is stored under.
    ///
    /// The inverse of [`crate::event::LoreMetadataEventData::new`], which reads
    /// the same pair back out. Nothing here can fail: the value already is the
    /// type it claims, which is the point of carrying a typed value rather than
    /// text plus a separate tag.
    ///
    /// A kind that already holds its stored bytes contiguously lends them out,
    /// so the two kinds of unbounded length cost nothing to encode; only the
    /// two that have to be laid out as bytes allocate, and both are tiny.
    pub fn to_stored(&self) -> (Cow<'_, [u8]>, LoreMetadataType) {
        match self {
            LoreMetadata::Address(address) => {
                (Cow::Borrowed(address.as_bytes()), LoreMetadataType::Address)
            }
            LoreMetadata::Boolean(flag) => (
                Cow::Owned(vec![u8::from(*flag != 0)]),
                LoreMetadataType::Boolean,
            ),
            LoreMetadata::Binary(block) => {
                (Cow::Borrowed(block.as_bytes()), LoreMetadataType::Binary)
            }
            LoreMetadata::Context(context) => {
                (Cow::Borrowed(context.data()), LoreMetadataType::Context)
            }
            LoreMetadata::Hash(hash) => (Cow::Borrowed(hash.data()), LoreMetadataType::Hash),
            LoreMetadata::Numeric(number) => (
                Cow::Owned(number.to_le_bytes().to_vec()),
                LoreMetadataType::Numeric,
            ),
            LoreMetadata::String(text) => {
                (Cow::Borrowed(text.as_bytes()), LoreMetadataType::String)
            }
        }
    }
}

impl ValidateText for LoreMetadata {
    fn validate_text(&self) -> Result<(), TextNotUtf8> {
        match self {
            LoreMetadata::String(text) => text.validate_text(),
            // Every other variant is fixed-width or opaque bytes; a binary value
            // is deliberately not text and must not be rejected for not being it.
            _ => Ok(()),
        }
    }
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// The kind of a tracked node.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreNodeType {
    /// A directory.
    Directory = 0,
    /// A file.
    File = 1,
    /// A symbolic link.
    Link = 2,
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// The change applied to a file.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreFileAction {
    /// The file is unchanged.
    Keep = 0,
    /// The file was added.
    Add = 1,
    /// The file was deleted.
    Delete = 2,
    /// The file was moved to a new path.
    Move = 3,
    /// The file was copied from another path.
    Copy = 4,
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// Where a branch is located.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreBranchLocation {
    /// A branch held locally.
    Local = 0,
    /// A branch held on the server.
    Remote = 1,
}

impl Display for LoreBranchLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoreBranchLocation::Local => write!(f, "local"),
            LoreBranchLocation::Remote => write!(f, "remote"),
        }
    }
}

/// A branch paired with a revision on that branch.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct LoreBranchPoint {
    /// The branch.
    pub branch: BranchId,
    /// The revision on the branch.
    pub revision: Hash,
}

impl From<&BranchPoint> for LoreBranchPoint {
    fn from(branch_point: &BranchPoint) -> Self {
        LoreBranchPoint {
            branch: branch_point.branch,
            revision: branch_point.revision,
        }
    }
}

impl From<FileAction> for LoreFileAction {
    fn from(value: FileAction) -> Self {
        LoreFileAction::from(value as u32)
    }
}

impl From<u16> for LoreFileAction {
    fn from(value: u16) -> Self {
        LoreFileAction::from(value as u32)
    }
}

impl From<u32> for LoreFileAction {
    fn from(value: u32) -> Self {
        if value == FileAction::Add as u32 {
            return LoreFileAction::Add;
        } else if value == FileAction::Delete as u32 {
            return LoreFileAction::Delete;
        } else if value == FileAction::Move as u32 {
            return LoreFileAction::Move;
        } else if value == FileAction::Copy as u32 {
            return LoreFileAction::Copy;
        }

        // `FileAction::Graft` maps here too. A graft replaces a directory's
        // subtree, which reads as a modification, and the C enum stays
        // unchanged.
        LoreFileAction::Keep
    }
}

impl LoreFileAction {
    pub fn as_string_short(&self) -> &'static str {
        match self {
            LoreFileAction::Add => "A",
            LoreFileAction::Delete => "D",
            LoreFileAction::Move => "V",
            LoreFileAction::Copy => "C",
            LoreFileAction::Keep => "M",
        }
    }
}

pub fn shutdown() {
    runtime_shutdown_timeout(std::time::Duration::from_secs(10));

    unsafe {
        unsafe extern "C" {
            fn rpmalloc_finalize();
        }

        rpmalloc_finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name arriving across the C boundary can hold any byte sequence. The
    /// formatting paths run on every dispatched command, so they must render
    /// such a string instead of assuming UTF-8.
    #[test]
    fn lore_string_renders_invalid_utf8_as_replacement_characters() {
        let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);

        assert_eq!(value.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);
        assert_eq!(format!("{value}"), "a\u{fffd}\u{fffd}b");
        assert_eq!(format!("{value:?}"), "a\u{fffd}\u{fffd}b");
    }

    /// Unlike formatting, serialization must not substitute: it carries the
    /// command to the service, where a replacement-character name would be
    /// accepted as valid text that the in-process path would have rejected.
    #[test]
    fn lore_string_serialization_rejects_invalid_utf8() {
        let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);
        assert!(
            serde_json::to_string(&value).is_err(),
            "serializing non-UTF-8 text must fail rather than substitute"
        );

        let valid = LoreString::from_str("doc.md");
        assert_eq!(
            serde_json::to_string(&valid).expect("valid text must serialize"),
            "\"doc.md\""
        );
    }

    /// Equality compares the raw bytes, so strings that differ only in an
    /// invalid sequence stay distinguishable.
    #[test]
    fn lore_string_equality_compares_bytes() {
        assert_eq!(LoreString::from_str("same"), LoreString::from_str("same"));
        assert_ne!(
            LoreString::from_bytes(&[0xff]),
            LoreString::from_bytes(&[0xfe])
        );
    }

    /// Every call clones its arguments before anything checks them, so cloning
    /// must copy the bytes rather than read them as text.
    #[test]
    fn lore_string_clone_copies_bytes_that_are_not_utf8() {
        let value = LoreString::from_bytes(&[b'a', 0xff, 0xfe, b'b']);

        let cloned = value.clone();
        assert_eq!(cloned.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);

        let mut assigned = LoreString::from_str("replaced");
        assigned.clone_from(&value);
        assert_eq!(assigned.as_bytes(), &[b'a', 0xff, 0xfe, b'b']);
    }

    #[test]
    fn validate_text_accepts_valid_utf8_and_empty_strings() {
        assert!(LoreString::from_str("doc.md").validate_text().is_ok());
        assert!(LoreString::default().validate_text().is_ok());
        assert!(LoreString::from_str("ünïcøde").validate_text().is_ok());
    }

    #[test]
    fn validate_text_rejects_bytes_that_are_not_utf8() {
        assert!(
            LoreString::from_bytes(&[b'a', 0xff])
                .validate_text()
                .is_err()
        );
    }

    /// An array reports which element failed, so the rejection points at one
    /// entry rather than the whole field.
    #[test]
    fn validate_text_names_the_array_element_that_failed() {
        let strings = LoreArray::from_vec(vec![
            LoreString::from_str("first"),
            LoreString::from_str("second"),
            LoreString::from_bytes(&[0xff]),
        ]);

        let error = strings
            .validate_text()
            .map_err(|error| error.inside("paths"))
            .expect_err("the element must fail");

        assert_eq!(error.field(), "paths[2]");
    }

    #[test]
    fn validate_text_passes_arguments_that_hold_no_text() {
        assert!(LoreArray::<LoreString>::default().validate_text().is_ok());
        assert!(LoreGlobalArgs::default().validate_text().is_ok());
    }

    #[test]
    fn validate_text_names_the_failing_field_of_the_global_arguments() {
        let globals = LoreGlobalArgs {
            identity: LoreString::from_bytes(&[b'i', 0xff]),
            ..LoreGlobalArgs::default()
        };

        let error = globals
            .validate_text()
            .map_err(|error| error.inside("globals"))
            .expect_err("the identity must fail");
        assert_eq!(error.field(), "globals.identity");
    }
}

#[cfg(test)]
mod metadata_repr_tests {
    use super::LoreBinary;
    use super::LoreMetadata;
    use super::LoreString;

    /// The JSON shape is a published wire format that existing clients read, and
    /// the serializer producing it is hand-written rather than derived, so the
    /// exact bytes are the contract — not just that a round trip works. Every
    /// kind is pinned, because each reaches JSON by its own route: a bool for a
    /// byte, hex text for the identifiers, and base64 for a block of raw bytes.
    #[test]
    fn json_keeps_the_adjacently_tagged_shape() {
        let hash = super::Hash::from([0xabu8; 32]);
        let context = super::Context::from([0xcdu8; 16]);
        let cases = [
            (
                LoreMetadata::String(LoreString::from_str("hi")),
                r#"{"tagName":"string","data":"hi"}"#.to_string(),
            ),
            (
                LoreMetadata::Numeric(4207),
                r#"{"tagName":"numeric","data":4207}"#.to_string(),
            ),
            (
                LoreMetadata::Boolean(1),
                r#"{"tagName":"boolean","data":true}"#.to_string(),
            ),
            (
                LoreMetadata::Boolean(0),
                r#"{"tagName":"boolean","data":false}"#.to_string(),
            ),
            (
                LoreMetadata::Binary(LoreBinary::from_bytes(&[0x00, 0xff, 0x01])),
                r#"{"tagName":"binary","data":"AP8B"}"#.to_string(),
            ),
            (
                LoreMetadata::Hash(hash),
                format!(r#"{{"tagName":"hash","data":"{}"}}"#, "ab".repeat(32)),
            ),
            (
                LoreMetadata::Context(context),
                format!(r#"{{"tagName":"context","data":"{}"}}"#, "cd".repeat(16)),
            ),
            (
                LoreMetadata::Address(super::Address { hash, context }),
                format!(
                    r#"{{"tagName":"address","data":"{}-{}"}}"#,
                    "ab".repeat(32),
                    "cd".repeat(16)
                ),
            ),
        ];

        for (value, want) in cases {
            let json = serde_json::to_string(&value).expect("serialize");
            assert_eq!(json, want, "the published shape must not drift");
            let back: LoreMetadata = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
        }
    }

    /// A boolean is a JSON bool but a byte in the C union, and the two must not
    /// disagree: any non-zero byte is true, and true reads back as exactly 1.
    #[test]
    fn a_non_zero_boolean_byte_normalizes_through_json() {
        let json = serde_json::to_string(&LoreMetadata::Boolean(37)).expect("serialize");
        assert_eq!(json, r#"{"tagName":"boolean","data":true}"#);
        let back: LoreMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, LoreMetadata::Boolean(1));
    }

    /// Every variant has to survive the format used between a client and the
    /// service, which cannot read the tagged shape at all.
    #[test]
    fn every_variant_survives_the_compact_format() {
        let values = [
            LoreMetadata::Address(super::Address::default()),
            LoreMetadata::Boolean(1),
            LoreMetadata::Binary(LoreBinary::from_bytes(&[0x00, 0xff])),
            LoreMetadata::Context(super::Context::default()),
            LoreMetadata::Hash(super::Hash::default()),
            LoreMetadata::Numeric(u64::MAX),
            LoreMetadata::String(LoreString::from_str("hi")),
        ];

        for value in values {
            let encoded = bitcode::serialize(&value).expect("serialize");
            let decoded: LoreMetadata = bitcode::deserialize(&encoded).expect("deserialize");
            assert_eq!(decoded, value, "{value:?} must survive the compact format");
        }
    }
}

#[cfg(test)]
mod binary_tests {
    use super::LoreBinary;

    /// `LoreBinary` owns its payload, so a clone survives the original being
    /// dropped. Before it owned anything, the clone was a copy of a pointer and
    /// this read freed memory.
    #[test]
    fn a_clone_outlives_the_value_it_came_from() {
        let clone = {
            let original = LoreBinary::from_bytes(&[0xde, 0xad, 0xbe, 0xef]);
            original.clone()
        };
        assert_eq!(clone.as_bytes(), &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn an_empty_block_is_a_null_pointer_of_zero_length() {
        let empty = LoreBinary::from_bytes(&[]);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.payload.is_null());
        assert_eq!(empty.as_bytes(), &[] as &[u8]);
        assert_eq!(empty, LoreBinary::default());
    }

    /// An event carrying a binary metadata value reaches an out-of-process
    /// caller as a serialized value, so it has to deserialize. It used to panic
    /// outright: the impl was `unimplemented!()`, which is why the revision-tree
    /// read verb refused binary values rather than delivering one.
    ///
    /// Both a self-describing format and a non-self-describing one are covered,
    /// because the two take different paths through the impl.
    #[test]
    fn a_binary_value_survives_serialization() {
        let block = LoreBinary::from_bytes(b"raw\x00bytes");

        let json = serde_json::to_vec(&block).expect("json serialize");
        let from_json: LoreBinary = serde_json::from_slice(&json).expect("json deserialize");
        assert_eq!(from_json, block, "json must round-trip a binary block");

        let encoded = bitcode::serialize(&block).expect("bitcode serialize");
        let from_bitcode: LoreBinary = bitcode::deserialize(&encoded).expect("bitcode deserialize");
        assert_eq!(
            from_bitcode, block,
            "bitcode must round-trip a binary block"
        );
    }

    /// Equality is by content, not by length or by pointer identity: two blocks
    /// of the same size holding different bytes are different values.
    #[test]
    fn blocks_of_equal_length_compare_by_content() {
        let block = LoreBinary::from_bytes(&[1, 2, 3, 4]);
        assert_eq!(block, LoreBinary::from_bytes(&[1, 2, 3, 4]));
        assert_ne!(block, LoreBinary::from_bytes(&[1, 2, 3, 5]));
        assert_ne!(block, LoreBinary::from_bytes(&[1, 2, 3]));
    }

    /// An empty block still has to survive a round trip: the deserializer has to
    /// produce the null-pointer form rather than a dangling allocation. Both
    /// formats are covered, since an empty block is the one input where the
    /// text encoding carries no characters at all.
    #[test]
    fn an_empty_block_survives_serialization() {
        let empty = LoreBinary::from_bytes(&[]);

        let json = serde_json::to_string(&empty).expect("json serialize");
        assert_eq!(json, r#""""#);
        let from_json: LoreBinary = serde_json::from_str(&json).expect("json deserialize");
        assert_eq!(from_json, empty);
        assert!(from_json.payload.is_null());

        let encoded = bitcode::serialize(&empty).expect("bitcode serialize");
        let decoded: LoreBinary = bitcode::deserialize(&encoded).expect("bitcode deserialize");
        assert_eq!(decoded, empty);
        assert!(decoded.payload.is_null());
    }

    /// Text that is not base64 is a malformed payload, not an empty block: a
    /// reader that quietly produced one would hand a caller a value the sender
    /// never wrote.
    #[test]
    fn json_text_that_is_not_base64_fails_to_read() {
        let result: Result<LoreBinary, _> = serde_json::from_str(r#""not base64!""#);
        assert!(result.is_err());
    }
}
