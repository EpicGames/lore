# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from test_utils import posix_join, to_posix

from lore import Lore
from lore_parsers import parse_status_count_json, parse_status_json

logger = logging.getLogger(__name__)


def _status_files_by_path(repo: Lore, **kwargs) -> dict[str, dict]:
    """Run `status --json` and return the file events keyed by posix path.

    Only `type == "file"` entries are kept so directory bookkeeping does not
    interfere with file-level assertions.
    """
    entries = parse_status_json(repo.status(json=True, offline=True, **kwargs))
    return {to_posix(e.get("path", "")): e for e in entries if e.get("type") == "file"}


@pytest.mark.smoke
def test_status(new_lore_repo):
    repo: Lore = new_lore_repo()
    for i in range(10):
        subpath = str(i)
        repo.make_dirs(subpath)
        for j in range(10):
            subsubpath = posix_join(subpath, str(j))
            repo.make_dirs(subsubpath)
            with repo.open_file(
                posix_join(subsubpath, "test.uasset"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(i + j + 30))

    repo.stage(scan=True)
    repo.commit()
    repo.repository_verify()

    status_path = "test.txt"
    status_subpath = posix_join("subpath", "another.txt")
    with repo.open_file(status_path, "w+b") as output_file:
        output_file.write(os.urandom(100))
    repo.make_dirs("subpath")
    with repo.open_file(status_subpath, "w+b") as output_file:
        output_file.write(os.urandom(200))

    # Status with path filter to an untracked file
    output = repo.status(status_path, unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("A ") and line.endswith(status_path), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Untracked files:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("M "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status with path filter to an untracked file in a subdir
    output = repo.status("subpath", unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("A ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Untracked files:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("M "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status with path filter to an untracked file in a subdir
    output = repo.status(status_subpath, unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("A ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Untracked files:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("M "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    repo.stage(scan=True)
    repo.commit()

    with repo.open_file(status_path, "w+b") as output_file:
        output_file.write(os.urandom(1000))
    repo.make_dirs("subpath")
    with repo.open_file(status_subpath, "w+b") as output_file:
        output_file.write(os.urandom(2000))

    # Status with path filter to an untracked file
    output = repo.status(status_path, unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("M ") and line.endswith(status_path), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Changes not staged for commit:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("A "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status with path filter to an untracked file in a subdir
    output = repo.status("subpath", unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("M ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Changes not staged for commit:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("A "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status with path filter to an untracked file in a subdir
    output = repo.status(status_subpath, unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("M ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Changes not staged for commit:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes"
            )
            assert not line.startswith("D ") or line.startswith("A "), (
                f"Filtered status --unstaged found unrelated changes"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    repo.remove_file(status_subpath)

    # Status with path filter to an untracked file in a subdir
    output = repo.status("subpath", unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("D ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Changes not staged for commit:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes: {line}"
            )
            assert not line.startswith("A ") or line.startswith("M "), (
                f"Filtered status --unstaged found unrelated changes: {line}"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status with path filter to an untracked file in a subdir
    output = repo.status(status_subpath, unstaged=True)

    counting = False
    num_unstaged = 0
    for line in output.splitlines():
        if counting:
            if line.startswith("Tracked changes:"):
                break
            num_unstaged += 1
            assert line.startswith("D ") and line.endswith(status_subpath), (
                f"Filtered status found unexpected modified file path: {line}"
            )
        elif line.rstrip() == "Changes not staged for commit:":
            counting = True
        else:
            assert not line.startswith("Changes not staged"), (
                f"Filtered status --unstaged found unrelated changes: {line}"
            )
            assert not line.startswith("A ") or line.startswith("M "), (
                f"Filtered status --unstaged found unrelated changes: {line}"
            )

    assert num_unstaged == 1, (
        f"Filtered status --unstaged did not return the expected paths, got {num_unstaged} expected 1"
    )

    # Status to an ignored file path
    with repo.open_file(repo.ignore_file(), "w+") as output_file:
        output_file.writelines(["testpath/"])

    repo.make_dirs("testpath")
    with repo.open_file(posix_join("testpath", "testfile.txt"), "w+") as output_file:
        output_file.writelines(["testing ignore"])

    output = repo.status(posix_join("testpath", "testfile.txt"), unstaged=True)

    assert "Changes not staged for commit:" not in output, (
        "Found unrelated unstaged change when query status for ignored file"
    )
    assert "Untracked files:" not in output, (
        "Found unrelated untracked change when query status for ignored file"
    )


def _status_entries(repo: Lore, **kwargs) -> dict[str, dict]:
    """Run `status --json` and return every entry keyed by posix path.

    Unlike `_status_files_by_path` this keeps directory entries too, so a
    test can assert that excluded subtrees produce no stray directory
    bookkeeping (e.g. phantom deletes) during a `--scan`.
    """
    entries = parse_status_json(repo.status(json=True, offline=True, **kwargs))
    return {to_posix(e.get("path", "")): e for e in entries}


@pytest.mark.smoke
def test_status_scan_view_filter(new_lore_repo, scratch_dir):
    """`status --scan` reconciles a view-filtered clone against the filesystem
    reporting exactly the changes to materialized (in-view) content and nothing
    for content the view filter excluded.

    The committed tree mixes top-level files, a deep chain
    (`assets/d0/d1/d2/d3/deep.txt`), a plainly excluded subtree
    (`assets/plain/**`), and a `drop/` subtree under which only nested `keep/`
    directories are re-included (`assets/**/drop/**/keep/`). The view keeps the
    `assets/` subtree, re-excludes `assets/plain/` and every `drop/`, then
    re-includes any `keep/` reachable beneath a `drop/`. A clone therefore
    materializes only `keep_root.txt`, the deep file, `mid/keep/mk.txt`, and the
    re-included `mid/drop/keep/kd.txt`.

    On a freshly materialized clone `--scan` must report no changes at all: in
    particular no phantom directory deletes for the excluded `plain/` and
    `drop/` subtrees that the filesystem never wrote. After genuinely editing
    three in-view files (a deep modify, an add, a delete) `--scan` must report
    exactly those three and persist their dirty flags, never surfacing any
    excluded file or a stray delete for an excluded directory.
    """
    repo: Lore = new_lore_repo()

    in_view = [
        posix_join("assets", "keep_root.txt"),
        posix_join("assets", "d0", "d1", "d2", "d3", "deep.txt"),
        posix_join("assets", "mid", "keep", "mk.txt"),
        posix_join("assets", "mid", "drop", "keep", "kd.txt"),
    ]
    excluded = [
        "root.txt",
        posix_join("top", "t.txt"),
        posix_join("assets", "plain", "p1.txt"),
        posix_join("assets", "plain", "nested", "p2.txt"),
        posix_join("assets", "mid", "drop", "d1.txt"),
        posix_join("assets", "mid", "drop", "sub", "d2.txt"),
    ]
    for p in in_view + excluded:
        repo.make_dirs(os.path.dirname(p))
        with repo.open_file(p, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    view_dir = scratch_dir("view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!assets/**\n")
        view_file.write("assets/plain/\n")
        view_file.write("assets/**/drop/\n")
        view_file.write("!assets/**/drop/**/keep/\n")
    clone: Lore = repo.clone(view=view_path)

    # The clone must materialize exactly the in-view set and nothing excluded.
    for p in in_view:
        assert clone.file_exists(p), f"view filter dropped an in-view file: {p}"
    for p in excluded:
        assert not clone.path_exists(p), (
            f"view filter materialized an excluded file: {p}"
        )

    # A pristine clone is in sync with the filesystem: --scan reports nothing.
    # Excluded subtrees (plain/, drop/) were never written, so they must not
    # surface as phantom directory deletes.
    clean = _status_entries(clone, scan=True)
    assert clean == {}, (
        f"--scan on a pristine view-filtered clone reported changes: {sorted(clean)}"
    )

    # Genuinely change three in-view files across depths.
    deep = posix_join("assets", "d0", "d1", "d2", "d3", "deep.txt")
    added = posix_join("assets", "mid", "keep", "added.txt")
    deleted = posix_join("assets", "keep_root.txt")
    with clone.open_file(deep, "w+b") as f:
        f.write(os.urandom(128))
    with clone.open_file(added, "w+b") as f:
        f.write(os.urandom(32))
    clone.remove_file(deleted)

    scanned = _status_entries(clone, scan=True)

    files = {p: e for p, e in scanned.items() if e.get("type") == "file"}
    assert set(files) == {to_posix(deep), to_posix(added), to_posix(deleted)}, (
        f"--scan reported the wrong file set: {sorted(files)}"
    )
    assert files[to_posix(deep)]["action"] == "keep"
    assert files[to_posix(added)]["action"] == "add"
    assert files[to_posix(deleted)]["action"] == "delete"
    assert all(e["flagDirty"] is True for e in files.values()), (
        f"scanned files should be flagged dirty: {files}"
    )

    # No excluded file may appear, and no excluded directory may be reported as
    # deleted just because the view filter walked it to reach re-included content.
    strays = {
        p: e.get("action")
        for p, e in scanned.items()
        if e.get("type") != "file" and e.get("action") == "delete"
    }
    assert strays == {}, (
        f"--scan reported stray deletes for excluded directories: {strays}"
    )

    # Suppressing phantom deletes must not hide a real one: removing an entire
    # materialized in-view directory still reports the directory as deleted.
    clone.remove_file(posix_join("assets", "mid", "keep", "mk.txt"))
    clone.remove_file(added)
    clone.remove_dir(posix_join("assets", "mid", "keep"))

    after_rmdir = _status_entries(clone, scan=True)
    keep_dir = to_posix(posix_join("assets", "mid", "keep"))
    assert keep_dir in after_rmdir, (
        f"a removed in-view directory must report a delete: {sorted(after_rmdir)}"
    )
    assert after_rmdir[keep_dir]["type"] == "directory"
    assert after_rmdir[keep_dir]["action"] == "delete"
    # The re-included sibling under drop/ is untouched, so it must not appear.
    assert to_posix(posix_join("assets", "mid", "drop", "keep")) not in after_rmdir, (
        f"untouched re-included directory should not be reported: {sorted(after_rmdir)}"
    )

    # The scan above marked the deleted file and directory DirtyDelete, so a bare
    # stage (no rescan) must pick the deletion up from the dirty flags; committing
    # then records it and removes the directory from the new revision.
    clone.stage()
    clone.commit()
    post = _status_entries(clone, scan=True)
    assert post == {}, (
        f"--scan after committing the deletion reported changes: {sorted(post)}"
    )


@pytest.mark.smoke
def test_status_scan_view_pure_exclusion(new_lore_repo, scratch_dir):
    """`status --scan` on a pristine clone under a pure-exclusion view reports
    no changes.

    The view excludes the multi-segment glob `some/path/**/with/*/*`. The
    ancestor directories of the matched content are not themselves matched by
    the glob, so they evaluate as "not excluded" even though nothing under them
    is in view and they were never materialized. Such directories must not be
    reported as phantom deletes."""
    repo: Lore = new_lore_repo()

    target = posix_join("some", "path", "is", "excluded", "with", "a", "file.txt")
    # .../with/a/sub matches the trailing `*/*`, so the directory is excluded and
    # deep.txt never materializes -- the whole .../with/a subtree is filtered out.
    deep = posix_join("some", "path", "is", "excluded", "with", "a", "sub", "deep.txt")
    keep = posix_join("some", "other", "kept.txt")
    for p in (target, deep, keep):
        repo.make_dirs(os.path.dirname(p))
        with repo.open_file(p, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    view_dir = scratch_dir("view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("some/path/**/with/*/*\n")
    clone: Lore = repo.clone(view=view_path)

    assert clone.file_exists(keep), "view dropped an in-view file"
    assert not clone.path_exists(target), "view materialized an excluded file"
    assert not clone.path_exists(deep), "view materialized a deeper excluded file"

    clean = _status_entries(clone, scan=True)
    assert clean == {}, (
        f"--scan on a pristine view-filtered clone reported changes: {sorted(clean)}"
    )


@pytest.mark.smoke
def test_status_revision_only(new_lore_repo):
    repo: Lore = new_lore_repo()

    with repo.open_file("test.txt", "w+b") as f:
        f.write(os.urandom(100))
    repo.stage(scan=True)
    repo.commit()

    # Create staged and unstaged changes
    with repo.open_file("staged.txt", "w+b") as f:
        f.write(os.urandom(100))
    repo.stage(scan=True)
    with repo.open_file("unstaged.txt", "w+b") as f:
        f.write(os.urandom(100))

    # --revision-only should emit revision info but no file changes
    output = repo.status(revision_only=True)

    assert "Repository" in output, "Expected repository header in revision-only output"
    assert "On branch" in output, "Expected branch info in revision-only output"
    assert "Changes staged for commit:" not in output, (
        "revision-only should not show staged changes"
    )
    assert "Changes not staged for commit:" not in output, (
        "revision-only should not show unstaged changes"
    )
    assert "Untracked files:" not in output, (
        "revision-only should not show untracked files"
    )


@pytest.mark.smoke
def test_status_count(new_lore_repo, scratch_dir):
    """`status --count` reports the directory and file totals of the tree.

    Covers: the full-tree total; agreement between `--count` and `--count
    --scan` (a single shared traversal); that no count event is emitted without
    `--count`; that the local view filter is honored (a view-filtered clone
    counts only its materialized subtree, not the filtered-out parts of the
    committed tree); the human-readable "Repository size" line; and that a
    staged add is reflected (the count walks the staged state when present).
    """
    repo: Lore = new_lore_repo()

    repo.make_dirs("included")
    repo.make_dirs("excluded")
    for name in ("a.txt", "b.txt"):
        with repo.open_file(posix_join("included", name), "w+b") as f:
            f.write(os.urandom(64))
    for name in ("x.txt", "y.txt"):
        with repo.open_file(posix_join("excluded", name), "w+b") as f:
            f.write(os.urandom(64))
    with repo.open_file("root.txt", "w+b") as f:
        f.write(os.urandom(64))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    count = parse_status_count_json(repo.status(count=True, json=True))
    assert count is not None, "Expected a repositoryStatusCount event with --count"
    assert count["directories"] == 2, f"Expected 2 directories, got {count}"
    assert count["files"] == 5, f"Expected 5 files, got {count}"

    count_scan = parse_status_count_json(repo.status(count=True, scan=True, json=True))
    assert count_scan is not None, "Expected a count event with --count --scan"
    assert count_scan["directories"] == 2 and count_scan["files"] == 5, (
        f"--count --scan disagreed with --count: {count_scan}"
    )

    assert parse_status_count_json(repo.status(json=True)) is None, (
        "Count event emitted without --count"
    )

    view_dir = scratch_dir("view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!included/**\n")
    clone: Lore = repo.clone(view=view_path)

    clone_count = parse_status_count_json(clone.status(count=True, json=True))
    assert clone_count is not None, "Expected a count event in the view-filtered clone"
    assert clone_count["directories"] == 1, (
        f"View filter ignored: expected only included/ (1 dir), got {clone_count}"
    )
    assert clone_count["files"] == 2, (
        f"View filter ignored: expected only included/ files (2), got {clone_count}"
    )

    assert "Repository size: 1 directories, 2 files" in clone.status(count=True), (
        "Unexpected repository size line in human-readable --count output"
    )

    with clone.open_file(posix_join("included", "c.txt"), "w+b") as f:
        f.write(os.urandom(64))
    clone.stage(posix_join("included", "c.txt"))
    staged_count = parse_status_count_json(clone.status(count=True, json=True))
    assert staged_count is not None, "Expected a count event after staging"
    assert staged_count["directories"] == 1 and staged_count["files"] == 3, (
        f"Staged add not reflected (count should walk the staged state): {staged_count}"
    )


@pytest.mark.smoke
def test_status_count_link(new_lore_repo, scratch_dir):
    """`status --count` counts a link mount as a directory, descends only into
    the linked subtree (honoring path remapping), and applies the local view
    filter to the linked content via the remapped mount path.

    The target repository holds a `mounted/` subtree (`top.txt`, `keep/k.txt`,
    `drop/d.txt`) that is linked, plus unrelated `unmounted/` and `loose.txt`
    that are not. The link remaps the target's `mounted/` to a differently
    named `lk/` in the main repository.

    Without a view filter the count covers only `lk` and the linked subtree
    alongside `root.txt` — 3 directories, 4 files — never the target's
    unmounted entries. A path argument into the link counts just that subtree
    (`lk/keep` is 1/1, the file `lk/keep/k.txt` is 0/1). A view-filtered clone
    that re-includes `root.txt` and `lk/**` but excludes `lk/drop/` drops
    exactly that part of the linked subtree — 2 directories, 3 files — proving
    the view filter reaches inside the link via the mount path.
    """
    link_repo: Lore = new_lore_repo()
    link_repo.make_dirs(posix_join("mounted", "keep"))
    link_repo.make_dirs(posix_join("mounted", "drop"))
    with link_repo.open_file(posix_join("mounted", "top.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with link_repo.open_file(posix_join("mounted", "keep", "k.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with link_repo.open_file(posix_join("mounted", "drop", "d.txt"), "w+b") as f:
        f.write(os.urandom(64))
    link_repo.make_dirs("unmounted")
    with link_repo.open_file(posix_join("unmounted", "u.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with link_repo.open_file("loose.txt", "w+b") as f:
        f.write(os.urandom(64))
    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    repo: Lore = new_lore_repo()
    with repo.open_file("root.txt", "w+b") as f:
        f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.link_add("lk", link_repo.get_id(), "mounted")
    repo.commit()
    repo.push()

    count = parse_status_count_json(repo.status(count=True, json=True))
    assert count is not None, "Expected a count event"
    assert count["directories"] == 3, (
        f"Expected 3 directories (lk + lk/keep + lk/drop), got {count}"
    )
    assert count["files"] == 4, (
        f"Expected 4 files (root.txt + lk/top.txt + lk/keep/k.txt + lk/drop/d.txt), got {count}"
    )

    assert parse_status_count_json(
        repo.status(posix_join("lk", "keep"), count=True, json=True)
    ) == {"directories": 1, "files": 1}
    assert parse_status_count_json(
        repo.status(posix_join("lk", "keep", "k.txt"), count=True, json=True)
    ) == {"directories": 0, "files": 1}

    view_dir = scratch_dir("link-view", create=True)
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!root.txt\n")
        view_file.write("!lk/**\n")
        view_file.write("lk/drop/\n")
    clone: Lore = repo.clone(view=view_path)

    clone_count = parse_status_count_json(clone.status(count=True, json=True))
    assert clone_count is not None, "Expected a count event in the view-filtered clone"
    assert clone_count["directories"] == 2, (
        f"View filter should drop lk/drop inside the link: expected 2 dirs, got {clone_count}"
    )
    assert clone_count["files"] == 3, (
        f"View filter should drop lk/drop/d.txt inside the link: expected 3 files, got {clone_count}"
    )


@pytest.mark.smoke
def test_status_count_paths(new_lore_repo):
    """`status --count <paths>` counts only the selected subtrees, summed.

    Each path argument resolves to a starting node; a directory contributes
    itself plus its descendants, a file contributes itself, and multiple paths
    are summed. With the tree `a/{a1,a2}`, `b/{b1, sub/b2}`, `root.txt` the
    whole repo is 3 directories and 5 files; `a` is 1/2, `b` is 2/2, `a b` is
    3/4, and a single file path is 0/1.
    """
    repo: Lore = new_lore_repo()
    repo.make_dirs("a")
    repo.make_dirs(posix_join("b", "sub"))
    with repo.open_file(posix_join("a", "a1.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with repo.open_file(posix_join("a", "a2.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with repo.open_file(posix_join("b", "b1.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with repo.open_file(posix_join("b", "sub", "b2.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with repo.open_file("root.txt", "w+b") as f:
        f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    assert parse_status_count_json(repo.status(count=True, json=True)) == {
        "directories": 3,
        "files": 5,
    }
    assert parse_status_count_json(repo.status("a", count=True, json=True)) == {
        "directories": 1,
        "files": 2,
    }
    assert parse_status_count_json(repo.status("b", count=True, json=True)) == {
        "directories": 2,
        "files": 2,
    }
    assert parse_status_count_json(repo.status(["a", "b"], count=True, json=True)) == {
        "directories": 3,
        "files": 4,
    }
    assert parse_status_count_json(
        repo.status(posix_join("b", "sub", "b2.txt"), count=True, json=True)
    ) == {"directories": 0, "files": 1}


@pytest.mark.smoke
def test_status_count_layer(new_lore_repo):
    """`status --count` includes layer content (a standalone state tree mounted
    into the parent), counting only the mapped source subsection, honoring path
    filtering, and applying the local view filter via the mount (target) path.

    The layer repository maps its `mapped/` subsection (`top.txt`, `keep/k.txt`,
    `drop/d.txt`) at a differently named `lay/`, while `outside/` and
    `loose.txt` stay unmapped. The whole-repo count is `root.txt` plus the `lay`
    mount and the mapped subtree — 3 directories, 4 files — never the unmapped
    entries. `--count lay` counts just the layer (3/3), `--count lay/keep` just
    that subtree (1/1), `--count root.txt` just the parent file (0/1), and a
    view filter excluding `lay/drop/` drops that part of the mapped content
    (2/3), proving the filter matches the target mount path, not the source.
    """
    layer_repo: Lore = new_lore_repo()
    layer_repo.make_dirs(posix_join("mapped", "keep"))
    layer_repo.make_dirs(posix_join("mapped", "drop"))
    with layer_repo.open_file(posix_join("mapped", "top.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with layer_repo.open_file(posix_join("mapped", "keep", "k.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with layer_repo.open_file(posix_join("mapped", "drop", "d.txt"), "w+b") as f:
        f.write(os.urandom(64))
    layer_repo.make_dirs("outside")
    with layer_repo.open_file(posix_join("outside", "o.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with layer_repo.open_file("loose.txt", "w+b") as f:
        f.write(os.urandom(64))
    layer_repo.stage(scan=True)
    layer_repo.commit()
    layer_repo.push()

    repo: Lore = new_lore_repo()
    with repo.open_file("root.txt", "w+b") as f:
        f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.layer_add("lay", layer_repo, "mapped/")

    assert parse_status_count_json(repo.status(count=True, json=True)) == {
        "directories": 3,
        "files": 4,
    }
    assert parse_status_count_json(repo.status("lay", count=True, json=True)) == {
        "directories": 3,
        "files": 3,
    }
    assert parse_status_count_json(
        repo.status(posix_join("lay", "keep"), count=True, json=True)
    ) == {"directories": 1, "files": 1}
    assert parse_status_count_json(repo.status("root.txt", count=True, json=True)) == {
        "directories": 0,
        "files": 1,
    }

    with repo.open_file(posix_join(repo.dot_dir(), "view"), "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!root.txt\n")
        view_file.write("!lay/**\n")
        view_file.write("lay/drop/\n")

    assert parse_status_count_json(repo.status(count=True, json=True)) == {
        "directories": 2,
        "files": 3,
    }


@pytest.mark.smoke
def test_status_count_link_and_layers(new_lore_repo):
    """`status --count` sums the parent tree, an in-tree link, and multiple
    layers, and path filtering selects across all of them.

    The main repository has `root.txt`, a link `lk` to another repository's
    `linked/` (`x.txt`, `y.txt`), and two layers: `la` (`a1.txt`, `a2.txt`) and
    `lb` (`b1.txt`, `sub/b2.txt`). The whole count is 4 directories, 7 files.
    Each path argument selects only its own subtree: `lk` is 1/2, `la` is 1/2,
    `lb` is 2/2, and `lk la lb` together is 4/6 (excluding `root.txt`).
    """
    link_repo: Lore = new_lore_repo()
    link_repo.make_dirs("linked")
    for name in ("x.txt", "y.txt"):
        with link_repo.open_file(posix_join("linked", name), "w+b") as f:
            f.write(os.urandom(64))
    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.push()

    layer_a: Lore = new_lore_repo()
    layer_a.make_dirs("la")
    for name in ("a1.txt", "a2.txt"):
        with layer_a.open_file(posix_join("la", name), "w+b") as f:
            f.write(os.urandom(64))
    layer_a.stage(scan=True)
    layer_a.commit()
    layer_a.push()

    layer_b: Lore = new_lore_repo()
    layer_b.make_dirs(posix_join("lb", "sub"))
    with layer_b.open_file(posix_join("lb", "b1.txt"), "w+b") as f:
        f.write(os.urandom(64))
    with layer_b.open_file(posix_join("lb", "sub", "b2.txt"), "w+b") as f:
        f.write(os.urandom(64))
    layer_b.stage(scan=True)
    layer_b.commit()
    layer_b.push()

    repo: Lore = new_lore_repo()
    with repo.open_file("root.txt", "w+b") as f:
        f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.link_add("lk", link_repo.get_id(), "linked")
    repo.commit()
    repo.push()
    repo.layer_add("la", layer_a, "la/")
    repo.layer_add("lb", layer_b, "lb/")

    assert parse_status_count_json(repo.status(count=True, json=True)) == {
        "directories": 4,
        "files": 7,
    }
    assert parse_status_count_json(repo.status("lk", count=True, json=True)) == {
        "directories": 1,
        "files": 2,
    }
    assert parse_status_count_json(repo.status("la", count=True, json=True)) == {
        "directories": 1,
        "files": 2,
    }
    assert parse_status_count_json(repo.status("lb", count=True, json=True)) == {
        "directories": 2,
        "files": 2,
    }
    assert parse_status_count_json(
        repo.status(["lk", "la", "lb"], count=True, json=True)
    ) == {"directories": 4, "files": 6}


@pytest.mark.smoke
def test_status_check_dirty(new_lore_repo):
    """`status --check-dirty` clears dirty flags that no longer reflect a real
    on-disk change while keeping genuinely modified files, across a directory
    hierarchy, and persists the cleared flags.

    A committed tree is fully flagged dirty with `lore dirty` (which marks
    unconditionally), then only two files are actually modified. --check-dirty
    must keep exactly those two, clear every other flag — including a whole
    subtree whose files all revert clean and a root-level file — and the
    cleared state must survive into a later plain status.
    """
    repo: Lore = new_lore_repo()

    repo.make_dirs(posix_join("top", "sub"))
    repo.make_dirs("clean")
    files = [
        posix_join("top", "a.txt"),
        posix_join("top", "b.txt"),
        posix_join("top", "sub", "c.txt"),
        posix_join("top", "sub", "d.txt"),
        posix_join("clean", "e.txt"),
        posix_join("clean", "f.txt"),
        "root.txt",
    ]
    for name in files:
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()

    # Genuinely modify two files; a different size makes them unambiguously
    # modified regardless of timestamp resolution.
    modified = {posix_join("top", "a.txt"), posix_join("top", "sub", "c.txt")}
    for name in modified:
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(128))

    # Flag every file dirty, including the five that were never touched.
    repo.dirty(files)

    pre = _status_files_by_path(repo)
    assert set(pre) == {to_posix(p) for p in files}, (
        f"expected all files flagged dirty before verification, got {sorted(pre)}"
    )
    assert all(pre[to_posix(p)]["flagDirty"] is True for p in files), (
        f"all pre-check entries should be dirty: {pre}"
    )

    checked = _status_files_by_path(repo, check_dirty=True)
    assert set(checked) == {to_posix(p) for p in modified}, (
        f"--check-dirty should keep only the modified files, got {sorted(checked)}"
    )
    for p in modified:
        entry = checked[to_posix(p)]
        assert entry["flagDirty"] is True, f"{p} should stay dirty: {entry}"
        assert entry["action"] == "keep", f"{p} should be a modify: {entry}"

    after = _status_files_by_path(repo)
    assert set(after) == {to_posix(p) for p in modified}, (
        f"cleared dirty flags should persist, plain status got {sorted(after)}"
    )


@pytest.mark.smoke
def test_status_check_dirty_add_delete(new_lore_repo):
    """`status --check-dirty` always keeps structural changes (adds and
    deletes) — there is no content to re-verify — while still clearing a stale
    dirty-modify in the same run, and persists the result.
    """
    repo: Lore = new_lore_repo()

    repo.make_dirs("dir")
    for name in ("keep.txt", "del.txt", posix_join("dir", "inner.txt")):
        with repo.open_file(name, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()

    # A brand-new file (add), a removed committed file (delete), and an
    # untouched committed file flagged dirty (a stale modify).
    with repo.open_file("added.txt", "w+b") as f:
        f.write(os.urandom(64))
    repo.remove_file("del.txt")
    repo.dirty(["added.txt", "del.txt", "keep.txt"])

    checked = _status_files_by_path(repo, check_dirty=True)

    assert "added.txt" in checked, f"a dirty add must be reported: {sorted(checked)}"
    assert checked["added.txt"]["action"] == "add"
    assert checked["added.txt"]["flagDirty"] is True

    assert "del.txt" in checked, f"a dirty delete must be reported: {sorted(checked)}"
    assert checked["del.txt"]["action"] == "delete"
    assert checked["del.txt"]["flagDirty"] is True

    assert "keep.txt" not in checked, (
        f"the stale dirty-modify must be cleared: {sorted(checked)}"
    )

    after = _status_files_by_path(repo)
    assert set(after) == {"added.txt", "del.txt"}, (
        f"add/delete must remain and the stale modify stay cleared, got {sorted(after)}"
    )


@pytest.mark.smoke
def test_status_check_dirty_rehashes_same_size(new_lore_repo):
    """`status --check-dirty` rehashes a dirty file when its size still matches
    the tracked node but its modification time has changed, so same-size edits
    are told apart from reverts by content rather than size alone.

    Two committed files of identical size are each overwritten in place with
    new content of the same size and flagged dirty. One keeps the new content;
    the other has its original bytes written back. --check-dirty must keep the
    genuinely changed file dirty and clear the reverted one — only a content
    hash comparison can distinguish them — and the cleared flag must persist.
    """
    repo: Lore = new_lore_repo()

    reverted_original = os.urandom(256)
    with repo.open_file("modified.bin", "w+b") as f:
        f.write(os.urandom(256))
    with repo.open_file("reverted.bin", "w+b") as f:
        f.write(reverted_original)
    repo.stage(scan=True)
    repo.commit()

    # Overwrite both in place with new, same-size content so the size check
    # cannot decide — the verification must fall through to the hash check.
    with repo.open_file("modified.bin", "w+b") as f:
        f.write(os.urandom(256))
    with repo.open_file("reverted.bin", "w+b") as f:
        f.write(os.urandom(256))
    repo.dirty(["modified.bin", "reverted.bin"])

    # Restore exactly the committed bytes for one file; its content now matches
    # the tracked node again even though its modification time has moved.
    with repo.open_file("reverted.bin", "w+b") as f:
        f.write(reverted_original)

    checked = _status_files_by_path(repo, check_dirty=True)
    assert set(checked) == {"modified.bin"}, (
        f"only the genuinely changed same-size file should stay dirty, got {sorted(checked)}"
    )
    assert checked["modified.bin"]["flagDirty"] is True
    assert checked["modified.bin"]["action"] == "keep"

    after = _status_files_by_path(repo)
    assert set(after) == {"modified.bin"}, (
        f"reverted file must remain cleared on a later status, got {sorted(after)}"
    )
