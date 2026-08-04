// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod anchor;
pub mod auth;
pub mod branch;
pub mod change;
pub mod cluster;
pub mod commit;
pub mod dependency;
pub mod diff;
pub mod environment;
pub mod error;
pub mod errors;
pub mod event;
pub mod file;
pub mod filter;
pub mod find;
pub mod fragment;
pub mod fs;
pub mod global;
pub mod hash;
pub mod history;
pub mod immutable;
pub mod infer;
pub mod instance;
pub mod interface;
pub mod layer;
pub mod link;
pub mod lock;
pub mod logging;
pub mod shared_store;
// Re-export lore_base for use by lore_ macros when expanded in downstream crates
#[doc(hidden)]
pub use lore_base;
pub mod lore;
pub mod merge;
pub mod merge_carry;
pub mod merge_resolution;
pub mod metadata;
pub mod nametable;
pub mod node;
pub mod notification;
pub mod path;
pub mod path_merge;
pub mod progress;
pub mod proto;
pub mod protocol;
pub mod relay;
pub mod repository;
pub mod revision;
pub mod revision_tree;
pub mod runtime;
pub mod stage;
pub mod state;
pub mod store;
pub mod util;

#[cfg(all(target_family = "windows", feature = "vfs"))]
pub mod projfs;
//#[cfg(all(target_family = "windows", feature = "vfs"))]
//pub mod swfs;

pub use lore_base::lore_drain_tasks;
pub use lore_base::lore_limit_drain_tasks;
pub use lore_base::lore_spawn_blocking;
pub use lore_base::lore_spawn_blocking_nocontext;
