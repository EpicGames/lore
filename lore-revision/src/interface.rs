// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::any::Any;
use std::fmt::Debug;
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::AtomicBool;

use lore_base::runtime::runtime_shutdown_timeout;
use lore_base::types::BranchPoint;
pub use lore_credential::user_info;
pub use lore_transport::drop_connections;
use serde::Deserialize;
use serde::Serialize;
use serde::ser::SerializeSeq;
use tokio::sync::Mutex;

use crate::change::FileAction;
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
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct LoreBinary {
    /// Pointer to the start of the byte block.
    pub payload: *const std::ffi::c_void,
    /// Number of bytes in the block.
    pub length: usize,
}

unsafe impl Send for LoreBinary {}
unsafe impl Sync for LoreBinary {}

impl LoreBinary {
    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.payload.cast::<u8>(), self.length) }
    }
}

impl Serialize for LoreBinary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.as_bytes())
    }
}

impl<'de> Deserialize<'de> for LoreBinary {
    #[allow(clippy::unimplemented)]
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // TODO(UCS-13323)
        unimplemented!("LoreBinary deserialization. Requires redesign of LoreBinary ownership")
    }
}

/// A string described by a pointer to its character data and a length, holding
/// text as a sequence of bytes.
///
/// The text is UTF-8. The length field counts the bytes before the trailing
/// NUL. An empty string is a NULL pointer with length 0, and a length of 0
/// means the string is empty.
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
        f.write_fmt(format_args!("{}", self.as_str()))
    }
}

impl LoreString {
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn len(&self) -> usize {
        self.length
    }

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
    fn clone(&self) -> Self {
        LoreString::from_str(self.as_str())
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
        self.as_str() == other.as_str()
    }
}

impl Display for LoreString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Drop for LoreString {
    fn drop(&mut self) {
        self.free();
    }
}

impl AsRef<str> for LoreString {
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

impl Serialize for LoreString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
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
#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tagName", content = "data", rename_all = "camelCase")]
pub enum LoreMetadata {
    /// An address value.
    Address(Address),
    /// A boolean value, stored as a byte.
    Boolean(#[serde(with = "u8_as_bool")] u8),
    /// A block of raw bytes.
    Binary(LoreBinary),
    /// A context value.
    Context(Context),
    /// A hash value.
    Hash(Hash),
    /// An unsigned integer value.
    Numeric(u64),
    /// A string value.
    String(LoreString),
}

/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
/// The kind of value held by a metadata entry.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoreMetadataType {
    /// A block of raw bytes.
    Binary = 0,
    /// An unsigned integer value.
    Numeric = 1,
    /// A string value.
    String = 2,
}

impl From<LoreMetadataType> for crate::metadata::MetadataType {
    fn from(value: LoreMetadataType) -> Self {
        match value {
            LoreMetadataType::Binary => Self::Binary,
            LoreMetadataType::Numeric => Self::Numeric,
            LoreMetadataType::String => Self::String,
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
