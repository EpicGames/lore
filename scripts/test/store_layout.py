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
# An immutable bucket header's second field: set bit, count of packfile sizes recorded, and the
# 8-byte id and size of each one after the first following the entries.
_SIZES_RECORDED = 1 << 31
_SIZES_COUNT_SHIFT = 24
_PACKFILE_SIZE_SIZE = 8


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


def index_hashes(store_root: str) -> set[bytes]:
    """The hash of every entry in the bucket files of the store at `store_root`, the
    directory holding `index/`."""
    hashes = set()
    index_root = os.path.join(store_root, "index")
    if not os.path.isdir(index_root):
        return hashes
    for group in os.listdir(index_root):
        group_dir = os.path.join(index_root, group)
        if not os.path.isdir(group_dir):
            continue
        for name in os.listdir(group_dir):
            if name.startswith(_BUCKET_PREFIX) and len(name) == len(_BUCKET_PREFIX) + 2:
                hashes.update(_entry_hashes(os.path.join(group_dir, name)))
    return hashes


def assert_all_index_entries_reachable(repo) -> None:
    """Fail when the repository's local store holds an entry no lookup can reach."""
    store_root = os.path.join(repo.dot_path(), "immutable")
    findings = unreachable_index_entries(store_root)
    assert not findings, (
        f"local store at {store_root} holds entries stranded by a fan-out level change:\n"
        + "\n".join(findings)
    )


def _torn_file(path: str, name: str) -> str | None:
    """Why the bucket or level file at `path` cannot be one a whole write left, or None."""
    size = os.path.getsize(path)
    if name in (_MARKER_FILENAME, _MARKER_FILENAME + ".pending"):
        return None if size == _HEADER_SIZE else f"{size} bytes, not {_HEADER_SIZE}"
    if size < _HEADER_SIZE:
        return f"{size} bytes, shorter than its header"
    with open(path, "rb") as handle:
        _version, packfile_sizes, count, _first_packfile_size = struct.unpack(
            "<IIII", handle.read(_HEADER_SIZE)
        )
    trailer_size = 0
    if packfile_sizes & _SIZES_RECORDED:
        recorded = (packfile_sizes & ~_SIZES_RECORDED) >> _SIZES_COUNT_SHIFT
        trailer_size = max(recorded - 1, 0) * _PACKFILE_SIZE_SIZE
    entries_size = size - _HEADER_SIZE - 4 * count - trailer_size
    if count == 0:
        return None if entries_size == 0 else f"{size} bytes for no entries"
    if (
        entries_size <= 0
        or entries_size % count != 0
        or entries_size // count <= _HASH_SIZE
    ):
        return f"{size} bytes for {count} entries"
    return None


def torn_index_files(store_root: str) -> list[str]:
    """Describe every bucket and level file under `store_root`'s `index/` that a write cut short
    could have left: a bucket file shorter than its header and the entries its header counts,
    or a level file that is not one whole header. A missing `index/` is a finding too."""
    index_root = os.path.join(store_root, "index")
    if not os.path.isdir(index_root):
        return [f"{index_root} is missing"]
    findings = []
    for group in sorted(os.listdir(index_root)):
        group_dir = os.path.join(index_root, group)
        if not os.path.isdir(group_dir):
            continue
        for name in sorted(os.listdir(group_dir)):
            is_bucket = (
                name.startswith(_BUCKET_PREFIX) and len(name) == len(_BUCKET_PREFIX) + 2
            )
            if not is_bucket and name not in (
                _MARKER_FILENAME,
                _MARKER_FILENAME + ".pending",
            ):
                continue
            reason = _torn_file(os.path.join(group_dir, name), name)
            if reason:
                findings.append(f"{group}/{name}: {reason}")
    return findings
