# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import pytest

from lore_ffi import HEADER_PATH, MIRRORED_STRUCTS, header_struct_fields


@pytest.mark.smoke
@pytest.mark.parametrize(
    "struct_name,mirror", MIRRORED_STRUCTS, ids=[name for name, _ in MIRRORED_STRUCTS]
)
def test_the_ctypes_mirror_holds_every_field_the_header_declares(struct_name, mirror):
    """The ctypes structs in `lore_ffi` are written by hand against `lore.h`.

    A field added to the C API leaves them short, and the library then reads
    past the end of the memory a caller allocated for one — a segfault inside
    the call, in whichever test happened to make it, pointing nowhere near the
    struct that caused it. Comparing the two here names the missing field
    instead.
    """
    if not HEADER_PATH.is_file():
        pytest.skip(f"no generated header to check against at {HEADER_PATH}")

    mirrored = [name for name, _type in mirror._fields_]

    assert mirrored == header_struct_fields(struct_name), (
        f"{mirror.__name__} no longer mirrors {struct_name}. Update its "
        f"_fields_ to match the header, in declaration order."
    )
