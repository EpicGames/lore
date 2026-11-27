// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Consolidated FFI error types for the Lore system.
//!
//! All discrete error types with FFI codes are defined here,
//! providing a single source of truth for FFI error code allocation.
//! This is the base error crate with no dependency on lore-storage.
//!
//! # Code allocation
//!
//! Codes are grouped into blocks by the kind of failure they report, so a
//! consumer can branch on a range when it only cares about the category and on
//! the exact value when it cares about the specific error.
//!
//! Every code is also a process exit status: the CLI returns the failing
//! error's code from `main` as `ExitCode::from(code as u8)`. That fixes two
//! things. Every code must fit in a `u8`, and no code may land on a value the
//! shell or the OS already spends on something else — a `lore` command that
//! exited 130 would be indistinguishable from one the user killed with Ctrl-C.
//! The reserved rows below are those values, and no group block overlaps one.
//!
//! | Range | Group | Meaning |
//! | --- | --- | --- |
//! | 0 | *reserved* | Success. |
//! | 1 | *reserved* | The CLI's own general failure (`ExitCode::FAILURE`), and the shell's generic error. |
//! | 2 | *reserved* | The CLI's usage exit, and the shell's "misuse of a builtin". |
//! | 3–15 | Input and validation | The request itself was malformed or names the wrong kind of thing. |
//! | 16–27 | Authentication and authorization | The caller is not known, or is known and not permitted. |
//! | 28–39 | Connectivity and availability | The remote could not be reached or is not currently serving. |
//! | 40–55 | Repository state | The repository is in a state that refuses the operation. |
//! | 56–63 | Already exists | Creating a resource that is already there. |
//! | 64–78 | *reserved* | The BSD `sysexits.h` codes (`EX_USAGE` through `EX_CONFIG`). |
//! | 79–99 | Not found | A named resource does not exist. |
//! | 100–109 | *reserved* | Not allocated: older Lore releases report 101, 102 and 103 as categories of error. |
//! | 110–117 | Configuration | Something the operation needs was never configured. |
//! | 118–125 | Resource limits | A size, depth, or efficiency bound was hit. |
//! | 126–128 | *reserved* | The shell's "found but not executable", "not found", and "bad argument to exit". |
//! | 129–192 | *reserved* | `128 + signal`, i.e. killed by a signal. 130 is Ctrl-C, 137 is `SIGKILL`, 143 is `SIGTERM`. |
//! | 193–200 | Library lifecycle | The library cannot serve the call. |
//! | 201–254 | *reserved* | Free for future groups. |
//! | 255 | *reserved* | `Internal`, which is `-1` truncated to a `u8`. |
//!
//! The trait carries a code as `i32` rather than a `u8` because `Internal` is
//! `-1`, which is not a code allocated here.
//!
//! Add a new error to the block that matches its group and take the next free
//! code in that block. Never renumber an existing code to close a gap: the gaps
//! are the headroom that keeps a group contiguous, and the tests below hold
//! every block clear of the reserved values so anything taken from that
//! headroom is a usable exit status. New groups come out of 201–254. A type
//! whose name ends in `NotFound` belongs in the not-found block even when it
//! also belongs to a subsystem that has its own errors elsewhere —
//! `PluginNotFound` sits with the other not-found codes, not with the other
//! plugin codes.
//!
//! Each type has the variant of [`LoreErrorCode`] named after it, equal to its code, and an arm
//! in [`LoreErrorCode::from_ffi_code`], which `#[derive(FfiError)]` checks at compile time.
//! A type's name is its enumerator's name in `lore.h`, so a type keeps its name as it keeps
//! its code.

use std::convert::Infallible;
use std::fmt;

use lore_error_set::ErrorSet;
use lore_error_set::FfiError;
use lore_error_set::ForwardAny;
use lore_error_set::Has;
use thiserror::Error;

/// Scopes `#[ffi_code(N)]` to this module.
///
/// `#[derive(FfiError)]` expands to a reference to this name, resolved where
/// the derive is written rather than in the macro crate. Declaring it here and
/// nowhere else means a code allocated outside this registry does not compile,
/// which is what keeps the allocation table below the single source of truth.
fn __ffi_code_registry_marker() {}

// ---------------------------------------------------------------------------
// Input and validation (3–15)
//
// The arguments the caller supplied cannot be acted on: they are malformed,
// they name the wrong kind of thing, or they ask for something the library
// does not implement.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid arguments: {reason}")]
#[ffi_code(3)]
pub struct InvalidArguments {
    pub reason: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid path: {path}")]
#[ffi_code(4)]
pub struct InvalidPath {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid address: {address}")]
#[ffi_code(5)]
pub struct InvalidAddress {
    pub address: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Path is not a link: {path}")]
#[ffi_code(6)]
pub struct NotALink {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Path is not a layer: {path}")]
#[ffi_code(7)]
pub struct NotALayer {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Node {node} has parent {actual_parent} but was reached as a child of {expected_parent}")]
#[ffi_code(8)]
pub struct InvalidNodeHierarchy {
    pub node: u32,
    pub expected_parent: u32,
    pub actual_parent: u32,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Operation not supported: {operation}")]
#[ffi_code(9)]
pub struct NotSupported {
    pub operation: String,
}

// ---------------------------------------------------------------------------
// Authentication and authorization (16–27)
//
// The caller's identity is missing, unusable, or insufficient for the
// operation.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Not authenticated")]
#[ffi_code(16)]
pub struct NotAuthenticated;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Not authorized to access repository")]
#[ffi_code(17)]
pub struct NotAuthorized;

/// Whether `error` is a refusal of the caller: [`NotAuthenticated`] or [`NotAuthorized`].
pub fn is_refusal<E: FfiError>(error: &E) -> bool {
    matches!(
        error.ffi_code(),
        NotAuthenticated::FFI_CODE | NotAuthorized::FFI_CODE
    )
}

/// `error` forwarded into `Target` when it is the remote's refusal of the caller, which keeps its
/// variant there, and `otherwise(error)` when it is any other failure.
#[track_caller]
pub fn forward_refusal_or<Source, Target>(
    error: Source,
    context: &str,
    otherwise: impl FnOnce(Source) -> Target,
) -> Target
where
    Source: ErrorSet + FfiError,
    Target: ErrorSet + Has<NotAuthenticated> + Has<NotAuthorized>,
{
    if !is_refusal(&error) {
        return otherwise(error);
    }
    let Err(target) = Err::<Infallible, Source>(error).forward_any::<Target>(context);
    target
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("No token stored")]
#[ffi_code(18)]
pub struct TokenNotFound;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Operation requires write access")]
#[ffi_code(19)]
pub struct WriteRequired;

// ---------------------------------------------------------------------------
// Connectivity and availability (28–39)
//
// Something the operation had to talk to could not be reached, dropped the
// connection, or is reachable but declining work. Usually the remote; also the
// local service process, which a call is relayed through and which the caller
// reaches the same way — over a connection that may not be there. Retrying
// later is often the right response.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Disconnected from server")]
#[ffi_code(28)]
pub struct Disconnected;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Not connected to remote: {reason}")]
#[ffi_code(29)]
pub struct NotConnected {
    pub reason: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Server is in maintenance mode")]
#[ffi_code(30)]
pub struct Maintenance;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Store overloaded, slow down")]
#[ffi_code(31)]
pub struct SlowDown;

/// No Lore service could be reached, and none could be started, or the service socket answered
/// that no service runs while the external directories of its virtual instances move.
///
/// Distinct from [`NotConnected`], which reports a remote: a caller that relays
/// its calls through the local service handles this differently, because it is
/// the one failure that means the operation did not run at all rather than ran
/// and failed. Running the operation in this process instead is a reasonable
/// answer to it and not to any other error here.
#[derive(Debug, Clone, Error, FfiError)]
#[error("Lore service unavailable: {reason}")]
#[ffi_code(32)]
pub struct ServiceUnavailable {
    pub reason: String,
}

/// The caller left or timed out, so the operation stopped without leaving partially completed
/// work.
///
/// Unlike [`Disconnected`], which reports a transport failure while the caller is still waiting,
/// this means nobody is waiting for the result.
#[derive(Debug, Clone, Error, FfiError)]
#[error("Operation cancelled")]
#[ffi_code(33)]
pub struct Cancelled;

// ---------------------------------------------------------------------------
// Repository state (40–55)
//
// The request is well-formed and permitted, but the repository is in a state
// that refuses it: nothing to do, history has moved on, or a rule protects the
// target.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Nothing staged for commit")]
#[ffi_code(40)]
pub struct NothingStaged;

#[derive(Debug, Clone, Error, FfiError)]
#[error("The branch's latest revision has moved on; sync to bring in its new revisions")]
#[ffi_code(41)]
pub struct BranchAdvanced;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch history is divergent")]
#[ffi_code(42)]
pub struct Divergent;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to commit when {path} is still in conflict")]
#[ffi_code(43)]
pub struct Conflict {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Local modifications prevent synchronization")]
#[ffi_code(44)]
pub struct LocalModifications;

#[derive(Debug, Clone, Error, FfiError)]
#[error("resource locked by somebody else")]
#[ffi_code(45)]
pub struct LockNotOwned;

#[derive(Debug, Clone, Error, FfiError)]
#[error("New metadata was identical to original")]
#[ffi_code(46)]
pub struct IdenticalMetadata;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to delete a protected branch: {branch}")]
#[ffi_code(47)]
pub struct DeleteProtected {
    pub branch: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Cannot delete the current branch: {branch}")]
#[ffi_code(48)]
pub struct DeleteCurrent {
    pub branch: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to delete default branch: {branch}")]
#[ffi_code(49)]
pub struct DeleteDefault {
    pub branch: String,
}

// Code 50 stays free so this block does not renumber.

#[derive(Debug, Clone, Error, FfiError)]
#[error("Mounts are busy: {paths}")]
#[ffi_code(51)]
pub struct MountsBusy {
    pub paths: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error(
    "Can't push the link at {path} (branch {branch}): the remote branch has newer revisions than yours"
)]
#[ffi_code(52)]
pub struct LinkDivergent {
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error(
    "The mounts of linked repository {repository} disagree on their revision, branch or flags: {mounts}; pin them at one revision with `lore link update <mount>`, and remove and add again a mount whose flags differ"
)]
#[ffi_code(53)]
pub struct LinkPinsDiverge {
    pub repository: String,
    pub mounts: String,
}

// ---------------------------------------------------------------------------
// Already exists (56–63)
//
// The operation would create something that is already there.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Target repository is already used in a layer")]
#[ffi_code(56)]
pub struct AlreadyLinked;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch {branch} already exists, use switch instead")]
#[ffi_code(57)]
pub struct BranchAlreadyExists {
    pub branch: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Repository already exist in path {path}")]
#[ffi_code(58)]
pub struct RepositoryAlreadyExists {
    pub path: String,
}

// ---------------------------------------------------------------------------
// Not found (79–99)
//
// A resource the operation named does not exist. The generic `NotFound` leads
// the block; the rest are ordered from content-addressed storage outwards to
// repository-level objects. The block starts at 79 because 64–78 belong to the
// BSD `sysexits.h` conventions.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Not found")]
#[ffi_code(79)]
pub struct NotFound;

#[derive(Clone, Error, FfiError)]
#[error("Address not found: {}", AddressNotFound::format_address(&self.address))]
#[ffi_code(80)]
pub struct AddressNotFound {
    /// Raw 48-byte address (32-byte hash + 16-byte context)
    pub address: [u8; 48],
}

impl AddressNotFound {
    fn format_address(address: &[u8; 48]) -> String {
        format!(
            "{}-{}",
            hex::encode(&address[..32]),
            hex::encode(&address[32..])
        )
    }
}

impl fmt::Debug for AddressNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AddressNotFound({})",
            Self::format_address(&self.address)
        )
    }
}

#[derive(Clone, Error, FfiError)]
#[error("Payload not found: {}", hex::encode(self.hash))]
#[ffi_code(81)]
pub struct PayloadNotFound {
    /// Raw 32-byte hash
    pub hash: [u8; 32],
}

impl fmt::Debug for PayloadNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PayloadNotFound({})", hex::encode(self.hash))
    }
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("file not found: {resource}")]
#[ffi_code(82)]
pub struct FileNotFound {
    pub resource: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Node not found")]
#[ffi_code(83)]
pub struct NodeNotFound;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Link not found")]
#[ffi_code(84)]
pub struct LinkNotFound;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Link not found at path: {path}")]
#[ffi_code(85)]
pub struct LinkPathNotFound {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Layer not found")]
#[ffi_code(86)]
pub struct LayerNotFound;

#[derive(Debug, Clone, Error, FfiError)]
#[error("branch not found: {branch}")]
#[ffi_code(87)]
pub struct BranchNotFound {
    pub branch: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("revision not found: {revision}")]
#[ffi_code(88)]
pub struct RevisionNotFound {
    pub revision: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Repository not found: {repository}")]
#[ffi_code(89)]
pub struct RepositoryNotFound {
    pub repository: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("A shared store was supposed to exist at {path}")]
#[ffi_code(90)]
pub struct SharedStoreNotFound {
    pub path: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("lock does not exist")]
#[ffi_code(91)]
pub struct LockNotFound;

#[derive(Debug, Clone, Error, FfiError)]
#[error(
    "Plugin '{plugin_name}' not found. Available plugins: {}",
    format_available_plugins(available_plugins)
)]
#[ffi_code(92)]
pub struct PluginNotFound {
    pub plugin_name: String,
    pub available_plugins: Vec<String>,
}

fn format_available_plugins(plugins: &[String]) -> String {
    if plugins.is_empty() {
        "none".to_string()
    } else {
        plugins.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Configuration (110–117)
//
// Something the operation needs was never configured, or was configured in a
// way that will not load.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("No commit identity configured; pass --identity or set identity in .lore/config.toml")]
#[ffi_code(110)]
pub struct MissingIdentity;

#[derive(Debug, Clone, Error, FfiError)]
#[error("No remote configured")]
#[ffi_code(111)]
pub struct NoRemote;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Plugin '{plugin_name}' configuration error: {message}")]
#[ffi_code(112)]
pub struct PluginConfigError {
    pub plugin_name: String,
    pub message: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Plugin '{plugin_name}' initialization failed: {message}")]
#[ffi_code(113)]
pub struct PluginInitError {
    pub plugin_name: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Resource limits (118–125)
//
// A size, depth, or efficiency bound was reached. The operation was refused on
// the bound, not on anything wrong with the request. This is the last block
// below 126, where the shell's own exit statuses begin.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Oversized: {context}")]
#[ffi_code(118)]
pub struct Oversized {
    pub context: String,
}

#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch history has reached maximum search depth")]
#[ffi_code(119)]
pub struct MaxHistorySearchDepth;

#[derive(Debug, Clone, Error, FfiError)]
#[error("Compression would be inefficient")]
#[ffi_code(120)]
pub struct InefficientCompression;

// ---------------------------------------------------------------------------
// Library lifecycle (193–200)
//
// The library itself cannot serve the call. The error is caused by the
// library's own state rather than the request, the caller's identity, or
// anything a remote did.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Error, FfiError)]
#[error("Lore has been shut down")]
#[ffi_code(193)]
pub struct ShutDown;

// ---------------------------------------------------------------------------
// The C names of the codes
// ---------------------------------------------------------------------------

/// The error codes Lore reports, each named after the error that carries it: the values of a
/// `Complete` event's `status`, of a synchronous call's return value and of
/// `lore_error_detail_t.error_code`.
///
/// cbindgen:prefix-with-name
/// cbindgen:rename-all=ScreamingSnakeCase
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    serde::Serialize,
    bitcode::Encode,
    bitcode::Decode,
    lore_macro::WireSchema,
)]
pub enum LoreErrorCode {
    /// No error; the operation succeeded.
    #[default]
    None = 0,
    /// The arguments supplied to the operation were invalid.
    InvalidArguments = 3,
    /// A path is not valid.
    InvalidPath = 4,
    /// An address is not valid.
    InvalidAddress = 5,
    /// A path does not name a link.
    NotALink = 6,
    /// A path does not name a layer.
    NotALayer = 7,
    /// A node was reached as the child of a node that is not its parent.
    InvalidNodeHierarchy = 8,
    /// The operation is not supported.
    NotSupported = 9,
    /// The caller is not authenticated.
    NotAuthenticated = 16,
    /// The caller is not authorized to access the repository.
    NotAuthorized = 17,
    /// No token is stored.
    TokenNotFound = 18,
    /// The operation requires write access.
    WriteRequired = 19,
    /// The connection to the server was lost.
    Disconnected = 28,
    /// The remote could not be reached.
    NotConnected = 29,
    /// The server is in maintenance mode.
    Maintenance = 30,
    /// The backing store is overloaded; the caller should retry later.
    SlowDown = 31,
    /// No Lore service could be reached or started, so the operation did not run.
    ServiceUnavailable = 32,
    /// The caller left or timed out, so the operation stopped.
    Cancelled = 33,
    /// Nothing is staged for commit.
    NothingStaged = 40,
    /// The branch's latest revision has moved on; sync to bring in its new revisions.
    BranchAdvanced = 41,
    /// The branch history is divergent.
    Divergent = 42,
    /// A path is still in conflict.
    Conflict = 43,
    /// Local modifications prevent the operation.
    LocalModifications = 44,
    /// The resource is locked by someone else.
    LockNotOwned = 45,
    /// The new metadata is identical to the original.
    IdenticalMetadata = 46,
    /// A protected branch can't be archived.
    DeleteProtected = 47,
    /// The current branch can't be archived.
    DeleteCurrent = 48,
    /// The repository's default branch can't be archived.
    DeleteDefault = 49,
    /// A mount the operation must change is busy.
    MountsBusy = 51,
    /// The remote branch of a link has revisions the local branch does not.
    LinkDivergent = 52,
    /// The mounts of a linked repository disagree on their revision, branch or flags.
    LinkPinsDiverge = 53,
    /// The target repository is already used in a layer.
    AlreadyLinked = 56,
    /// The branch already exists.
    BranchAlreadyExists = 57,
    /// A repository already exists at the path.
    RepositoryAlreadyExists = 58,
    /// A requested item was not found.
    NotFound = 79,
    /// A content-addressable object could not be found in any store.
    AddressNotFound = 80,
    /// A payload could not be found with its hash.
    PayloadNotFound = 81,
    /// A file path could not be resolved to a tracked node or found in the file system.
    FileNotFound = 82,
    /// A node was not found.
    NodeNotFound = 83,
    /// A link was not found.
    LinkNotFound = 84,
    /// No link is mounted at the path.
    LinkPathNotFound = 85,
    /// A layer was not found.
    LayerNotFound = 86,
    /// A branch was not found.
    BranchNotFound = 87,
    /// A revision was not found.
    RevisionNotFound = 88,
    /// A repository was not found.
    RepositoryNotFound = 89,
    /// No shared store exists at the path.
    SharedStoreNotFound = 90,
    /// A lock does not exist.
    LockNotFound = 91,
    /// A plugin was not found.
    PluginNotFound = 92,
    /// No commit identity is configured.
    MissingIdentity = 110,
    /// No remote is configured.
    NoRemote = 111,
    /// A plugin's configuration is not valid.
    PluginConfigError = 112,
    /// A plugin failed to initialize.
    PluginInitError = 113,
    /// A size bound refused the operation, such as a name over its length bound.
    Oversized = 118,
    /// A history search reached its maximum depth.
    MaxHistorySearchDepth = 119,
    /// Compression would be inefficient.
    InefficientCompression = 120,
    /// Lore has been shut down.
    ShutDown = 193,
    /// An internal error occurred.
    Internal = -1,
}

const _: () = assert!(LoreErrorCode::Internal as i32 == lore_error_set::Internal::FFI_CODE);

impl LoreErrorCode {
    /// The code `code` names: [`Self::None`] for `0`, and [`Self::Internal`] for a code no error
    /// type carries.
    pub const fn from_ffi_code(code: i32) -> Self {
        match code {
            0 => Self::None,
            InvalidArguments::FFI_CODE => Self::InvalidArguments,
            InvalidPath::FFI_CODE => Self::InvalidPath,
            InvalidAddress::FFI_CODE => Self::InvalidAddress,
            NotALink::FFI_CODE => Self::NotALink,
            NotALayer::FFI_CODE => Self::NotALayer,
            InvalidNodeHierarchy::FFI_CODE => Self::InvalidNodeHierarchy,
            NotSupported::FFI_CODE => Self::NotSupported,
            NotAuthenticated::FFI_CODE => Self::NotAuthenticated,
            NotAuthorized::FFI_CODE => Self::NotAuthorized,
            TokenNotFound::FFI_CODE => Self::TokenNotFound,
            WriteRequired::FFI_CODE => Self::WriteRequired,
            Disconnected::FFI_CODE => Self::Disconnected,
            NotConnected::FFI_CODE => Self::NotConnected,
            Maintenance::FFI_CODE => Self::Maintenance,
            SlowDown::FFI_CODE => Self::SlowDown,
            ServiceUnavailable::FFI_CODE => Self::ServiceUnavailable,
            Cancelled::FFI_CODE => Self::Cancelled,
            NothingStaged::FFI_CODE => Self::NothingStaged,
            BranchAdvanced::FFI_CODE => Self::BranchAdvanced,
            Divergent::FFI_CODE => Self::Divergent,
            Conflict::FFI_CODE => Self::Conflict,
            LocalModifications::FFI_CODE => Self::LocalModifications,
            LockNotOwned::FFI_CODE => Self::LockNotOwned,
            IdenticalMetadata::FFI_CODE => Self::IdenticalMetadata,
            DeleteProtected::FFI_CODE => Self::DeleteProtected,
            DeleteCurrent::FFI_CODE => Self::DeleteCurrent,
            DeleteDefault::FFI_CODE => Self::DeleteDefault,
            MountsBusy::FFI_CODE => Self::MountsBusy,
            LinkDivergent::FFI_CODE => Self::LinkDivergent,
            LinkPinsDiverge::FFI_CODE => Self::LinkPinsDiverge,
            AlreadyLinked::FFI_CODE => Self::AlreadyLinked,
            BranchAlreadyExists::FFI_CODE => Self::BranchAlreadyExists,
            RepositoryAlreadyExists::FFI_CODE => Self::RepositoryAlreadyExists,
            NotFound::FFI_CODE => Self::NotFound,
            AddressNotFound::FFI_CODE => Self::AddressNotFound,
            PayloadNotFound::FFI_CODE => Self::PayloadNotFound,
            FileNotFound::FFI_CODE => Self::FileNotFound,
            NodeNotFound::FFI_CODE => Self::NodeNotFound,
            LinkNotFound::FFI_CODE => Self::LinkNotFound,
            LinkPathNotFound::FFI_CODE => Self::LinkPathNotFound,
            LayerNotFound::FFI_CODE => Self::LayerNotFound,
            BranchNotFound::FFI_CODE => Self::BranchNotFound,
            RevisionNotFound::FFI_CODE => Self::RevisionNotFound,
            RepositoryNotFound::FFI_CODE => Self::RepositoryNotFound,
            SharedStoreNotFound::FFI_CODE => Self::SharedStoreNotFound,
            LockNotFound::FFI_CODE => Self::LockNotFound,
            PluginNotFound::FFI_CODE => Self::PluginNotFound,
            MissingIdentity::FFI_CODE => Self::MissingIdentity,
            NoRemote::FFI_CODE => Self::NoRemote,
            PluginConfigError::FFI_CODE => Self::PluginConfigError,
            PluginInitError::FFI_CODE => Self::PluginInitError,
            Oversized::FFI_CODE => Self::Oversized,
            MaxHistorySearchDepth::FFI_CODE => Self::MaxHistorySearchDepth,
            InefficientCompression::FFI_CODE => Self::InefficientCompression,
            ShutDown::FFI_CODE => Self::ShutDown,
            _ => Self::Internal,
        }
    }

    /// The code of `result`: [`Self::None`] on success, and the error's own code on failure.
    pub fn of<T, E: FfiError>(result: &Result<T, E>) -> Self {
        match result {
            Ok(_) => Self::None,
            Err(error) => Self::from_ffi_code(error.ffi_code()),
        }
    }
}
