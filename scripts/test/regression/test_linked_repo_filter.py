import pytest
import os

from lore import Lore


def _write_view_filter(tmp_path_factory, *lines: str) -> str:
    """Write a view filter file holding `lines` and return its path."""
    temp_path = tmp_path_factory.mktemp("viewfilter")
    view_filter = os.path.join(temp_path, "view_filter.txt")
    with open(view_filter, "w+") as output_file:
        output_file.writelines(lines)
    return view_filter


@pytest.mark.regression
def test_view_filter_when_adding_linked_repo(new_lore_repo, tmp_path_factory):
    repo: Lore = new_lore_repo()

    repo.write_commit_push(None, {"a.txt": os.urandom(1024)})

    cloned = repo.clone(view=_write_view_filter(tmp_path_factory, "/target_dir/c.txt"))

    linked_repo: Lore = new_lore_repo()
    linked_repo.write_commit_push(
        None,
        {"source_dir/b.txt": os.urandom(1024), "source_dir/c.txt": os.urandom(1024)},
    )

    cloned.link_add("target_dir", linked_repo.get_id(), "source_dir")

    cloned.compare_file(linked_repo, "target_dir/b.txt", "source_dir/b.txt")
    assert not cloned.file_exists("target_dir/c.txt")
