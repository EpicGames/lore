# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The `[auth]` table of the user-level `config.toml`, which the `lore` command applies."""

import subprocess
from pathlib import Path

import pytest


def auth_config_env(lore_subprocess_env: dict[str, str]) -> dict[str, str]:
    """The test's environment without the variables that would override `[auth]`."""
    env = dict(lore_subprocess_env)
    env.pop("LORE_AUTH_STORE", None)
    env.pop("LORE_AUTH_PATH", None)
    return env


def write_user_config(global_dir_name: str, text: str) -> Path:
    """Writes the user-level `config.toml` of the test's global directory, and answers its
    directory."""
    config_dir = Path(global_dir_name) / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text(text, encoding="utf-8")
    return config_dir


@pytest.mark.smoke
def test_a_relative_auth_path_puts_the_token_store_beside_the_config(
    lore_executable_path, lore_subprocess_env, global_dir_name, scratch_dir
):
    """With `store = "fallback"` and a relative `path`, a command run from another directory
    keeps the token store under the directory that holds `config.toml`."""
    config_dir = write_user_config(
        global_dir_name, '[auth]\nstore = "fallback"\npath = "tokens"\n'
    )
    elsewhere = scratch_dir("elsewhere", create=True)

    subprocess.run(
        [lore_executable_path, "auth", "clear"],
        cwd=elsewhere,
        env=auth_config_env(lore_subprocess_env),
        check=True,
        capture_output=True,
    )

    assert (config_dir / "tokens" / "tokenstore.toml").exists()
    assert not (elsewhere / "tokens").exists()


@pytest.mark.smoke
def test_an_auth_store_naming_neither_store_stops_the_command(
    lore_executable_path, lore_subprocess_env, global_dir_name
):
    """A `[auth] store` that names neither store fails the command rather than falling back to
    the secure store."""
    write_user_config(global_dir_name, '[auth]\nstore = "file"\n')

    result = subprocess.run(
        [lore_executable_path, "auth", "list"],
        env=auth_config_env(lore_subprocess_env),
        capture_output=True,
    )

    # 1 is the exit status the command gives for a configuration error before it runs.
    assert result.returncode == 1


@pytest.mark.smoke
def test_a_fallback_store_without_a_path_stops_the_command(
    lore_executable_path, lore_subprocess_env, global_dir_name
):
    """`store = "fallback"` with no `path` fails the command rather than replacing the key of
    the token store programs using the secure store share."""
    write_user_config(global_dir_name, '[auth]\nstore = "fallback"\n')

    result = subprocess.run(
        [lore_executable_path, "auth", "list"],
        env=auth_config_env(lore_subprocess_env),
        capture_output=True,
    )

    assert result.returncode == 1
