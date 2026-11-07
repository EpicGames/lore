# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import sys
import pytest
from pathlib import Path

from error_types import CaseMismatch, CaseVariantConflict
from lore import Lore
from test_utils import to_posix
from status_util import (
    Operation,
    verify_operations_in_status,
)

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_case(new_lore_repo):
    repo: Lore = new_lore_repo()
    # Generate some files
    text_file = "text-File.txt"
    other_file = os.path.join("some", "Path", "file.uasset")
    another_file = os.path.join("other", "path", "another.File")

    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(123456))

    repo.make_dirs(os.path.dirname(other_file))
    with repo.open_file(other_file, "w+b") as output_file:
        output_file.write(os.urandom(1234))

    repo.make_dirs(os.path.dirname(another_file))
    with repo.open_file(other_file, "w+b") as output_file:
        output_file.write(os.urandom(12345))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    repo.branch_create("test-branch")
    repo.push()
    repo.branch_switch("main")

    # Test case rename
    repo.remove_file(text_file)
    text_file = "Text-file.txt"
    with repo.open_file(text_file, "w+b") as output_file:
        output_file.write(os.urandom(1234567))

    repo.remove_file(other_file)
    repo.rmtree("other")

    # This should fail as we don't specify the case handling
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Specify rename to update the repository
    repo.stage(scan=True, case="rename")
    repo.commit()
    repo.push()

    # Clone the repo and ensure case is correct
    clone = repo.clone()

    files = os.listdir(clone.path)
    assert "Text-file.txt" in files, "Renamed file not cloned correctly"
    assert "other" not in files, "Deleted directory remains in clone"

    # Sync and make sure file is renamed
    repo.sync("@1")

    files = os.listdir(repo.path)
    assert "text-File.txt" in files, "Renamed file not synced correctly"
    assert "Text-file.txt" not in files, "Renamed file not synced correctly"
    assert "other" in files, "Deleted directory not restored by sync"
    repo.sync("@2")

    files = os.listdir(repo.path)
    assert "Text-file.txt" in files, "Renamed file not synced correctly"
    assert "text-File.txt" not in files, "Renamed file not synced correctly"
    assert "other" not in files, "Deleted directory not re-deleted by sync"

    # Branch switch and make sure file is renamed
    repo.branch_switch("test-branch")

    files = os.listdir(repo.path)
    assert "text-File.txt" in files, "Renamed file not synced correctly"
    assert "Text-file.txt" not in files, "Renamed file not synced correctly"
    assert "other" in files, "Deleted directory not restored by sync"


@pytest.mark.smoke
def test_case_only_rename_reaches_sync(new_lore_repo):
    """A revision whose only change renames a file in case commits without
    force, and a sync of another instance renames the file on disk."""
    repo: Lore = new_lore_repo()
    docs = Path("Docs")
    repo.write_commit_push(
        None, {docs / "Guide.txt": "guide\n", docs / "notes.txt": "notes\n"}
    )
    clone = repo.clone()

    repo.move(docs / "Guide.txt", docs / "guide.txt")
    repo.stage(scan=True, case="rename")
    repo.commit()
    repo.push()

    clone.sync()
    assert sorted(os.listdir(clone.path / docs)) == ["guide.txt", "notes.txt"]
    with clone.open_file(docs / "guide.txt") as f:
        assert f.read() == "guide\n"


@pytest.mark.smoke
def test_case_directory_rename(new_lore_repo):
    repo: Lore = new_lore_repo()

    """
    Create a directory structure:
    a/b/
        c/
        case.txt
        ignored.txt
        deleted.txt
    """
    original_dir = Path("a") / "b"
    nested_dir = "c"
    changed_nested_dir = "C"
    case_file = "case.txt"
    changed_case_file = "CASE.TXT"
    ignored_file = "ignored.txt"
    deleted_file = "deleted.txt"
    added_file = "added.txt"

    repo.write_commit_push(
        None,
        {
            original_dir / nested_dir / "empty_file.txt": "",
            original_dir / case_file: os.urandom(1234),
            original_dir / ignored_file: os.urandom(2345),
            original_dir / deleted_file: os.urandom(3456),
        },
    )

    # Rename the top-level directory to different casing and modify a file
    renamed_dir = Path("a") / "B"
    repo.move(original_dir, renamed_dir)
    repo.move(renamed_dir / nested_dir, renamed_dir / changed_nested_dir)
    repo.remove_file(renamed_dir / case_file)
    with repo.open_file(renamed_dir / changed_case_file, "w+b") as f:
        f.write(os.urandom(4567))
    with repo.open_file(renamed_dir / added_file, "w+b") as f:
        f.write(os.urandom(5678))
    repo.remove_file(renamed_dir / deleted_file)

    # Verify that the rename shows up in status
    verify_operations_in_status(
        repo.status(unstaged=True),
        [],
        [
            (Operation.MOVE, to_posix(original_dir), to_posix(renamed_dir)),
            (
                Operation.MOVE,
                to_posix(original_dir / nested_dir),
                to_posix(renamed_dir / changed_nested_dir),
            ),
            (
                Operation.MOVE,
                to_posix(original_dir / case_file),
                to_posix(renamed_dir / changed_case_file),
            ),
            (Operation.DELETE, to_posix(original_dir / deleted_file)),
        ],
        [to_posix(renamed_dir / added_file)],
    )

    # Staging without a case flag should fail
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Rename: update the repository to match the file system casing
    repo.stage(scan=True, case="rename")

    # Check the status during the staging
    verify_operations_in_status(
        repo.status(),
        [
            (Operation.MOVE, to_posix(original_dir), to_posix(renamed_dir)),
            (
                Operation.MOVE,
                to_posix(original_dir / case_file),
                to_posix(renamed_dir / changed_case_file),
            ),
            (Operation.ADD, to_posix(renamed_dir / added_file)),
            (Operation.DELETE, to_posix(original_dir / deleted_file)),
        ],
    )

    repo.commit()
    repo.push()

    # Verify the renamed directory on disk
    files = [str(p) for p in repo.list_paths()]
    assert str(renamed_dir) in files, "Directory was not renamed on disk"
    assert str(original_dir) not in files, "Original directory still exists on disk"

    # Verify subdirectories and files are intact
    for file_name in [changed_case_file, ignored_file, added_file]:
        assert os.path.isfile(repo.path / renamed_dir / file_name)

    # Clone and verify the new casing is in the repository
    clone = repo.clone()
    clone_files = [str(p) for p in clone.list_paths()]
    assert str(renamed_dir) in clone_files, "Renamed directory not in clone"
    assert str(original_dir) not in clone_files, "Original directory casing in clone"
    for file_name in [changed_case_file, ignored_file, added_file]:
        assert os.path.isfile(clone.path / renamed_dir / file_name)

    # Sync back to revision 1 and verify original casing is restored: the files revision 1
    # holds, under the names it spells them with.
    repo.sync("@1")
    files = [str(p) for p in repo.list_paths()]
    assert str(original_dir) in files, "Original directory not restored by sync"
    assert str(renamed_dir) not in files, "Renamed directory not reverted by sync"
    for file_name in [case_file, ignored_file, deleted_file]:
        assert os.path.isfile(repo.path / original_dir / file_name)


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
def test_case_directory_keep(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create a directory structure: Content/Maps/level.umap, Content/Audio/music.ogg
    original_dir = "Content"
    map_file = os.path.join(original_dir, "Maps", "level.umap")
    audio_file = os.path.join(original_dir, "Audio", "music.ogg")

    repo.make_dirs(os.path.dirname(map_file))
    with repo.open_file(map_file, "w+b") as f:
        f.write(os.urandom(4567))
    repo.make_dirs(os.path.dirname(audio_file))
    with repo.open_file(audio_file, "w+b") as f:
        f.write(os.urandom(5678))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Rename the top-level directory to different casing and modify a file
    renamed_dir = "content"
    repo.move(original_dir, renamed_dir)
    with repo.open_file(os.path.join(renamed_dir, "Maps", "level.umap"), "w+b") as f:
        f.write(os.urandom(7890))

    # Staging without a case flag should fail
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Keep: preserve the repository casing, rename the file system back
    repo.stage(scan=True, case="keep")

    # The directory on disk should have been renamed back to match the repository
    files = os.listdir(repo.path)
    assert original_dir in files, "Directory was not renamed back to match repository"
    assert renamed_dir not in files, "Local casing was not corrected by keep"

    # Verify subdirectories and files are intact after the rename-back
    assert os.path.isfile(os.path.join(repo.path, original_dir, "Maps", "level.umap"))
    assert os.path.isfile(os.path.join(repo.path, original_dir, "Audio", "music.ogg"))

    repo.commit()
    repo.push()

    # Clone and verify the original casing is preserved in the repository
    clone = repo.clone()
    clone_files = os.listdir(clone.path)
    assert original_dir in clone_files, "Repository casing not preserved in clone"
    assert renamed_dir not in clone_files, "Local casing leaked into repository"
    assert os.path.isfile(os.path.join(clone.path, original_dir, "Maps", "level.umap"))
    assert os.path.isfile(os.path.join(clone.path, original_dir, "Audio", "music.ogg"))


@pytest.mark.smoke
def test_case_keep(new_lore_repo):
    repo: Lore = new_lore_repo()

    # Create a file with initial casing and push it
    original_name = "Hello-World.txt"
    with repo.open_file(original_name, "w+b") as f:
        f.write(os.urandom(1234))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Rename the file on disk to a different casing
    renamed = "hello-world.txt"
    repo.remove_file(original_name)
    with repo.open_file(renamed, "w+b") as f:
        f.write(os.urandom(5678))

    # Staging without a case flag should fail
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Stage with keep: should accept the content change but preserve repository casing
    repo.stage(scan=True, case="keep")

    # The file on disk should have been renamed back to match the repository
    files = os.listdir(repo.path)
    assert original_name in files, "File was not renamed to match repository casing"
    assert renamed not in files, "Local casing was not corrected by keep"

    repo.commit()
    repo.push()

    # Clone and verify the original casing is preserved
    clone = repo.clone()
    clone_files = os.listdir(clone.path)
    assert original_name in clone_files, "Repository casing not preserved in clone"
    assert renamed not in clone_files, "Local casing leaked into repository"


@pytest.mark.smoke
@pytest.mark.parametrize("force", [False, True])
def test_case_variant_in_a_deleted_directory_is_deleted_with_it(new_lore_repo, force):
    """A file the working tree spells in another case than the revision does is deleted with
    the directory a later revision deletes, forced or not."""
    repo: Lore = new_lore_repo()
    directory = Path("Plugin")
    repo.write_commit_push(None, {directory / "VerseInput.Build.cs": os.urandom(1174)})

    repo.move(directory / "VerseInput.Build.cs", directory / "VerseInput.build.cs")
    repo.stage(scan=True, case="rename")
    repo.commit()
    repo.push()

    repo.rmtree(directory)
    repo.stage(scan=True)
    repo.commit()
    repo.push()

    repo.sync("@1")
    repo.sync("@2")
    repo.move(directory / "VerseInput.build.cs", directory / "VerseInput.Build.cs")
    repo.sync("@3", force=force)
    assert not repo.path_exists(directory), "The sync has to delete the directory"


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_case_directory_unification_keep(new_lore_repo):
    """On a case-sensitive FS, two directories differing only in case can coexist.
    Stage with keep should unify them under the repository's original casing,
    merging the contents of both directories."""
    repo: Lore = new_lore_repo()

    # Create initial directory structure and push
    dir_name = "Assets"
    file_a = os.path.join(dir_name, "Shared", "original.bin")
    repo.make_dirs(os.path.dirname(file_a))
    with repo.open_file(file_a, "w+b") as f:
        f.write(os.urandom(1111))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Create a second directory with different casing containing new files
    variant_dir = "assets"
    file_b = os.path.join(variant_dir, "Shared", "added.bin")
    repo.make_dirs(os.path.dirname(file_b))
    with repo.open_file(file_b, "w+b") as f:
        f.write(os.urandom(2222))

    # Both directories exist on disk
    assert os.path.isdir(os.path.join(repo.path, dir_name))
    assert os.path.isdir(os.path.join(repo.path, variant_dir))

    # Staging without case flag should fail
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Keep: unify under the repository casing (Assets), merging contents
    repo.stage(scan=True, case="keep")

    # Only the original casing should remain on disk
    entries = os.listdir(repo.path)
    assert dir_name in entries, "Original directory casing not preserved"
    assert variant_dir not in entries, "Variant directory was not removed"

    # Both files should be present under the unified directory
    shared = os.path.join(repo.path, dir_name, "Shared")
    shared_files = os.listdir(shared)
    assert "original.bin" in shared_files, "Original file missing after unification"
    assert "added.bin" in shared_files, "Added file missing after unification"

    repo.commit()
    repo.push()

    # Clone and verify unified contents
    clone = repo.clone()
    clone_shared = os.path.join(clone.path, dir_name, "Shared")
    clone_shared_files = os.listdir(clone_shared)
    assert "original.bin" in clone_shared_files, "Original file missing in clone"
    assert "added.bin" in clone_shared_files, "Added file missing in clone"


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_case_directory_unification_rename(new_lore_repo):
    """On a case-sensitive FS, three directories differing only in case can coexist.
    Stage with rename should unify them under the last alphabetically (the new casing),
    merging the contents of all directories on disk and in the repository."""
    repo: Lore = new_lore_repo()

    # Create initial directory structure and push
    dir_name = "Assets"
    file_a = os.path.join(dir_name, "Shared", "original.bin")
    repo.make_dirs(os.path.dirname(file_a))
    with repo.open_file(file_a, "w+b") as f:
        f.write(os.urandom(3333))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Create a second directory with different casing containing new files
    variant_dir = "assets"
    file_b = os.path.join(variant_dir, "Shared", "added.bin")
    repo.make_dirs(os.path.dirname(file_b))
    with repo.open_file(file_b, "w+b") as f:
        f.write(os.urandom(4444))

    # Create a third directory with yet another casing
    variant_dir_upper = "ASSETS"
    file_c = os.path.join(variant_dir_upper, "Shared", "extra.bin")
    repo.make_dirs(os.path.dirname(file_c))
    with repo.open_file(file_c, "w+b") as f:
        f.write(os.urandom(5555))

    # All three directories exist on disk
    assert os.path.isdir(os.path.join(repo.path, dir_name))
    assert os.path.isdir(os.path.join(repo.path, variant_dir))
    assert os.path.isdir(os.path.join(repo.path, variant_dir_upper))

    # Staging without case flag should fail
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Rename: unify under the last alphabetically non-matching variant (assets),
    # merging contents on disk
    repo.stage(scan=True, case="rename")

    # Only the winning variant should remain on disk
    entries = os.listdir(repo.path)
    assert variant_dir in entries, "Renamed directory not on disk"
    assert dir_name not in entries, "Old-cased directory Assets was not removed"
    assert variant_dir_upper not in entries, (
        "Old-cased directory ASSETS was not removed"
    )

    # All three files should be present under the unified directory
    shared = os.path.join(repo.path, variant_dir, "Shared")
    shared_files = os.listdir(shared)
    assert "original.bin" in shared_files, "Original file missing after unification"
    assert "added.bin" in shared_files, "Added file missing after unification"
    assert "extra.bin" in shared_files, "Extra file missing after unification"

    repo.commit()
    repo.push()

    # Clone and verify the new casing is in the repository with all files
    clone = repo.clone()
    clone_entries = os.listdir(clone.path)
    assert variant_dir in clone_entries, "Renamed directory not in clone"
    assert dir_name not in clone_entries, (
        "Original directory casing Assets still in clone"
    )
    assert variant_dir_upper not in clone_entries, "Directory casing ASSETS in clone"

    clone_shared = os.path.join(clone.path, variant_dir, "Shared")
    clone_shared_files = os.listdir(clone_shared)
    assert "original.bin" in clone_shared_files, "Original file missing in clone"
    assert "added.bin" in clone_shared_files, "Added file missing in clone"
    assert "extra.bin" in clone_shared_files, "Extra file missing in clone"

    # Sync back to revision 1 and verify original state is restored
    repo.sync("@1")
    entries = os.listdir(repo.path)
    assert dir_name in entries, "Original directory not restored by sync"
    assert os.path.isfile(os.path.join(repo.path, dir_name, "Shared", "original.bin"))


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_case_file_variant_conflict(new_lore_repo):
    """On a case-sensitive FS, files differing only in case should be rejected by
    stage --case rename because we cannot silently discard file content."""
    repo: Lore = new_lore_repo()

    # Create a file and push it
    with repo.open_file("readme.txt", "w+b") as f:
        f.write(os.urandom(1234))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Create a second file differing only in case
    with repo.open_file("README.txt", "w+b") as f:
        f.write(os.urandom(5678))

    # Both files exist on disk
    assert os.path.isfile(os.path.join(repo.path, "readme.txt"))
    assert os.path.isfile(os.path.join(repo.path, "README.txt"))

    # Staging without case flag should fail with mismatch
    with pytest.raises(CaseMismatch):
        repo.stage(scan=True)

    # Staging with rename should also fail because we cannot silently pick one file
    with pytest.raises(CaseVariantConflict):
        repo.stage(scan=True, case="rename")

    # Staging with keep should also fail
    with pytest.raises(CaseVariantConflict):
        repo.stage(scan=True, case="keep")


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_case_mixed_file_directory_variant_conflict(new_lore_repo):
    """On a case-sensitive FS, a file and directory differing only in case should be
    rejected by stage --case rename/keep because we cannot unify them."""
    repo: Lore = new_lore_repo()

    # Create a directory and push it
    dir_file = os.path.join("Data", "info.bin")
    repo.make_dirs(os.path.dirname(dir_file))
    with repo.open_file(dir_file, "w+b") as f:
        f.write(os.urandom(1234))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Create a file whose name is a case variant of the directory
    with repo.open_file("data", "w+b") as f:
        f.write(os.urandom(5678))

    # Both exist on disk: Data/ (directory) and data (file)
    assert os.path.isdir(os.path.join(repo.path, "Data"))
    assert os.path.isfile(os.path.join(repo.path, "data"))

    # Staging with rename should fail — cannot unify a file and directory
    with pytest.raises(CaseVariantConflict):
        repo.stage(scan=True, case="rename")

    # Staging with keep should also fail
    with pytest.raises(CaseVariantConflict):
        repo.stage(scan=True, case="keep")


@pytest.mark.own_vfs(
    "keeps case variants side by side, which a virtual clone's mount serves as one"
)
@pytest.mark.smoke
@pytest.mark.skipif(
    sys.platform != "linux", reason="Requires case-sensitive filesystem"
)
def test_case_directory_unification_keep_no_match(new_lore_repo):
    """On a case-sensitive FS, when multiple directory case variants exist but none
    matches the repository node name, stage --case keep should unify them all under
    the repository casing."""
    repo: Lore = new_lore_repo()

    # Create initial directory with casing "Content" and push
    dir_name = "Content"
    file_a = os.path.join(dir_name, "Maps", "level.umap")
    repo.make_dirs(os.path.dirname(file_a))
    with repo.open_file(file_a, "w+b") as f:
        f.write(os.urandom(4567))

    repo.stage(scan=True, offline=True)
    repo.commit(offline=True)
    repo.push()

    # Remove the original directory and create two variants with different casing,
    # neither matching the repository name "Content"
    repo.rmtree(dir_name)
    assert not os.path.exists(os.path.join(repo.path, dir_name))

    variant_a = "content"
    file_b = os.path.join(variant_a, "Maps", "level.umap")
    repo.make_dirs(os.path.dirname(file_b))
    with repo.open_file(file_b, "w+b") as f:
        f.write(os.urandom(5678))

    variant_b = "CONTENT"
    file_c = os.path.join(variant_b, "Audio", "music.ogg")
    repo.make_dirs(os.path.dirname(file_c))
    with repo.open_file(file_c, "w+b") as f:
        f.write(os.urandom(6789))

    # Neither variant matches the repository name
    assert os.path.isdir(os.path.join(repo.path, variant_a))
    assert os.path.isdir(os.path.join(repo.path, variant_b))
    assert not os.path.exists(os.path.join(repo.path, dir_name))

    # Keep: should unify all variants under the repository casing "Content"
    repo.stage(scan=True, case="keep")

    entries = os.listdir(repo.path)
    assert dir_name in entries, "Repository casing 'Content' not restored on disk"
    assert variant_a not in entries, "Variant 'content' was not removed"
    assert variant_b not in entries, "Variant 'CONTENT' was not removed"

    # Both file contents should be present under the unified directory
    assert os.path.isfile(os.path.join(repo.path, dir_name, "Maps", "level.umap"))
    assert os.path.isfile(os.path.join(repo.path, dir_name, "Audio", "music.ogg"))

    repo.commit()
    repo.push()

    # Clone should have the repository casing with all files
    clone = repo.clone()
    clone_entries = os.listdir(clone.path)
    assert dir_name in clone_entries, "Repository casing not in clone"
    assert os.path.isfile(os.path.join(clone.path, dir_name, "Maps", "level.umap"))
    assert os.path.isfile(os.path.join(clone.path, dir_name, "Audio", "music.ogg"))
