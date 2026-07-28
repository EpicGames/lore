# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal ctypes bindings for the public Lore C API (`liblore` / `lore.h`).

This is the same surface the SDK bindings are built on, so a test driving it
observes exactly the API-level behavior an SDK consumer sees — including
return codes for calls whose errors the CLI's human-oriented output layer
never surfaces.

Only the types needed by the tests are bound. Struct layouts mirror the
cbindgen-generated `lore.h` next to the built library; the synchronous entry
points return `0` on success or the failing error's FFI code (see
lore-base/src/error.rs for the code registry).
"""

import ctypes
import sys
from ctypes import (
    POINTER,
    Structure,
    c_char_p,
    c_int32,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from pathlib import Path

# FFI codes from lore-base/src/error.rs (`#[ffi_code(...)]`), which the
# header does not export as constants.
NOT_AUTHENTICATED = 12
NOT_SUPPORTED = 18


def library_filename() -> str:
    if sys.platform == "win32":
        return "lore.dll"
    if sys.platform == "darwin":
        return "liblore.dylib"
    return "liblore.so"


class LoreString(Structure):
    """`lore_string_t`: pointer + length, no terminator requirement."""

    _fields_ = [("string", c_char_p), ("length", c_size_t)]


class LoreStringArray(Structure):
    """`lore_string_array_t`: pointer to first element + count."""

    _fields_ = [("ptr", POINTER(LoreString)), ("count", c_size_t)]


class LoreGlobalArgs(Structure):
    """`lore_global_args_t`. Field order and types must match lore.h."""

    _fields_ = [
        ("repository_path", LoreString),
        ("correlation_id", LoreString),
        ("identity", LoreString),
        ("force", c_uint8),
        ("offline", c_uint8),
        ("local", c_uint8),
        ("remote", c_uint8),
        ("dry_run", c_uint8),
        ("no_atime", c_uint8),
        ("max_connections", c_uint32),
        ("search_limit", c_uint32),
        ("search_nearest", c_uint8),
        ("no_gc", c_uint8),
        ("in_memory", c_uint8),
        ("file_count_limit", c_uint64),
        ("file_size_limit", c_uint64),
        ("compress_task_limit", c_uint64),
        ("store_keep_alive", c_uint8),
        ("store_keep_alive_seconds", c_uint64),
        ("sync_data", c_uint8),
        ("cache", c_uint8),
    ]


class LoreEventCallbackConfig(Structure):
    """`lore_event_callback_config_t`. A null `func` receives no events."""

    _fields_ = [("user_context", c_uint64), ("func", c_void_p)]


class LoreAuthUserInfoArgs(Structure):
    """`lore_auth_user_info_args_t`."""

    _fields_ = [("user_ids", LoreStringArray)]


class LoreLibrary:
    """A loaded `liblore` with the bound entry points."""

    def __init__(self, library_path: str | Path):
        self._lib = ctypes.CDLL(str(library_path))
        self._lib.lore_auth_user_info.restype = c_int32
        self._lib.lore_auth_user_info.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreAuthUserInfoArgs),
            LoreEventCallbackConfig,
        ]

    def auth_user_info(self, repository_path: str, user_ids: list[str]) -> int:
        """Call `lore_auth_user_info` (the SDK's `authUserInfo`) without an
        event callback and return its FFI code: 0 on success, the failing
        error's code otherwise."""
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        id_bytes = [user_id.encode() for user_id in user_ids]

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        ids = (LoreString * len(id_bytes))(
            *(LoreString(encoded, len(encoded)) for encoded in id_bytes)
        )
        args = LoreAuthUserInfoArgs(LoreStringArray(ids, len(id_bytes)))

        no_callback = LoreEventCallbackConfig(0, None)
        return self._lib.lore_auth_user_info(
            ctypes.byref(globals_args), ctypes.byref(args), no_callback
        )
