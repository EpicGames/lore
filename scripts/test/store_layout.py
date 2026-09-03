# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Checks on the on-disk layout of a local immutable store.

The bucket a hash belongs to is derived from its group's fan-out level, so an entry is
only reachable from the bucket file that level places it in. An entry sitting in any
other file is present on disk and unreachable, and reads of it report `Address not
found`. `repository verify` cannot see this: it walks the entries the index resolves,
which is exactly the set that excludes them.
"""

import os
import struct

_MARKER_MAGIC = int.from_bytes(b"LVNO", "little")
_MARKER_FILENAME = "level"
_BUCKET_PREFIX = "index_"
_HEADER_SIZE = 16
_HASH_SIZE = 32
_FAN_OUT_LEVEL_MAX = 256


def _bucket_for(bucket_byte: int, level: int) -> int:
    """The bucket `bucket_byte` belongs to in a group holding `level` buckets."""
    if level <= 1:
        return 0
    return bucket_byte >> (_FAN_OUT_LEVEL_MAX // level).bit_length() - 1


def _committed_level(group_dir: str) -> int:
    """The level a group's marker records, or the pre-fan-out level when it has none."""
    try:
        with open(os.path.join(group_dir, _MARKER_FILENAME), "rb") as handle:
            magic, _version, level, _reserved = struct.unpack("<IIII", handle.read(16))
    except (OSError, struct.error):
        return _FAN_OUT_LEVEL_MAX
    return level if magic == _MARKER_MAGIC else _FAN_OUT_LEVEL_MAX


def _entry_hashes(path: str):
    """The hash of every entry in a bucket file."""
    with open(path, "rb") as handle:
        blob = handle.read()
    if len(blob) < _HEADER_SIZE:
        return
    _version, _unused, count, _unused_two = struct.unpack("<IIII", blob[:_HEADER_SIZE])
    if count == 0:
        return
    start = _HEADER_SIZE + 4 * count
    stride = (len(blob) - start) // count
    if stride <= _HASH_SIZE:
        return
    for index in range(count):
        offset = start + index * stride
        yield blob[offset : offset + _HASH_SIZE]


def unreachable_index_entries(store_root: str) -> list[str]:
    """Describe every entry that its group's committed level places in another bucket.

    `store_root` is the directory holding `index/`. A missing directory yields nothing,
    which is what a repository backed by a shared store looks like from here.
    """
    findings = []
    index_root = os.path.join(store_root, "index")
    if not os.path.isdir(index_root):
        return findings
    for group in sorted(os.listdir(index_root)):
        group_dir = os.path.join(index_root, group)
        if not os.path.isdir(group_dir):
            continue
        level = _committed_level(group_dir)
        for name in sorted(os.listdir(group_dir)):
            if not name.startswith(_BUCKET_PREFIX):
                continue
            suffix = name[len(_BUCKET_PREFIX) :]
            if len(suffix) != 2:
                continue
            bucket = int(suffix, 16)
            stranded = sum(
                1
                for digest in _entry_hashes(os.path.join(group_dir, name))
                if _bucket_for(digest[1], level) != bucket
            )
            if stranded:
                findings.append(
                    f"{group}/{name}: {stranded} entries unreachable at level {level}"
                )
    return findings


def assert_all_index_entries_reachable(repo) -> None:
    """Fail when the repository's local store holds an entry no lookup can reach."""
    store_root = os.path.join(repo.dot_path(), "immutable")
    findings = unreachable_index_entries(store_root)
    assert not findings, (
        f"local store at {store_root} holds entries stranded by a fan-out level change:\n"
        + "\n".join(findings)
    )
