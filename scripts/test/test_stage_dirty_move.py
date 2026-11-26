# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Smoke tests of a stage taking a move marked with `lore file dirty move` as a move, and of the
commit that follows: the revision holds the node at its new path, its delta records the move, and
the file's history continues at the source."""

import pytest
from lore_parsers import parse_jsonl, parse_status_json

from lore import Lore


def committed_base(repo: Lore, files: dict[str, str], directories: tuple[str, ...] = ()) -> None:
    """Writes `files` and makes `directories`, stages the whole tree with a scan and commits it."""
    repo.write_files(files)
    for directory in directories:
        repo.make_dirs(directory)
    repo.stage(scan=True, offline=True)
    repo.commit("base", offline=True)


def staged_by(repo: Lore, how: str, destination: str) -> None:
    """Stages the working tree as `how` names: the destination by name, the whole tree from the
    dirty marks, or the whole tree with a scan."""
    if how == "destination":
        repo.stage(destination, offline=True)
    else:
        repo.stage(".", scan=how == "scan", offline=True)


def revision_paths(repo: Lore) -> set[str]:
    """The paths the branch's latest revision holds, directories without their trailing slash."""
    dump = repo.repository_dump(revision="main@LATEST", offline=True)
    return {
        line.split(" ", 1)[0].rstrip("/")
        for line in dump.splitlines()
        if " id " in line and " parent " in line
    }


def latest_delta(repo: Lore) -> dict[str, dict]:
    """The delta of the branch's latest revision, by path."""
    deltas = parse_jsonl(
        repo.revision_info(delta=True, json=True, offline=True), "revisionInfoDelta"
    )
    return {delta["path"]: delta for delta in deltas}


def assert_nothing_left(repo: Lore) -> None:
    """Asserts that `lore status` reports nothing staged or changed."""
    entries = parse_status_json(repo.status(json=True, offline=True))
    assert entries == [], f"status must report nothing after the commit, got {entries}"


@pytest.mark.smoke
@pytest.mark.parametrize("how", ["destination", "whole", "scan"])
@pytest.mark.parametrize("edited", [False, True], ids=["unchanged", "edited"])
def test_a_stage_commits_a_dirty_moved_file_as_a_move(new_lore_repo, how, edited):
    """A file moved with `mv` and `lore file dirty move`, its content unchanged or edited, is staged
    as a move by a stage of its destination, of the whole tree, or of the whole tree with a scan.
    The commit records the move, with modify set where the content changed, and the file's history
    continues at the source."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"src/ex/walk.rs": "walk\n", "dst/other.txt": "other\n"})

    repo.move("src/ex/walk.rs", "dst/walk.rs")
    repo.dirty_move("src/ex/walk.rs", "dst/walk.rs", offline=True)
    if edited:
        repo.write_files({"dst/walk.rs": "walk, edited\n"})
        repo.dirty("dst/walk.rs", offline=True)

    staged_by(repo, how, "dst/walk.rs")
    repo.commit("Move the file", offline=True)

    paths = revision_paths(repo)
    assert "dst/walk.rs" in paths and "src/ex/walk.rs" not in paths, paths
    delta = latest_delta(repo)
    assert delta.get("dst/walk.rs", {}).get("action") == "move", delta
    assert delta["dst/walk.rs"]["fromPath"] == "src/ex/walk.rs", delta
    assert delta["dst/walk.rs"]["flagModify"] == edited, delta

    history = parse_jsonl(repo.file_history("dst/walk.rs", json=True, offline=True), "fileHistory")
    assert [(entry["action"], entry["path"], entry["fromPath"]) for entry in history] == [
        ("move", "dst/walk.rs", "src/ex/walk.rs"),
        ("add", "src/ex/walk.rs", ""),
    ], history
    assert_nothing_left(repo)


@pytest.mark.smoke
@pytest.mark.parametrize("how", ["destination", "whole", "scan"])
def test_a_stage_commits_a_dirty_moved_directory_as_a_move(new_lore_repo, how):
    """A directory holding a file, moved with `mv` and `lore file dirty move`, is staged as a move
    by a stage of its destination, of the whole tree, or of the whole tree with a scan: the
    revision holds the file at its new path and nothing at the old one."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"d/k.txt": "k\n", "other.txt": "other\n"})

    repo.move("d", "e")
    repo.dirty_move("d", "e", offline=True)

    staged_by(repo, how, "e")
    repo.commit("Move the directory", offline=True)

    paths = revision_paths(repo)
    assert {"e", "e/k.txt"} <= paths and not any(
        path == "d" or path.startswith("d/") for path in paths
    ), paths
    assert latest_delta(repo).get("e", {}).get("action") == "move"


@pytest.mark.smoke
@pytest.mark.parametrize(
    ("named", "scan"),
    [("e", False), ("d", False), (".", False), (".", True)],
    ids=["destination", "source", "whole", "scan"],
)
@pytest.mark.parametrize(
    "below", [(), ("d/Sub", "d/Other")], ids=["empty", "only-directories"]
)
def test_a_stage_commits_a_dirty_moved_directory_holding_no_file(
    new_lore_repo, named, scan, below
):
    """A directory holding no file, empty or holding only empty directories, moved with `mv` and
    `lore file dirty move`, is staged by a stage naming its destination or the path it left, or of
    the whole tree, with or without a scan: the revision holds the directory and what it holds at
    the new path and nothing at the old one, and status reports nothing left."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"Top.txt": "top\n"}, ("d", *below))

    repo.move("d", "e")
    repo.dirty_move("d", "e", offline=True)

    repo.stage(named, scan=scan, offline=True)
    repo.commit("Move the directory", offline=True)

    paths = revision_paths(repo)
    expected = {"e", *(path.replace("d/", "e/", 1) for path in below)}
    assert expected <= paths and not any(
        path == "d" or path.startswith("d/") for path in paths
    ), paths
    assert latest_delta(repo).get("e", {}).get("action") == "move"
    assert_nothing_left(repo)


@pytest.mark.smoke
def test_a_stage_of_one_file_of_a_dirty_moved_directory_commits_its_whole_move(new_lore_repo):
    """A stage of one unchanged file of a directory moved with `mv` and `lore file dirty move`
    stages the directory's move, and the other file that moves with it: the commit records both
    files moved, and each one's history continues at its committed path."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"d/k.txt": "k\n", "d/other.txt": "other\n", "Top.txt": "top\n"})

    repo.move("d", "e")
    repo.dirty_move("d", "e", offline=True)
    repo.stage("e/k.txt", offline=True)
    repo.commit("Move the directory", offline=True)

    delta = latest_delta(repo)
    for name in ("k.txt", "other.txt"):
        entry = delta.get(f"e/{name}", {})
        assert (entry.get("action"), entry.get("fromPath")) == ("move", f"d/{name}"), delta
        history = parse_jsonl(repo.file_history(f"e/{name}", json=True, offline=True), "fileHistory")
        assert [(item["action"], item["fromPath"]) for item in history] == [
            ("move", f"d/{name}"),
            ("add", ""),
        ], history
    assert_nothing_left(repo)


@pytest.mark.smoke
@pytest.mark.parametrize("named", ["source", "both", "new-at-source", "directories"])
@pytest.mark.parametrize("edited", [False, True], ids=["unchanged", "edited"])
def test_a_stage_naming_the_path_a_dirty_move_left_commits_the_move(new_lore_repo, named, edited):
    """A file moved with `mv` and `lore file dirty move`, its content unchanged or edited, is staged
    as a move by a stage naming the path it left, alone or with its destination, or the directories
    holding both; where a new file marked dirty stands at the path it left, the stage takes the new
    file and the move. The commit records the move, with modify set where the content changed, and
    the file's history continues at the source."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"src/ex/walk.rs": "walk\n", "dst/other.txt": "other\n"})

    repo.move("src/ex/walk.rs", "dst/walk.rs")
    repo.dirty_move("src/ex/walk.rs", "dst/walk.rs", offline=True)
    if edited:
        repo.write_files({"dst/walk.rs": "walk, edited\n"})
        repo.dirty("dst/walk.rs", offline=True)
    if named == "new-at-source":
        repo.write_files({"src/ex/walk.rs": "new\n"})
        repo.dirty("src/ex/walk.rs", offline=True)

    paths = {
        "source": ["src/ex/walk.rs"],
        "new-at-source": ["src/ex/walk.rs"],
        "both": ["src/ex/walk.rs", "dst/walk.rs"],
        "directories": ["src", "dst"],
    }[named]
    repo.stage(paths, offline=True)
    repo.commit("Move the file", offline=True)

    paths = revision_paths(repo)
    assert "dst/walk.rs" in paths, paths
    assert ("src/ex/walk.rs" in paths) == (named == "new-at-source"), paths
    delta = latest_delta(repo)
    assert delta.get("dst/walk.rs", {}).get("action") == "move", delta
    assert delta["dst/walk.rs"]["fromPath"] == "src/ex/walk.rs", delta
    assert delta["dst/walk.rs"]["flagModify"] == edited, delta

    history = parse_jsonl(repo.file_history("dst/walk.rs", json=True, offline=True), "fileHistory")
    assert [(entry["action"], entry["path"], entry["fromPath"]) for entry in history] == [
        ("move", "dst/walk.rs", "src/ex/walk.rs"),
        ("add", "src/ex/walk.rs", ""),
    ], history
    assert_nothing_left(repo)


@pytest.mark.smoke
@pytest.mark.parametrize("edited", [False, True], ids=["unchanged", "edited"])
def test_a_stage_of_a_directory_above_only_the_path_a_dirty_move_left_stages_nothing(
    new_lore_repo, edited
):
    """A stage of a directory above the path a dirty move left, and not above its destination,
    stages nothing of the move: status still reports it as not staged."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"src/ex/walk.rs": "walk\n", "dst/other.txt": "other\n"})

    repo.move("src/ex/walk.rs", "dst/walk.rs")
    repo.dirty_move("src/ex/walk.rs", "dst/walk.rs", offline=True)
    if edited:
        repo.write_files({"dst/walk.rs": "walk, edited\n"})
        repo.dirty("dst/walk.rs", offline=True)

    repo.stage("src", offline=True)

    entries = parse_status_json(repo.status(json=True, offline=True))
    moved = [entry for entry in entries if entry["path"] == "dst/walk.rs"]
    assert [(entry["action"], entry["flagStaged"]) for entry in moved] == [("move", False)], entries


def dirty_moved_directory(repo: Lore, held: str) -> list[tuple[str, str]]:
    """Commits `Top.txt` and `d` holding what `held` names, moves `d` to `e` with `mv` and
    `lore file dirty move`, and answers the moves a status reports, as (source, destination)."""
    files = {"Top.txt": "top\n"}
    below: tuple[str, ...] = ()
    if held == "a-file":
        files["d/k.txt"] = "k\n"
    elif held == "only-directories":
        below = ("d/Sub", "d/Other")
    committed_base(repo, files, ("d", *below))
    repo.move("d", "e")
    repo.dirty_move("d", "e", offline=True)
    moved = [("d", "e"), *((path, path.replace("d/", "e/", 1)) for path in below)]
    if held == "a-file":
        moved.append(("d/k.txt", "e/k.txt"))
    return sorted(moved)


def reported_moves(repo: Lore, scan: bool) -> list[tuple[str, str, str, bool]]:
    """What `lore status --json` reports, as (action, source, path, staged), sorted."""
    entries = parse_status_json(repo.status(scan=scan, json=True, offline=True))
    return sorted(
        (entry["action"], entry["fromPath"], entry["path"], bool(entry["flagStaged"]))
        for entry in entries
    )


@pytest.mark.smoke
@pytest.mark.parametrize("scan", [False, True], ids=["plain", "scan"])
@pytest.mark.parametrize("held", ["empty", "only-directories", "a-file"])
def test_status_reports_a_dirty_moved_directory_as_a_move(new_lore_repo, held, scan):
    """A status of a directory moved with `mv` and `lore file dirty move`, empty, holding only
    directories or holding a file, reports it as moved from its committed path, with each node
    below it that moves with it, before a stage; after one, the staged move of the directory
    stands for them, as a status reports a directory's move `lore stage move` staged."""
    repo: Lore = new_lore_repo()
    moved = dirty_moved_directory(repo, held)

    expected = [("move", source, path, False) for source, path in moved]
    assert reported_moves(repo, scan) == expected
    listing = repo.status(scan=scan, offline=True)
    assert "V d/ -> e/" in [line.strip() for line in listing.splitlines()], listing

    repo.stage(".", offline=True)
    assert reported_moves(repo, scan) == [("move", "d", "e", True)]


@pytest.mark.smoke
@pytest.mark.parametrize("edited", [False, True], ids=["unchanged", "edited"])
def test_a_stage_of_a_dirty_moved_file_reports_the_path_it_left(new_lore_repo, edited):
    """The stage event of a file moved with `lore file dirty move`, its content unchanged or
    edited, names the path the file had in the revision the stage is staged on."""
    repo: Lore = new_lore_repo()
    committed_base(repo, {"a.txt": "a\n", "other.txt": "other\n"})

    repo.move("a.txt", "b.txt")
    repo.dirty_move("a.txt", "b.txt", offline=True)
    if edited:
        repo.write_files({"b.txt": "b, edited\n"})
        repo.dirty("b.txt", offline=True)

    events = parse_jsonl(repo.stage("b.txt", json=True, offline=True), "fileStageFile")
    assert [(event["action"], event["path"], event["fromPath"]) for event in events] == [
        ("move", "b.txt", "a.txt")
    ], events
