# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The directories an operation writing the working tree creates under a view.

Every directory the view keeps is created, whatever it holds. A directory the
view excludes is created only where it holds, at any depth, what the operation
creates: a kept file, a kept directory, or a link or layer mount. Lore walks such
a directory only where a re-inclusion could match below it. A clone, its dry run,
a sync, a branch switch, a change of view, a reset, `layer add` and `link add`
agree on it, and neither `lore dirty .` nor a scan reports a directory the
operation left out as missing, or one it created as added.
"""

import os
import shutil

import pytest
from error_types import NothingStagedError
from lore_parsers import parse_status_json
from test_utils import commit_and_push_files, to_posix, view_file, working_tree_files

from lore import Lore

# A view holding each half of the rule: `a/b` is kept and everything in it
# excluded, the excluded `a/c` holds the one file a line keeps, the excluded
# `a/e/f` holds a re-included `.bat`, and the excluded `a/d` holds only what the
# view excludes, though the `.bat` re-inclusion sends the walk into it.
VIEW = ["**", "!/a/**/*.bat", "!/a/b/", "/a/b/*", "!/a/c/file.txt"]
# The same view without the lines keeping `a/b` and `a/c/file.txt`.
NARROW_VIEW = ["**", "!/a/**/*.bat"]
# A view of extension rules, which keeps every directory, and below `a` only the
# file `a/e/f/deep.bat`.
EXTENSION_VIEW = ["*.txt", "*.thing"]
# A view excluding everything in `a`, which keeps `a` itself.
SUBTREE_VIEW = ["/a/**"]
# A view keeping `foo/bar/something`, which holds only the excluded `specific`.
FOO_VIEW = ["/foo/", "!/foo/bar/", "/foo/bar/something/specific/"]
FILES = [
    "a/b/x.txt",
    "a/c/file.txt",
    "a/c/other.txt",
    "a/d/some.thing",
    "a/e/f/deep.bat",
    "foo/bar/b.txt",
    "foo/bar/something/specific/s.txt",
    "foo/top.txt",
    "top.txt",
]
# What a clone of `FILES` holds under `VIEW`, directories included.
CREATED = ["a", "a/b", "a/c", "a/c/file.txt", "a/e", "a/e/f", "a/e/f/deep.bat"]
# Every directory `FILES` names.
DIRECTORIES = [
    "a",
    "a/b",
    "a/c",
    "a/d",
    "a/e",
    "a/e/f",
    "foo",
    "foo/bar",
    "foo/bar/something",
    "foo/bar/something/specific",
]


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
    `a/b`, which the view keeps, `a/c`, which holds the file a line keeps, the
    directories holding a `.bat`, and `a` above them, and no `a/d`, which the view
    excludes and which holds only `some.thing`."""
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
def test_a_clone_creates_each_directory_the_view_keeps_whatever_it_holds(
    new_lore_repo, scratch_dir
):
    """A clone creates each directory the view keeps, whatever it holds: nested
    directories holding only files extension rules exclude, `a` under `/a/**`,
    which excludes what `a` holds and not `a`, and `foo/bar/something` under
    `FOO_VIEW`, which holds only the excluded `specific`. Neither `lore dirty .`
    nor a scan reports a change."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(
        repo,
        [
            "ext/b/c/x.bat",
            "ext/b/y.uasset",
            "ext/kept.txt",
            "a/x.txt",
            "a/b/y.txt",
            "foo/bar/b.txt",
            "foo/bar/something/specific/s.txt",
            "foo/top.txt",
            "top.txt",
        ],
    )
    for name, rules, created in [
        (
            "extensions",
            ["*.bat", "*.uasset", "/a/", "/foo/", "/top.txt"],
            ["ext", "ext/b", "ext/b/c", "ext/kept.txt"],
        ),
        ("subtree", ["/a/**", "/ext/", "/foo/"], ["a", "top.txt"]),
        (
            "foo",
            FOO_VIEW + ["/a/", "/ext/"],
            ["foo", "foo/bar", "foo/bar/b.txt", "foo/bar/something", "top.txt"],
        ),
    ]:
        clone: Lore = repo.clone(view=view_file(scratch_dir, name, rules))

        assert working_tree_files(clone, directories=True) == created, rules
        assert_nothing_missing(clone, f"{rules}")


@pytest.mark.smoke
def test_a_clone_creates_no_directory_the_view_excludes_holding_nothing_kept(
    new_lore_repo, scratch_dir
):
    """A clone creates no directory the view excludes that holds nothing the view
    keeps, though a re-inclusion sends the walk into it: `Engine` under `/*` and
    `!*/Saved/Config/` with no `Saved/Config` in the revision, and `a/b` under
    `**` and `!/a/**/*.bat` when it holds no `.bat` file. Neither `lore dirty .`
    nor a scan reports one missing."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, ["Engine/Build/b.txt", "a/b/x.txt", "a/y.bat"])
    for name, rules, created in [
        ("reinclusion", ["/*", "!*/Saved/Config/"], []),
        ("extension", ["**", "!/a/**/*.bat"], ["a", "a/y.bat"]),
    ]:
        clone: Lore = repo.clone(view=view_file(scratch_dir, name, rules))

        assert working_tree_files(clone, directories=True) == created, rules
        assert_nothing_missing(clone, f"{rules}")


# Each exclusion and re-inclusion of `/a`, `/a/`, `/a/*` and `/a/**`, with what a
# clone of `SHAPES` under it holds. `/a` and `/a/` name `a`; `/a/*` and `/a/**`
# name what `a` holds. Below `**`, `!/a/*` keeps the entries of `a` and nothing
# below them; below `/*`, what lies below them follows them.
SHAPES = ["a/b/g.txt", "a/f.txt", "z.txt"]
SHAPE_EMPTY = ("a/b/c", "a/e")
EVERYTHING_IN_A = ["a", "a/b", "a/b/c", "a/b/g.txt", "a/e", "a/f.txt"]
SHAPE_VIEWS = [
    ("exclude-a", ["/a"], ["z.txt"]),
    ("exclude-a-directory", ["/a/"], ["z.txt"]),
    ("exclude-a-entries", ["/a/*"], ["a", "z.txt"]),
    ("exclude-a-subtree", ["/a/**"], ["a", "z.txt"]),
    ("include-a", ["**", "!/a"], EVERYTHING_IN_A),
    ("include-a-directory", ["**", "!/a/"], EVERYTHING_IN_A),
    ("include-a-entries", ["**", "!/a/*"], ["a", "a/b", "a/e", "a/f.txt"]),
    ("include-a-subtree", ["**", "!/a/**"], EVERYTHING_IN_A),
    ("include-a-entries-below-top", ["/*", "!/a/*"], EVERYTHING_IN_A),
    (
        "subtree-then-entries",
        ["/a/**", "!/a/b/*"],
        ["a", "a/b", "a/b/c", "a/b/g.txt", "z.txt"],
    ),
]


@pytest.mark.smoke
@pytest.mark.parametrize(
    "name,rules,created", SHAPE_VIEWS, ids=[name for name, _, _ in SHAPE_VIEWS]
)
def test_a_clone_and_a_change_of_view_tell_a_directory_from_what_it_holds(
    new_lore_repo, scratch_dir, name, rules, created
):
    """A clone under the view creates what the view keeps of `SHAPES` and the
    directories above it, a change of view from no view to it leaves the same
    tree, and a change of view from it to a view excluding nothing the revision
    holds brings back everything. After each, neither a scan nor `lore dirty .`
    reports a directory added or deleted."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, SHAPES, SHAPE_EMPTY)
    view = view_file(scratch_dir, name, rules)

    clone: Lore = repo.clone(view=view)
    # The clone holds exactly what the view keeps, directories included.
    assert working_tree_files(clone, directories=True) == created, rules
    assert_nothing_missing(clone, f"a clone under {rules}")

    instance: Lore = repo.clone()
    instance.sync(view=view)
    # Narrowing from no view leaves what a clone under the view holds.
    assert working_tree_files(instance, directories=True) == created, rules
    assert_nothing_missing(instance, f"sync --view to {rules}")

    clone.sync(view=view_file(scratch_dir, name + "-wide", ["/absent/"]))
    # Widening to a view that excludes nothing the revision holds brings back
    # everything.
    assert working_tree_files(clone, directories=True) == sorted(
        EVERYTHING_IN_A + ["z.txt"]
    )
    assert_nothing_missing(clone, f"sync --view from {rules} to a wide view")


@pytest.mark.smoke
def test_a_clone_creates_a_kept_directory_whose_entries_the_view_excludes(
    new_lore_repo, scratch_dir
):
    """A clone creates a directory the view keeps, with its ancestors, though the
    view excludes everything in it: one the view excludes before a line keeps it,
    one it already keeps, each directory a wildcard re-inclusion matches, and the
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
    """A clone creates each directory above a kept file deep below directories,
    and above a link mount below such a chain; `layer add` creates the chain above
    its mount. A clone with no view creates every directory, empty or not."""
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
    [VIEW, EXTENSION_VIEW, SUBTREE_VIEW, FOO_VIEW],
    ids=["example", "extension", "subtree", "foo"],
)
def test_a_sync_and_a_branch_switch_create_what_a_clone_does(
    new_lore_repo, scratch_dir, rules
):
    """A sync to a revision that adds the directories, and a switch to a branch
    holding them, leave the tree a clone under the same view holds, under the
    example view, a view of extension rules, `/a/**`, and `FOO_VIEW`."""
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
def test_a_sync_moves_a_file_into_directories_it_adds(new_lore_repo, scratch_dir):
    """A sync of a revision that moves a kept file into directories it adds
    creates them, and leaves the tree an instance cloned with the same view
    holds."""
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
    view holds: the directory the view keeps, and the chain above each kept
    file."""
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
    """A change of view creates the directories the new view keeps and those it
    keeps something in, and removes those it no longer creates once they are
    empty, from no view to `VIEW`, from `VIEW` to a view without its kept
    directories, to a view of extension rules, to `/a/**`, to `FOO_VIEW` and back,
    so the tree holds what a clone with the view does. A directory holding an
    untracked file stays."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, FILES)
    view = view_file(scratch_dir, "view", VIEW)
    narrow = view_file(scratch_dir, "narrow", NARROW_VIEW)
    extension = view_file(scratch_dir, "extension", EXTENSION_VIEW)
    subtree = view_file(scratch_dir, "subtree", SUBTREE_VIEW)
    foo = view_file(scratch_dir, "foo", FOO_VIEW)
    instance: Lore = repo.clone()

    for target in [view, narrow, extension, subtree, foo, view]:
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
@pytest.mark.parametrize(
    "rules,files,removed,deleted",
    [
        (["**", "!/a/b/", "/a/b/*", "!/k.txt"], ["a/b/x.txt", "k.txt"], "a", "a/b"),
        (["*.bat"], ["a/b/x.bat", "k.txt"], "a/b", "a/b"),
        (["/a/**"], ["a/x.txt", "k.txt"], "a", "a"),
        (
            FOO_VIEW,
            ["foo/bar/b.txt", "foo/bar/something/specific/s.txt", "k.txt"],
            "foo/bar/something",
            "foo/bar/something",
        ),
    ],
    ids=["kept", "extension", "subtree", "foo"],
)
def test_scan_and_dirty_report_the_deletion_of_a_kept_directory(
    new_lore_repo, scratch_dir, rules, files, removed, deleted
):
    """Once the user removes a directory holding one the view keeps whose content
    the view leaves out, `status --scan` reports the kept directory deleted, and
    after `lore dirty .` a status reports the removed directory deleted, a removed
    tree being reported by its top directory."""
    repo: Lore = new_lore_repo()
    commit_and_push_files(repo, files)
    clone: Lore = repo.clone(view=view_file(scratch_dir, "view", rules))
    # The kept directory stands after the clone, though it holds nothing kept.
    assert deleted in working_tree_files(clone, directories=True)
    shutil.rmtree(f"{clone.path}/{removed}")

    scanned = parse_status_json(clone.status(json=True, offline=True, scan=True))
    clone.dirty(".", offline=True)
    marked = parse_status_json(clone.status(json=True, offline=True))

    for what, entries, path in [
        ("status --scan", scanned, deleted),
        ("lore dirty .", marked, removed),
    ]:
        reported = {
            to_posix(entry["path"]) for entry in entries if entry["action"] == "delete"
        }
        assert path in reported, f"{what} reported {entries}"


# A view keeping `a/b` and leaving out everything in it, beside `k.txt` and `o`.
EMPTIED_VIEW = ["**", "!/a/b/", "/a/b/*", "!/k.txt", "!/o/"]
EMPTIED_FILES = ["a/b/x.txt", "a/b/c/y.txt", "a/h.txt", "h/z.txt", "k.txt", "o/p.txt"]
# Each directory an instance holds under a view that a test removes, with the view
# and the files committed: `a`, which the view excludes and which holds the kept
# `a/b`, `src`, which holds `src/x.txt`, which the view keeps, and `src/y.bat`,
# which `*.bat` leaves out, `bin`, which the view keeps and which holds only a
# `.bat` file, and `foo/bar/something`, which `FOO_VIEW` keeps and which holds
# only the excluded `specific`.
REMOVED = pytest.mark.parametrize(
    ("rules", "files", "removed"),
    [
        (EMPTIED_VIEW, EMPTIED_FILES, "a"),
        (["*.bat"], ["src/x.txt", "src/y.bat", "k.txt"], "src"),
        (["*.bat"], ["bin/y.bat", "k.txt"], "bin"),
        (
            FOO_VIEW,
            [
                "foo/bar/b.txt",
                "foo/bar/something/specific/s.txt",
                "foo/top.txt",
                "k.txt",
            ],
            "foo/bar/something",
        ),
    ],
    ids=[
        "excluded-parent",
        "holding-hidden",
        "only-hidden-files",
        "only-hidden-directory",
    ],
)


def under(path: str, directory: str) -> bool:
    """Whether `path` is `directory` or a path below it."""
    return path == directory or path.startswith(directory + "/")


def listed_deleted(entries: list[dict], directory: str) -> dict | None:
    """The entry a status lists `directory` by, where it lists the directory
    deleted and nothing at or below it but deletions."""
    below = [entry for entry in entries if under(to_posix(entry["path"]), directory)]
    if any(entry["action"] != "delete" for entry in below):
        return None
    return next((e for e in below if to_posix(e["path"]) == directory), None)


def removed_under_view(
    repo: Lore, scratch_dir, rules, files, removed
) -> tuple[Lore, list[str]]:
    """An instance of `files` cloned under `rules` whose working tree lacks
    `removed`, which `status --scan`, then `lore dirty .` and a plain status, list
    as deleted, and what cloning it created, directories included."""
    commit_and_push_files(repo, files)
    instance: Lore = repo.clone(view=view_file(scratch_dir, "view", rules))
    created = working_tree_files(instance, directories=True)
    shutil.rmtree(f"{instance.path}/{removed}")
    scanned = parse_status_json(instance.status(json=True, offline=True, scan=True))
    assert listed_deleted(scanned, removed), f"status --scan listed {scanned}"
    instance.dirty(".", offline=True)
    marked = parse_status_json(instance.status(json=True, offline=True))
    assert listed_deleted(marked, removed), f"lore dirty . marked {marked}"
    return instance, created


@pytest.mark.smoke
@REMOVED
def test_staging_a_removed_directory_deletes_it_with_everything_below_it(
    new_lore_repo, scratch_dir, rules, files, removed
):
    """Once the user removes a directory the instance holds under its view,
    status lists it as deleted, `lore stage .` stages its deletion, and
    committing records it: an instance freshly cloned with no view holds nothing at or
    below the directory, the files the view hides included, and every other file.
    The view's other paths stay, and status is clean."""
    repo: Lore = new_lore_repo()
    instance, created = removed_under_view(repo, scratch_dir, rules, files, removed)
    kept = [path for path in created if not under(path, removed)]

    instance.stage(".", offline=True)
    staged = parse_status_json(instance.status(json=True, offline=True))
    entry = listed_deleted(staged, removed)
    assert entry and entry["flagStaged"], f"lore stage . left {staged}"
    instance.commit("Removed", offline=True)
    instance.push()

    assert working_tree_files(instance, directories=True) == kept
    assert_nothing_missing(instance, "the committed deletion")
    fresh: Lore = repo.clone()
    assert working_tree_files(fresh) == [p for p in files if not under(p, removed)]


@pytest.mark.smoke
@REMOVED
def test_unstaging_a_removed_directory_takes_back_its_deletion_and_reset_creates_it(
    new_lore_repo, scratch_dir, rules, files, removed
):
    """`lore unstage` of a removed directory whose deletion is staged takes the
    whole deletion back, so status lists the directory as deleted and not staged,
    and `lore reset` creates the directory again as cloning did."""
    repo: Lore = new_lore_repo()
    instance, created = removed_under_view(repo, scratch_dir, rules, files, removed)
    instance.stage(".", offline=True)

    instance.unstage(removed, offline=True)
    unstaged = parse_status_json(instance.status(json=True, offline=True))
    entry = listed_deleted(unstaged, removed)
    assert entry and not entry["flagStaged"], f"lore unstage left {unstaged}"
    assert not any(e["flagStaged"] for e in unstaged), f"lore unstage left {unstaged}"

    instance.reset(removed)
    assert working_tree_files(instance, directories=True) == created
    assert_nothing_missing(instance, "the reset directory")


@pytest.mark.own_vfs("links a repository, which a virtual instance does not hold")
@pytest.mark.smoke
@pytest.mark.parametrize("scan", [False, True], ids=["marked", "scanned"])
@pytest.mark.parametrize(
    "rules",
    [EMPTIED_VIEW[:3] + ["!/a/b/lib/", "!/a/b/lib/**"], EMPTIED_VIEW[:3]],
    ids=["kept-link", "hidden-link"],
)
def test_staging_a_removed_directory_keeps_a_link_below_it(
    new_lore_repo, scratch_dir, rules, scan
):
    """In a repository that registers a link, a removed `a` stays where it holds
    `a/b`, which the view keeps and which holds the link's mount and a file the
    view hides, whether the view keeps the link or hides it: staging the marks of
    `lore dirty .`, or `lore stage --scan .`, keeps the link, the directories on
    its path and what the view hides beside it."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push_files(source, ["s.txt"])
    commit_and_push_files(repo, ["a/b/x.txt", "k.txt"])
    repo.link_add("a/b/lib", source.get_id(), "/")
    repo.commit("Add link")
    repo.push()
    instance: Lore = repo.clone(
        view=view_file(scratch_dir, "view", rules + ["!/k.txt"])
    )
    shutil.rmtree(f"{instance.path}/a")

    if scan:
        instance.stage(".", scan=True, offline=True)
    else:
        instance.dirty(".", offline=True)
        instance.stage(".", offline=True)
    try:
        instance.commit("Removed")
        instance.push()
    except NothingStagedError:
        pass

    fresh: Lore = repo.clone()
    assert working_tree_files(fresh) == ["a/b/lib/s.txt", "a/b/x.txt", "k.txt"]


@pytest.mark.own_vfs("links a repository, which a virtual instance does not hold")
@pytest.mark.smoke
@pytest.mark.parametrize("scan", [False, True], ids=["marked", "scanned"])
def test_an_offline_stage_takes_a_removed_directory_beside_a_link_the_view_leaves_out(
    new_lore_repo, scratch_dir, scan
):
    """In an instance cloned with a view that leaves out `z`, which holds a link
    at `z/deep/lib`, staging the removed `src` offline, by the marks of
    `lore dirty .` or by `lore stage --scan .`, stages `src` as deleted without
    reading what the view leaves out, and committing removes it."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push_files(source, ["s.txt"])
    hidden = [f"z/deep/f{index}.txt" for index in range(2000)]
    commit_and_push_files(repo, ["src/x.txt", "src/y.txt", "k.txt", *hidden])
    repo.link_add("z/deep/lib", source.get_id(), "/")
    repo.commit("Add link")
    repo.push()
    instance: Lore = repo.clone(view=view_file(scratch_dir, "view", ["/z/"]))
    shutil.rmtree(f"{instance.path}/src")

    if scan:
        instance.stage(".", scan=True, offline=True)
    else:
        instance.dirty(".", offline=True)
        instance.stage(".", offline=True)
    staged = parse_status_json(instance.status(json=True, offline=True))
    entry = listed_deleted(staged, "src")
    assert entry and entry["flagStaged"], f"lore stage . left {staged}"
    instance.commit("Removed")
    instance.push()

    fresh: Lore = repo.clone()
    assert not any(under(path, "src") for path in working_tree_files(fresh))


@pytest.mark.own_vfs(
    "links and layers a repository, which a virtual clone does not hold"
)
@pytest.mark.smoke
def test_sync_view_removes_what_it_no_longer_creates_in_links_and_layers(
    new_lore_repo, scratch_dir
):
    """A narrowing `sync --view` removes the directories the new view excludes
    below a link's mount and a layer's as in the repository's own tree, leaving
    each mount, the directories the view keeps, and the files it keeps."""
    repo: Lore = new_lore_repo()
    source: Lore = new_lore_repo(repo.name + "_source")
    commit_and_push_files(repo, ["main/b/x.txt", "keep.bat"])
    commit_and_push_files(source, ["ext/b/x.txt", "s.bat"])
    clone: Lore = repo.clone()
    clone.layer_add("layer", source, "/")
    clone.link_add("link", source.get_id(), "/")
    clone.commit("Add link")
    clone.push()

    clone.sync(view=view_file(scratch_dir, "view", ["b/", "*.txt"]))

    assert working_tree_files(clone, directories=True) == [
        "keep.bat",
        "layer",
        "layer/ext",
        "layer/s.bat",
        "link",
        "link/ext",
        "link/s.bat",
        "main",
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
