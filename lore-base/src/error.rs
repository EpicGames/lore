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
//! | 100–109 | *reserved* | The legacy `LoreError` categories (101, 102, 103). |
//! | 110–117 | Configuration | Something the operation needs was never configured. |
//! | 118–125 | Resource limits | A size, depth, or efficiency bound was hit. |
//! | 126–128 | *reserved* | The shell's "found but not executable", "not found", and "bad argument to exit". |
//! | 129–192 | *reserved* | `128 + signal`, i.e. killed by a signal. 130 is Ctrl-C, 137 is `SIGKILL`, 143 is `SIGTERM`. |
//! | 193–254 | *reserved* | Free for future groups. |
//! | 255 | *reserved* | `Internal`, which is `-1` truncated to a `u8`. |
//!
//! The trait carries a code as `i32` rather than a `u8` because `Internal` is
//! `-1`, which is not a code allocated here.
//!
//! Add a new error to the block that matches its group and take the next free
//! code in that block. Never renumber an existing code to close a gap: the gaps
//! are the headroom that keeps a group contiguous, and the tests below hold
//! every block clear of the reserved values so anything taken from that
//! headroom is a usable exit status. New groups come out of 193–254. A type
//! whose name ends in `NotFound` belongs in the not-found block even when it
//! also belongs to a subsystem that has its own errors elsewhere —
//! `PluginNotFound` sits with the other not-found codes, not with the other
//! plugin codes.

use std::fmt;

use lore_error_set::FfiError;
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
// The remote could not be reached, dropped the connection, or is reachable but
// declining work. Retrying later is often the right response.
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
#[error("Branch has been advanced by another instance, sync and re-stage to commit")]
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
// way that will not load. The block starts at 110 because 100–109 is reserved
// for the legacy `LoreError` categories, which share this numeric space.
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

#[cfg(test)]
mod tests {
    use std::ops::RangeInclusive;

    use super::*;

    /// The group a code is allocated from, and the block it owns.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Group {
        Input,
        Auth,
        Connectivity,
        RepositoryState,
        AlreadyExists,
        NotFound,
        Configuration,
        ResourceLimits,
    }

    impl Group {
        const ALL: &'static [Self] = &[
            Self::Input,
            Self::Auth,
            Self::Connectivity,
            Self::RepositoryState,
            Self::AlreadyExists,
            Self::NotFound,
            Self::Configuration,
            Self::ResourceLimits,
        ];

        fn block(self) -> RangeInclusive<i32> {
            match self {
                Self::Input => 3..=15,
                Self::Auth => 16..=27,
                Self::Connectivity => 28..=39,
                Self::RepositoryState => 40..=55,
                Self::AlreadyExists => 56..=63,
                Self::NotFound => 79..=99,
                Self::Configuration => 110..=117,
                Self::ResourceLimits => 118..=125,
            }
        }
    }

    /// Values no discrete error code may take, and why.
    ///
    /// The CLI returns a failing error's code as the process exit status
    /// (`ExitCode::from(code as u8)` in `lore-client`), so a code landing on one
    /// of these is indistinguishable from something the shell, the OS, or the
    /// CLI's own startup path reports.
    const RESERVED: &[(RangeInclusive<i32>, &str)] = &[
        (0..=0, "success"),
        (
            1..=1,
            "the CLI's ExitCode::FAILURE, and the shell's generic error",
        ),
        (
            2..=2,
            "the CLI's usage exit, and the shell's misuse-of-a-builtin",
        ),
        (64..=78, "the BSD sysexits.h codes"),
        (100..=109, "the legacy LoreError categories"),
        (
            126..=128,
            "the shell's not-executable, not-found, and bad-exit-argument statuses",
        ),
        (129..=192, "128 + signal number: killed by a signal"),
        (255..=255, "Internal, which is -1 truncated to a u8"),
    ];

    /// Every discrete error type, the group it was allocated from, and the code
    /// it carries. A new type belongs here too — the tests below hold the
    /// grouping and exit-status invariants, and only cover what this list names.
    fn registry() -> Vec<(&'static str, Group, i32)> {
        fn text() -> String {
            "test".to_string()
        }

        vec![
            (
                "InvalidArguments",
                Group::Input,
                InvalidArguments { reason: text() }.ffi_code(),
            ),
            (
                "InvalidPath",
                Group::Input,
                InvalidPath { path: text() }.ffi_code(),
            ),
            (
                "InvalidAddress",
                Group::Input,
                InvalidAddress { address: text() }.ffi_code(),
            ),
            (
                "NotALink",
                Group::Input,
                NotALink { path: text() }.ffi_code(),
            ),
            (
                "NotALayer",
                Group::Input,
                NotALayer { path: text() }.ffi_code(),
            ),
            (
                "InvalidNodeHierarchy",
                Group::Input,
                InvalidNodeHierarchy {
                    node: 1,
                    expected_parent: 2,
                    actual_parent: 3,
                }
                .ffi_code(),
            ),
            (
                "NotSupported",
                Group::Input,
                NotSupported { operation: text() }.ffi_code(),
            ),
            ("NotAuthenticated", Group::Auth, NotAuthenticated.ffi_code()),
            ("NotAuthorized", Group::Auth, NotAuthorized.ffi_code()),
            ("TokenNotFound", Group::Auth, TokenNotFound.ffi_code()),
            ("WriteRequired", Group::Auth, WriteRequired.ffi_code()),
            ("Disconnected", Group::Connectivity, Disconnected.ffi_code()),
            (
                "NotConnected",
                Group::Connectivity,
                NotConnected { reason: text() }.ffi_code(),
            ),
            ("Maintenance", Group::Connectivity, Maintenance.ffi_code()),
            ("SlowDown", Group::Connectivity, SlowDown.ffi_code()),
            (
                "NothingStaged",
                Group::RepositoryState,
                NothingStaged.ffi_code(),
            ),
            (
                "BranchAdvanced",
                Group::RepositoryState,
                BranchAdvanced.ffi_code(),
            ),
            ("Divergent", Group::RepositoryState, Divergent.ffi_code()),
            (
                "Conflict",
                Group::RepositoryState,
                Conflict { path: text() }.ffi_code(),
            ),
            (
                "LocalModifications",
                Group::RepositoryState,
                LocalModifications.ffi_code(),
            ),
            (
                "LockNotOwned",
                Group::RepositoryState,
                LockNotOwned.ffi_code(),
            ),
            (
                "IdenticalMetadata",
                Group::RepositoryState,
                IdenticalMetadata.ffi_code(),
            ),
            (
                "DeleteProtected",
                Group::RepositoryState,
                DeleteProtected { branch: text() }.ffi_code(),
            ),
            (
                "DeleteCurrent",
                Group::RepositoryState,
                DeleteCurrent { branch: text() }.ffi_code(),
            ),
            (
                "DeleteDefault",
                Group::RepositoryState,
                DeleteDefault { branch: text() }.ffi_code(),
            ),
            (
                "AlreadyLinked",
                Group::AlreadyExists,
                AlreadyLinked.ffi_code(),
            ),
            (
                "BranchAlreadyExists",
                Group::AlreadyExists,
                BranchAlreadyExists { branch: text() }.ffi_code(),
            ),
            (
                "RepositoryAlreadyExists",
                Group::AlreadyExists,
                RepositoryAlreadyExists { path: text() }.ffi_code(),
            ),
            ("NotFound", Group::NotFound, NotFound.ffi_code()),
            (
                "AddressNotFound",
                Group::NotFound,
                AddressNotFound { address: [0; 48] }.ffi_code(),
            ),
            (
                "PayloadNotFound",
                Group::NotFound,
                PayloadNotFound { hash: [0; 32] }.ffi_code(),
            ),
            (
                "FileNotFound",
                Group::NotFound,
                FileNotFound { resource: text() }.ffi_code(),
            ),
            ("NodeNotFound", Group::NotFound, NodeNotFound.ffi_code()),
            ("LinkNotFound", Group::NotFound, LinkNotFound.ffi_code()),
            (
                "LinkPathNotFound",
                Group::NotFound,
                LinkPathNotFound { path: text() }.ffi_code(),
            ),
            ("LayerNotFound", Group::NotFound, LayerNotFound.ffi_code()),
            (
                "BranchNotFound",
                Group::NotFound,
                BranchNotFound { branch: text() }.ffi_code(),
            ),
            (
                "RevisionNotFound",
                Group::NotFound,
                RevisionNotFound { revision: text() }.ffi_code(),
            ),
            (
                "RepositoryNotFound",
                Group::NotFound,
                RepositoryNotFound { repository: text() }.ffi_code(),
            ),
            (
                "SharedStoreNotFound",
                Group::NotFound,
                SharedStoreNotFound { path: text() }.ffi_code(),
            ),
            ("LockNotFound", Group::NotFound, LockNotFound.ffi_code()),
            (
                "PluginNotFound",
                Group::NotFound,
                PluginNotFound {
                    plugin_name: text(),
                    available_plugins: Vec::new(),
                }
                .ffi_code(),
            ),
            (
                "MissingIdentity",
                Group::Configuration,
                MissingIdentity.ffi_code(),
            ),
            ("NoRemote", Group::Configuration, NoRemote.ffi_code()),
            (
                "PluginConfigError",
                Group::Configuration,
                PluginConfigError {
                    plugin_name: text(),
                    message: text(),
                }
                .ffi_code(),
            ),
            (
                "PluginInitError",
                Group::Configuration,
                PluginInitError {
                    plugin_name: text(),
                    message: text(),
                }
                .ffi_code(),
            ),
            (
                "Oversized",
                Group::ResourceLimits,
                Oversized { context: text() }.ffi_code(),
            ),
            (
                "MaxHistorySearchDepth",
                Group::ResourceLimits,
                MaxHistorySearchDepth.ffi_code(),
            ),
            (
                "InefficientCompression",
                Group::ResourceLimits,
                InefficientCompression.ffi_code(),
            ),
        ]
    }

    #[test]
    fn every_code_sits_in_its_group_block() {
        for (name, group, code) in registry() {
            assert!(
                group.block().contains(&code),
                "{name} has code {code}, outside the {:?} block {:?}",
                group,
                group.block()
            );
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut seen: Vec<(&'static str, i32)> = Vec::new();
        for (name, _group, code) in registry() {
            if let Some((other, _)) = seen.iter().find(|(_, taken)| *taken == code) {
                panic!("{name} and {other} both use code {code}");
            }
            seen.push((name, code));
        }
    }

    #[test]
    fn every_code_survives_the_cast_to_a_process_exit_status() {
        for (name, _group, code) in registry() {
            let truncated = code as u8;
            assert_eq!(
                i32::from(truncated),
                code,
                "{name} has code {code}, which the CLI's `code as u8` would report as {truncated}"
            );
        }
    }

    #[test]
    fn no_code_lands_on_a_reserved_exit_status() {
        for (name, _group, code) in registry() {
            for (range, reason) in RESERVED {
                assert!(
                    !range.contains(&code),
                    "{name} has code {code}, which is reserved for {reason}"
                );
            }
        }
    }

    /// The headroom in a block is where the next error type's code comes from,
    /// so it is the block — not just the codes in use today — that has to stay
    /// clear of the reserved statuses.
    #[test]
    fn no_group_block_overlaps_a_reserved_exit_status() {
        for group in Group::ALL {
            let block = group.block();
            for (range, reason) in RESERVED {
                let overlap = block.start().max(range.start())..=block.end().min(range.end());
                assert!(
                    overlap.is_empty(),
                    "the {group:?} block {block:?} overlaps {overlap:?}, reserved for {reason}"
                );
            }
        }
    }

    #[test]
    fn group_blocks_do_not_overlap_each_other() {
        for (index, group) in Group::ALL.iter().enumerate() {
            for other in &Group::ALL[index + 1..] {
                let (block, other_block) = (group.block(), other.block());
                let overlap =
                    block.start().max(other_block.start())..=block.end().min(other_block.end());
                assert!(
                    overlap.is_empty(),
                    "the {group:?} block {block:?} and the {other:?} block {other_block:?} overlap at {overlap:?}"
                );
            }
        }
    }
}
