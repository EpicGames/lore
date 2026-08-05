# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_push_content_scan_with_nothing_pending_is_a_no_op(new_lore_repo):
    """--scan with no locally-cached, non-durable content should not error."""
    repo: Lore = new_lore_repo()

    test_file = "test.txt"
    with repo.open_file(test_file, "w+b") as f:
        f.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Test commit")
    repo.push()

    # Everything just committed and pushed is already durable on the remote,
    # so a scan should find nothing to push.
    repo.repository_push_content(scan=True)


@pytest.mark.smoke
def test_push_content_explicit_address_already_durable_is_a_no_op(new_lore_repo):
    """An explicit address that's already durable on the remote should not error."""
    repo: Lore = new_lore_repo()

    test_file = "test.txt"
    with repo.open_file(test_file, "w+b") as f:
        f.write(os.urandom(1000))

    repo.stage(scan=True)
    repo.commit("Test commit")
    repo.push()

    file_info = repo.file_info(test_file)[0]
    address = f"{file_info.hash}-{file_info.context}"

    repo.repository_push_content(addresses=[address])
