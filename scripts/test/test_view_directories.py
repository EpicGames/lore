# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The directories an operation writing the working tree creates under a view.

A directory is created where a view line names it, where it holds, at any depth,
what the operation creates (a kept file, a link or layer mount, or a directory a
line names), and where the repository holds it empty. A directory the walk only
passes through, because the view includes it by its parent's verdict or because a
re-inclusion could match below it, is not created. A clone, its dry run, a sync, a
branch switch, a change of view, `layer add` and `link add` agree on it, and
neither `lore dirty .` nor a scan reports a directory left out as missing.
"""

import os
import shutil

import pytest
from lore_parsers import parse_status_json
from test_utils import commit_and_push_files, to_posix, view_file, working_tree_files

from lore import Lore

# A view holding each half of the rule: `a/b` is named and everything in it
# excluded, `a/c` holds the one file a line names, `a/e/f` holds a re-included
# `.bat`, and `a/d` holds only what the view excludes, though the `.bat`
# re-inclusion sends the walk into it.
VIEW = ["**", "!/a/**/*.bat", "!/a/b/", "/a/b/*", "!/a/c/file.txt"]
# The same view without the lines naming `a/b` and `a/c/file.txt`.
NARROW_VIEW = ["**", "!/a/**/*.bat"]
# A view of extension rules, which keeps only `a/e/f/deep.bat` below `a`.
EXTENSION_VIEW = ["*.txt", "*.thing"]
# A view excluding everything in `a`, which leaves `a` itself uncreated.
SUBTREE_VIEW = ["/a/**"]
FILES = [
    "a/b/x.txt",
    "a/c/file.txt",
    "a/c/other.txt",
    "a/d/some.thing",
    "a/e/f/deep.bat",
    "top.txt",
]
# What a clone of `FILES` holds under `VIEW`, directories included.
CREATED = ["a", "a/b", "a/c", "a/c/file.txt", "a/e", "a/e/f", "a/e/f/deep.bat"]
# Every directory `FILES` names.
DIRECTORIES = ["a", "a/b", "a/c", "a/d", "a/e", "a/e/f"]


def assert_nothing_missing(instance: Lore, what: str) -> None:
    """A scan reports no changes, and `lore dirty .` marks nothing but the files
    the working tree holds, which a plain status then lists as kept."""
    scanned = parse_status_json(instance.status(json=True, offline=True, scan=True))
    assert scanned == [], f"{what}: status --scan reported {scanned}"
    instance.dirty(".", offline=True)
    listed = parse_status_json(instance.status(json=True, offline=True))
    held = set(working_tree_files(instance))
    changed = [
        entry
        for entry in listed
        if entry["action"] != "keep" or to_posix(entry["path"]) not in held
    ]
    assert changed == [], f"{what}: lore dirty . marked {changed}"


@pytest.mark.smoke
def test_a_clone_under_the_example_view_creates_a_b_and_a_c_and_no_a_d(
    new_lore_repo, scratch_dir
):
    """Under `**`, `!/a/**/*.bat`, `!/a/b/` and `!/a/c/file.txt` a clone creates
    `a/b`, which a line names, `a/c`, which holds the file a line names, the
    directories holding a `.bat`, and `a` above them, and no `a/d`, which holds
    only `some.thing`."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(
        repo,
        [
            "a/b/x.txt",
            "a/c/file.txt",
            "a/c/other.txt",
            "a/d/some.thing",
            "a/e/run.bat",
            "a/e/f/deep.bat",
        ],
    )
    view = view_file(
        scratch_dir, "example", ["**", "!/a/**/*.bat", "!/a/b/", "!/a/c/file.txt"]
    )

    clone: Lore = repo.clone(view=view)

    assert working_tree_files(clone, directories=True) == [
        "a",
        "a/b",
        "a/b/x.txt",
        "a/c",
        "a/c/file.txt",
        "a/e",
        "a/e/f",
        "a/e/f/deep.bat",
        "a/e/run.bat",
    ]
    assert_nothing_missing(clone, "the example view")


@pytest.mark.smoke
def test_a_clone_creates_no_directory_it_only_passes_through(
    new_lore_repo, scratch_dir
):
    """A clone creates no directory the view includes by its parent's verdict or
    enters because a re-inclusion could match below it, where nothing below it is
    created: nested directories holding only files a view of extension rules
    excludes, `a` under `/a/**`, and `Engine` under `/*` and `!*/Saved/Config/`
    with no `Saved/Config` in the revision. Neither `lore dirty .` nor a scan
    reports one missing."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(
        repo,
        [
            "ext/b/c/x.bat",
            "ext/b/y.uasset",
            "ext/kept.txt",
            "a/x.txt",
            "a/b/y.txt",
            "Engine/Build/b.txt",
            "top.txt",
        ],
    )
    for name, rules, created in [
        (
            "extensions",
            ["*.bat", "*.uasset", "/a/", "/Engine/", "/top.txt"],
            ["ext", "ext/kept.txt"],
        ),
        ("subtree", ["/a/**", "/ext/", "/Engine/"], ["top.txt"]),
        ("reinclusion", ["/*", "!*/Saved/Config/"], []),
    ]:
        clone: Lore = repo.clone(view=view_file(scratch_dir, name, rules))

        assert working_tree_files(clone, directories=True) == created, rules
        assert_nothing_missing(clone, f"{rules}")


@pytest.mark.smoke
def test_a_clone_creates_a_directory_a_view_line_names(new_lore_repo, scratch_dir):
    """A clone creates a directory a view line names, with its ancestors, though the
    view excludes everything in it: one the view excludes before the line, one it
    already includes, each directory a wildcard re-inclusion matches, and the
    how-to's `Engine/Content`."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(
        repo,
        [
            "a/b/x.txt",
            "a/b/c/y.txt",
            "Engine/Content/a.uasset",
            "Engine/Source/s.cpp",
            "Engine/Saved/Config/e.ini",
            "Game/Saved/Config/g.ini",
            "Game/Saved/Logs/l.txt",
        ],
    )
    for name, rules, created in [
        ("excluded", ["**", "!/a/b/", "/a/b/*"], ["a", "a/b"]),
        (
            "included",
            ["!/a/b/", "/a/b/*", "/Engine/", "/Game/"],
            ["a", "a/b"],
        ),
        (
            "wildcard",
            ["/*", "!*/Saved/Config/", "/*/Saved/Config/*"],
            [
                "Engine",
                "Engine/Saved",
                "Engine/Saved/Config",
                "Game",
                "Game/Saved",
                "Game/Saved/Config",
            ],
        ),
        (
            "empty",
            ["/*", "!/Engine/Content/", "/Engine/Content/*"],
            ["Engine", "Engine/Content"],
        ),
    ]:
        clone: Lore = repo.clone(view=view_file(scratch_dir, name, rules))

        assert working_tree_files(clone, directories=True) == created, rules
        assert_nothing_missing(clone, f"{rules}")


@pytest.mark.own_vfs(
    "links and layers a repository, which a virtual clone does not hold"
)
@pytest.mark.smoke
def test_a_clone_creates_each_directory_holding_what_it_keeps(
    new_lore_repo, scratch_dir
):
    """A clone creates each directory above a kept file deep below directories no
    line names, and above a link mount below such a chain; `layer add` creates the
    chain above its mount. A clone with no view creates every directory, empty or
    not."""
    repo: Lore = new_lore_repo()
    link_repo: Lore = new_lore_repo(repo.name + "_link")
    layer_repo: Lore = new_lore_repo(repo.name + "_layer")
    commit_and_push_files(
        repo, ["a/b/c/d/kept.txt", "a/b/x.bat", "l/m/x.bat"], ("e/f",)
    )
    commit_and_push_files(link_repo, ["linked.txt"])
    commit_and_push_files(layer_repo, ["layered.txt"])
    repo.link_add("l/m/link", link_repo.get_id(), "/")
    repo.commit("Add link")
    repo.push()

    clone: Lore = repo.clone(view=view_file(scratch_dir, "extensions", ["*.bat"]))
    clone.layer_add("p/q/layer", layer_repo, "/")

    assert working_tree_files(clone, directories=True) == [
        "a",
        "a/b",
        "a/b/c",
        "a/b/c/d",
        "a/b/c/d/kept.txt",
        "e",
        "e/f",
        "l",
        "l/m",
        "l/m/link",
        "l/m/link/linked.txt",
        "p",
        "p/q",
        "p/q/layer",
        "p/q/layer/layered.txt",
    ]

    unfiltered: Lore = repo.clone()
    assert working_tree_files(unfiltered, directories=True) == [
        "a",
        "a/b",
        "a/b/c",
        "a/b/c/d",
        "a/b/c/d/kept.txt",
        "a/b/x.bat",
        "e",
        "e/f",
        "l",
        "l/m",
        "l/m/link",
        "l/m/link/linked.txt",
        "l/m/x.bat",
    ]


@pytest.mark.smoke
def test_a_dry_run_clone_lists_the_directories_a_clone_creates(
    new_lore_repo, scratch_dir
):
    """A dry run of a clone lists each directory the clone creates, and none it
    leaves out."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, FILES)
    view = view_file(scratch_dir, "view", VIEW)

    output = repo.run(
        [
            "repository",
            "clone",
            repo.remote_path,
            str(scratch_dir("dry-run", create=True)),
            "--view",
            view,
        ],
        dry_run=True,
    ).splitlines()

    listed = [directory for directory in DIRECTORIES if directory in output]
    assert listed == [path for path in CREATED if path in DIRECTORIES], output


@pytest.mark.smoke
@pytest.mark.parametrize(
    "rules",
    [VIEW, EXTENSION_VIEW, SUBTREE_VIEW],
    ids=["example", "extension", "subtree"],
)
def test_a_sync_and_a_branch_switch_create_what_a_clone_does(
    new_lore_repo, scratch_dir, rules
):
    """A sync to a revision that adds the directories, and a switch to a branch
    holding them, leave the tree a clone under the same view holds, under the
    example view, a view of extension rules, and `/a/**`."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, ["base.bat"])
    view = view_file(scratch_dir, "view", rules)
    synced: Lore = repo.clone(view=view)
    switched: Lore = repo.clone(view=view)

    repo.branch_create("tree")
    commit_and_push_files(repo, FILES)
    repo.branch_switch("main")
    commit_and_push_files(repo, FILES)

    synced.sync()
    switched.branch_switch("tree")

    cloned = working_tree_files(repo.clone(view=view), directories=True)
    if rules == VIEW:
        assert cloned == CREATED
    assert working_tree_files(synced, directories=True) == cloned
    assert working_tree_files(switched, directories=True) == cloned
    assert_nothing_missing(synced, "the synced tree")
    assert_nothing_missing(switched, "the switched tree")


@pytest.mark.smoke
def test_a_sync_moves_a_file_into_directories_no_view_line_names(
    new_lore_repo, scratch_dir
):
    """A sync of a revision that moves a kept file into directories it adds,
    which no line of the view names, creates them as the parents of the file,
    and leaves the tree an instance cloned with the same view holds."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, FILES)
    view = view_file(scratch_dir, "view", EXTENSION_VIEW)
    synced: Lore = repo.clone(view=view)

    repo.make_dirs("a/g/h")
    repo.stage("a/g")
    repo.move("a/e/f/deep.bat", "a/g/h/deep.bat")
    repo.stage_move("a/e/f/deep.bat", "a/g/h/deep.bat")
    repo.commit()
    repo.push()

    synced.sync()

    held = working_tree_files(synced, directories=True)
    assert "a/g/h/deep.bat" in held
    assert held == working_tree_files(repo.clone(view=view), directories=True)
    assert_nothing_missing(synced, "the synced tree")


@pytest.mark.smoke
def test_a_reset_restores_the_directories_a_clone_creates(new_lore_repo, scratch_dir):
    """`lore reset` of a tree the user removed creates what a clone under the same
    view holds: the directory a line names, and the chain above each kept file."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, FILES)
    clone: Lore = repo.clone(view=view_file(scratch_dir, "view", VIEW))
    shutil.rmtree(f"{clone.path}/a")

    clone.reset(".")

    assert working_tree_files(clone, directories=True) == CREATED
    assert_nothing_missing(clone, "the reset tree")


@pytest.mark.smoke
def test_sync_view_narrows_and_widens_to_what_a_clone_creates(
    new_lore_repo, scratch_dir
):
    """A change of view creates the directories the new view names and those it
    keeps something in, and removes those it no longer creates once they are
    empty, from no view to `VIEW`, from `VIEW` to a view without its named
    directories, to a view of extension rules, to `/a/**` and back, so the tree
    holds what a clone with the view does. A directory holding an untracked file
    stays."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, FILES)
    view = view_file(scratch_dir, "view", VIEW)
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    extension = view_file(scratch_dir, "extension", EXTENSION_VIEW)
    subtree = view_file(scratch_dir, "subtree", SUBTREE_VIEW)
    instance: Lore = repo.clone()

    for target in [view, narrow, extension, subtree, view]:
        instance.sync(view=target)

        assert working_tree_files(instance, directories=True) == working_tree_files(
            repo.clone(view=target), directories=True
        ), target
        assert_nothing_missing(instance, target)

    instance.write_files({"a/b/untracked.txt": "untracked"})
    instance.sync(view=narrow)
    assert working_tree_files(instance, "a/b") == ["a/b/untracked.txt"]


@pytest.mark.own_vfs(
    "links and layers a repository, which a virtual clone does not hold"
)
@pytest.mark.smoke
def test_layer_add_and_link_add_create_what_a_clone_does(new_lore_repo, scratch_dir):
    """`layer add` and `link add` create below their mounts what a clone of the
    mounted tree creates under the same view, and nothing is reported missing."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push_files(repo, ["base.bat"])
    commit_and_push_files(source, FILES)
    view = view_file(
        scratch_dir,
        "mounted",
        ["**"]
        + [
            rule.replace("/a/", f"/{mount}/a/", 1)
            for mount in ["layer", "link"]
            for rule in VIEW[1:]
        ],
    )
    clone: Lore = repo.clone(view=view)

    clone.layer_add("layer", source, "/")
    clone.link_add("link", source.get_id(), "/")
    clone.commit("Add link")

    expected = sorted(
        ["layer", "link"]
        + [f"{mount}/{path}" for mount in ["layer", "link"] for path in CREATED]
    )
    assert working_tree_files(clone, directories=True) == expected
    assert_nothing_missing(clone, "the mounted trees")


@pytest.mark.smoke
def test_scan_and_dirty_report_the_deletion_of_a_named_directory(
    new_lore_repo, scratch_dir
):
    """Once the user removes `a`, which holds `a/b`, a directory a view line
    names whose content the view leaves out, `status --scan` and `lore dirty .`
    both report `a` deleted."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, ["a/b/x.txt", "k.txt"])
    clone: Lore = repo.clone(
        view=view_file(scratch_dir, "view", ["**", "!/a/b/", "/a/b/*", "!/k.txt"])
    )
    assert working_tree_files(clone, directories=True) == ["a", "a/b", "k.txt"]
    shutil.rmtree(f"{clone.path}/a")

    scanned = parse_status_json(clone.status(json=True, offline=True, scan=True))
    clone.dirty(".", offline=True)
    marked = parse_status_json(clone.status(json=True, offline=True))

    for what, entries in [("status --scan", scanned), ("lore dirty .", marked)]:
        deleted = {
            to_posix(entry["path"]) for entry in entries if entry["action"] == "delete"
        }
        assert "a" in deleted, f"{what} reported {entries}"


@pytest.mark.own_vfs(
    "links and layers a repository, which a virtual clone does not hold"
)
@pytest.mark.smoke
def test_sync_view_removes_what_it_no_longer_creates_in_links_and_layers(
    new_lore_repo, scratch_dir
):
    """A narrowing `sync --view` removes the directories the new view no longer
    creates below a link's mount and a layer's as in the repository's own tree,
    leaving each mount and what the view keeps."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push_files(repo, ["main/b/x.txt", "keep.bat"])
    commit_and_push_files(source, ["ext/b/x.txt", "s.bat"])
    clone: Lore = repo.clone()
    clone.layer_add("layer", source, "/")
    clone.link_add("link", source.get_id(), "/")
    clone.commit("Add link")
    clone.push()

    clone.sync(view=view_file(scratch_dir, "view", ["*.txt"]))

    assert working_tree_files(clone, directories=True) == [
        "keep.bat",
        "layer",
        "layer/s.bat",
        "link",
        "link/s.bat",
    ]


@pytest.mark.smoke
def test_a_merge_under_an_unchanged_view_keeps_what_a_branch_switch_keeps(
    new_lore_repo, scratch_dir
):
    """A merge that deletes the one kept file of `a/b`, beside a file the view
    excludes, keeps `a` and `a/b`, as a switch to a branch holding the same tree
    does: neither changes the view."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, ["a/b/x.txt", "a/b/y.bat"])
    repo.branch_create("feature")
    os.remove(f"{repo.path}/a/b/x.txt")
    repo.stage(scan=True)
    repo.commit()
    repo.push()
    repo.branch_switch("main")
    commit_and_push_files(repo, ["other.txt"])
    view = view_file(scratch_dir, "view", ["*.bat"])
    merged: Lore = repo.clone(view=view)
    switched: Lore = repo.clone(view=view)

    merged.branch_merge("feature")
    switched.branch_switch("feature")

    assert working_tree_files(switched, directories=True) == ["a", "a/b"]
    assert working_tree_files(merged, directories=True) == ["a", "a/b", "other.txt"]
