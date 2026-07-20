# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import logging
import os
import platform

import pytest

from error_types import ServiceCallError
from lore import Lore

logger = logging.getLogger(__name__)

LORE_SERVICE_ENVIRONMENT = {"LORE_USE_SERVICE": "1"}


def service_supported():
    return platform.system() in ("Windows", "Linux", "Darwin")


@pytest.mark.smoke
@pytest.mark.skip(reason="Unknown issue specifically running in CI for OSS")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_down(new_lore_repo):
    with pytest.raises(ServiceCallError):
        new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())


@pytest.mark.smoke
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_call(new_lore_repo, background_lore_service):
    repo: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())

    # Add a single file so status has output
    file_name = "test.uasset"
    with repo.open_file(file_name, "w+b") as output_file:
        output_file.write(os.urandom(30))

    repo.stage(scan=True)

    status_output = repo.status()

    # Assert that single file is added
    assert "A " + file_name in map(
        lambda line: line.strip(" "), status_output.splitlines()
    )
