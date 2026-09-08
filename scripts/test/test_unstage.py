# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import logging
import os

import pytest
from test_utils import to_posix
from lore_parsers import parse_jsonl, parse_status_json

from lore import Lore

logger = logging.getLogger(__name__)


def has_staged_anchor(repo: Lore) -> bool:
    """Check whether the repository has a staged revision by querying status."""
    output = repo.status(json=True, offline=True)
    zero_hash = "0" * 64
    for line in output.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        data = event.get("data", {})
        revision_staged = data.get("revisionStaged", "")
        if revision_staged and revision_staged != zero_hash:
            return True
    return False


@pytest.mark.smoke
def test_unstage_clears_stage_flags_keeps_dirty(new_lore_repo):
    """Unstage clears the stage flags on affected nodes but preserves the dirty
    flag, so a staged add survives as a dirty add. The staged anchor is NOT
    removed while any staged or dirty node remains; removing it entirely is the
    job of `status --reset`."""

    repo: Lore = new_lore_repo()

    # Setup: initial commit so main has content
    with repo.open_file("initial.txt", "w+") as f:
        f.write("initial content\n")
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    assert not has_staged_anchor(repo), "No staged anchor should exist after commit"

    repo.branch_create("test-unstage")

    # Scenario 1: Stage file in new directory, unstage the directory
    new_dir = "newDir"
    new_dir_file = os.path.join(new_dir, "file.txt")
    repo.make_dirs(new_dir)
    with repo.open_file(new_dir_file, "w+") as f:
        f.write("content in new directory\n")

    repo.stage(new_dir_file)
    assert has_staged_anchor(repo), "Staged anchor should exist after staging"

    repo.unstage(new_dir)

    assert has_staged_anchor(repo), (
        "anchor preserved after unstage — the unstaged add remains as a dirty add"
    )
    s1 = parse_status_json(repo.status(json=True))
    s1_file = next(
        (e for e in s1 if to_posix(e["path"]) == to_posix(new_dir_file)), None
    )
    assert (
        s1_file is not None
        and s1_file["flagStaged"] is False
        and s1_file["flagDirty"] is True
    ), "unstage clears the stage flag but keeps the dirty flag on the add"

    repo.branch_switch("main")
    repo.branch_switch("test-unstage")

    # Scenario 2: Stage file in new directory, unstage the file directly —
    # the parent directory remains staged (it's also a StagedAdd), then unstage it too
    another_dir = "anotherDir"
    another_file = os.path.join(another_dir, "file.txt")
    repo.make_dirs(another_dir)
    with repo.open_file(another_file, "w+") as f:
        f.write("content in another directory\n")

    repo.stage(another_file)
    assert has_staged_anchor(repo), "Staged anchor should exist after staging"

    repo.unstage(another_file)

    assert has_staged_anchor(repo), (
        "anchor preserved — parent directory is still staged"
    )

    s2 = parse_status_json(repo.status(json=True))
    s2_file = next(
        (e for e in s2 if to_posix(e["path"]) == to_posix(another_file)), None
    )
    assert (
        s2_file is not None
        and s2_file["flagStaged"] is False
        and s2_file["flagDirty"] is True
    ), "unstaged file becomes a dirty add (stage flag cleared, dirty kept)"
    s2_dir = next((e for e in s2 if to_posix(e["path"]) == to_posix(another_dir)), None)
    assert s2_dir is not None and s2_dir["flagStaged"] is True, (
        "parent directory stays staged when only its child is unstaged"
    )

    repo.unstage(another_dir)

    assert has_staged_anchor(repo), (
        "anchor preserved — the unstaged nodes remain as dirty adds"
    )

    repo.branch_switch("main")
    repo.branch_switch("test-unstage")

    # Scenario 3: Stage two directories, unstage one at a time by directory path
    dir_a = "dirA"
    dir_b = "dirB"
    file_a = os.path.join(dir_a, "a.txt")
    file_b = os.path.join(dir_b, "b.txt")

    repo.make_dirs(dir_a)
    repo.make_dirs(dir_b)
    with repo.open_file(file_a, "w+") as f:
        f.write("file A\n")
    with repo.open_file(file_b, "w+") as f:
        f.write("file B\n")

    repo.stage(file_a)
    repo.stage(file_b)
    assert has_staged_anchor(repo), "Staged anchor should exist after staging"

    # Unstage first directory — its nodes become dirty adds; dirB stays staged
    repo.unstage(dir_a)

    assert has_staged_anchor(repo), (
        "anchor preserved — dirB is still staged, dirA is now a dirty add"
    )

    s3 = parse_status_json(repo.status(json=True))
    s3_fa = next((e for e in s3 if to_posix(e["path"]) == to_posix(file_a)), None)
    assert (
        s3_fa is not None
        and s3_fa["flagStaged"] is False
        and s3_fa["flagDirty"] is True
    ), "file A is a dirty add after unstaging dirA"
    s3_fb = next((e for e in s3 if to_posix(e["path"]) == to_posix(file_b)), None)
    assert s3_fb is not None and s3_fb["flagStaged"] is True, "file B remains staged"

    # Unstage second directory — its nodes also become dirty adds
    repo.unstage(dir_b)

    assert has_staged_anchor(repo), (
        "anchor preserved — all unstaged nodes remain as dirty adds"
    )

    repo.branch_switch("main")


def get_unstage_counts(output: str) -> dict:
    """Extract the count object from the fileUnstageEnd event in JSON output."""
    events = parse_jsonl(output, "fileUnstageEnd")
    assert len(events) == 1, f"Expected 1 fileUnstageEnd event, got {len(events)}"
    return events[0]["count"]


def get_unstage_file_events(output: str) -> list[dict]:
    """Extract all fileUnstageFile events from JSON output."""
    return parse_jsonl(output, "fileUnstageFile")


@pytest.mark.smoke
def test_unstage_discard_counts(new_lore_repo):
    """Verify that unstage reports correct discard and unstage counts in the
    fileUnstageEnd event for various scenarios. Unstaging a staged add keeps it
    as a dirty add (counted as UNSTAGED, not discarded); only files emit
    per-node Keep events, directories are counted but do not emit events."""

    repo: Lore = new_lore_repo()

    # Setup: initial commit with a file so we can test unstage of committed files
    with repo.open_file("committed.txt", "w+") as f:
        f.write("committed content\n")
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.branch_create("test-discard-counts")

    # Scenario 1: Unstage a new staged file — kept as a dirty add (unstaged, not discarded)
    with repo.open_file("new_file.txt", "w+") as f:
        f.write("new content\n")

    repo.stage("new_file.txt")
    output = repo.unstage("new_file.txt", json=True)
    counts = get_unstage_counts(output)

    assert counts["fileDiscardedCount"] == 0, (
        f"Scenario 1: expected fileDiscardedCount=0, got {counts['fileDiscardedCount']}"
    )
    assert counts["fileUnstagedCount"] == 1, (
        f"Scenario 1: expected fileUnstagedCount=1, got {counts['fileUnstagedCount']}"
    )
    assert counts["directoryDiscardedCount"] == 0
    assert counts["directoryUnstagedCount"] == 0

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 2: Unstage a new file while another committed file is also staged (non-clear path)
    with repo.open_file("new_file2.txt", "w+") as f:
        f.write("another new file\n")

    # Modify the committed file so it can be staged
    with repo.open_file("committed.txt", "w+") as f:
        f.write("modified committed content\n")

    repo.stage("new_file2.txt")
    repo.stage("committed.txt")

    # Unstage only the new file — committed.txt remains staged, so clear=false
    output = repo.unstage("new_file2.txt", json=True)
    counts = get_unstage_counts(output)

    assert counts["fileDiscardedCount"] == 0, (
        f"Scenario 2: expected fileDiscardedCount=0, got {counts['fileDiscardedCount']}"
    )
    assert counts["fileUnstagedCount"] == 1, (
        f"Scenario 2: expected fileUnstagedCount=1, got {counts['fileUnstagedCount']}"
    )

    # Clean up: unstage committed.txt too
    repo.unstage("committed.txt")

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 3: Unstage a modified committed file (unstage, not discard)
    with repo.open_file("committed.txt", "w+") as f:
        f.write("modified again\n")

    repo.stage("committed.txt")
    output = repo.unstage("committed.txt", json=True)
    counts = get_unstage_counts(output)

    assert counts["fileUnstagedCount"] == 1, (
        f"Scenario 3: expected fileUnstagedCount=1, got {counts['fileUnstagedCount']}"
    )
    assert counts["fileDiscardedCount"] == 0, (
        f"Scenario 3: expected fileDiscardedCount=0, got {counts['fileDiscardedCount']}"
    )

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 4: Unstage a new directory with 1 file
    dir1 = "newdir1"
    dir1_file = os.path.join(dir1, "file.txt")
    repo.make_dirs(dir1)
    with repo.open_file(dir1_file, "w+") as f:
        f.write("file in new dir\n")

    repo.stage(dir1_file)
    output = repo.unstage(dir1, json=True)
    counts = get_unstage_counts(output)

    assert counts["directoryUnstagedCount"] == 1, (
        f"Scenario 4: expected directoryUnstagedCount=1, got {counts['directoryUnstagedCount']}"
    )
    assert counts["fileUnstagedCount"] == 1, (
        f"Scenario 4: expected fileUnstagedCount=1, got {counts['fileUnstagedCount']}"
    )
    assert counts["directoryDiscardedCount"] == 0
    assert counts["fileDiscardedCount"] == 0

    file_events = get_unstage_file_events(output)
    event_paths = [e["path"] for e in file_events]
    assert event_paths == [to_posix(dir1_file)], (
        f"Scenario 4: expected event for {dir1_file}, got {event_paths}"
    )

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 5: Unstage a new directory with 3 files
    dir2 = "newdir2"
    repo.make_dirs(dir2)
    for i in range(3):
        with repo.open_file(os.path.join(dir2, f"file{i}.txt"), "w+") as f:
            f.write(f"content {i}\n")

    repo.stage(os.path.join(dir2, "file0.txt"))
    repo.stage(os.path.join(dir2, "file1.txt"))
    repo.stage(os.path.join(dir2, "file2.txt"))
    output = repo.unstage(dir2, json=True)
    counts = get_unstage_counts(output)

    assert counts["directoryUnstagedCount"] == 1, (
        f"Scenario 5: expected directoryUnstagedCount=1, got {counts['directoryUnstagedCount']}"
    )
    assert counts["fileUnstagedCount"] == 3, (
        f"Scenario 5: expected fileUnstagedCount=3, got {counts['fileUnstagedCount']}"
    )
    assert counts["directoryDiscardedCount"] == 0
    assert counts["fileDiscardedCount"] == 0

    file_events = get_unstage_file_events(output)
    event_paths = sorted([e["path"] for e in file_events])
    expected_paths = sorted(
        [to_posix(os.path.join(dir2, f"file{i}.txt")) for i in range(3)]
    )
    assert event_paths == expected_paths, (
        f"Scenario 5: expected events for {expected_paths}, got {event_paths}"
    )
    assert all(e["action"] == "keep" for e in file_events), (
        "Scenario 5: kept (unstaged) files should have action=keep"
    )

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 6: Unstage a nested dir/subdir/file.txt
    nested_dir = "nested"
    nested_subdir = os.path.join(nested_dir, "subdir")
    nested_file = os.path.join(nested_subdir, "deep.txt")
    repo.make_dirs(nested_subdir)
    with repo.open_file(nested_file, "w+") as f:
        f.write("deeply nested\n")

    repo.stage(nested_file)
    output = repo.unstage(nested_dir, json=True)
    counts = get_unstage_counts(output)

    assert counts["directoryUnstagedCount"] == 2, (
        f"Scenario 6: expected directoryUnstagedCount=2, got {counts['directoryUnstagedCount']}"
    )
    assert counts["fileUnstagedCount"] == 1, (
        f"Scenario 6: expected fileUnstagedCount=1, got {counts['fileUnstagedCount']}"
    )
    assert counts["directoryDiscardedCount"] == 0
    assert counts["fileDiscardedCount"] == 0

    # Only files emit per-node events; kept directories are counted but emit none.
    file_events = get_unstage_file_events(output)
    event_paths = sorted([e["path"] for e in file_events])
    assert event_paths == [to_posix(nested_file)], (
        f"Scenario 6: expected event only for {nested_file}, got {event_paths}"
    )

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 7: Unstage multiple new files at once
    for i in range(4):
        with repo.open_file(f"multi_{i}.txt", "w+") as f:
            f.write(f"multi content {i}\n")
        repo.stage(f"multi_{i}.txt")

    output = repo.unstage(json=True)
    counts = get_unstage_counts(output)

    assert counts["fileUnstagedCount"] == 4, (
        f"Scenario 7: expected fileUnstagedCount=4, got {counts['fileUnstagedCount']}"
    )
    assert counts["fileDiscardedCount"] == 0
    assert counts["directoryDiscardedCount"] == 0

    repo.branch_switch("main")
    repo.branch_switch("test-discard-counts")

    # Scenario 8: Deep nested structure — files at multiple levels get individual events
    # Structure: deep/a.txt, deep/mid/b.txt, deep/mid/bottom/c.txt
    deep = "deep"
    mid = os.path.join(deep, "mid")
    bottom = os.path.join(mid, "bottom")
    repo.make_dirs(bottom)

    deep_files = {
        os.path.join(deep, "a.txt"): "file at top",
        os.path.join(mid, "b.txt"): "file at mid",
        os.path.join(bottom, "c.txt"): "file at bottom",
    }
    for path, content in deep_files.items():
        with repo.open_file(path, "w+") as f:
            f.write(content + "\n")

    for path in deep_files:
        repo.stage(path)

    output = repo.unstage(deep, json=True)
    counts = get_unstage_counts(output)

    # 3 directories: deep, mid, bottom (deep counted in unstage_node, mid and
    # bottom counted via demote_subnodes_to_dirty) — all kept as dirty adds.
    assert counts["directoryUnstagedCount"] == 3, (
        f"Scenario 8: expected directoryUnstagedCount=3, got {counts['directoryUnstagedCount']}"
    )
    assert counts["fileUnstagedCount"] == 3, (
        f"Scenario 8: expected fileUnstagedCount=3, got {counts['fileUnstagedCount']}"
    )
    assert counts["directoryDiscardedCount"] == 0
    assert counts["fileDiscardedCount"] == 0

    # Only files emit per-node events; nested kept directories are counted but emit none.
    file_events = get_unstage_file_events(output)
    event_paths = sorted([e["path"] for e in file_events])
    expected_paths = sorted([to_posix(p) for p in deep_files])
    assert event_paths == expected_paths, (
        f"Scenario 8: expected events for {expected_paths}, got {event_paths}"
    )

    repo.branch_switch("main")


@pytest.mark.smoke
def test_restage_after_unstage_promotes_dirty_add_back_to_staged_add(new_lore_repo):
    """A staged add that is unstaged becomes a dirty add (the file is still
    pending — the user just dropped the intent to include it in the next
    commit). Re-staging that file by name must promote it back to a staged
    add, even when the file content on disk is byte-identical to the node's
    stored hash from the original stage. The promotion is what `stage` is
    supposed to do; the byte-identical filesystem comparison is irrelevant
    once a node carries the Dirty flag."""

    repo: Lore = new_lore_repo()
    with repo.open_file("file.txt", "w+") as f:
        f.write("hello\n")

    repo.stage("file.txt")
    after_stage = parse_status_json(repo.status(json=True))
    entry = next(
        (e for e in after_stage if to_posix(e["path"]) == to_posix("file.txt")), None
    )
    assert (
        entry is not None and entry["flagStaged"] is True and entry["flagDirty"] is True
    ), f"baseline: file.txt should be a staged dirty add after `stage`, got {entry}"

    repo.unstage(".")
    after_unstage = parse_status_json(repo.status(json=True))
    entry = next(
        (e for e in after_unstage if to_posix(e["path"]) == to_posix("file.txt")), None
    )
    assert (
        entry is not None
        and entry["flagStaged"] is False
        and entry["flagDirty"] is True
    ), f"baseline: file.txt should be a dirty add after `unstage`, got {entry}"

    output = repo.stage("file.txt", json=True)
    stage_events = parse_jsonl(output, "fileStageFile")
    assert any(to_posix(e["path"]) == to_posix("file.txt") for e in stage_events), (
        "re-stage of file.txt did not emit a fileStageFile event — staging was "
        f"silently skipped. Events: {stage_events}"
    )

    after_restage = parse_status_json(repo.status(json=True))
    entry = next(
        (e for e in after_restage if to_posix(e["path"]) == to_posix("file.txt")), None
    )
    assert (
        entry is not None and entry["flagStaged"] is True and entry["flagDirty"] is True
    ), (
        "after re-stage, file.txt should be a staged dirty add again "
        f"(equivalent to its post-original-stage state), got {entry}"
    )
