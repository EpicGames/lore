# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Minimal ctypes bindings for the public Lore C API (`liblore` / `lore.h`).

This is the same surface the SDK bindings are built on, so a test driving it
observes exactly the API-level behavior an SDK consumer sees — including
return codes for calls whose errors the CLI's human-oriented output layer
never surfaces.

Run as a script, this module is the driver a test invokes as a subprocess:

    python lore_ffi.py auth-user-info <library-path> <repository-path> [user-id...]
    python lore_ffi.py service-start <library-path>
    python lore_ffi.py service-stop <library-path>
    python lore_ffi.py revision-sync <library-path> <repository-path> <view-file>

exiting with the call's FFI code. Tests go through `Lore`'s `*_capi` methods
rather than importing `LoreLibrary` directly:
loading the library into the pytest process would leak its global state
(connection and authz caches, the tokio runtime, a panic hook) across every
test sharing that xdist worker, let a panic in the library take the worker
down with it, and force environment setup through the worker's own `os.environ`.
Importing this module for its constants is safe — nothing loads the library
until `LoreLibrary` is constructed.

Only the types needed by the tests are bound. Struct layouts mirror the
cbindgen-generated `lore.h` next to the built library; the synchronous entry
points return `0` on success or the failing error's FFI code (see
lore-base/src/error.rs for the code registry).
"""

import ctypes
import re
import sys
from ctypes import (
    POINTER,
    Structure,
    c_char_p,
    c_int,
    c_int32,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from pathlib import Path

# FFI codes from lore-base/src/error.rs (`#[ffi_code(...)]`), which the
# header does not export as constants. That module allocates codes in blocks
# by error group: 16-27 is authentication and authorization, 3-15 is input and
# validation.
NOT_AUTHENTICATED = 16
NOT_SUPPORTED = 9

# The generated header the structs below mirror, checked against them by
# test_lore_ffi.py. Relative to this file so it resolves wherever the tests run.
HEADER_PATH = Path(__file__).parents[2] / "lore-capi" / "lore.h"


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
        ("working_directory", LoreString),
        ("correlation_id", LoreString),
        ("identity", LoreString),
        ("force", c_uint8),
        ("offline", c_uint8),
        ("local", c_uint8),
        ("remote", c_uint8),
        ("dry_run", c_uint8),
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
        ("identity_token", LoreString),
        ("access_token", LoreString),
        ("stats", c_uint32),
        ("event_interval_ms", c_uint64),
    ]


class LoreEventCallbackConfig(Structure):
    """`lore_event_callback_config_t`. A null `func` receives no events."""

    _fields_ = [("user_context", c_uint64), ("func", c_void_p)]


class LoreAuthUserInfoArgs(Structure):
    """`lore_auth_user_info_args_t`."""

    _fields_ = [("user_ids", LoreStringArray)]


class LoreServiceStartArgs(Structure):
    """`lore_service_start_args_t`. Carries no arguments of its own.

    cbindgen gives a field-less struct an `int _unused;`, so the mirror has one
    too and the layout check compares like with like.
    """

    _fields_ = [("_unused", c_int)]


class LoreServiceStopArgs(Structure):
    """`lore_service_stop_args_t`. Carries no arguments of its own."""

    _fields_ = [("_unused", c_int)]


class LoreRevisionSyncArgs(Structure):
    """`lore_revision_sync_args_t`. `view` names the view filter file the
    working tree is left materialized under, empty to keep the instance's own."""

    _fields_ = [
        ("revision", LoreString),
        ("forward_changes", c_uint8),
        ("reset", c_uint8),
        ("root_files", LoreStringArray),
        ("dependency_tags", LoreStringArray),
        ("dependency_recursive", c_uint8),
        ("dependency_depth_limit", c_uint32),
        ("view", LoreString),
    ]


# Every struct above, paired with the header type it mirrors. A struct bound
# here belongs in this list: it is what test_lore_ffi.py checks the mirrors
# against, so a field added to the C API is reported as a named mismatch rather
# than read past the end of an allocation at the next call.
MIRRORED_STRUCTS = [
    ("lore_string_t", LoreString),
    ("lore_string_array_t", LoreStringArray),
    ("lore_global_args_t", LoreGlobalArgs),
    ("lore_event_callback_config_t", LoreEventCallbackConfig),
    ("lore_auth_user_info_args_t", LoreAuthUserInfoArgs),
    ("lore_service_start_args_t", LoreServiceStartArgs),
    ("lore_service_stop_args_t", LoreServiceStopArgs),
    ("lore_revision_sync_args_t", LoreRevisionSyncArgs),
]

# One field per line, either a function pointer (`void (*func)(...)`) or a plain
# declaration ending in the field name (`uint8_t force;`).
_HEADER_FIELD = re.compile(r"\(\*(?P<pointer>\w+)\)|(?P<plain>\w+)\s*;$")


def header_struct_fields(struct_name: str) -> list[str]:
    """The field names of `struct_name` in `lore.h`, in declaration order.

    Reading the header rather than restating it keeps the mirrors below honest:
    they are hand-written, and a field added to the C API is invisible to them
    until something dereferences the memory past their end.
    """
    header = HEADER_PATH.read_text()
    body = re.search(
        rf"typedef struct {struct_name} {{(.*?)\n}} {struct_name};", header, re.S
    )
    if body is None:
        raise LookupError(f"{struct_name} is not declared in {HEADER_PATH}")

    fields = []
    for line in body.group(1).splitlines():
        line = line.strip()
        if not line or line.startswith("//"):
            continue
        field = _HEADER_FIELD.search(line)
        if field is None:
            raise ValueError(f"cannot read a field name from {struct_name}: {line}")
        fields.append(field.group("pointer") or field.group("plain"))
    return fields


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
        self._lib.lore_service_start.restype = c_int32
        self._lib.lore_service_start.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreServiceStartArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_service_stop.restype = c_int32
        self._lib.lore_service_stop.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreServiceStopArgs),
            LoreEventCallbackConfig,
        ]
        self._lib.lore_revision_sync.restype = c_int32
        self._lib.lore_revision_sync.argtypes = [
            POINTER(LoreGlobalArgs),
            POINTER(LoreRevisionSyncArgs),
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

    def service_start(self) -> int:
        """Call `lore_service_start`, returning its FFI code.

        No repository: a service serves whichever ones its callers name, so
        starting one is not about any of them.
        """
        return self._lib.lore_service_start(
            ctypes.byref(LoreGlobalArgs()),
            ctypes.byref(LoreServiceStartArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def service_stop(self) -> int:
        """Call `lore_service_stop`, returning its FFI code.

        `0` whether or not one was running: a stop asks for none to be, and none
        running is that state.
        """
        return self._lib.lore_service_stop(
            ctypes.byref(LoreGlobalArgs()),
            ctypes.byref(LoreServiceStopArgs()),
            LoreEventCallbackConfig(0, None),
        )

    def revision_sync(self, repository_path: str, view: str) -> int:
        """Call `lore_revision_sync` with `view` and nothing else set, returning
        its FFI code.

        The entry point an SDK consumer reaches a view change through. `view`
        empty is the call every consumer that does not want one makes, and has
        to leave the instance's own view standing.
        """
        # Encoded buffers must outlive the call; keep references on the stack.
        path_bytes = repository_path.encode()
        view_bytes = view.encode()

        globals_args = LoreGlobalArgs()
        globals_args.repository_path = LoreString(path_bytes, len(path_bytes))

        args = LoreRevisionSyncArgs()
        args.view = LoreString(view_bytes, len(view_bytes))

        return self._lib.lore_revision_sync(
            ctypes.byref(globals_args),
            ctypes.byref(args),
            LoreEventCallbackConfig(0, None),
        )


USAGE = """usage:
  lore_ffi.py auth-user-info <library-path> <repository-path> [user-id...]
  lore_ffi.py service-start <library-path>
  lore_ffi.py service-stop <library-path>
  lore_ffi.py revision-sync <library-path> <repository-path> <view-file>"""


def main(argv: list[str]) -> int:
    match argv:
        case ["auth-user-info", library_path, repository_path, *user_ids]:
            return LoreLibrary(library_path).auth_user_info(repository_path, user_ids)
        case ["service-start", library_path]:
            return LoreLibrary(library_path).service_start()
        case ["service-stop", library_path]:
            return LoreLibrary(library_path).service_stop()
        case ["revision-sync", library_path, repository_path, view]:
            return LoreLibrary(library_path).revision_sync(repository_path, view)
        case _:
            print(USAGE, file=sys.stderr)
            return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
