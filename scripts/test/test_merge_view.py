# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


def _write_view_filter(tmp_path_factory, *lines: str) -> str:
    """Write a view filter file holding `lines` and return its path."""
    temp_path = tmp_path_factory.mktemp("viewfilter")
    view_filter = os.path.join(temp_path, "view_filter.txt")
    with open(view_filter, "w+") as output_file:
        output_file.writelines(lines)
    return view_filter


@pytest.mark.smoke
def test_merge_restart(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo()
    source_bin_file1 = "file1.bin"
    source_bin_file2 = "file2.bin"
    source_bin_file3 = "subdir/file3.bin"

    with repo.open_file(source_bin_file1, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    with repo.open_file(source_bin_file2, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.make_dirs(os.path.dirname(source_bin_file3))
    with repo.open_file(source_bin_file3, "w+b") as output_file:
        output_file.write(os.urandom(4096))

    # (source) Stage the files
    repo.stage(scan=True)

    # (source) Commit the files
    repo.commit()

    # (source) Verify the repository
    repo.repository_verify()

    # (source) Push the repository
    repo.branch_push()

    temp_path = tmp_path_factory.mktemp("viewfilter")
    view_filter = os.path.join(temp_path, "view_filter.txt")

    with open(view_filter, "w+") as output_file:
        output_file.writelines(["/file2.bin\n", "/subdir/*\n"])

    # (clone) Clone the repository
    clone = repo.clone(view=view_filter)

    cloned_bin_file1 = "file1.bin"
    cloned_bin_file2 = "file2.bin"
    cloned_bin_file3 = "subdir/file3.bin"

    # (clone) Make a branch
    clone.branch_create("feature-branch")

    # (clone) Switch back to main for now
    clone.branch_switch("main")

    # (clone) Ensure view filter was properly applied in all operations
    assert clone.file_exists(cloned_bin_file1), (
        "File not cloned while it should be for " + cloned_bin_file1
    )
    assert not clone.file_exists(cloned_bin_file2), (
        "File cloned while it should not be for " + cloned_bin_file2
    )
    assert not clone.file_exists(cloned_bin_file3), (
        "File cloned while it should not be for " + cloned_bin_file3
    )

    # (source) Modify all files on source branch
    with repo.open_file(source_bin_file1, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    with repo.open_file(source_bin_file2, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    with repo.open_file(source_bin_file3, "w+b") as output_file:
        output_file.write(os.urandom(4096))

    # (source) Stage the modify
    repo.file_stage(scan=True)

    # (source) Commit the modify
    repo.commit("Modified all files")

    # (source) Push the repository
    repo.branch_push()

    # (clone) Sync on main branch
    clone.revision_sync()

    # (clone) Switch to feature branch
    clone.branch_switch("feature-branch")

    # (clone) Merge in main branch
    clone.branch_merge_start("main")

    # (clone) Ensure view filter was properly applied in all operations
    assert clone.file_exists(cloned_bin_file1), (
        "File not cloned while it should be for " + cloned_bin_file1
    )
    assert not clone.file_exists(cloned_bin_file2), (
        "File cloned while it should not be for " + cloned_bin_file2
    )
    assert not clone.file_exists(cloned_bin_file3), (
        "File cloned while it should not be for " + cloned_bin_file3
    )


@pytest.mark.smoke
def test_merge_out_of_view_directory_not_materialized(new_lore_repo, tmp_path_factory):
    """A merge that adds a brand-new directory outside the local view filter
    must not create that directory on a sparse working tree.

    Realize gates the filesystem work for a node on the view filter. That
    covers directory creation, link cloning and the file write, so neither the
    directory nor the file inside it reaches disk.
    """
    repo: Lore = new_lore_repo()

    # A single in-view file so the clone has content to materialize.
    in_view_file = "keep.bin"
    with repo.open_file(in_view_file, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit()
    repo.repository_verify()
    repo.branch_push()

    # Clone with a view filter that excludes the `hidden/` directory itself and
    # everything under it. `/hidden` (no trailing `/*`) excludes the directory
    # node too; `/hidden/*` would exclude only the contents, leaving the folder.
    temp_path = tmp_path_factory.mktemp("viewfilter")
    view_filter = os.path.join(temp_path, "view_filter.txt")
    with open(view_filter, "w+") as output_file:
        output_file.writelines(["/hidden\n"])

    clone = repo.clone(view=view_filter)
    clone.branch_create("feature-branch")
    clone.branch_switch("main")

    # (source) Add a brand-new out-of-view directory and file on main.
    out_of_view_file = "hidden/newfile.bin"
    repo.make_dirs(os.path.dirname(out_of_view_file))
    with repo.open_file(out_of_view_file, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit("Add out-of-view directory")
    repo.branch_push()

    # (clone) Sync main, switch to feature, merge main under the view filter.
    clone.revision_sync()
    clone.branch_switch("feature-branch")
    clone.branch_merge_start("main")

    assert not clone.file_exists(out_of_view_file), (
        "out-of-view file materialized on disk: " + out_of_view_file
    )
    assert not clone.path_exists("hidden"), (
        "out-of-view directory materialized on disk: hidden/"
    )


@pytest.mark.smoke
def test_merge_out_of_view_link_not_materialized(new_lore_repo, tmp_path_factory):
    """A merge that adds a link outside the local view filter must not clone the
    linked contents onto a sparse working tree.

    The link mount point is out of view, so realize skips cloning the linked
    repository's content just as it skips an out-of-view file.
    """
    repo: Lore = new_lore_repo()

    in_view_file = "keep.bin"
    with repo.open_file(in_view_file, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    repo.stage(scan=True)
    repo.commit()
    repo.branch_push()

    # A separate repository whose content will be linked in.
    link_repo = new_lore_repo()
    linked_content = "linked.bin"
    with link_repo.open_file(linked_content, "w+b") as output_file:
        output_file.write(os.urandom(4096))
    link_repo.stage(scan=True)
    link_repo.commit()
    link_repo.branch_push()

    # Clone the main repo with a view filter that excludes the link mount point
    # itself. `/linkdir` (no trailing `/*`) excludes the link node too; `/linkdir/*`
    # would exclude only its contents, leaving the mount point in view.
    temp_path = tmp_path_factory.mktemp("viewfilter")
    view_filter = os.path.join(temp_path, "view_filter.txt")
    with open(view_filter, "w+") as output_file:
        output_file.writelines(["/linkdir\n"])

    clone = repo.clone(view=view_filter)
    clone.branch_create("feature-branch")
    clone.branch_switch("main")

    # (source) Add the link at an out-of-view path on main.
    repo.link_add("linkdir", link_repo.get_id(), "/")
    repo.commit("Add out-of-view link")
    repo.branch_push()

    # (clone) Sync main, switch to feature, merge main under the view filter.
    clone.revision_sync()
    clone.branch_switch("feature-branch")
    clone.branch_merge_start("main")

    # The linked content must not be materialized on the sparse working tree.
    assert not clone.file_exists("linkdir/linked.bin"), (
        "out-of-view linked content materialized on disk: linkdir/linked.bin"
    )


@pytest.mark.smoke
def test_merge_grafts_untouched_out_of_view_subtree(new_lore_repo, tmp_path_factory):
    """A merge adopts a whole out-of-view subtree the merging branch never
    touched, and the adopted content is correct at every depth.

    The merging branch left the subtree identical to the base, so nothing inside
    it needs a three-way merge. The subtree is also out of view, so it never
    reaches the working tree. The merge can therefore take the other branch's
    version directly. The result must match a per-file merge: the sparse clone
    stays clean, and a full clone of the merged branch holds the other branch's
    content.
    """
    repo: Lore = new_lore_repo()

    in_view_file = "keep.txt"
    out_of_view_files = [
        "hidden/top.txt",
        "hidden/deep/one.txt",
        "hidden/deep/nested/two.txt",
    ]

    repo.write_files(
        {in_view_file: "keep v1\n"} | {p: "v1\n" for p in out_of_view_files}
    )
    repo.stage(scan=True)
    repo.commit()
    repo.branch_push()

    # Exclude the hidden/ subtree at every depth. `/hidden` also generates the
    # `hidden/**` rule that excludes the contents, which is what allows the
    # subtree to be adopted.
    view_filter = _write_view_filter(tmp_path_factory, "/hidden\n")

    clone = repo.clone(view=view_filter)
    clone.branch_create("feature-branch")

    # The feature branch changes only the in-view file, leaving the whole
    # hidden/ subtree identical to the base revision.
    clone.write_files({in_view_file: "keep v2 on feature\n"})
    clone.stage(scan=True)
    clone.commit("feature changes the in-view file")
    clone.branch_push()

    # main advances the out-of-view subtree at every depth.
    clone.branch_switch("main")
    repo.write_files({p: "v2\n" for p in out_of_view_files})
    repo.stage(scan=True)
    repo.commit("main changes the out-of-view subtree")
    repo.branch_push()

    clone.revision_sync()
    clone.branch_switch("feature-branch")
    merge_output = clone.branch_merge_start("main")

    # Confirm the subtree was adopted. Without this the test also passes on
    # the per-file merge path and proves nothing.
    assert "out-of-view subtrees" in merge_output, (
        "expected the merge to adopt the out-of-view subtree wholesale, "
        "output was:\n" + merge_output
    )

    # Adopting a subtree is a tree operation only, so nothing out of view is
    # written to the sparse working tree.
    assert not clone.path_exists("hidden"), (
        "out-of-view directory materialized on disk: hidden/"
    )

    # The merge must still have brought main's content into the tree. Push the
    # merged branch and clone it with no view filter to read it back.
    clone.branch_push()
    verify = repo.clone(branch="feature-branch")
    for path in out_of_view_files:
        assert verify.file_exists(path), (
            "merged branch is missing the out-of-view file: " + path
        )
        with verify.open_file(path) as input_file:
            assert input_file.read() == "v2\n", (
                "merged branch did not adopt main's content for " + path
            )


@pytest.mark.smoke
def test_merge_does_not_graft_when_view_reincludes_subpath(
    new_lore_repo, tmp_path_factory
):
    """A view that re-includes part of an excluded subtree must still write the
    re-included paths to disk during a merge.

    Adopting a subtree skips realizing any of it. That is only sound when every
    path below it is out of view. A re-inclusion puts some of those paths back
    in view, so they must still reach the working tree and the merge falls back
    to the per-file path.
    """
    repo: Lore = new_lore_repo()

    in_view_file = "keep.txt"
    reincluded_file = "hidden/keep/wanted.txt"
    excluded_file = "hidden/other.txt"

    repo.write_files(
        {in_view_file: "keep v1\n", reincluded_file: "v1\n", excluded_file: "v1\n"}
    )
    repo.stage(scan=True)
    repo.commit()
    repo.branch_push()

    # Exclude hidden/, then re-include hidden/keep/.
    view_filter = _write_view_filter(tmp_path_factory, "/hidden\n", "!/hidden/keep\n")

    clone = repo.clone(view=view_filter)
    assert clone.file_exists(reincluded_file), (
        "re-included file not cloned: " + reincluded_file
    )
    assert not clone.file_exists(excluded_file), (
        "excluded file cloned: " + excluded_file
    )

    clone.branch_create("feature-branch")

    # feature leaves hidden/ untouched, so without the re-inclusion the
    # subtree would qualify for adoption.
    clone.write_files({in_view_file: "keep v2 on feature\n"})
    clone.stage(scan=True)
    clone.commit("feature changes the in-view file")
    clone.branch_push()

    # main changes both the re-included and the excluded path.
    clone.branch_switch("main")
    repo.write_files({reincluded_file: "v2\n", excluded_file: "v2\n"})
    repo.stage(scan=True)
    repo.commit("main changes the out-of-view subtree")
    repo.branch_push()

    clone.revision_sync()
    clone.branch_switch("feature-branch")
    merge_output = clone.branch_merge_start("main")

    # The re-inclusion must have stopped the wholesale adoption.
    assert "out-of-view subtrees" not in merge_output, (
        "merge adopted a subtree that the view partly re-includes, "
        "output was:\n" + merge_output
    )

    # The re-included path is in view, so the merge must write main's content
    # to disk. Adopting the subtree would skip it and leave v1 behind.
    assert clone.file_exists(reincluded_file), (
        "re-included file missing after merge: " + reincluded_file
    )
    with clone.open_file(reincluded_file) as input_file:
        assert input_file.read() == "v2\n", (
            "re-included file was not realized by the merge: " + reincluded_file
        )

    # Its genuinely excluded sibling stays out of the working tree.
    assert not clone.file_exists(excluded_file), (
        "excluded file materialized on disk: " + excluded_file
    )


@pytest.mark.smoke
def test_merge_out_of_view_delete_reaches_tree(new_lore_repo, tmp_path_factory):
    """A merge that deletes an out-of-view file records the delete in the tree.

    The file is absent from a sparse working tree, so the on-disk removal has
    nothing to do. The tree must still lose the node, and its siblings must
    stay.
    """
    repo: Lore = new_lore_repo()
    deleted_file = "hidden/gone.txt"
    kept_file = "hidden/stay.txt"
    repo.write_files({"keep.txt": "v1\n", deleted_file: "v1\n", kept_file: "v1\n"})
    repo.stage(scan=True)
    repo.commit()
    repo.branch_push()

    view_filter = _write_view_filter(tmp_path_factory, "/hidden\n")
    clone = repo.clone(view=view_filter)
    clone.branch_create("feature-branch")
    clone.write_files({"keep.txt": "v2 on feature\n"})
    clone.stage(scan=True)
    clone.commit("feature changes the in-view file")
    clone.branch_push()

    # main deletes one out-of-view file.
    clone.branch_switch("main")
    os.remove(os.path.join(repo.path, deleted_file))
    repo.stage(scan=True)
    repo.commit("main deletes an out-of-view file")
    repo.branch_push()

    clone.revision_sync()
    clone.branch_switch("feature-branch")
    clone.branch_merge_start("main")

    # Read the tree back through a clone with no view filter.
    clone.branch_push()
    verify = repo.clone(branch="feature-branch")
    assert not verify.file_exists(deleted_file), (
        "delete did not reach the tree: " + deleted_file
    )
    assert verify.file_exists(kept_file), "sibling wrongly removed: " + kept_file


@pytest.mark.smoke
def test_merge_out_of_view_conflict_takes_theirs(new_lore_repo, tmp_path_factory):
    """A conflict at an out-of-view path adopts the incoming side, and writes
    nothing to a sparse working tree.

    A sparse working tree holds no file at such a path, so there is nothing to
    compare and no merge result to edit. Adopting the incoming side keeps the
    branch converging with the one being merged. An in-view conflict in the same
    merge must still behave as a conflict.

    Reaching an out-of-view conflict needs the branch to already hold a change
    at an out-of-view path, which it gets by merging a branch that changed one.
    """
    repo: Lore = new_lore_repo()
    out_of_view_file = "hidden/x.txt"
    in_view_file = "shown/y.txt"
    repo.write_files({out_of_view_file: "base\n", in_view_file: "base\n"})
    repo.stage(scan=True)
    repo.commit()
    repo.branch_push()

    # A second branch changes both files. feature takes these on as its own side.
    repo.branch_create("other")
    repo.write_files({out_of_view_file: "other\n", in_view_file: "other\n"})
    repo.stage(scan=True)
    repo.commit("other changes both files")
    repo.branch_push()
    repo.branch_switch("main")

    view_filter = _write_view_filter(tmp_path_factory, "/hidden\n")
    clone = repo.clone(view=view_filter)
    clone.branch_create("feature-branch")
    clone.revision_sync()
    clone.branch_merge_start("other")
    clone.branch_push()

    # main changes both files differently, so both paths conflict.
    repo.write_files({out_of_view_file: "main\n", in_view_file: "main\n"})
    repo.stage(scan=True)
    repo.commit("main changes both files")
    repo.branch_push()

    clone.revision_sync()
    merge_output = clone.branch_merge_start("main", check=False)

    # The out-of-view path is adopted, so it counts as merged rather than
    # conflicted. The in-view path is still a conflict.
    assert "1 merged, 1 conflicted" in merge_output, (
        "expected one adopted path and one conflict, output was:\n" + merge_output
    )
    assert in_view_file in merge_output, (
        "in-view conflict missing from the merge output: " + in_view_file
    )
    assert in_view_file in clone.run(["status"], check=False), (
        "in-view conflict missing from status: " + in_view_file
    )

    # Adopting is a tree operation only.
    for suffix in ("", "~mine", "~theirs", "~base"):
        assert not clone.file_exists(out_of_view_file + suffix), (
            "materialized on disk: " + out_of_view_file + suffix
        )
    assert not clone.path_exists("hidden"), (
        "out-of-view directory materialized on disk: hidden/"
    )

    # Only the in-view conflict needs resolving.
    clone.branch_merge_resolve_theirs(in_view_file)
    clone.commit("resolve the in-view conflict")
    clone.branch_push()

    # Read the tree back with no view filter. Both paths hold main's content.
    verify = repo.clone(branch="feature-branch")
    for path in (out_of_view_file, in_view_file):
        with verify.open_file(path) as input_file:
            assert input_file.read() == "main\n", (
                "expected the incoming side to be adopted for " + path
            )
