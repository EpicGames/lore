# SPDX-FileCopyrightText: 2026 LoreLab.io
# SPDX-License-Identifier: MIT
import os

import grpc
import pytest
from protobuf_wire import (
    encode_bytes_field,
    encode_varint_field,
    field_bool,
    field_bytes,
    parse_fields,
)

from lore import Lore


def _delta(repo: Lore, revision: str) -> dict[str, tuple[str, bool]]:
    """The file entries of `revision`'s delta, as path -> (action, merged)."""
    output = repo.run(["revision", "info", revision, "--delta"])
    _, _, changes = output.partition("\n\n")
    entries = {}
    for line in changes.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        entries[parts[1]] = (parts[0], parts[-1] == "(M)")
    return entries


def _files(
    entries: dict[str, tuple[str, bool]], *paths: str
) -> dict[str, tuple[str, bool]]:
    """`entries` cut down to `paths`, so directory entries are not asserted on."""
    return {path: entries[path] for path in paths if path in entries}


def _diff(repo: Lore, source: str, target: str) -> set[str]:
    """The paths `revision diff` reports from `source` to `target`."""
    output = repo.revision_diff(source, target=target)
    return {line.split()[1] for line in output.splitlines() if len(line.split()) >= 2}


def _setup(repo: Lore) -> None:
    repo.make_dirs(os.path.join("src", "core"))
    with repo.open_file(os.path.join("src", "core", "helpers.txt"), "w+") as f:
        f.write("helpers\n")
    with repo.open_file("readme.txt", "w+") as f:
        f.write("readme\n")
    repo.stage(scan=True)
    repo.commit("Base", offline=True)
    repo.push()


def _push_concurrent(repo: Lore) -> None:
    """Move main from another clone, with a change the pushed revision lacks."""
    clone = repo.clone()
    clone.make_dirs(os.path.join("docs", "api"))
    with clone.open_file(os.path.join("docs", "api", "reference.txt"), "w+") as f:
        f.write("reference\n")
    clone.stage(scan=True, offline=True)
    clone.commit("Concurrent", offline=True)
    clone.push()


@pytest.mark.regression
def test_server_fast_forward_merge_of_a_merge_records_its_own_delta(new_lore_repo):
    """The server's fast-forward merge records what it merged in.

    When a push with `fast_forward_merge` meets a moved branch head, the server builds a
    merge revision S, with the moved head as first parent and the pushed revision as
    second. `try_fast_forward_merge` used to apply the pushed changes without freezing
    the tree, so S kept its first parent's delta instead of recording what it merged in.

    Steps:
      1. Base on main. `feature` modifies src/core/helpers.txt and adds src/extra/new.txt.
      2. Merge feature into main locally (M), without pushing it.
      3. A clone pushes docs/api/reference.txt to main.
      4. Push M with `fast_forward_merge`. The server builds S.

    S's delta should hold the merged-in files, marked as merged, and none of its first
    parent's. A diff from M to S should report only the first parent's change. Before the
    fix, S's delta held only docs/api/reference.txt, and the diff also reported both of
    feature's files, which S and M hold identically.
    """
    repo: Lore = new_lore_repo()
    _setup(repo)

    repo.branch_create("feature")
    repo.make_dirs(os.path.join("src", "extra"))
    with repo.open_file(os.path.join("src", "extra", "new.txt"), "w+") as f:
        f.write("new on feature\n")
    with repo.open_file(os.path.join("src", "core", "helpers.txt"), "w+") as f:
        f.write("helpers changed on feature\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Feature", offline=True)
    repo.push()

    repo.branch_switch("main")
    repo.branch_merge_start("feature")
    merge = repo.branch_info().local_latest

    _push_concurrent(repo)

    repo.push(fast_forward_merge=True)
    repo.sync()
    server_merge = repo.branch_info().local_latest
    assert repo.revision_info(server_merge).merge.split()[1] == merge, (
        "the case needs a server-built merge with M as second parent"
    )

    paths = ("src/core/helpers.txt", "src/extra/new.txt", "docs/api/reference.txt")
    assert _files(_delta(repo, server_merge), *paths) == {
        "src/core/helpers.txt": ("M", True),
        "src/extra/new.txt": ("A", True),
    }
    assert _diff(repo, merge, server_merge) == {
        "docs",
        "docs/api",
        "docs/api/reference.txt",
    }


@pytest.mark.regression
def test_server_fast_forward_merge_of_a_commit_records_its_own_delta(new_lore_repo):
    """As above, with a plain commit pushed rather than a merge.

    The same case, reached the way a pull request merge reaches it: the source branch's
    tip is pushed onto a target that has moved, and the server builds S from it.

    Steps:
      1. Base on main. Commit a change to src/core/helpers.txt (T), without pushing it.
      2. A clone pushes docs/api/reference.txt to main.
      3. Push T with `fast_forward_merge`. The server builds S.
    """
    repo: Lore = new_lore_repo()
    _setup(repo)

    with repo.open_file(os.path.join("src", "core", "helpers.txt"), "w+") as f:
        f.write("helpers changed\n")
    repo.stage(scan=True, offline=True)
    repo.commit("Change helpers.txt", offline=True)
    commit = repo.branch_info().local_latest

    _push_concurrent(repo)

    repo.push(fast_forward_merge=True)
    repo.sync()
    server_merge = repo.branch_info().local_latest
    assert repo.revision_info(server_merge).merge.split()[1] == commit, (
        "the case needs a server-built merge with T as second parent"
    )

    paths = ("src/core/helpers.txt", "docs/api/reference.txt")
    assert _files(_delta(repo, server_merge), *paths) == {
        "src/core/helpers.txt": ("M", True),
    }
    assert _diff(repo, commit, server_merge) == {
        "docs",
        "docs/api",
        "docs/api/reference.txt",
    }


def _branch_push(
    grpc_target: str, repo: Lore, branch_id: str, revision: str
) -> tuple[str, bool]:
    """A direct `BranchPush` of `revision` onto `branch_id`, with `fast_forward_merge`, as a pull
    request merge sends it. Returns the resulting tip, and whether the server merged."""
    request = (
        encode_bytes_field(1, bytes.fromhex(branch_id))
        + encode_bytes_field(2, bytes.fromhex(revision))
        + encode_varint_field(4, 1)
    )
    with grpc.insecure_channel(grpc_target) as channel:
        call = channel.unary_unary(
            "/lore.revision.v1.RevisionService/BranchPush",
            request_serializer=lambda r: r,
            response_deserializer=parse_fields,
        )
        fields = call(
            request,
            timeout=30,
            metadata=(("urc-repository-id-bin", bytes.fromhex(repo.get_id())),),
        )
    return field_bytes(fields, 1).hex(), field_bool(fields, 3)


def _files_on_main(repo: Lore) -> set[str]:
    """Every file a fresh clone of main holds."""
    clone = repo.clone()
    files = set()
    for root, dirs, names in os.walk(clone.path):
        dirs[:] = [d for d in dirs if d != ".lore"]
        for name in names:
            files.add(
                os.path.relpath(os.path.join(root, name), clone.path).replace(
                    os.sep, "/"
                )
            )
    return files


def _commit(repo: Lore, path: str, message: str, offline: bool = True) -> None:
    with repo.open_file(path, "w+") as f:
        f.write(f"{path}\n")
    repo.stage(scan=True, offline=offline)
    repo.commit(message, offline=offline)


def _two_commit_feature(repo: Lore) -> tuple[str, str]:
    """Base on main, then `feature` with two commits. Returns main's id and feature's tip."""
    _commit(repo, "readme.txt", "Base")
    repo.push()
    main = repo.branch_info().id
    repo.branch_create("feature")
    _commit(repo, "first.txt", "First")
    _commit(repo, "second.txt", "Second")
    repo.push()
    return main, repo.branch_info().local_latest


@pytest.mark.regression
def test_server_push_of_several_commits_onto_an_unmoved_head_keeps_them_all(
    new_lore_repo, lore_grpc_target
):
    """A push more than one commit ahead of an unmoved head is a fast-forward.

    The server used to apply a push directly only when the pushed revision's direct parent
    was the head, and otherwise to merge from that parent rather than from where the two
    histories meet. So a branch of two commits, pushed onto an unmoved main, was merged from
    its first commit: only the second commit's change was applied, and the first was lost.
    """
    repo: Lore = new_lore_repo()
    main, tip = _two_commit_feature(repo)

    result, merged = _branch_push(lore_grpc_target, repo, main, tip)

    assert _files_on_main(repo) == {"readme.txt", "first.txt", "second.txt"}
    assert (result, merged) == (tip.lower(), False), (
        "the push should be a plain fast-forward"
    )


@pytest.mark.regression
def test_server_push_of_several_commits_onto_a_moved_head_keeps_them_all(
    new_lore_repo, lore_grpc_target
):
    """As above, with the head moved, so the server must build a merge.

    The merge must apply both commits, from where the histories meet, and keep the head's own
    change. Merged from the tip's parent instead, it applied only the second commit.
    """
    repo: Lore = new_lore_repo()
    main, tip = _two_commit_feature(repo)

    repo.branch_switch("main")
    clone = repo.clone()
    _commit(clone, "docs.txt", "Concurrent")
    clone.push()

    _, merged = _branch_push(lore_grpc_target, repo, main, tip)

    assert _files_on_main(repo) == {"readme.txt", "first.txt", "second.txt", "docs.txt"}
    assert merged, "the head moved, so the server should build a merge"


@pytest.mark.regression
def test_server_push_of_a_sync_merge_onto_an_unmoved_head_keeps_both_sides(
    new_lore_repo, lore_grpc_target
):
    """A branch whose tip is a sync merge, pushed onto an unmoved head, is a fast-forward.

    Merged from the sync merge's first parent instead, it lost the branch's earlier changes,
    or failed with "Failed to apply tree changes for fast-forward merge: Node not found".
    """
    repo: Lore = new_lore_repo()
    _commit(repo, "readme.txt", "Base")
    repo.push()
    main = repo.branch_info().id
    repo.branch_create("feature")
    _commit(repo, "feature.txt", "Feature")
    repo.push()

    other = repo.clone()
    other.branch_switch("feature")
    # Online: a fresh clone switched to feature has none of its metadata blocks cached
    _commit(other, "remote.txt", "Remote", offline=False)
    other.push()

    with repo.open_file("local.txt", "w+") as f:
        f.write("local.txt\n")
    repo.stage("local.txt")
    repo.commit("Local", local=True)
    repo.sync()
    repo.push()
    tip = repo.branch_info().local_latest
    assert len(repo.revision_info(tip).merge.split()) == 2, (
        "the case needs a sync merge tip"
    )

    result, merged = _branch_push(lore_grpc_target, repo, main, tip)

    assert _files_on_main(repo) == {
        "readme.txt",
        "feature.txt",
        "remote.txt",
        "local.txt",
    }
    assert (result, merged) == (tip.lower(), False), (
        "the push should be a plain fast-forward"
    )
