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
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skip(reason="Unknown issue specifically running in CI for OSS")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_down(new_lore_repo):
    with pytest.raises(ServiceCallError):
        new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
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


@pytest.mark.smoke
@pytest.mark.xdist_group("lore_service")
@pytest.mark.skipif(
    not service_supported(), reason="Service not supported on " + platform.system()
)
def test_service_resolves_relative_paths_against_caller(
    new_lore_repo, lore_service_in_directory, tmp_path
):
    """Relative paths belong to the directory the command was run in.

    The service resolves them, and its own working directory is unrelated to
    the caller's, so a service started elsewhere must not pull them towards
    itself. Every other service test passes an absolute repository path, which
    cannot catch this.
    """
    # Start the service in a directory unrelated to where the commands run, so
    # that a relative path resolved there rather than at the caller would show.
    service_directory = tmp_path / "service_elsewhere"
    caller_directory = tmp_path / "caller"
    service_directory.mkdir()
    caller_directory.mkdir()
    lore_service_in_directory(service_directory)

    # Seed a remote to clone from. Routed through the service like the rest,
    # but against the repository's own absolute path, so unaffected by the
    # service's directory.
    source: Lore = new_lore_repo(environment_vars=LORE_SERVICE_ENVIRONMENT.copy())
    with source.open_file("seed.txt", "w+") as seed_file:
        seed_file.write("seed\n")
    source.stage(scan=True, offline=True)
    source.commit("Seed", offline=True)
    source.push()

    # Clone to a relative path from the caller's directory. It must land there,
    # not under the service's directory.
    clone_name = "relative_clone"
    source.run(
        ["repository", "clone", source.remote_path, clone_name],
        cwd=str(caller_directory),
        use_os_dir=True,
    )

    clone_path = caller_directory / clone_name
    assert (clone_path / ".lore").is_dir(), (
        f"Clone must land under the caller's directory, not the service's. "
        f"{caller_directory} contains {list(caller_directory.iterdir())}"
    )
    assert not (service_directory / clone_name).exists(), (
        f"Clone must not land under the service's directory. "
        f"{service_directory} contains {list(service_directory.iterdir())}"
    )

    # Stage a relative path from inside the clone.
    clone = Lore(
        lore_executable_path=source.lore_executable_path,
        path=str(clone_path),
        name=clone_name,
        global_dir=source.global_dir,
        environment_vars=LORE_SERVICE_ENVIRONMENT.copy(),
        remote_url=source.remote,
        remote_path=source.remote_path,
        create_repo=False,
    )
    file_name = "added.uasset"
    (clone_path / file_name).write_bytes(os.urandom(30))
    clone.stage(file_name, relative_paths=True)

    status_output = clone.status()
    assert "A " + file_name in map(
        lambda line: line.strip(" "), status_output.splitlines()
    ), f"Staged file should show as added: {status_output}"
