# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest
from lore_parsers import parse_status_json
from test_utils import to_posix

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_view(new_lore_repo, tmp_path_factory):
    view_dir = tmp_path_factory.mktemp("view")
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    unicode_dir = "奇怪的路徑"
    unicode_file = os.path.join(unicode_dir, "کاراکترهای یونیکد")
    first_dir = "aaaa"
    second_dir = "bbbb"

    with repo.open_file(text_file, "w+") as output_file:
        output_file.writelines(["One line\n", "Another line\n", "Third line\n"])

    repo.make_dirs(os.path.dirname(unicode_file))
    with repo.open_file(unicode_file, "w+", encoding="utf-8") as output_file:
        output_file.writelines(["只需將一些文本寫入文件即可\n"])

    for i in range(4):
        subpath = os.path.join(first_dir, str(i))
        repo.make_dirs(subpath)
        for j in range(5):
            with repo.open_file(
                os.path.join(subpath, str(j) + ".uasset"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(1024))

        subpath = os.path.join(second_dir, str(i))
        repo.make_dirs(subpath)
        for j in range(5):
            with repo.open_file(
                os.path.join(subpath, str(j) + ".uasset"), "w+b"
            ) as output_file:
                output_file.write(os.urandom(1024))

    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Create a view filter
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("**\n")
        view_file.write("!" + second_dir + "/1/**\n")

    # Clone the repository with a view filter
    clone = repo.clone(view=view_path)

    os.unlink(view_path)

    # Verify files contents, mode and last modified timestamp
    for index in range(5):
        assert repo.compare_file(
            clone, os.path.join(second_dir, "1", str(index) + ".uasset")
        )

    assert not os.path.exists(os.path.join(clone.path, first_dir)), (
        "Directory not filtered out as expected: " + os.path.join(clone.path, first_dir)
    )
    assert not os.path.exists(os.path.join(clone.path, text_file)), (
        "Top level file not filtered out as expected: "
        + os.path.join(clone.path, text_file)
    )
    assert not os.path.exists(os.path.join(clone.path, unicode_dir)), (
        "Directory not filtered out as expected: "
        + os.path.join(clone.path, unicode_dir)
    )

    for index in range(4):
        if index == 1:
            continue
        test_path2 = os.path.join(clone.path, second_dir, str(index))
        assert not os.path.exists(test_path2), (
            "Directory not filtered out as expected: " + test_path2
        )

    # Modify and stage some files in a branch in the filtered repository clone
    clone.branch_create("test-filter")

    with clone.open_file(
        os.path.join(second_dir, "1", "1.uasset"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1024))

    os.unlink(os.path.join(clone.path, second_dir, "1", "2.uasset"))

    with clone.open_file(
        os.path.join(second_dir, "1", "3.uasset"), "w+b"
    ) as output_file:
        output_file.write(os.urandom(1024))

    clone.stage(os.path.join(second_dir, "1", "1.uasset"))
    clone.commit("Modification commit")

    clone.stage(scan=True)
    clone.commit("Second modification commit")
    clone.push()

    repo.branch_switch("test-filter")
    repo.sync()

    # Verify files contents, mode and last modified timestamp

    for i in range(5):
        if i == 2:
            test_path = os.path.join(repo.path, second_dir, "1", str(i) + ".uasset")
            assert not os.path.exists(test_path), (
                "File not deleted as expected: " + test_path
            )
        else:
            assert repo.compare_file(
                clone, os.path.join(second_dir, "1", str(i) + ".uasset")
            )

    assert os.path.exists(os.path.join(repo.path, first_dir)), (
        "Directory not retained as expected: " + os.path.join(repo.path, first_dir)
    )

    assert os.path.exists(os.path.join(repo.path, text_file)), (
        "Top level file not retained as expected: " + os.path.join(repo.path, text_file)
    )

    assert os.path.exists(os.path.join(repo.path, unicode_dir)), (
        "Directory not retained as expected: " + os.path.join(repo.path, unicode_dir)
    )

    for i in range(4):
        if i == 1:
            continue
        test_path2 = os.path.join(repo.path, second_dir, str(i))
        assert os.path.exists(test_path2), (
            "Directory not retained as expected: " + test_path2
        )


@pytest.mark.smoke
def test_view_clone_materializes_directory_emptied_by_filter(
    new_lore_repo, tmp_path_factory
):
    """Cloning with a view filter that excludes every child of a directory --
    but not the directory node itself -- still materializes the (now empty)
    directory, and a following status agrees that nothing is missing.

    The committed tree has a directory `data` holding two files and a nested
    subdirectory with its own file, plus an unrelated top-level file. The view
    filter `data/**` excludes everything *under* `data` (both files, the `sub`
    subdirectory, and `sub/nested.txt`) while leaving the `data` directory node
    itself in view.

    Expected after the clone:
      - the unrelated in-view file `keep.txt` is materialized,
      - none of the excluded children exist on disk, and
      - `data` is materialized as an empty directory (this is the regression:
        a clone that drops a directory once the view filter removes all its
        children leaves `data` missing).

    Because the `data` node is in view, `status --scan` (a.k.a. --unstaged) on
    the fresh clone must report no changes. If the clone skipped creating the
    emptied directory, clone and status disagree about the view and `data`
    surfaces as a phantom delete.
    """
    repo: Lore = new_lore_repo()

    keep = "keep.txt"
    emptied_dir = "data"
    filtered_children = [
        os.path.join(emptied_dir, "file1.txt"),
        os.path.join(emptied_dir, "file2.txt"),
        os.path.join(emptied_dir, "sub", "nested.txt"),
    ]
    with repo.open_file(keep, "w+b") as f:
        f.write(os.urandom(64))
    for child in filtered_children:
        repo.make_dirs(os.path.dirname(child))
        with repo.open_file(child, "w+b") as f:
            f.write(os.urandom(64))
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    # Pure-exclusion view: `data/**` drops every descendant of `data` while the
    # `data` directory node itself stays in view.
    view_dir = tmp_path_factory.mktemp("view")
    view_path = os.path.join(view_dir, "view.txt")
    with open(view_path, "w+") as view_file:
        view_file.write("data/**\n")
    clone: Lore = repo.clone(view=view_path)

    # Sanity: the unrelated file is in view; every child of `data` is filtered.
    assert clone.file_exists(keep), "in-view file should be materialized"
    for child in filtered_children:
        assert not clone.path_exists(child), (
            f"view filter materialized an excluded child: {child}"
        )
    assert not clone.path_exists(os.path.join(emptied_dir, "sub")), (
        "excluded subdirectory should not be materialized"
    )

    # The directory node is in view, so it must be materialized even though the
    # filter left it empty.
    emptied_abs = os.path.join(clone.path, emptied_dir)
    assert os.path.isdir(emptied_abs), (
        "clone did not materialize the directory left empty by the view filter: "
        + emptied_dir
    )
    assert os.listdir(emptied_abs) == [], (
        "directory emptied by the view filter should have no materialized "
        f"children, got: {os.listdir(emptied_abs)}"
    )

    # status --scan must agree with the clone's view: no changes at all, and in
    # particular no phantom delete of the in-view (empty) `data` directory.
    entries = parse_status_json(clone.status(json=True, offline=True, scan=True))
    by_path = {to_posix(e.get("path", "")): e for e in entries}
    assert by_path == {}, (
        "status --scan on a pristine view-filtered clone reported changes "
        f"(clone/status view mismatch): {sorted(by_path)}"
    )
