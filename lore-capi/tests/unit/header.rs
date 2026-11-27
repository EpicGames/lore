// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Checks of the generated C header, `lore.h`, against the values C callers compile in.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The value of every event tag the header declares. A tag keeps its value while the header
/// declares it, and a new tag takes a value above the highest here, so a caller compiled against
/// one header reads the events of a library built from another.
const EVENT_TAGS: &[(&str, i64)] = &[
    ("LORE_EVENT_PROGRESS", 0),
    ("LORE_EVENT_COMPLETE", 2),
    ("LORE_EVENT_METADATA", 3),
    ("LORE_EVENT_LOG", 4),
    ("LORE_EVENT_END", 5),
    ("LORE_EVENT_MAINTENANCE", 6),
    ("LORE_EVENT_AUTH_URL", 7),
    ("LORE_EVENT_AUTH_USER_INFO", 8),
    ("LORE_EVENT_AUTH_USER_TOKEN", 9),
    ("LORE_EVENT_AUTH_IDENTITY", 10),
    ("LORE_EVENT_BRANCH_CREATE", 11),
    ("LORE_EVENT_BRANCH_MULTIPLE_INSTANCE", 12),
    ("LORE_EVENT_BRANCH_ARCHIVE", 13),
    ("LORE_EVENT_BRANCH_LIST_BEGIN", 14),
    ("LORE_EVENT_BRANCH_LIST_ENTRY", 15),
    ("LORE_EVENT_BRANCH_LIST_END", 16),
    ("LORE_EVENT_BRANCH_MERGE_ABORT_BEGIN", 17),
    ("LORE_EVENT_BRANCH_MERGE_ABORT_END", 18),
    ("LORE_EVENT_BRANCH_INFO", 19),
    ("LORE_EVENT_BRANCH_DIFF_BEGIN", 20),
    ("LORE_EVENT_BRANCH_DIFF_CHANGE_BEGIN", 21),
    ("LORE_EVENT_BRANCH_DIFF_CHANGE", 22),
    ("LORE_EVENT_BRANCH_DIFF_CHANGE_END", 23),
    ("LORE_EVENT_BRANCH_DIFF_CONFLICT_BEGIN", 24),
    ("LORE_EVENT_BRANCH_DIFF_CONFLICT", 25),
    ("LORE_EVENT_BRANCH_DIFF_CONFLICT_END", 26),
    ("LORE_EVENT_BRANCH_DIFF_END", 27),
    ("LORE_EVENT_BRANCH_LATEST_LIST_ENTRY", 28),
    ("LORE_EVENT_BRANCH_MERGE_CONFLICT_FILE", 29),
    ("LORE_EVENT_BRANCH_MERGE_LINK_SKIPPED", 30),
    ("LORE_EVENT_BRANCH_MERGE_UNRESOLVE_FILE", 31),
    ("LORE_EVENT_BRANCH_MERGE_UNRESOLVE_REVISION", 32),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FILE_BEGIN", 33),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FILE", 34),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FILE_END", 35),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_BEGIN", 36),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_PROGRESS", 37),
    ("LORE_EVENT_BRANCH_MERGE_INTO_FRAGMENT_END", 38),
    ("LORE_EVENT_BRANCH_MERGE_INTO_REVISION", 39),
    ("LORE_EVENT_BRANCH_MERGE_INTO_SYNC_BEGIN", 40),
    ("LORE_EVENT_BRANCH_MERGE_INTO_SYNC_END", 41),
    ("LORE_EVENT_BRANCH_MERGE_RESOLVE_FILE", 42),
    ("LORE_EVENT_BRANCH_MERGE_RESOLVE_REVISION", 43),
    ("LORE_EVENT_BRANCH_MERGE_START_BEGIN", 44),
    ("LORE_EVENT_BRANCH_MERGE_START_END", 45),
    ("LORE_EVENT_CHERRY_PICK_START_BEGIN", 46),
    ("LORE_EVENT_CHERRY_PICK_START_END", 47),
    ("LORE_EVENT_CHERRY_PICK_ABORT_BEGIN", 48),
    ("LORE_EVENT_CHERRY_PICK_ABORT_END", 49),
    ("LORE_EVENT_CHERRY_PICK_CONFLICT_FILE", 50),
    ("LORE_EVENT_CHERRY_PICK_UNRESOLVE_FILE", 51),
    ("LORE_EVENT_CHERRY_PICK_UNRESOLVE_REVISION", 52),
    ("LORE_EVENT_CHERRY_PICK_RESOLVE_FILE", 53),
    ("LORE_EVENT_CHERRY_PICK_RESOLVE_REVISION", 54),
    ("LORE_EVENT_REVERT_START_BEGIN", 55),
    ("LORE_EVENT_REVERT_START_END", 56),
    ("LORE_EVENT_REVERT_ABORT_BEGIN", 57),
    ("LORE_EVENT_REVERT_ABORT_END", 58),
    ("LORE_EVENT_REVERT_RESOLVE_FILE", 59),
    ("LORE_EVENT_REVERT_RESOLVE_REVISION", 60),
    ("LORE_EVENT_REVERT_CONFLICT_FILE", 61),
    ("LORE_EVENT_REVERT_UNRESOLVE_FILE", 62),
    ("LORE_EVENT_REVERT_UNRESOLVE_REVISION", 63),
    ("LORE_EVENT_BRANCH_PROTECT", 64),
    ("LORE_EVENT_BRANCH_PUSH", 65),
    ("LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_BEGIN", 66),
    ("LORE_EVENT_BRANCH_PUSH_REVISION_UPDATE_END", 67),
    ("LORE_EVENT_BRANCH_PUSH_FRAGMENT_BEGIN", 68),
    ("LORE_EVENT_BRANCH_PUSH_FRAGMENT_PROGRESS", 69),
    ("LORE_EVENT_BRANCH_PUSH_FRAGMENT_END", 70),
    ("LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_BEGIN", 71),
    ("LORE_EVENT_BRANCH_PUSH_BRANCH_CREATE_END", 72),
    ("LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_BEGIN", 73),
    ("LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_UPDATE", 74),
    ("LORE_EVENT_BRANCH_PUSH_REVISION_PUSH_END", 75),
    ("LORE_EVENT_BRANCH_RESET", 76),
    ("LORE_EVENT_BRANCH_SWITCH_BEGIN", 77),
    ("LORE_EVENT_BRANCH_SWITCH_END", 78),
    ("LORE_EVENT_BRANCH_UNPROTECT", 79),
    ("LORE_EVENT_FILE_INFO", 80),
    ("LORE_EVENT_FILE_DIFF", 81),
    ("LORE_EVENT_FILE_HASH", 82),
    ("LORE_EVENT_FILE_HISTORY", 83),
    ("LORE_EVENT_FILE_WRITE", 84),
    ("LORE_EVENT_FILE_OBLITERATE", 85),
    ("LORE_EVENT_FILE_DUMP", 86),
    ("LORE_EVENT_FILE_DEPENDENCY_ADD_BEGIN", 87),
    ("LORE_EVENT_FILE_DEPENDENCY_ADD_ENTRY", 88),
    ("LORE_EVENT_FILE_DEPENDENCY_ADD_END", 89),
    ("LORE_EVENT_FILE_DEPENDENCY_REMOVE_BEGIN", 90),
    ("LORE_EVENT_FILE_DEPENDENCY_REMOVE_ENTRY", 91),
    ("LORE_EVENT_FILE_DEPENDENCY_REMOVE_END", 92),
    ("LORE_EVENT_FILE_DEPENDENCY_LIST_BEGIN", 93),
    ("LORE_EVENT_FILE_DEPENDENCY_LIST_FILE", 94),
    ("LORE_EVENT_FILE_DEPENDENCY_LIST_ENTRY", 95),
    ("LORE_EVENT_FILE_DEPENDENCY_LIST_FILE_END", 96),
    ("LORE_EVENT_FILE_DEPENDENCY_LIST_END", 97),
    ("LORE_EVENT_FILE_RESET_BEGIN", 98),
    ("LORE_EVENT_FILE_RESET_PROGRESS", 99),
    ("LORE_EVENT_FILE_RESET_END", 100),
    ("LORE_EVENT_FILE_RESET_FILE", 101),
    ("LORE_EVENT_FILTER_EXCLUDE", 102),
    ("LORE_EVENT_FILE_STAGE_BEGIN", 103),
    ("LORE_EVENT_FILE_STAGE_PROGRESS", 104),
    ("LORE_EVENT_FILE_STAGE_END", 105),
    ("LORE_EVENT_FILE_STAGE_REVISION", 106),
    ("LORE_EVENT_FILE_STAGE_FILE", 107),
    ("LORE_EVENT_FILE_UNSTAGE_BEGIN", 108),
    ("LORE_EVENT_FILE_UNSTAGE_PROGRESS", 109),
    ("LORE_EVENT_FILE_UNSTAGE_END", 110),
    ("LORE_EVENT_FILE_UNSTAGE_REVISION", 111),
    ("LORE_EVENT_FILE_UNSTAGE_FILE", 112),
    ("LORE_EVENT_FRAGMENT_WRITE", 113),
    ("LORE_EVENT_LAYER_ADD", 114),
    ("LORE_EVENT_LAYER_ENTRY", 115),
    ("LORE_EVENT_LAYER_REMOVE", 116),
    ("LORE_EVENT_LAYER_STAGED_ENTRY", 117),
    ("LORE_EVENT_LINK_BRANCH_CREATE", 118),
    ("LORE_EVENT_LINK_CHANGE", 119),
    ("LORE_EVENT_LINK_ENTRY", 120),
    ("LORE_EVENT_LINK_INFO", 121),
    ("LORE_EVENT_LOCK_FILE_ACQUIRE_BEGIN", 122),
    ("LORE_EVENT_LOCK_FILE_ACQUIRE", 123),
    ("LORE_EVENT_LOCK_FILE_STATUS_BEGIN", 124),
    ("LORE_EVENT_LOCK_FILE_STATUS", 125),
    ("LORE_EVENT_LOCK_FILE_QUERY_BEGIN", 126),
    ("LORE_EVENT_LOCK_FILE_QUERY", 127),
    ("LORE_EVENT_LOCK_FILE_RELEASE_BEGIN", 128),
    ("LORE_EVENT_LOCK_FILE_RELEASE", 129),
    ("LORE_EVENT_METADATA_CLEAR_FILE", 130),
    ("LORE_EVENT_METADATA_CLEAR_REVISION", 131),
    ("LORE_EVENT_PATH_IGNORE", 132),
    ("LORE_EVENT_REPOSITORY_CREATE", 133),
    ("LORE_EVENT_REPOSITORY_CLONE_BEGIN", 134),
    ("LORE_EVENT_REPOSITORY_CLONE_PROGRESS", 135),
    ("LORE_EVENT_REPOSITORY_CLONE_END", 136),
    ("LORE_EVENT_DEPENDENCY_RESOLVE_BEGIN", 137),
    ("LORE_EVENT_DEPENDENCY_RESOLVE_ITEM", 138),
    ("LORE_EVENT_DEPENDENCY_RESOLVE_END", 139),
    ("LORE_EVENT_REPOSITORY_DATA", 140),
    ("LORE_EVENT_REPOSITORY_CONFIG_GET", 141),
    ("LORE_EVENT_REPOSITORY_DUMP_BEGIN", 142),
    ("LORE_EVENT_REPOSITORY_DUMP_END", 143),
    ("LORE_EVENT_REPOSITORY_LIST_ENTRY", 144),
    ("LORE_EVENT_REPOSITORY_INSTANCE", 145),
    ("LORE_EVENT_REPOSITORY_VERIFY_STATE_BEGIN", 146),
    ("LORE_EVENT_REPOSITORY_VERIFY_STATE_END", 147),
    ("LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT", 148),
    ("LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT_MATCH", 149),
    ("LORE_EVENT_REPOSITORY_VERIFY_FRAGMENT_REMOTE", 150),
    ("LORE_EVENT_REPOSITORY_STATE_DUMP", 151),
    ("LORE_EVENT_REPOSITORY_STATE_DUMP_NODE", 152),
    ("LORE_EVENT_REPOSITORY_STATUS_REVISION", 153),
    ("LORE_EVENT_REPOSITORY_STATUS_FILE", 154),
    ("LORE_EVENT_REPOSITORY_STATUS_COUNT", 155),
    ("LORE_EVENT_REPOSITORY_STATUS_SUMMARY", 156),
    ("LORE_EVENT_REPOSITORY_STORE_IMMUTABLE_QUERY", 157),
    ("LORE_EVENT_REVISION_COMMIT_BEGIN", 158),
    ("LORE_EVENT_REVISION_COMMIT_PROGRESS", 159),
    ("LORE_EVENT_REVISION_COMMIT_END", 160),
    ("LORE_EVENT_REVISION_COMMIT_REVISION", 161),
    ("LORE_EVENT_REVISION_INFO", 162),
    ("LORE_EVENT_REVISION_INFO_DELTA", 163),
    ("LORE_EVENT_REVISION_DIFF_FILE", 164),
    ("LORE_EVENT_REVISION_FIND", 165),
    ("LORE_EVENT_REVISION_HISTORY", 166),
    ("LORE_EVENT_REVISION_HISTORY_ENTRY", 167),
    ("LORE_EVENT_REVISION_RESTORE_FILE_BEGIN", 168),
    ("LORE_EVENT_REVISION_RESTORE_FILE", 169),
    ("LORE_EVENT_REVISION_RESTORE_FILE_END", 170),
    ("LORE_EVENT_REVISION_RESTORE_FRAGMENT_BEGIN", 171),
    ("LORE_EVENT_REVISION_RESTORE_FRAGMENT_PROGRESS", 172),
    ("LORE_EVENT_REVISION_RESTORE_FRAGMENT_END", 173),
    ("LORE_EVENT_REVISION_RESTORE_REVISION", 174),
    ("LORE_EVENT_REVISION_RESTORE_SYNC_BEGIN", 175),
    ("LORE_EVENT_REVISION_RESTORE_SYNC_END", 176),
    ("LORE_EVENT_REVISION_RESOLVE", 177),
    ("LORE_EVENT_REVISION_SYNC_TARGET", 178),
    ("LORE_EVENT_REVISION_SYNC_FILE", 179),
    ("LORE_EVENT_REVISION_SYNC_PROGRESS", 180),
    ("LORE_EVENT_REVISION_SYNC_REVISION", 181),
    ("LORE_EVENT_REVISION_BISECT", 182),
    ("LORE_EVENT_NOTIFICATION_BRANCH_CREATED", 183),
    ("LORE_EVENT_NOTIFICATION_BRANCH_DELETED", 184),
    ("LORE_EVENT_NOTIFICATION_BRANCH_PUSHED", 185),
    ("LORE_EVENT_NOTIFICATION_RESOURCE_LOCKED", 186),
    ("LORE_EVENT_NOTIFICATION_RESOURCE_UNLOCKED", 187),
    ("LORE_EVENT_NOTIFICATION_SUBSCRIBED", 188),
    ("LORE_EVENT_NOTIFICATION_UNSUBSCRIBED", 189),
    ("LORE_EVENT_SHARED_STORE_CREATE", 190),
    ("LORE_EVENT_SHARED_STORE_INFO", 191),
    ("LORE_EVENT_SHARED_STORE_LIST", 192),
    ("LORE_EVENT_LINK_STAGED_ENTRY", 193),
    ("LORE_EVENT_STORAGE_OPENED", 194),
    ("LORE_EVENT_STORAGE_PUT_ITEM_COMPLETE", 195),
    ("LORE_EVENT_STORAGE_GET_HEADER", 196),
    ("LORE_EVENT_STORAGE_GET_DATA", 197),
    ("LORE_EVENT_STORAGE_GET_ITEM_COMPLETE", 198),
    ("LORE_EVENT_STORAGE_GET_METADATA_ITEM_COMPLETE", 199),
    ("LORE_EVENT_STORAGE_COPY_ITEM_COMPLETE", 200),
    ("LORE_EVENT_STORAGE_OBLITERATE_ITEM_COMPLETE", 201),
    ("LORE_EVENT_STORAGE_UPLOAD_ITEM_COMPLETE", 202),
    ("LORE_EVENT_REVISION_TREE_LOADED", 203),
    ("LORE_EVENT_REVISION_TREE_RESOLVE_PATH_COMPLETE", 204),
    ("LORE_EVENT_REVISION_TREE_CHILD", 205),
    ("LORE_EVENT_REVISION_TREE_NODE_INFO", 206),
    ("LORE_EVENT_REVISION_TREE_NODE_PATH", 207),
    ("LORE_EVENT_REVISION_TREE_ADD_COMPLETE", 208),
    ("LORE_EVENT_REVISION_TREE_DELETE_COMPLETE", 209),
    ("LORE_EVENT_REVISION_TREE_MODIFY_COMPLETE", 210),
    ("LORE_EVENT_REVISION_TREE_MOVE_COMPLETE", 211),
    ("LORE_EVENT_REVISION_TREE_METADATA_SET_COMPLETE", 212),
    ("LORE_EVENT_REVISION_TREE_METADATA_GET_COMPLETE", 213),
    ("LORE_EVENT_REVISION_TREE_COMMIT_COMPLETE", 214),
    ("LORE_EVENT_REVISION_TREE_CLOSE_COMPLETE", 215),
    ("LORE_EVENT_REVISION_TREE_LIST_CHILDREN_BEGIN", 216),
    ("LORE_EVENT_REVISION_TREE_INFO", 217),
    ("LORE_EVENT_STORAGE_MUTABLE_LOAD_ITEM_COMPLETE", 218),
    ("LORE_EVENT_STORAGE_MUTABLE_STORE_ITEM_COMPLETE", 219),
    (
        "LORE_EVENT_STORAGE_MUTABLE_COMPARE_AND_SWAP_ITEM_COMPLETE",
        220,
    ),
    ("LORE_EVENT_STORAGE_MUTABLE_LIST_ENTRY", 221),
    ("LORE_EVENT_STORAGE_MUTABLE_LIST_ITEM_COMPLETE", 222),
    ("LORE_EVENT_EVICTION_BEGIN", 223),
    ("LORE_EVENT_EVICTION_PROGRESS", 224),
    ("LORE_EVENT_EVICTION_END", 225),
    ("LORE_EVENT_COMPACTION_BEGIN", 226),
    ("LORE_EVENT_COMPACTION_PROGRESS", 227),
    ("LORE_EVENT_COMPACTION_END", 228),
    ("LORE_EVENT_REVISION_TREE_BATCH_COMPLETE", 229),
    ("LORE_EVENT_REVISION_TREE_METADATA_CLEAR_COMPLETE", 230),
    ("LORE_EVENT_REVISION_COMMIT_STATS", 231),
    ("LORE_EVENT_BRANCH_PUSH_STATS", 232),
    ("LORE_EVENT_SERVICE_STATUS", 233),
    ("LORE_EVENT_SERVICE_MESSAGE", 234),
    ("LORE_EVENT_AUTH_PENDING", 235),
    ("LORE_EVENT_STORAGE_GET_FRAGMENT", 236),
    ("LORE_EVENT_FILE_AUDIT", 237),
    ("LORE_EVENT_REPOSITORY_INSTANCE_DELETE_BEGIN", 238),
    ("LORE_EVENT_REPOSITORY_INSTANCE_DELETE_PROGRESS", 239),
    ("LORE_EVENT_REPOSITORY_INSTANCE_DELETE_COMPLETE", 240),
    ("LORE_EVENT_REPOSITORY_INSTANCE_MOVE_BEGIN", 241),
    ("LORE_EVENT_REPOSITORY_INSTANCE_MOVE_COMPLETE", 242),
    ("LORE_EVENT_REPOSITORY_INSTANCE_CLONE_BEGIN", 243),
    ("LORE_EVENT_REPOSITORY_INSTANCE_CLONE_PROGRESS", 244),
    ("LORE_EVENT_REPOSITORY_INSTANCE_CLONE_COMPLETE", 245),
    ("LORE_EVENT_REVISION_GRAPH_BRANCH", 246),
    ("LORE_EVENT_REVISION_GRAPH_INSTANCE", 247),
    ("LORE_EVENT_REVISION_GRAPH_REVISION", 248),
    ("LORE_EVENT_REVISION_GRAPH_LOCATION_COMPLETE", 249),
    ("LORE_EVENT_REVISION_GRAPH_BRANCH_METADATA", 250),
    ("LORE_EVENT_REVISION_GRAPH_BRANCH_SYNCED", 251),
    ("LORE_EVENT_BRANCH_MERGE_LINK_PIN_CONFLICT", 252),
    ("LORE_EVENT_BRANCH_MERGE_LINK_PIN_RESOLVED", 253),
    ("LORE_EVENT_STORAGE_PUT_STATS", 254),
    ("LORE_EVENT_REVISION_GRAPH_BRANCH_PUSHED", 255),
];

/// The text of a file of the workspace, at `path` from the workspace root.
fn workspace_file(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(path);
    fs::read_to_string(&full).unwrap_or_else(|err| panic!("reading {}: {err}", full.display()))
}

/// The enumerators of the C enum `name` that `header` declares, with their values.
fn enumerators(header: &str, name: &str) -> BTreeMap<String, i64> {
    let start = header
        .find(&format!("enum {name} {{"))
        .unwrap_or_else(|| panic!("lore.h declares no enum {name}"));
    let body = &header[start..];
    let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];

    let mut declared = BTreeMap::new();
    let mut value = 0;
    for line in body.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let line = line.trim_end_matches(',');
        let (enumerator, assigned) = match line.split_once('=') {
            Some((enumerator, assigned)) => (enumerator.trim(), Some(assigned.trim())),
            None => (line, None),
        };
        if let Some(assigned) = assigned {
            value = assigned.parse().unwrap();
        }
        declared.insert(enumerator.to_string(), value);
        value += 1;
    }
    declared
}

/// Every type of the error code registry, `lore-base/src/error.rs`, with its code.
fn registry() -> Vec<(String, i64)> {
    let source = workspace_file("lore-base/src/error.rs");
    let mut lines = source.lines().map(str::trim);
    let mut registry = Vec::new();
    while let Some(line) = lines.next() {
        let Some(code) = line
            .strip_prefix("#[ffi_code(")
            .and_then(|rest| rest.strip_suffix(")]"))
        else {
            continue;
        };
        let name = lines
            .next()
            .and_then(|line| line.strip_prefix("pub struct "))
            .unwrap_or_else(|| panic!("no type follows #[ffi_code({code})]"));
        let name: String = name
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        registry.push((name, code.parse().unwrap()));
    }
    registry
}

/// `name`, a Rust type name, as the C enumerator of `lore_error_code_t` that names it.
fn error_code_enumerator(name: &str) -> String {
    let mut enumerator = String::from("LORE_ERROR_CODE");
    for (index, letter) in name.chars().enumerate() {
        if index == 0 || letter.is_ascii_uppercase() {
            enumerator.push('_');
        }
        enumerator.push(letter.to_ascii_uppercase());
    }
    enumerator
}

#[test]
fn event_tags_keep_their_values() {
    let declared = enumerators(&workspace_file("lore-capi/lore.h"), "lore_event_id_t");
    let mut moved = Vec::new();
    for (tag, value) in EVENT_TAGS {
        if declared.get(*tag).is_some_and(|declared| declared != value) {
            moved.push(format!("{tag}: {value} -> {:?}", declared.get(*tag)));
        }
    }
    let highest = EVENT_TAGS.iter().map(|(_, value)| *value).max().unwrap();
    for (tag, value) in &declared {
        if !EVENT_TAGS.iter().any(|(known, _)| known == tag) && *value <= highest {
            moved.push(format!(
                "{tag}: a new tag at {value}, at or below {highest}"
            ));
        }
    }
    assert!(moved.is_empty(), "{moved:#?}");
}

#[test]
fn error_codes_name_every_registry_code() {
    let declared = enumerators(&workspace_file("lore-capi/lore.h"), "lore_error_code_t");
    let mut expected: BTreeMap<String, i64> = registry()
        .into_iter()
        .map(|(name, code)| (error_code_enumerator(&name), code))
        .collect();
    expected.insert("LORE_ERROR_CODE_NONE".to_string(), 0);
    expected.insert("LORE_ERROR_CODE_INTERNAL".to_string(), -1);
    assert_eq!(declared, expected);
}
