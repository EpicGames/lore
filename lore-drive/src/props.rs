// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Custom user properties (string key → string value) per tree node,
//! persisted in the workspace's **mutable store** with the value bytes in
//! the **immutable (CAS) store** — the lore idiom of a mutable pointer into
//! immutable content, at zero schema cost:
//!
//! ```text
//! mutable[ blake3("lore-drive:props:v1:" ++ node_id_le) ]  (KeyType::Untyped)
//!        └──> Hash of a CAS JSON blob {"v":1,"node_id":N,"props":{k:v,…}}
//! ```
//!
//! - One mutable entry **per node**, not per property: a property edit is
//!   read-modify-write of one small JSON blob (all edits already serialize
//!   through lore-drive's `write_gate`, so plain store — no CAS-swap — is
//!   race-free).
//! - Keyed by `node_id`, which lore preserves across rename / move /
//!   content replacement, so properties follow the item the way a browser
//!   user expects.  Node slots can be *reused* after deletions, therefore
//!   lore-drive deletes the subtree's property entries whenever it deletes
//!   nodes (see `handle_delete`).  Deletions performed by the `lore` CLI
//!   behind lore-drive's back can leave orphaned entries — harmless until
//!   the slot is reused, documented in REST_API.md.
//! - Storing `Hash::default()` removes a mutable key, so an emptied
//!   property map costs nothing.
//! - The blob self-describes (`v`, `node_id`): on load the expected key is
//!   recomputed and any mismatching or undecodable blob is treated as
//!   absent, so foreign `Untyped` entries can never masquerade as ours.

use std::collections::BTreeMap;

use lore_base::types::Context;
use lore_base::types::Hash;
use serde::Deserialize;
use serde::Serialize;

/// Namespace prefix hashed into every property key (version-tagged so a
/// future layout change can migrate by dual-reading).
const PROPS_KEY_PREFIX: &[u8] = b"lore-drive:props:v1:";

/// Current blob layout version.
pub const PROPS_BLOB_VERSION: u32 = 1;

/// Hard caps keeping blobs small and the UI honest.
pub const MAX_PROP_KEY_LEN: usize = 256;
pub const MAX_PROP_VALUE_LEN: usize = 4096;
pub const MAX_PROPS_PER_NODE: usize = 256;

/// CAS blob layout: self-describing so search/list can verify provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropsBlob {
    /// Layout version — reject blobs from the future.
    pub v: u32,
    /// Owning node id; must round-trip to the mutable key.
    pub node_id: u32,
    /// The user properties. `BTreeMap` for deterministic serialization
    /// (identical maps hash to identical CAS addresses — free dedup).
    pub props: BTreeMap<String, String>,
}

/// The mutable-store key under which `node_id`'s properties live.
pub fn props_key(node_id: u32) -> Hash {
    let mut buf = Vec::with_capacity(PROPS_KEY_PREFIX.len() + 4);
    buf.extend_from_slice(PROPS_KEY_PREFIX);
    buf.extend_from_slice(&node_id.to_le_bytes());
    Hash::hash_buffer(&buf)
}

/// Deterministic CAS context (dedup tag) for `node_id`'s blob — derived
/// from the props key so the full `Address` can be reconstructed from the
/// node id alone at load time.
pub fn props_context(node_id: u32) -> Context {
    let key = props_key(node_id);
    let mut ctx = Context::default();
    ctx.data_mut().copy_from_slice(&key.data()[..16]);
    ctx
}

/// Serialize a property map into CAS blob bytes.
pub fn encode_blob(node_id: u32, props: &BTreeMap<String, String>) -> Vec<u8> {
    serde_json::to_vec(&PropsBlob {
        v: PROPS_BLOB_VERSION,
        node_id,
        props: props.clone(),
    })
    .expect("PropsBlob serialization is infallible")
}

/// Decode CAS blob bytes back into a property map, verifying that the blob
/// belongs to `node_id` and speaks a known layout version. `None` on any
/// mismatch — callers treat that as "no properties".
pub fn decode_blob(node_id: u32, bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let blob: PropsBlob = serde_json::from_slice(bytes).ok()?;
    if blob.v != PROPS_BLOB_VERSION || blob.node_id != node_id {
        return None;
    }
    Some(blob.props)
}

/// Validate one property assignment. `Err` carries a human-readable reason.
pub fn validate_prop(
    key: &str,
    value: &str,
    existing: &BTreeMap<String, String>,
) -> Result<(), String> {
    if key.is_empty() {
        return Err("property key must not be empty".into());
    }
    if key.len() > MAX_PROP_KEY_LEN {
        return Err(format!("property key exceeds {MAX_PROP_KEY_LEN} bytes"));
    }
    if value.len() > MAX_PROP_VALUE_LEN {
        return Err(format!("property value exceeds {MAX_PROP_VALUE_LEN} bytes"));
    }
    if key.chars().any(char::is_control) || value.chars().any(char::is_control) {
        return Err("property keys and values must not contain control characters".into());
    }
    if !existing.contains_key(key) && existing.len() >= MAX_PROPS_PER_NODE {
        return Err(format!("a node holds at most {MAX_PROPS_PER_NODE} properties"));
    }
    Ok(())
}

/// Case-insensitive substring match used by `/search` for names, property
/// keys and property values.
pub fn search_matches(haystack: &str, needle_lower: &str) -> bool {
    !needle_lower.is_empty() && haystack.to_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_derivation_is_stable_and_distinct() {
        // Stability: the same node id always derives the same key (the
        // on-disk contract — changing this breaks every stored property).
        assert_eq!(props_key(7), props_key(7));
        assert_eq!(props_context(7), props_context(7));
        // Distinctness across node ids.
        assert_ne!(props_key(7), props_key(8));
        assert_ne!(props_context(7), props_context(8));
        // Never the null hash (which would *delete* a mutable key).
        assert!(!props_key(0).is_zero());
        // Context is the first half of the key hash.
        assert_eq!(props_context(3).data(), &props_key(3).data()[..16]);
    }

    #[test]
    fn blob_roundtrip() {
        let mut props = BTreeMap::new();
        props.insert("owner".to_owned(), "nsauzede".to_owned());
        props.insert("review".to_owned(), "pending".to_owned());
        let bytes = encode_blob(42, &props);
        assert_eq!(decode_blob(42, &bytes), Some(props.clone()));
        // Deterministic serialization: same map → same bytes (CAS dedup).
        assert_eq!(bytes, encode_blob(42, &props));
    }

    #[test]
    fn blob_provenance_is_enforced() {
        let props: BTreeMap<String, String> =
            [("k".to_owned(), "v".to_owned())].into_iter().collect();
        let bytes = encode_blob(42, &props);
        // Wrong node id → treated as absent.
        assert_eq!(decode_blob(43, &bytes), None);
        // Future version → treated as absent.
        let mut blob: PropsBlob = serde_json::from_slice(&bytes).unwrap();
        blob.v = PROPS_BLOB_VERSION + 1;
        let future = serde_json::to_vec(&blob).unwrap();
        assert_eq!(decode_blob(42, &future), None);
        // Garbage → treated as absent.
        assert_eq!(decode_blob(42, b"not json"), None);
    }

    #[test]
    fn validation_rules() {
        let empty = BTreeMap::new();
        assert!(validate_prop("k", "v", &empty).is_ok());
        assert!(validate_prop("", "v", &empty).is_err());
        assert!(validate_prop(&"k".repeat(MAX_PROP_KEY_LEN + 1), "v", &empty).is_err());
        assert!(validate_prop("k", &"v".repeat(MAX_PROP_VALUE_LEN + 1), &empty).is_err());
        assert!(validate_prop("k\n", "v", &empty).is_err());
        assert!(validate_prop("k", "v\t", &empty).is_err());
        // Capacity: replacing an existing key is always allowed; adding a
        // new one past the cap is not.
        let mut full = BTreeMap::new();
        for i in 0..MAX_PROPS_PER_NODE {
            full.insert(format!("k{i}"), "v".to_owned());
        }
        assert!(validate_prop("k0", "updated", &full).is_ok());
        assert!(validate_prop("brand-new", "v", &full).is_err());
    }

    #[test]
    fn search_matching() {
        assert!(search_matches("Quarterly Report.PDF", "report"));
        assert!(search_matches("owner", "owner"));
        assert!(!search_matches("owner", "renwo"));
        // An empty query matches nothing (the handler rejects it anyway).
        assert!(!search_matches("anything", ""));
    }
}
