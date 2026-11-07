// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::ops::RangeInclusive;

use lore_base::error::*;
use lore_error_set::ffi::FfiError;

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
    LibraryLifecycle,
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
        Self::LibraryLifecycle,
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
            Self::LibraryLifecycle => 193..=200,
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
            "ServiceUnavailable",
            Group::Connectivity,
            ServiceUnavailable { reason: text() }.ffi_code(),
        ),
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
            "MountsBusy",
            Group::RepositoryState,
            MountsBusy { paths: text() }.ffi_code(),
        ),
        (
            "LinkDivergent",
            Group::RepositoryState,
            LinkDivergent {
                path: text(),
                branch: text(),
            }
            .ffi_code(),
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
        ("ShutDown", Group::LibraryLifecycle, ShutDown.ffi_code()),
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

/// The remote's refusal of the caller, told apart from every other failure and forwarded with its
/// variant.
mod refusal {
    use lore_base::error::*;
    use lore_error_set::prelude::*;

    #[error_set]
    enum Remote {
        NotAuthenticated,
        NotAuthorized,
        Disconnected,
    }

    #[error_set]
    enum Command {
        NotAuthenticated,
        NotAuthorized,
        FileNotFound,
    }

    fn file_not_found(error: Remote) -> Command {
        Command::FileNotFound(
            FileNotFound {
                resource: "file".to_string(),
            }
            .chain_err_from(error, "resolving the file"),
        )
    }

    #[test]
    fn a_refusal_is_unauthenticated_or_unauthorized() {
        assert!(is_refusal(&Remote::from(NotAuthenticated)));
        assert!(is_refusal(&Remote::from(NotAuthorized)));
        assert!(!is_refusal(&Remote::from(Disconnected)));
        assert!(!is_refusal(&Remote::internal("failed")));
    }

    /// A refusal is forwarded with its variant, and every other failure takes the mapping the
    /// caller gives, here the not-found a lookup reports.
    #[test]
    fn a_refusal_is_forwarded_and_any_other_failure_mapped() {
        let forward = |error| forward_refusal_or(error, "resolving the file", file_not_found);

        let error = forward(Remote::from(NotAuthenticated));
        assert!(error.is_not_authenticated(), "{error:?}");
        let error = forward(Remote::from(NotAuthorized));
        assert!(error.is_not_authorized(), "{error:?}");
        let error = forward(Remote::from(Disconnected));
        assert!(error.is_file_not_found(), "{error:?}");
        let error = forward(Remote::internal("failed"));
        assert!(error.is_file_not_found(), "{error:?}");
    }
}
