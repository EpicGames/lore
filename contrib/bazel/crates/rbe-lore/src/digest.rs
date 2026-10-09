// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Digest helpers. Bazel's default (and the only function this server advertises) is SHA-256
//! over the blob bytes, rendered lowercase hex.

use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use rbe_proto::reapi::Digest;
use sha2::Digest as _;
use sha2::Sha256;

/// SHA-256 of the empty byte string. The REAPI requires servers to treat the empty blob as
/// always present and never require it to be uploaded, so it is special-cased on every path.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Read buffer for hashing a file. Large enough that a multi-megabyte object costs a handful of
/// reads, small enough to stay a fixed cost per concurrent slot.
#[lore_macro::test_pub]
const HASH_CHUNK_BYTES: usize = 256 * 1024;

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

pub fn of(data: &[u8]) -> Digest {
    Digest {
        hash: sha256_hex(data),
        size_bytes: data.len() as i64,
    }
}

/// Digest a file without holding it. The content is about to be handed to Lore as a path, so
/// reading it into memory here purely to hash it would put every output back in this process.
///
/// One blocking task for the whole file rather than an async read per chunk, each of which is a
/// hop to the blocking pool.
pub async fn of_file(path: &Path) -> Result<Digest> {
    let path = path.to_path_buf();
    crate::spawn_blocking(move || of_file_blocking(&path))
        .await
        .map_err(|e| anyhow::anyhow!("digesting a file panicked: {e}"))?
}

fn of_file_blocking(path: &Path) -> Result<Digest> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to digest it", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    let mut size: i64 = 0;
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("reading {} to digest it", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size += read as i64;
    }
    Ok(Digest {
        hash: hex::encode(hasher.finalize()),
        size_bytes: size,
    })
}

pub fn is_empty_digest(d: &Digest) -> bool {
    is_empty_blob(&d.hash, d.size_bytes)
}

pub fn is_empty_blob(hash: &str, size: i64) -> bool {
    size == 0 && hash == EMPTY_SHA256
}

/// `(hash, size)` as the store's `get_many`/`put_many`/`exists_many` want it.
pub fn key_of(d: &Digest) -> (String, i64) {
    (d.hash.clone(), d.size_bytes)
}

pub fn fmt(d: &Digest) -> String {
    format!("{}/{}", &d.hash[..d.hash.len().min(12)], d.size_bytes)
}
