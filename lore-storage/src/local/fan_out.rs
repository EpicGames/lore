// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Fan-out helpers for the lazy progressive bucket layout used by
//! `LocalImmutableStore` and `LocalMutableStore`.

use std::path::Path;
use std::path::PathBuf;

use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::Hash;

/// The set of valid bucket counts a group can be at. Fan-out only ever moves a
/// group to the next-higher value in this list.
pub const LEVEL_LADDER: [usize; 5] = [1, 32, 64, 128, 256];

/// Maximum bucket count per group. Equals the last entry in `LEVEL_LADDER` and matches the
/// existing `BUCKET_COUNT` in the local store implementations. Server-mode stores start here.
pub const FAN_OUT_LEVEL_MAX: usize = 256;

/// Default per-bucket entry threshold that triggers fan-out at the next serialize. Used by
/// `MutableStoreSettings::default()` and `ImmutableStoreSettings::default()`.
pub const FAN_OUT_THRESHOLD_DEFAULT: usize = 1000;

/// Filename of the per-group level marker, relative to the group directory.
pub const MARKER_FILENAME: &str = "level";

/// Filename of the per-group two-phase commit sentinel, relative to the group directory.
/// Present iff a fan-out commit started but did not yet reach the marker write step. Recovery
/// during store open detects this and rolls forward.
pub const LEVEL_PENDING_FILENAME: &str = "level.pending";

/// Filename prefix of a bucket index file inside a group directory.
pub const BUCKET_FILENAME_PREFIX: &str = "index_";

/// Filename suffix appended to bucket files during a fan-out commit. Bucket file `index_<bb>`
/// gets written to `index_<bb>.new` first; the rename to the final name only happens after
/// every `.new` is on disk and the `level.pending` sentinel has been written.
pub const BUCKET_NEW_SUFFIX: &str = ".new";

/// Magic value at the start of the marker file. Bytes spell `LVNO` on disk.
const MARKER_MAGIC: u32 = u32::from_le_bytes(*b"LVNO");

/// Marker file format version. Independent of `MutableStoreVersion` /
/// `ImmutableStoreVersion`; bumped only if the marker file's binary layout changes.
const MARKER_VERSION: u32 = 1;

#[repr(C)]
#[derive(Default, IntoBytes, FromBytes, Immutable)]
struct LevelMarkerHeader {
    magic: u32,
    version: u32,
    bucket_count: u32,
    _reserved: u32,
}

/// Compute the bucket index for `key` at a group whose bucket count is `bucket_count`.
///
/// The same hash byte (`key.data()[1]`) is interpreted at different bit widths depending on
/// the level: level 1 always returns 0; level `N` (a power of two in `{32, 64, 128, 256}`)
/// returns the high `log2(N)` bits of `key.data()[1]`. Going from level `N` to level `M` (with
/// `M > N`, both in the ladder) splits each old bucket into `M / N` new buckets — the new
/// index is the same hash byte right-shifted by a smaller amount, so an entry's new bucket
/// is always within the contiguous range `[old_idx * M/N, old_idx * M/N + M/N - 1]`.
///
/// # Arguments
/// * `key` — the entry's hash. Only `data()[1]` is consulted; `data()[0]` selected the group
///   and is irrelevant here.
/// * `bucket_count` — the group's current `bucket_count`. Must be `1` or a power of two in
///   `[2, 256]`.
///
/// # Returns
/// The bucket index in `[0, bucket_count)`.
///
/// # Invariants
/// * `bucket_count == 1 || (bucket_count.is_power_of_two() && bucket_count <= 256)`. Violation
///   is a `debug_assert!` (release builds skip the check).
/// * Result is always in `[0, bucket_count)`.
pub fn bucket_index_for(key: &Hash, bucket_count: usize) -> usize {
    debug_assert!(
        bucket_count == 1 || (bucket_count.is_power_of_two() && bucket_count <= 256),
        "bucket_count must be 1 or a power of two ≤ 256, got {bucket_count}"
    );
    if bucket_count <= 1 {
        return 0;
    }
    let shift = (256usize / bucket_count).trailing_zeros();
    (key.data()[1] as usize) >> shift
}

/// Compute the target level for a group whose largest bucket has overshot the threshold.
///
/// Returns the smallest value in `LEVEL_LADDER` such that splitting the group's worst bucket
/// `M / current_level` ways would bring its expected post-split entry count to or below
/// `threshold`. Concretely: returns the smallest `M ∈ LEVEL_LADDER` with
/// `M ≥ ceil(current_level * b_max / threshold)`, capped at 256.
///
/// # Arguments
/// * `current_level` — the group's current `bucket_count`. Typically a value from
///   `LEVEL_LADDER`.
/// * `b_max` — the maximum entry count observed across the group's buckets.
/// * `threshold` — the per-bucket fan-out threshold from the store's settings.
///
/// # Returns
/// A value from `LEVEL_LADDER` (always ≤ 256). When `b_max ≤ threshold` the function returns
/// the smallest ladder value ≥ `current_level`, which for typical inputs equals
/// `current_level` itself (no transition needed).
///
/// # Invariants
/// * `threshold > 0`. Violation is a `debug_assert!`.
/// * Multiplication `current_level * b_max` saturates at `usize::MAX` rather than overflowing.
/// * Result is always a member of `LEVEL_LADDER`.
pub fn level_for(current_level: usize, b_max: usize, threshold: usize) -> usize {
    debug_assert!(threshold > 0, "threshold must be positive");
    let product = current_level.saturating_mul(b_max);
    let required = product.div_ceil(threshold);
    LEVEL_LADDER
        .iter()
        .copied()
        .find(|&m| m >= required)
        .unwrap_or(256)
}

/// Read the level marker file in `group_path`, returning the recorded bucket count.
///
/// # Arguments
/// * `group_path` — the per-group directory (e.g., `<store>/index/<gg>/`).
///
/// # Returns
/// * `Ok(None)` when no marker file exists. Such a group is either pre-fan-out or never
///   written; [`read_group_level`] tells the two apart.
/// * `Ok(Some(level))` when the marker is present, has a valid magic and version, and
///   parses to a level value.
/// * `Err(io::Error)` for I/O failures, truncated files, mismatched magic, or unsupported
///   version. The error kind is `InvalidData` for corruption.
///
/// # Invariants
/// * The marker file is always exactly `size_of::<LevelMarkerHeader>()` bytes.
/// * A successfully-parsed marker has `magic == MARKER_MAGIC` and `version == MARKER_VERSION`.
pub async fn read_level_marker(group_path: &Path) -> std::io::Result<Option<usize>> {
    read_level_header_file(&group_path.join(MARKER_FILENAME)).await
}

/// Write the level marker file in `group_path`, recording the current bucket count.
///
/// # Arguments
/// * `group_path` — the per-group directory. Must already exist.
/// * `level` — the bucket count to record. Should be a value from `LEVEL_LADDER`.
/// * `sync_data` — when true, fsync the file before returning.
///
/// # Returns
/// * `Ok(())` on success.
/// * `Err(io::Error)` for any I/O failure during file creation, write, or fsync.
///
/// # Invariants
/// * Existing marker file (if any) is truncated and rewritten — this is a full overwrite.
/// * `level` is cast to u32; must fit (always true for ladder values ≤ 256).
pub async fn write_level_marker(
    group_path: &Path,
    level: usize,
    sync_data: bool,
) -> std::io::Result<()> {
    write_level_header_file(&group_path.join(MARKER_FILENAME), level, sync_data).await
}

/// Number of bytes a bucket file name occupies: the prefix plus two hex digits.
const BUCKET_FILENAME_LEN: usize = BUCKET_FILENAME_PREFIX.len() + 2;

/// Write `value` as lowercase two-digit hex into the first two bytes of `out`.
///
/// Path components are built through this rather than `format!`: on the per-bucket flush and
/// recovery paths that is one `String` allocated and dropped per bucket, for two bytes.
pub(crate) fn write_hex_byte(out: &mut [u8], value: u8) {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    out[0] = HEX[(value >> 4) as usize];
    out[1] = HEX[(value & 0x0f) as usize];
}

/// Append the group directory component for `group_index`: the lowercase 2-digit hex of
/// `group_index as u8`.
pub fn push_group_dir(path: &mut PathBuf, group_index: usize) {
    let mut name = [0u8; 2];
    write_hex_byte(&mut name, group_index as u8);
    path.push(std::str::from_utf8(&name).unwrap_or_default());
}

/// Format the path for a group directory inside an index directory: `<index_path>/<gg>` where
/// `<gg>` is the lowercase 2-digit hex of `group_index as u8`.
pub fn group_dir_path(index_path: &Path, group_index: usize) -> PathBuf {
    let mut path = index_path.to_path_buf();
    push_group_dir(&mut path, group_index);
    path
}

/// Format the path for a bucket index file inside a group directory: `<group_path>/index_<bb>`
/// where `<bb>` is the lowercase 2-digit hex of `bucket_index as u8`.
///
/// # Invariants
/// * `bucket_index` is cast to `u8`; values ≥ 256 wrap, but `bucket_index` is always in
///   `[0, 256)` per the level ladder.
pub fn bucket_path(group_path: &Path, bucket_index: usize) -> PathBuf {
    let mut name = [0u8; BUCKET_FILENAME_LEN];
    name[..BUCKET_FILENAME_PREFIX.len()].copy_from_slice(BUCKET_FILENAME_PREFIX.as_bytes());
    write_hex_byte(
        &mut name[BUCKET_FILENAME_PREFIX.len()..],
        bucket_index as u8,
    );
    group_path.join(std::str::from_utf8(&name).unwrap_or_default())
}

/// What a group's directory records about the bucket layout it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupLevel {
    /// The level marker records this bucket count.
    Marked(usize),
    /// Bucket files but no marker: the pre-fan-out layout, entries spread over all
    /// [`FAN_OUT_LEVEL_MAX`] buckets.
    PreFanOut,
    /// Neither marker nor bucket files, so the group has never been written. The level it
    /// starts at is the caller's to choose.
    Unwritten,
}

/// The bucket count a [`GroupLevel::Unwritten`] group starts at.
///
/// A fan-out-aware store records the level a group starts at in its level marker, so it may
/// start anywhere on the ladder. A store still serializing at a pre-fan-out version writes no
/// marker, so the next open has only the bucket files to go by and reads such a group at
/// [`FAN_OUT_LEVEL_MAX`]; starting it lower would put its entries where that read cannot find
/// them.
pub fn unwritten_group_level(fan_out_aware: bool, initial_fan_out_level: usize) -> usize {
    if fan_out_aware {
        initial_fan_out_level
    } else {
        FAN_OUT_LEVEL_MAX
    }
}

/// Read what `group_path` records about its bucket layout. Any pending fan-out commit must
/// already have been recovered, or a group caught mid-commit reads as [`GroupLevel::PreFanOut`].
///
/// # Returns
/// * `Ok(GroupLevel)` classifying the group.
/// * `Err(io::Error)` for the failures [`read_level_marker`] reports.
pub async fn read_group_level(group_path: &Path) -> std::io::Result<GroupLevel> {
    match read_level_marker(group_path).await? {
        Some(level) => Ok(GroupLevel::Marked(level)),
        None if group_has_buckets(group_path).await => Ok(GroupLevel::PreFanOut),
        None => Ok(GroupLevel::Unwritten),
    }
}

/// Whether `group_path` holds at least one committed bucket file.
///
/// A group is serialized as a whole, so this separates a group that has been written
/// from one that has not. `.new` twins are ignored: they belong to a fan-out commit
/// that never reached its commit point and record no committed layout.
///
/// # Returns
/// * `true` when the directory holds a file named `index_<bb>`.
/// * `false` when it holds none, or cannot be read at all.
async fn group_has_buckets(group_path: &Path) -> bool {
    let Ok(mut entries) = lore_io::IoDriver::global().read_dir(group_path).await else {
        return false;
    };
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name.to_str() else {
            continue;
        };
        if name.starts_with(BUCKET_FILENAME_PREFIX) && !name.ends_with(BUCKET_NEW_SUFFIX) {
            return true;
        }
    }
    false
}

/// Format the path for the in-progress (`.new`) twin of a bucket index file used during
/// fan-out commits.
pub fn bucket_new_path(group_path: &Path, bucket_index: usize) -> PathBuf {
    let mut name = [0u8; BUCKET_FILENAME_LEN + BUCKET_NEW_SUFFIX.len()];
    name[..BUCKET_FILENAME_PREFIX.len()].copy_from_slice(BUCKET_FILENAME_PREFIX.as_bytes());
    write_hex_byte(
        &mut name[BUCKET_FILENAME_PREFIX.len()..],
        bucket_index as u8,
    );
    name[BUCKET_FILENAME_LEN..].copy_from_slice(BUCKET_NEW_SUFFIX.as_bytes());
    group_path.join(std::str::from_utf8(&name).unwrap_or_default())
}

/// Read the `level.pending` sentinel in `group_path`, returning the recorded target bucket count.
///
/// The pending file shares the binary layout of the level marker (same 16-byte header) so the
/// existing read path can be reused.
///
/// # Returns
/// * `Ok(None)` when no pending file exists.
/// * `Ok(Some(level))` when present and well-formed.
/// * `Err(io::Error)` for I/O failures, truncation, mismatched magic, or unsupported version.
pub async fn read_level_pending(group_path: &Path) -> std::io::Result<Option<usize>> {
    read_level_header_file(&group_path.join(LEVEL_PENDING_FILENAME)).await
}

/// Write the `level.pending` sentinel in `group_path` with the recorded target bucket count.
/// Same binary layout as the level marker.
pub async fn write_level_pending(
    group_path: &Path,
    level: usize,
    sync_data: bool,
) -> std::io::Result<()> {
    write_level_header_file(&group_path.join(LEVEL_PENDING_FILENAME), level, sync_data).await
}

/// Delete the `level.pending` sentinel. Returns `Ok(())` whether or not it existed.
pub async fn delete_level_pending(group_path: &Path) -> std::io::Result<()> {
    match lore_io::IoDriver::global()
        .remove_file(group_path.join(LEVEL_PENDING_FILENAME))
        .await
    {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Read a level-header file (marker or pending) from an explicit path. Shared format helper;
/// one backend dispatch covering open, read and close, the file being the header itself.
async fn read_level_header_file(path: &Path) -> std::io::Result<Option<usize>> {
    let bytes = match lore_io::IoDriver::global().read_file_bytes(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut header = LevelMarkerHeader::new_zeroed();
    let expected = size_of::<LevelMarkerHeader>();
    if bytes.len() < expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "level header file is {} bytes, expected {expected}",
                bytes.len()
            ),
        ));
    }
    header.as_mut_bytes().copy_from_slice(&bytes[..expected]);
    if header.magic != MARKER_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "level header file has invalid magic 0x{:08x}, expected 0x{:08x}",
                header.magic, MARKER_MAGIC
            ),
        ));
    }
    if header.version != MARKER_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "level header file has unsupported version {}, expected {}",
                header.version, MARKER_VERSION
            ),
        ));
    }
    Ok(Some(header.bucket_count as usize))
}

/// The serialized level header as the one segment a gather writes.
///
/// Fixed size — the header's length is a compile-time constant — and behind a pointer, because
/// [`lore_io::StableBufList`] requires a segment to keep its address when the value moves and the
/// ring backend moves the segment list into its operation entry after taking the pointers.
struct LevelHeaderSegment(Box<[u8; size_of::<LevelMarkerHeader>()]>);

impl lore_io::StableBufList for LevelHeaderSegment {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.0.as_ref().as_slice())
    }
}

/// Write a level-header file (marker or pending) to an explicit path. Shared format helper;
/// one backend dispatch covering create, write and (when `sync_data`) sync-all.
async fn write_level_header_file(
    path: &Path,
    level: usize,
    sync_data: bool,
) -> std::io::Result<()> {
    let header = LevelMarkerHeader {
        magic: MARKER_MAGIC,
        version: MARKER_VERSION,
        bucket_count: level as u32,
        _reserved: 0,
    };
    let mut segment = Box::new([0u8; size_of::<LevelMarkerHeader>()]);
    segment.copy_from_slice(header.as_bytes());
    lore_io::IoDriver::global()
        .write_file_segments(
            path,
            &lore_io::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            LevelHeaderSegment(segment),
            sync_data,
        )
        .await?;
    Ok(())
}

/// Recover from a possibly-interrupted fan-out commit in `group_path`.
///
/// If `level.pending` exists, this function rolls forward by:
/// 1. Renaming `index_<bb>.new` → `index_<bb>` for each `bb in 0..target` whose `.new` file
///    is still present (already-renamed buckets are silently skipped).
/// 2. Writing the level marker for `target`.
/// 3. Deleting `level.pending`.
///
/// All steps are idempotent, so a recovery interrupted mid-way and re-run on the next open
/// converges to the same state.
///
/// # Returns
/// * `Ok(None)` if no recovery was needed (no `level.pending` present).
/// * `Ok(Some(level))` if recovery rolled forward to `level`.
/// * `Err(io::Error)` if recovery encountered an I/O error it could not work around. Individual
///   `.new` rename failures are logged but do not abort the routine — the next open retries.
pub async fn recover_level_transition(
    group_path: &Path,
    sync_data: bool,
) -> std::io::Result<Option<usize>> {
    let target = match read_level_pending(group_path).await? {
        Some(level) => level,
        None => return Ok(None),
    };

    for bb in 0..target {
        let new_path = bucket_new_path(group_path, bb);
        let final_path = bucket_path(group_path, bb);
        match lore_io::IoDriver::global()
            .rename(&new_path, &final_path)
            .await
        {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Either already renamed in a previous (interrupted) recovery attempt, or
                // never existed (an empty bucket at an index ≥ N skipped writing `.new`).
            }
            Err(err) => {
                lore_base::lore_warn!(
                    "Recovery rename {} -> {} failed: {err}",
                    new_path.display(),
                    final_path.display()
                );
            }
        }
    }

    write_level_marker(group_path, target, sync_data).await?;
    delete_level_pending(group_path).await?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_with_byte_one(byte: u8) -> Hash {
        let mut hash = Hash::default();
        hash.data_mut()[1] = byte;
        hash
    }

    #[test]
    fn level_ladder_is_powers_of_two_starting_at_one() {
        assert_eq!(LEVEL_LADDER, [1, 32, 64, 128, 256]);
        for &m in &LEVEL_LADDER[1..] {
            assert!(m.is_power_of_two(), "level {m} must be power of two");
            assert!(
                m <= FAN_OUT_LEVEL_MAX,
                "level {m} must be <= FAN_OUT_LEVEL_MAX"
            );
        }
    }

    #[test]
    fn fan_out_level_max_matches_top_of_ladder() {
        assert_eq!(FAN_OUT_LEVEL_MAX, LEVEL_LADDER[LEVEL_LADDER.len() - 1]);
    }

    #[test]
    fn fan_out_threshold_default_is_one_thousand() {
        assert_eq!(FAN_OUT_THRESHOLD_DEFAULT, 1000);
    }

    #[test]
    fn bucket_index_for_level_one_always_zero() {
        for byte in [0u8, 0x05, 0x80, 0xFF] {
            let key = key_with_byte_one(byte);
            assert_eq!(bucket_index_for(&key, 1), 0);
        }
    }

    #[test]
    fn bucket_index_for_level_256_returns_full_byte() {
        for byte in [0u8, 0x05, 0x42, 0x80, 0xFF] {
            let key = key_with_byte_one(byte);
            assert_eq!(bucket_index_for(&key, 256), byte as usize);
        }
    }

    #[test]
    fn bucket_index_for_level_32_takes_top_5_bits() {
        // byte = 0b101_01010 (0xAA): top 5 bits = 0b10101 = 21
        let key = key_with_byte_one(0xAA);
        assert_eq!(bucket_index_for(&key, 32), 21);
        // byte = 0xFF: top 5 bits = 0b11111 = 31
        let key = key_with_byte_one(0xFF);
        assert_eq!(bucket_index_for(&key, 32), 31);
        // byte = 0x00: 0
        let key = key_with_byte_one(0x00);
        assert_eq!(bucket_index_for(&key, 32), 0);
    }

    #[test]
    fn bucket_index_for_level_64_takes_top_6_bits() {
        // byte = 0xAA = 0b10101010: top 6 bits = 0b101010 = 42
        let key = key_with_byte_one(0xAA);
        assert_eq!(bucket_index_for(&key, 64), 42);
    }

    #[test]
    fn bucket_index_for_level_128_takes_top_7_bits() {
        // byte = 0xAA = 0b10101010: top 7 bits = 0b1010101 = 85
        let key = key_with_byte_one(0xAA);
        assert_eq!(bucket_index_for(&key, 128), 85);
    }

    #[test]
    fn split_n_to_2n_preserves_bucket_membership_via_high_bit() {
        // For any byte, the bucket at level 2N is either 2*idx_N or 2*idx_N + 1.
        for byte in 0..=255u8 {
            let key = key_with_byte_one(byte);
            let idx_32 = bucket_index_for(&key, 32);
            let idx_64 = bucket_index_for(&key, 64);
            assert!(
                idx_64 == 2 * idx_32 || idx_64 == 2 * idx_32 + 1,
                "byte 0x{byte:02x}: idx_32={idx_32}, idx_64={idx_64}"
            );
        }
    }

    #[test]
    fn level_for_below_threshold_is_current_level() {
        // b_max ≤ threshold ⇒ no fan-out required ⇒ current_level returned (still in ladder).
        assert_eq!(level_for(1, 800, 1000), 1);
        assert_eq!(level_for(1, 1000, 1000), 1);
        assert_eq!(level_for(32, 999, 1000), 32);
    }

    #[test]
    fn level_for_5k_at_level_1_is_32() {
        // 1 * 5000 / 1000 = 5, smallest ladder M >= 5 is 32
        assert_eq!(level_for(1, 5000, 1000), 32);
    }

    #[test]
    fn level_for_1500_at_level_32_is_64() {
        // 32 * 1500 / 1000 = 48, smallest ladder M >= 48 is 64
        assert_eq!(level_for(32, 1500, 1000), 64);
    }

    #[test]
    fn level_for_1500_at_level_128_is_256() {
        // 128 * 1500 / 1000 = 192, smallest ladder M >= 192 is 256
        assert_eq!(level_for(128, 1500, 1000), 256);
    }

    #[test]
    fn level_for_caps_at_256() {
        // Even an extreme b_max can't push us past 256.
        assert_eq!(level_for(128, 10_000, 1000), 256);
        assert_eq!(level_for(256, 10_000, 1000), 256);
    }

    #[test]
    fn level_for_uses_ceiling_division() {
        // 1*1001/1000 ceiling is 2, smallest ladder M ≥ 2 is 32; floor would wrongly return current_level=1 even though trigger has fired.
        assert_eq!(level_for(1, 1001, 1000), 32);
    }

    fn temp_group_dir() -> crate::test_util::TempDir {
        crate::test_util::TempDir::new("fan_out_marker_")
    }

    #[tokio::test]
    async fn read_level_marker_missing_returns_none() {
        let dir = temp_group_dir();
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn write_then_read_round_trip_every_ladder_value() {
        for &level in &LEVEL_LADDER {
            let dir = temp_group_dir();
            write_level_marker(dir.path(), level, true).await.unwrap();
            assert_eq!(read_level_marker(dir.path()).await.unwrap(), Some(level));
        }
    }

    #[tokio::test]
    async fn read_level_marker_with_corrupt_magic_errors() {
        let dir = temp_group_dir();
        let marker = dir.path().join(MARKER_FILENAME);
        std::fs::write(
            &marker,
            [0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let err = read_level_marker(dir.path()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_level_marker_truncated_errors() {
        let dir = temp_group_dir();
        let marker = dir.path().join(MARKER_FILENAME);
        std::fs::write(&marker, [b'L', b'V', b'N', b'O', 1, 0, 0, 0]).unwrap();
        let err = read_level_marker(dir.path()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_level_marker_unsupported_version_errors() {
        let dir = temp_group_dir();
        let marker = dir.path().join(MARKER_FILENAME);
        let mut bytes = vec![];
        bytes.extend_from_slice(&MARKER_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(&32u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&marker, bytes).unwrap();
        let err = read_level_marker(dir.path()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn write_level_marker_overwrites_existing() {
        let dir = temp_group_dir();
        write_level_marker(dir.path(), 1, false).await.unwrap();
        write_level_marker(dir.path(), 64, false).await.unwrap();
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), Some(64));
    }

    #[tokio::test]
    async fn marker_file_is_exactly_16_bytes() {
        let dir = temp_group_dir();
        write_level_marker(dir.path(), 32, false).await.unwrap();
        let metadata = std::fs::metadata(dir.path().join(MARKER_FILENAME)).unwrap();
        assert_eq!(metadata.len(), 16);
    }

    #[tokio::test]
    async fn read_level_pending_missing_returns_none() {
        let dir = temp_group_dir();
        assert_eq!(read_level_pending(dir.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn level_pending_round_trip_every_ladder_value() {
        for &level in &LEVEL_LADDER {
            let dir = temp_group_dir();
            write_level_pending(dir.path(), level, false).await.unwrap();
            assert_eq!(read_level_pending(dir.path()).await.unwrap(), Some(level));
        }
    }

    #[tokio::test]
    async fn delete_level_pending_is_idempotent() {
        let dir = temp_group_dir();
        // Delete on missing file succeeds.
        delete_level_pending(dir.path()).await.unwrap();
        // Delete after write removes it.
        write_level_pending(dir.path(), 32, false).await.unwrap();
        delete_level_pending(dir.path()).await.unwrap();
        assert_eq!(read_level_pending(dir.path()).await.unwrap(), None);
        // Second delete is also fine.
        delete_level_pending(dir.path()).await.unwrap();
    }

    #[test]
    fn bucket_path_lowercase_two_digit_hex() {
        let dir = temp_group_dir();
        assert_eq!(
            bucket_path(dir.path(), 0).file_name().unwrap(),
            std::ffi::OsStr::new("index_00")
        );
        assert_eq!(
            bucket_path(dir.path(), 0xab).file_name().unwrap(),
            std::ffi::OsStr::new("index_ab")
        );
        assert_eq!(
            bucket_path(dir.path(), 255).file_name().unwrap(),
            std::ffi::OsStr::new("index_ff")
        );
    }

    #[test]
    fn bucket_new_path_appends_dot_new() {
        let dir = temp_group_dir();
        assert_eq!(
            bucket_new_path(dir.path(), 0xab).file_name().unwrap(),
            std::ffi::OsStr::new("index_ab.new")
        );
    }

    #[test]
    fn group_dir_path_lowercase_two_digit_hex() {
        let dir = temp_group_dir();
        assert_eq!(
            group_dir_path(dir.path(), 0).file_name().unwrap(),
            std::ffi::OsStr::new("00")
        );
        assert_eq!(
            group_dir_path(dir.path(), 255).file_name().unwrap(),
            std::ffi::OsStr::new("ff")
        );
    }

    #[test]
    fn every_path_formatter_agrees_with_hex_formatting() {
        let dir = temp_group_dir();
        for index in 0..=255usize {
            let byte = index as u8;
            let mut hex = [0u8; 2];
            write_hex_byte(&mut hex, byte);
            assert_eq!(std::str::from_utf8(&hex).unwrap(), format!("{byte:02x}"));
            assert_eq!(
                group_dir_path(dir.path(), index).file_name().unwrap(),
                std::ffi::OsStr::new(&format!("{byte:02x}"))
            );
            assert_eq!(
                bucket_path(dir.path(), index).file_name().unwrap(),
                std::ffi::OsStr::new(&format!("{BUCKET_FILENAME_PREFIX}{byte:02x}"))
            );
            assert_eq!(
                bucket_new_path(dir.path(), index).file_name().unwrap(),
                std::ffi::OsStr::new(&format!(
                    "{BUCKET_FILENAME_PREFIX}{byte:02x}{BUCKET_NEW_SUFFIX}"
                ))
            );
        }
    }

    #[tokio::test]
    async fn group_has_buckets_missing_directory_is_false() {
        let dir = temp_group_dir();
        assert!(!group_has_buckets(&dir.path().join("absent")).await);
    }

    #[tokio::test]
    async fn group_has_buckets_empty_directory_is_false() {
        let dir = temp_group_dir();
        assert!(!group_has_buckets(dir.path()).await);
    }

    #[tokio::test]
    async fn group_has_buckets_level_files_alone_are_false() {
        let dir = temp_group_dir();
        write_level_marker(dir.path(), 32, false).await.unwrap();
        write_level_pending(dir.path(), 64, false).await.unwrap();
        assert!(!group_has_buckets(dir.path()).await);
    }

    #[tokio::test]
    async fn group_has_buckets_uncommitted_new_twins_alone_are_false() {
        let dir = temp_group_dir();
        for bb in 0..4 {
            std::fs::write(bucket_new_path(dir.path(), bb), [bb as u8; 8]).unwrap();
        }
        assert!(!group_has_buckets(dir.path()).await);
    }

    #[tokio::test]
    async fn group_has_buckets_committed_bucket_is_true() {
        let dir = temp_group_dir();
        std::fs::write(bucket_path(dir.path(), 0x37), b"bucket").unwrap();
        std::fs::write(bucket_new_path(dir.path(), 0x38), b"twin").unwrap();
        assert!(group_has_buckets(dir.path()).await);
    }

    #[test]
    fn unwritten_group_level_follows_the_initial_level_when_fan_out_aware() {
        for &level in &LEVEL_LADDER {
            assert_eq!(unwritten_group_level(true, level), level);
        }
    }

    #[test]
    fn unwritten_group_level_is_max_without_fan_out_awareness() {
        for &level in &LEVEL_LADDER {
            assert_eq!(unwritten_group_level(false, level), FAN_OUT_LEVEL_MAX);
        }
    }

    #[tokio::test]
    async fn read_group_level_marker_wins_over_bucket_files() {
        let dir = temp_group_dir();
        std::fs::write(bucket_path(dir.path(), 0x37), b"bucket").unwrap();
        write_level_marker(dir.path(), 64, false).await.unwrap();
        assert_eq!(
            read_group_level(dir.path()).await.unwrap(),
            GroupLevel::Marked(64)
        );
    }

    #[tokio::test]
    async fn read_group_level_buckets_without_marker_are_pre_fan_out() {
        let dir = temp_group_dir();
        std::fs::write(bucket_path(dir.path(), 0x37), b"bucket").unwrap();
        assert_eq!(
            read_group_level(dir.path()).await.unwrap(),
            GroupLevel::PreFanOut
        );
    }

    #[tokio::test]
    async fn read_group_level_empty_group_is_unwritten() {
        let dir = temp_group_dir();
        assert_eq!(
            read_group_level(dir.path()).await.unwrap(),
            GroupLevel::Unwritten
        );
    }

    #[tokio::test]
    async fn read_group_level_propagates_a_corrupt_marker() {
        let dir = temp_group_dir();
        std::fs::write(dir.path().join(MARKER_FILENAME), [0xDE; 16]).unwrap();
        let err = read_group_level(dir.path()).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn recover_level_transition_no_pending_is_noop() {
        let dir = temp_group_dir();
        assert_eq!(
            recover_level_transition(dir.path(), false).await.unwrap(),
            None
        );
        // No marker should be created when there's nothing to recover.
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn recover_level_transition_renames_all_new_files() {
        let dir = temp_group_dir();
        // Set up "after pending, before any rename" state for target=4: four .new files exist with synthetic content; pending says target=4; no marker yet.
        for bb in 0..4 {
            std::fs::write(bucket_new_path(dir.path(), bb), [bb as u8; 8]).unwrap();
        }
        write_level_pending(dir.path(), 4, false).await.unwrap();

        let recovered = recover_level_transition(dir.path(), false).await.unwrap();
        assert_eq!(recovered, Some(4));

        // All .new files renamed to final.
        for bb in 0..4 {
            assert!(!bucket_new_path(dir.path(), bb).exists());
            let bytes = std::fs::read(bucket_path(dir.path(), bb)).unwrap();
            assert_eq!(bytes, vec![bb as u8; 8]);
        }
        // Marker reflects target.
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), Some(4));
        // Pending deleted.
        assert!(!dir.path().join(LEVEL_PENDING_FILENAME).exists());
    }

    #[tokio::test]
    async fn recover_level_transition_skips_already_renamed_buckets() {
        let dir = temp_group_dir();
        // Mid-rename state: bb=0 already renamed (final present, no .new); bb=1 still .new.
        std::fs::write(bucket_path(dir.path(), 0), b"final-0").unwrap();
        std::fs::write(bucket_new_path(dir.path(), 1), b"new-1").unwrap();
        write_level_pending(dir.path(), 2, false).await.unwrap();

        let recovered = recover_level_transition(dir.path(), false).await.unwrap();
        assert_eq!(recovered, Some(2));

        assert_eq!(
            std::fs::read(bucket_path(dir.path(), 0)).unwrap(),
            b"final-0"
        );
        assert_eq!(std::fs::read(bucket_path(dir.path(), 1)).unwrap(), b"new-1");
        assert!(!bucket_new_path(dir.path(), 1).exists());
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), Some(2));
        assert!(!dir.path().join(LEVEL_PENDING_FILENAME).exists());
    }

    #[tokio::test]
    async fn recover_level_transition_is_idempotent() {
        let dir = temp_group_dir();
        for bb in 0..2 {
            std::fs::write(bucket_new_path(dir.path(), bb), [bb as u8; 4]).unwrap();
        }
        write_level_pending(dir.path(), 2, false).await.unwrap();

        let first = recover_level_transition(dir.path(), false).await.unwrap();
        let second = recover_level_transition(dir.path(), false).await.unwrap();
        // First run rolls forward; second run is a no-op since pending is gone.
        assert_eq!(first, Some(2));
        assert_eq!(second, None);
        assert_eq!(read_level_marker(dir.path()).await.unwrap(), Some(2));
    }
}
