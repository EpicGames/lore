// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! A call that sends branch metadata records logs one warning where it leaves entries out, and none
//! where it leaves none out.
//!
//! In its own binary: it installs the process-wide log callback.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore::interface::LoreGlobalArgs;
use lore::revision_graph::record::emit_branch_metadata;
use lore_base::log::LoreLogLevel;
use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::BranchId;
use lore_revision::interface::ExecutionContext;
use lore_revision::interface::LoreBranchLocation;
use lore_revision::metadata::Metadata;
use lore_revision::metadata::MetadataType;
use lore_revision::relay::EventDispatcher;

static WARNINGS: AtomicUsize = AtomicUsize::new(0);

/// Counts a warning from the module that sends the branch metadata records.
fn capture(level: LoreLogLevel, location: &str, _message: &str) {
    if level == LoreLogLevel::Warn && location == "lore::revision_graph::record" {
        WARNINGS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Appends an entry with type tag `tag`, which no setter can write, to `metadata`, which must
/// already hold an entry so its header is written.
fn append_raw_entry(metadata: &mut Metadata, key: &[u8], value: &[u8], tag: u32) {
    for field in [key.len() as u32, value.len() as u32, tag] {
        metadata.buffer.extend_from_slice(&field.to_ne_bytes());
    }
    metadata.buffer.extend_from_slice(key);
    metadata.buffer.extend_from_slice(value);
}

/// The warnings logged by one call over `metadata`, for the entry with `key` where it's `Some`.
fn warnings_of(metadata: &Metadata, key: Option<&str>) -> usize {
    emit_branch_metadata(
        1,
        LoreBranchLocation::Local,
        BranchId::from([7; 16]),
        metadata,
        key,
    );
    WARNINGS.swap(0, Ordering::Relaxed)
}

/// **A call logs one warning where it leaves out any entry, and none otherwise.** A non-UTF-8 key,
/// a value that doesn't decode as its type and a type tag no metadata type defines, between
/// readable entries, make one warning; a key that selects a readable entry or no entry makes none.
#[tokio::test]
async fn one_warning_for_a_call_that_leaves_entries_out() {
    lore_base::log::set_log_level(LoreLogLevel::Warn);
    lore_base::log::set_log_callback(Some(capture));

    let mut readable = Metadata::new();
    readable.set_string("first", "one").expect("sets the entry");
    readable.set_string("last", "two").expect("sets the entry");
    let mut unreadable = Metadata::new();
    unreadable
        .set_string("first", "one")
        .expect("sets the entry");
    unreadable
        .set(b"\xe4\xb8", b"value", MetadataType::String)
        .expect("sets the entry");
    unreadable
        .set(b"short", &[1; 31], MetadataType::Hash)
        .expect("sets the entry");
    append_raw_entry(&mut unreadable, b"unknown", b"value", 9);
    unreadable
        .set_string("last", "two")
        .expect("sets the entry");
    unreadable
        .check_buffer()
        .expect("the entries are well formed");

    let execution = Arc::new(ExecutionContext::new_client(
        LoreGlobalArgs::default(),
        EventDispatcher::no_dispatch(),
    ));
    let warnings = LORE_CONTEXT
        .scope(execution, async {
            [
                warnings_of(&readable, None),
                warnings_of(&unreadable, None),
                warnings_of(&unreadable, Some("first")),
                warnings_of(&unreadable, Some("short")),
                warnings_of(&unreadable, Some("absent")),
            ]
        })
        .await;
    lore_base::log::set_log_callback(None);

    assert_eq!(warnings, [0, 1, 0, 1, 0]);
}
