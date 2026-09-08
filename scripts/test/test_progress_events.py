# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os

import pytest
from lore_parsers import parse_jsonl

from lore import Lore

logger = logging.getLogger(__name__)

# Known test file content for predictable byte counts (~300 KiB total across 3 files)
FILE_SIZE = 100 * 1024  # 100 KiB per file
FILE_CONTENTS = {
    "file_a.bin": os.urandom(FILE_SIZE),
    "file_b.bin": os.urandom(FILE_SIZE),
    "subdir/file_c.bin": os.urandom(FILE_SIZE),
}

TOTAL_FILE_COUNT = len(FILE_CONTENTS)
TOTAL_BYTE_COUNT = sum(len(v) for v in FILE_CONTENTS.values())


def create_test_repo(repo: Lore):
    """Populate a repo with known files, stage, commit, and push."""
    for path, content in FILE_CONTENTS.items():
        dirname = os.path.dirname(path)
        if dirname:
            repo.make_dirs(dirname)
        with repo.open_file(path, "w+b") as f:
            f.write(content)
    repo.stage(scan=True, offline=True)
    repo.commit("Initial commit", offline=True)
    repo.push()


def print_events(label, events):
    """Pretty-print a list of progress events for debugging."""
    print(f"\n{'=' * 70}")
    print(f"  {label}  ({len(events)} events)")
    print(f"{'=' * 70}")
    for i, event in enumerate(events):
        print(f"  [{i}] {json.dumps(event, indent=None)}")
    print(f"{'─' * 70}")


def assert_progress_is_increasing(events, field):
    """Assert that a numeric field never decreases across ordered progress events."""
    values = [e[field] for e in events if field in e]
    for i in range(1, len(values)):
        assert values[i] >= values[i - 1], (
            f"{field} decreased from {values[i - 1]} to {values[i]} at event index {i}"
        )


def assert_discovery_complete_consistency(
    progress_events, end_event=None, discovery_field="discoveryComplete"
):
    """
    Once discoveryComplete becomes True, it must remain True for all subsequent events.
    Also verify that at least one event (progress or end) has discoveryComplete=True.
    """
    saw_complete = False
    any_complete = False
    for i, event in enumerate(progress_events):
        is_complete = event.get(discovery_field, False)
        if is_complete:
            any_complete = True
        if saw_complete:
            assert is_complete, (
                f"discoveryComplete reverted to False at event index {i} after being True"
            )
        if is_complete:
            saw_complete = True

    # The end event (if provided) should also have discoveryComplete=True
    if end_event is not None:
        end_complete = end_event.get(discovery_field, False)
        if end_complete:
            any_complete = True
        if saw_complete:
            assert end_complete, (
                "discoveryComplete reverted to False in end event after being True in progress"
            )

    assert any_complete, "discoveryComplete was never True in any progress or end event"


def assert_end_gte_last_progress(last_progress, end_event, fields):
    """Assert that the end event values are >= the last progress event for the given fields."""
    for field in fields:
        p_val = last_progress.get(field, 0)
        e_val = end_event.get(field, 0)
        assert e_val >= p_val, (
            f"End event {field}={e_val} is less than last progress {field}={p_val}"
        )


def assert_work_lte_total(events, field_pairs):
    """
    Assert that work-done fields never exceed their corresponding total fields.

    field_pairs: list of (work_field, total_field) tuples, e.g.
        [("fileComplete", "fileCount"), ("bytesTransferred", "bytesTotal")]
    """
    for i, event in enumerate(events):
        for work_field, total_field in field_pairs:
            work = event.get(work_field, 0)
            total = event.get(total_field, 0)
            assert work <= total, (
                f"Event {i}: {work_field}={work} exceeds {total_field}={total}"
            )


def assert_post_discovery_totals(
    events, expected_totals, discovery_field="discoveryComplete"
):
    """
    Verify that every progress event where discoveryComplete is True reports the
    expected total values. Once discovery finishes the totals are known and must
    match the actual file/byte counts for the operation.

    expected_totals: dict mapping field name -> expected value, e.g.
        {"bytesTotal": 100_000_000, "fileCount": 10_000}
    """
    for i, event in enumerate(events):
        if not event.get(discovery_field, False):
            continue
        for field, expected in expected_totals.items():
            actual = event.get(field)
            assert actual == expected, (
                f"Post-discovery event {i}: {field}={actual}, expected {expected}"
            )


# ─── Clone progress ────────────────────────────────────────────────


@pytest.mark.smoke
def test_clone_progress_events(new_lore_repo, scratch_dir):
    """Clone progress events report correct file/byte counts and monotonic progress."""
    source: Lore = new_lore_repo()
    create_test_repo(source)

    # Clone with JSON to capture progress events
    new_repo_path = str(scratch_dir("clone_progress_", create=True))
    output = source.run(
        ["repository", "clone", source.remote + source.name, new_repo_path],
        json=True,
    )

    progress_events = parse_jsonl(output, "repositoryCloneProgress")
    end_events = parse_jsonl(output, "repositoryCloneEnd")

    assert len(end_events) == 1, "Expected exactly one clone end event"

    # Extract count sub-objects
    progress_counts = [e["count"] for e in progress_events]
    end_count = end_events[0]["count"]

    print_events("CLONE progress (count)", progress_counts)
    print_events("CLONE end (count)", [end_count])

    # Must have at least one progress event with discoveryComplete=true
    assert len(progress_counts) > 0, "Expected at least one clone progress event"
    assert_discovery_complete_consistency(progress_counts)

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_counts,
        [
            ("fileComplete", "fileCount"),
            ("bytesTransferred", "bytesTotal"),
        ],
    )

    # Progress should be monotonically increasing
    assert_progress_is_increasing(progress_counts, "fileComplete")
    assert_progress_is_increasing(progress_counts, "bytesTransferred")

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_counts,
        {
            "fileCount": TOTAL_FILE_COUNT,
            "bytesTotal": TOTAL_BYTE_COUNT,
        },
    )

    # End event should reflect the correct totals
    assert end_count["fileCount"] == TOTAL_FILE_COUNT, (
        f"Expected {TOTAL_FILE_COUNT} files, got {end_count['fileCount']}"
    )
    assert end_count["bytesTotal"] == TOTAL_BYTE_COUNT, (
        f"Expected {TOTAL_BYTE_COUNT} bytes total, got {end_count['bytesTotal']}"
    )
    assert end_count["fileComplete"] == TOTAL_FILE_COUNT, (
        f"Expected {TOTAL_FILE_COUNT} files complete, got {end_count['fileComplete']}"
    )
    assert end_count["bytesTransferred"] == TOTAL_BYTE_COUNT, (
        f"Expected {TOTAL_BYTE_COUNT} bytes transferred, got {end_count['bytesTransferred']}"
    )
    assert end_count["discoveryComplete"] is True, (
        "discoveryComplete should be True in end event"
    )


# ─── Commit progress ───────────────────────────────────────────────


@pytest.mark.smoke
def test_commit_progress_events(new_lore_repo):
    """Commit progress events report correct file/byte counts and monotonic progress."""
    repo: Lore = new_lore_repo()

    # Create files
    for path, content in FILE_CONTENTS.items():
        dirname = os.path.dirname(path)
        if dirname:
            repo.make_dirs(dirname)
        with repo.open_file(path, "w+b") as f:
            f.write(content)
    repo.stage(scan=True, offline=True)

    # Commit with JSON output
    output = repo.commit("Test commit", offline=True, json=True)

    progress_events = parse_jsonl(output, "revisionCommitProgress")
    end_events = parse_jsonl(output, "revisionCommitEnd")

    assert len(end_events) == 1, "Expected exactly one commit end event"

    progress_counts = [e["count"] for e in progress_events]
    end_count = end_events[0]["count"]

    print_events("COMMIT progress (count)", progress_counts)
    print_events("COMMIT end (count)", [end_count])

    # Must have at least one progress event with discoveryComplete=true
    assert len(progress_counts) > 0, "Expected at least one commit progress event"
    assert_discovery_complete_consistency(progress_counts)

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_counts,
        [
            ("fileCount", "fileTotal"),
            ("bytesTransferred", "bytesTotal"),
            ("directoryCount", "directoryTotal"),
        ],
    )

    # Progress should be monotonically increasing
    assert_progress_is_increasing(progress_counts, "fileCount")
    assert_progress_is_increasing(progress_counts, "bytesTransferred")
    assert_progress_is_increasing(progress_counts, "directoryCount")

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_counts,
        {
            "fileTotal": TOTAL_FILE_COUNT,
            "bytesTotal": TOTAL_BYTE_COUNT,
        },
    )

    # End event should reflect correct totals
    assert end_count["fileTotal"] == TOTAL_FILE_COUNT, (
        f"Expected {TOTAL_FILE_COUNT} file total, got {end_count['fileTotal']}"
    )
    assert end_count["bytesTotal"] == TOTAL_BYTE_COUNT, (
        f"Expected {TOTAL_BYTE_COUNT} bytes total, got {end_count['bytesTotal']}"
    )
    assert end_count["fileCount"] == TOTAL_FILE_COUNT, (
        f"Expected {TOTAL_FILE_COUNT} files processed, got {end_count['fileCount']}"
    )
    assert end_count["bytesTransferred"] == TOTAL_BYTE_COUNT, (
        f"Expected {TOTAL_BYTE_COUNT} bytes transferred, got {end_count['bytesTransferred']}"
    )
    assert end_count["discoveryComplete"] is True, (
        "discoveryComplete should be True in end event"
    )
    assert end_count["fileModifyCount"] == TOTAL_FILE_COUNT, (
        f"Expected {TOTAL_FILE_COUNT} modified files, got {end_count['fileModifyCount']}"
    )
    assert end_count["fileDeleteCount"] == 0, (
        f"Expected 0 deleted files, got {end_count['fileDeleteCount']}"
    )


# ─── Sync progress ─────────────────────────────────────────────────


@pytest.mark.smoke
def test_sync_progress_events(new_lore_repo):
    """Sync progress events report correct file/byte counts and monotonic progress."""
    source: Lore = new_lore_repo()
    create_test_repo(source)

    # Clone repo
    cloned = source.clone()

    # Add more files in source and push a new revision
    extra_file_size = 50 * 1024  # 50 KiB each
    extra_files = {
        "extra_1.bin": os.urandom(extra_file_size),
        "extra_2.bin": os.urandom(extra_file_size),
    }
    for path, content in extra_files.items():
        with source.open_file(path, "w+b") as f:
            f.write(content)
    source.stage(scan=True, offline=True)
    source.commit("Add extra files", offline=True)
    source.push()

    extra_file_count = len(extra_files)
    extra_byte_count = sum(len(v) for v in extra_files.values())

    # Sync cloned repo with JSON output
    output = cloned.sync(json=True)

    progress_events = parse_jsonl(output, "revisionSyncProgress")
    revision_events = parse_jsonl(output, "revisionSyncRevision")

    assert len(progress_events) > 0, "Expected at least one sync progress event"
    assert len(revision_events) == 1, "Expected exactly one sync revision event"

    print_events("SYNC progress", progress_events)
    print_events("SYNC revision", revision_events)

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_events,
        [
            ("fileUpdate", "fileUpdateTotal"),
            ("bytesUpdate", "bytesUpdateTotal"),
            ("fileDelete", "fileDeleteTotal"),
        ],
    )

    # Progress should be monotonically increasing
    assert_progress_is_increasing(progress_events, "fileUpdate")
    assert_progress_is_increasing(progress_events, "bytesUpdate")

    # Discovery complete consistency
    assert_discovery_complete_consistency(progress_events)

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_events,
        {
            "fileUpdateTotal": extra_file_count,
            "bytesUpdateTotal": extra_byte_count,
        },
    )

    # Final progress event should reflect the synced files
    # (sync always emits a final progress event after processing)
    last_progress = progress_events[-1]
    assert last_progress["fileUpdateTotal"] == extra_file_count, (
        f"Expected {extra_file_count} file updates, got {last_progress['fileUpdateTotal']}"
    )
    assert last_progress["bytesUpdateTotal"] == extra_byte_count, (
        f"Expected {extra_byte_count} bytes, got {last_progress['bytesUpdateTotal']}"
    )
    assert last_progress["fileUpdate"] == extra_file_count, (
        f"Expected {extra_file_count} files updated, got {last_progress['fileUpdate']}"
    )
    assert last_progress["bytesUpdate"] == extra_byte_count, (
        f"Expected {extra_byte_count} bytes updated, got {last_progress['bytesUpdate']}"
    )
    assert last_progress["fileDeleteTotal"] == 0, (
        f"Expected 0 deletes, got {last_progress['fileDeleteTotal']}"
    )
    assert last_progress["fileConflict"] == 0, (
        f"Expected 0 conflicts, got {last_progress['fileConflict']}"
    )
    assert last_progress["discoveryComplete"] is True, (
        "discoveryComplete should be True in final sync progress event"
    )

    # Revision event should indicate a clean sync (no merge, no conflict)
    assert revision_events[0]["flagMerge"] is False
    assert revision_events[0]["flagConflict"] is False


# ─── Push progress ──────────────────────────────────────────────────


@pytest.mark.smoke
def test_push_progress_events(new_lore_repo):
    """Push progress events should be small since commit uploads file content."""
    repo: Lore = new_lore_repo()

    # Create files, stage, and commit (online so commit uploads content)
    for path, content in FILE_CONTENTS.items():
        dirname = os.path.dirname(path)
        if dirname:
            repo.make_dirs(dirname)
        with repo.open_file(path, "w+b") as f:
            f.write(content)
    repo.stage(scan=True, offline=True)
    repo.commit("Test commit for push")

    # Push with JSON output
    output = repo.push(json=True)

    push_events = parse_jsonl(output, "branchPush")
    fragment_begin_events = parse_jsonl(output, "branchPushFragmentBegin")
    fragment_progress_events = parse_jsonl(output, "branchPushFragmentProgress")
    fragment_end_events = parse_jsonl(output, "branchPushFragmentEnd")

    print_events("PUSH branchPush", push_events)
    print_events("PUSH fragmentBegin", fragment_begin_events)
    print_events("PUSH fragmentProgress", fragment_progress_events)
    print_events("PUSH fragmentEnd", fragment_end_events)

    assert len(push_events) == 1, "Expected exactly one branchPush event"

    # Push should not be flagged as already pushed
    assert push_events[0]["flagAlreadyPushed"] is False

    # Must have fragment begin/end and at least one progress event
    assert len(fragment_begin_events) == 1, "Expected exactly one fragment begin event"
    assert len(fragment_end_events) == 1, "Expected exactly one fragment end event"
    assert len(fragment_progress_events) > 0, (
        "Expected at least one fragment progress event"
    )

    # Work done must never exceed totals
    assert_work_lte_total(
        fragment_progress_events,
        [
            ("complete", "count"),
            ("bytesTransferred", "bytesTotal"),
        ],
    )

    # Fragment transfer happened - verify progress is monotonic
    assert_progress_is_increasing(fragment_progress_events, "complete")
    assert_progress_is_increasing(fragment_progress_events, "bytesTransferred")

    # Last progress event should reflect completed push
    last_progress = fragment_progress_events[-1]
    assert last_progress["complete"] == last_progress["count"], (
        f"Last progress complete={last_progress['complete']} != count={last_progress['count']}"
    )
    assert last_progress["bytesTransferred"] == last_progress["bytesTotal"], (
        f"Last progress bytesTransferred={last_progress['bytesTransferred']} "
        f"!= bytesTotal={last_progress['bytesTotal']}"
    )

    # Push should be small since commit uploads file content
    end = fragment_end_events[0]
    assert end["bytesTransferred"] < TOTAL_BYTE_COUNT // 2, (
        f"Push transferred {end['bytesTransferred']} bytes, expected less than half "
        f"of the {TOTAL_BYTE_COUNT} byte payload (commit should upload most content)"
    )

    # Verify revision push events
    rev_push_end = parse_jsonl(output, "branchPushRevisionPushEnd")
    assert len(rev_push_end) == 1, "Expected exactly one revision push end event"


# ─── Large repo constants ──────────────────────────────────────────

LARGE_FILE_COUNT = 10_000
LARGE_FILE_SIZE = 10_000  # 10 000 bytes per file
LARGE_TOTAL_BYTES = LARGE_FILE_COUNT * LARGE_FILE_SIZE
LARGE_DIR_COUNT = 100  # spread files across 100 directories (100 files each)


def generate_large_repo_files(repo: Lore):
    """Generate 10 000 files of 10 000 byte random binary data spread across directories."""
    for d in range(LARGE_DIR_COUNT):
        dir_path = f"dir_{d:03d}"
        repo.make_dirs(dir_path)
        files_per_dir = LARGE_FILE_COUNT // LARGE_DIR_COUNT
        for f in range(files_per_dir):
            file_path = os.path.join(dir_path, f"file_{f:04d}.bin")
            with repo.open_file(file_path, "w+b") as fh:
                fh.write(os.urandom(LARGE_FILE_SIZE))


def create_large_test_repo(repo: Lore):
    """Generate large files, stage, commit, and push."""
    generate_large_repo_files(repo)
    repo.stage(scan=True)
    repo.commit("Initial large commit")
    repo.push()


# ─── Large repo: Commit progress ──────────────────────────────────


@pytest.mark.smoke
def test_large_commit_progress_events(new_lore_repo):
    """Commit progress for 10k x 10 000 byte files reports correct counts and monotonic progress."""
    repo: Lore = new_lore_repo()
    generate_large_repo_files(repo)
    repo.stage(scan=True)

    output = repo.commit("Large commit", json=True)

    progress_events = parse_jsonl(output, "revisionCommitProgress")
    end_events = parse_jsonl(output, "revisionCommitEnd")

    assert len(progress_events) > 0, "Expected at least one commit progress event"
    assert len(end_events) == 1, "Expected exactly one commit end event"

    progress_counts = [e["count"] for e in progress_events]
    end_count = end_events[0]["count"]

    print_events("LARGE COMMIT progress (count)", progress_counts)
    print_events("LARGE COMMIT end (count)", [end_count])

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_counts,
        [
            ("fileCount", "fileTotal"),
            ("bytesTransferred", "bytesTotal"),
            ("directoryCount", "directoryTotal"),
        ],
    )

    # Monotonic progress
    assert_progress_is_increasing(progress_counts, "fileCount")
    assert_progress_is_increasing(progress_counts, "bytesTransferred")
    assert_progress_is_increasing(progress_counts, "directoryCount")

    # Must have discoveryComplete=true in progress events
    assert_discovery_complete_consistency(progress_counts)

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_counts,
        {
            "fileTotal": LARGE_FILE_COUNT,
            "bytesTotal": LARGE_TOTAL_BYTES,
        },
    )

    # With 10k files we expect multiple progress ticks
    assert len(progress_counts) > 1, (
        "Expected multiple progress events for a large commit"
    )

    # End event must be >= last progress event
    assert_end_gte_last_progress(
        progress_counts[-1],
        end_count,
        ["fileCount", "bytesTransferred", "directoryCount"],
    )

    # End event totals
    assert end_count["fileTotal"] == LARGE_FILE_COUNT, (
        f"Expected {LARGE_FILE_COUNT} files, got {end_count['fileTotal']}"
    )
    assert end_count["bytesTotal"] == LARGE_TOTAL_BYTES, (
        f"Expected {LARGE_TOTAL_BYTES} bytes, got {end_count['bytesTotal']}"
    )
    assert end_count["fileCount"] == LARGE_FILE_COUNT, (
        f"Expected {LARGE_FILE_COUNT} files processed, got {end_count['fileCount']}"
    )
    assert end_count["bytesTransferred"] == LARGE_TOTAL_BYTES, (
        f"Expected {LARGE_TOTAL_BYTES} bytes transferred, got {end_count['bytesTransferred']}"
    )
    assert end_count["discoveryComplete"] is True
    assert end_count["fileModifyCount"] == LARGE_FILE_COUNT
    assert end_count["fileDeleteCount"] == 0


# ─── Large repo: Push progress ─────────────────────────────────────


@pytest.mark.smoke
def test_large_push_progress_events(new_lore_repo):
    """Push progress for a large repo after commit. Commit uploads content so push is metadata-heavy."""
    repo: Lore = new_lore_repo()
    generate_large_repo_files(repo)
    repo.stage(scan=True)
    repo.commit("Large commit for push")

    output = repo.push(json=True)

    push_events = parse_jsonl(output, "branchPush")
    fragment_begin_events = parse_jsonl(output, "branchPushFragmentBegin")
    fragment_progress_events = parse_jsonl(output, "branchPushFragmentProgress")
    fragment_end_events = parse_jsonl(output, "branchPushFragmentEnd")

    print_events("LARGE PUSH branchPush", push_events)
    print_events("LARGE PUSH fragmentBegin", fragment_begin_events)
    print_events("LARGE PUSH fragmentProgress", fragment_progress_events)
    print_events("LARGE PUSH fragmentEnd", fragment_end_events)

    assert len(push_events) == 1, "Expected exactly one branchPush event"
    assert push_events[0]["flagAlreadyPushed"] is False

    # Work done must never exceed totals
    assert_work_lte_total(
        fragment_progress_events,
        [
            ("complete", "count"),
            ("bytesTransferred", "bytesTotal"),
        ],
    )

    if len(fragment_begin_events) > 0:
        assert_progress_is_increasing(fragment_progress_events, "complete")
        assert_progress_is_increasing(fragment_progress_events, "bytesTransferred")

        assert len(fragment_end_events) == 1
        end = fragment_end_events[0]
        assert end["fragments"] > 0, "Expected fragments to be pushed"
        assert end["bytesTransferred"] > 0, "Expected bytes transferred during push"

        # Push should be small since commit uploads file content
        assert end["bytesTransferred"] < LARGE_TOTAL_BYTES // 2, (
            f"Push transferred {end['bytesTransferred']} bytes, expected less than half "
            f"of the {LARGE_TOTAL_BYTES} byte payload (commit should upload most content)"
        )

        # End event bytes must be >= last progress event bytes
        if len(fragment_progress_events) > 0:
            last_frag = fragment_progress_events[-1]
            assert end["bytesTransferred"] >= last_frag["bytesTransferred"], (
                "Push fragment end bytesTransferred < last progress bytesTransferred"
            )
            assert end["fragments"] >= last_frag["complete"], (
                "Push fragment end fragments < last progress complete"
            )

    rev_push_end = parse_jsonl(output, "branchPushRevisionPushEnd")
    assert len(rev_push_end) == 1, "Expected exactly one revision push end event"


# ─── Large repo: Clone progress ────────────────────────────────────


@pytest.mark.smoke
def test_large_clone_progress_events(new_lore_repo, scratch_dir):
    """Clone progress for 10k x 10 000 byte files reports correct counts and monotonic progress."""
    source: Lore = new_lore_repo()
    create_large_test_repo(source)

    new_repo_path = str(scratch_dir("large_clone_progress_", create=True))
    output = source.run(
        ["repository", "clone", source.remote + source.name, new_repo_path],
        json=True,
    )

    progress_events = parse_jsonl(output, "repositoryCloneProgress")
    end_events = parse_jsonl(output, "repositoryCloneEnd")

    assert len(progress_events) > 0, "Expected at least one clone progress event"
    assert len(end_events) == 1, "Expected exactly one clone end event"

    progress_counts = [e["count"] for e in progress_events]
    end_count = end_events[0]["count"]

    print_events("LARGE CLONE progress (count)", progress_counts)
    print_events("LARGE CLONE end (count)", [end_count])

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_counts,
        [
            ("fileComplete", "fileCount"),
            ("bytesTransferred", "bytesTotal"),
        ],
    )

    # Monotonic progress
    assert_progress_is_increasing(progress_counts, "fileComplete")
    assert_progress_is_increasing(progress_counts, "bytesTransferred")

    # Must have discoveryComplete=true in progress events
    assert_discovery_complete_consistency(progress_counts)

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_counts,
        {
            "fileCount": LARGE_FILE_COUNT,
            "bytesTotal": LARGE_TOTAL_BYTES,
        },
    )

    # With 10k files there must be multiple progress events
    assert len(progress_counts) > 1, (
        "Expected multiple progress events for a large clone"
    )

    # End event must be >= last progress event
    assert_end_gte_last_progress(
        progress_counts[-1],
        end_count,
        ["fileComplete", "bytesTransferred"],
    )

    # End event totals
    assert end_count["fileCount"] == LARGE_FILE_COUNT, (
        f"Expected {LARGE_FILE_COUNT} files, got {end_count['fileCount']}"
    )
    assert end_count["bytesTotal"] == LARGE_TOTAL_BYTES, (
        f"Expected {LARGE_TOTAL_BYTES} bytes, got {end_count['bytesTotal']}"
    )
    assert end_count["fileComplete"] == LARGE_FILE_COUNT, (
        f"Expected {LARGE_FILE_COUNT} files complete, got {end_count['fileComplete']}"
    )
    assert end_count["bytesTransferred"] == LARGE_TOTAL_BYTES, (
        f"Expected {LARGE_TOTAL_BYTES} bytes transferred, got {end_count['bytesTransferred']}"
    )
    assert end_count["discoveryComplete"] is True


# ─── Large repo: Sync progress ─────────────────────────────────────


@pytest.mark.smoke
def test_large_sync_progress_events(new_lore_repo):
    """Sync progress for a large batch of new files reports correct counts and monotonic progress."""
    source: Lore = new_lore_repo()
    create_large_test_repo(source)

    # Clone the repo at the initial revision
    cloned = source.clone()

    # Add a large batch of new files in source and push
    extra_count = 500
    extra_size = LARGE_FILE_SIZE
    extra_total_bytes = extra_count * extra_size
    extra_dir = "extra_sync"
    source.make_dirs(extra_dir)
    for f in range(extra_count):
        file_path = os.path.join(extra_dir, f"sync_{f:04d}.bin")
        with source.open_file(file_path, "w+b") as fh:
            fh.write(os.urandom(extra_size))
    source.stage(scan=True)
    source.commit("Add extra files for sync")
    source.push()

    # Sync the clone
    output = cloned.sync(json=True)

    progress_events = parse_jsonl(output, "revisionSyncProgress")
    revision_events = parse_jsonl(output, "revisionSyncRevision")

    assert len(progress_events) > 0, "Expected at least one sync progress event"
    assert len(revision_events) == 1, "Expected exactly one sync revision event"

    print_events("LARGE SYNC progress", progress_events)
    print_events("LARGE SYNC revision", revision_events)

    # Work done must never exceed totals
    assert_work_lte_total(
        progress_events,
        [
            ("fileUpdate", "fileUpdateTotal"),
            ("bytesUpdate", "bytesUpdateTotal"),
            ("fileDelete", "fileDeleteTotal"),
        ],
    )

    # Monotonic progress
    assert_progress_is_increasing(progress_events, "fileUpdate")
    assert_progress_is_increasing(progress_events, "bytesUpdate")

    # Discovery complete consistency
    assert_discovery_complete_consistency(progress_events)

    # After discovery, totals must match expected values
    assert_post_discovery_totals(
        progress_events,
        {
            "fileUpdateTotal": extra_count,
            "bytesUpdateTotal": extra_total_bytes,
        },
    )

    # With 500 large files there should be multiple progress events
    assert len(progress_events) > 1, (
        "Expected multiple progress events for a large sync"
    )

    # Final progress should match the extra files
    last_progress = progress_events[-1]
    assert last_progress["fileUpdateTotal"] == extra_count, (
        f"Expected {extra_count} file updates, got {last_progress['fileUpdateTotal']}"
    )
    assert last_progress["bytesUpdateTotal"] == extra_total_bytes, (
        f"Expected {extra_total_bytes} bytes, got {last_progress['bytesUpdateTotal']}"
    )
    assert last_progress["fileUpdate"] == extra_count, (
        f"Expected {extra_count} files updated, got {last_progress['fileUpdate']}"
    )
    assert last_progress["bytesUpdate"] == extra_total_bytes, (
        f"Expected {extra_total_bytes} bytes updated, got {last_progress['bytesUpdate']}"
    )
    assert last_progress["fileDeleteTotal"] == 0
    assert last_progress["fileConflict"] == 0
    assert last_progress["discoveryComplete"] is True

    # Clean sync
    assert revision_events[0]["flagMerge"] is False
    assert revision_events[0]["flagConflict"] is False
