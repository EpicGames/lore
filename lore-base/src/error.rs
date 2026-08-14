// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Consolidated FFI error types for the Lore system.
//!
//! All discrete error types with FFI codes are defined here,
//! providing a single source of truth for FFI error code allocation.
//! This is the base error crate with no dependency on lore-storage.

use std::fmt;

use lore_error_set::FfiError;
use thiserror::Error;

// FFI code 1
#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid arguments: {reason}")]
#[ffi_code(1)]
pub struct InvalidArguments {
    pub reason: String,
}

// FFI code 2
#[derive(Clone, Error, FfiError)]
#[error("Address not found: {}", AddressNotFound::format_address(&self.address))]
#[ffi_code(2)]
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

// FFI code 3
#[derive(Debug, Clone, Error, FfiError)]
#[error("file not found: {resource}")]
#[ffi_code(3)]
pub struct FileNotFound {
    pub resource: String,
}

// FFI code 4
#[derive(Clone, Error, FfiError)]
#[error("Payload not found: {}", hex::encode(self.hash))]
#[ffi_code(4)]
pub struct PayloadNotFound {
    /// Raw 32-byte hash
    pub hash: [u8; 32],
}

impl fmt::Debug for PayloadNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PayloadNotFound({})", hex::encode(self.hash))
    }
}

// FFI code 5
#[derive(Debug, Clone, Error, FfiError)]
#[error("Store overloaded, slow down")]
#[ffi_code(5)]
pub struct SlowDown;

// FFI code 6
#[derive(Debug, Clone, Error, FfiError)]
#[error("Disconnected from server")]
#[ffi_code(6)]
pub struct Disconnected;

// FFI code 7
#[derive(Debug, Clone, Error, FfiError)]
#[error("Not authorized to access repository")]
#[ffi_code(7)]
pub struct NotAuthorized;

// FFI code 8
#[derive(Debug, Clone, Error, FfiError)]
#[error("lock does not exist")]
#[ffi_code(8)]
pub struct LockNotFound;

// FFI code 9
#[derive(Debug, Clone, Error, FfiError)]
#[error("resource locked by somebody else")]
#[ffi_code(9)]
pub struct LockNotOwned;

// FFI code 10
#[derive(Debug, Clone, Error, FfiError)]
#[error("A shared store was supposed to exist at {path}")]
#[ffi_code(10)]
pub struct SharedStoreNotFound {
    pub path: String,
}

// FFI code 11
#[derive(Debug, Clone, Error, FfiError)]
#[error("Server is in maintenance mode")]
#[ffi_code(11)]
pub struct Maintenance;

// FFI code 12
#[derive(Debug, Clone, Error, FfiError)]
#[error("Not authenticated")]
#[ffi_code(12)]
pub struct NotAuthenticated;

// FFI code 13
#[derive(Debug, Clone, Error, FfiError)]
#[error("Not found")]
#[ffi_code(13)]
pub struct NotFound;

// FFI code 14
#[derive(Debug, Clone, Error, FfiError)]
#[error("No remote configured")]
#[ffi_code(14)]
pub struct NoRemote;

// FFI code 15
#[derive(Debug, Clone, Error, FfiError)]
#[error("Node not found")]
#[ffi_code(15)]
pub struct NodeNotFound;

// FFI code 16
#[derive(Debug, Clone, Error, FfiError)]
#[error("Link not found")]
#[ffi_code(16)]
pub struct LinkNotFound;

// FFI code 17
#[derive(Debug, Clone, Error, FfiError)]
#[error("Not connected to remote: {reason}")]
#[ffi_code(17)]
pub struct NotConnected {
    pub reason: String,
}

// FFI code 18
#[derive(Debug, Clone, Error, FfiError)]
#[error("Operation not supported: {operation}")]
#[ffi_code(18)]
pub struct NotSupported {
    pub operation: String,
}

// FFI code 19
#[derive(Debug, Clone, Error, FfiError)]
#[error("Target repository is already used in a layer")]
#[ffi_code(19)]
pub struct AlreadyLinked;

// FFI code 20
#[derive(Debug, Clone, Error, FfiError)]
#[error("Layer not found")]
#[ffi_code(20)]
pub struct LayerNotFound;

// FFI code 21
#[derive(Debug, Clone, Error, FfiError)]
#[error("Nothing staged for commit")]
#[ffi_code(21)]
pub struct NothingStaged;

// FFI code 22
#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch has been advanced by another instance, sync and re-stage to commit")]
#[ffi_code(22)]
pub struct BranchAdvanced;

// FFI code 23
#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to commit when {path} is still in conflict")]
#[ffi_code(23)]
pub struct Conflict {
    pub path: String,
}

// FFI code 24
#[derive(Debug, Clone, Error, FfiError)]
#[error("Link not found at path: {path}")]
#[ffi_code(24)]
pub struct LinkPathNotFound {
    pub path: String,
}

// FFI code 25
#[derive(Debug, Clone, Error, FfiError)]
#[error("Path is not a link: {path}")]
#[ffi_code(25)]
pub struct NotALink {
    pub path: String,
}

// FFI code 26
#[derive(Debug, Clone, Error, FfiError)]
#[error("Oversized: {context}")]
#[ffi_code(26)]
pub struct Oversized {
    pub context: String,
}

// FFI code 27
#[derive(Debug, Clone, Error, FfiError)]
#[error(
    "Plugin '{plugin_name}' not found. Available plugins: {}",
    format_available_plugins(available_plugins)
)]
#[ffi_code(27)]
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

// FFI code 28
#[derive(Debug, Clone, Error, FfiError)]
#[error("Plugin '{plugin_name}' configuration error: {message}")]
#[ffi_code(28)]
pub struct PluginConfigError {
    pub plugin_name: String,
    pub message: String,
}

// FFI code 29
#[derive(Debug, Clone, Error, FfiError)]
#[error("Plugin '{plugin_name}' initialization failed: {message}")]
#[ffi_code(29)]
pub struct PluginInitError {
    pub plugin_name: String,
    pub message: String,
}

// FFI code 30
#[derive(Debug, Clone, Error, FfiError)]
#[error("Operation requires write access")]
#[ffi_code(30)]
pub struct WriteRequired;

// FFI code 31
#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid path: {path}")]
#[ffi_code(31)]
pub struct InvalidPath {
    pub path: String,
}

// FFI code 32
#[derive(Debug, Clone, Error, FfiError)]
#[error("invalid address: {address}")]
#[ffi_code(32)]
pub struct InvalidAddress {
    pub address: String,
}

// FFI code 33
#[derive(Debug, Clone, Error, FfiError)]
#[error("revision not found: {revision}")]
#[ffi_code(33)]
pub struct RevisionNotFound {
    pub revision: String,
}

// FFI code 34
#[derive(Debug, Clone, Error, FfiError)]
#[error("branch not found: {branch}")]
#[ffi_code(34)]
pub struct BranchNotFound {
    pub branch: String,
}
// FFI code 35
#[derive(Debug, Clone, Error, FfiError)]
#[error("New metadata was identical to original")]
#[ffi_code(35)]
pub struct IdenticalMetadata;

// FFI code 36
#[derive(Debug, Clone, Error, FfiError)]
#[error("No token stored")]
#[ffi_code(36)]
pub struct TokenNotFound;

// FFI code 37
#[derive(Debug, Clone, Error, FfiError)]
#[error("Path is not a layer: {path}")]
#[ffi_code(37)]
pub struct NotALayer {
    pub path: String,
}

// FFI code 38
#[derive(Debug, Clone, Error, FfiError)]
#[error("Node {node} has parent {actual_parent} but was reached as a child of {expected_parent}")]
#[ffi_code(38)]
pub struct InvalidNodeHierarchy {
    pub node: u32,
    pub expected_parent: u32,
    pub actual_parent: u32,
}

// FFI code 39
#[derive(Debug, Clone, Error, FfiError)]
#[error("Local modifications prevent synchronization")]
#[ffi_code(39)]
pub struct LocalModifications;

// FFI code 40
#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch {branch} already exists, use switch instead")]
#[ffi_code(40)]
pub struct BranchAlreadyExists {
    pub branch: String,
}

// FFI code 41
#[derive(Debug, Clone, Error, FfiError)]
#[error("Repository already exist in path {path}")]
#[ffi_code(41)]
pub struct RepositoryAlreadyExists {
    pub path: String,
}

// FFI code 42
#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to delete a protected branch: {branch}")]
#[ffi_code(42)]
pub struct DeleteProtected {
    pub branch: String,
}

// FFI code 43
#[derive(Debug, Clone, Error, FfiError)]
#[error("Cannot delete the current branch: {branch}")]
#[ffi_code(43)]
pub struct DeleteCurrent {
    pub branch: String,
}

// FFI code 44
#[derive(Debug, Clone, Error, FfiError)]
#[error("Unable to delete default branch: {branch}")]
#[ffi_code(44)]
pub struct DeleteDefault {
    pub branch: String,
}

// FFI code 45
#[derive(Debug, Clone, Error, FfiError)]
#[error("Repository not found: {repository}")]
#[ffi_code(45)]
pub struct RepositoryNotFound {
    pub repository: String,
}

// FFI code 46
#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch history is divergent")]
#[ffi_code(46)]
pub struct Divergent;

// FFI code 47
#[derive(Debug, Clone, Error, FfiError)]
#[error("Branch history has reached maximum search depth")]
#[ffi_code(47)]
pub struct MaxHistorySearchDepth;

// FFI code 48
#[derive(Debug, Clone, Error, FfiError)]
#[error("No commit identity configured; pass --identity or set identity in .lore/config.toml")]
#[ffi_code(48)]
pub struct MissingIdentity;

// FFI code 49
#[derive(Debug, Clone, Error, FfiError)]
#[error("Compression would be inefficient")]
#[ffi_code(49)]
pub struct InefficientCompression;
