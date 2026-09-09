"""Allocations per source line, inside the window a test marked.

`LORE_ALLOCATOR=tracking` writes `allocations.dmp`: one fixed-size record per allocation,
holding a timestamp, the pointer, the size, the number of callstack frames that follow and
those frames as instruction pointers relative to the load base. A size of u64::MAX marks a
free.

A test brackets the work it measures by allocating a block of size 0xABCD either side of it
(see `mark_window` in `lore-revision/tests/path_allocations.rs`), because the fixture around
the work allocates far more than the work does. Records between the first two markers are the
ones being measured.

Each allocation is charged to the shallowest frame in `lore-revision/src` that is not the
allocator itself, which is the line that asked for the memory:

    LORE_ALLOCATOR=tracking LORE_SCAN_FILES=200 \
        target/debug/deps/path_allocations-<hash> a_scan_of_an_unchanged_tree --test-threads=1
    python3 scripts/allocations/attribute.py allocations.dmp target/debug/deps/path_allocations-<hash> 200

Needs a debug build: a release build inlines the walk into anonymous async frames that carry
no usable line. It also needs `MAX_CALLSTACK_FRAMES` in `lore-base/src/allocator/tracking.rs`
raised from its default, which a debug build spends entirely on the allocator's own frames;
the record declares how many it carries, so this reads either.
"""

import collections
import struct
import subprocess
import sys

HEADER = struct.Struct("=QQQQ")
FREE = 0xFFFFFFFFFFFFFFFF
MARKER = 0xABCD


def records(data):
    """Every record in the dump, as (size, callstack).

    Each record declares how many frames it carries, so the layout follows whatever
    `MAX_CALLSTACK_FRAMES` the allocator was built with.
    """
    frames = HEADER.unpack_from(data, 0)[3]
    record = struct.Struct(f"={'Q' * (4 + frames)}")
    for offset in range(0, len(data) - record.size + 1, record.size):
        fields = record.unpack_from(data, offset)
        yield fields[2], tuple(frame for frame in fields[4:] if frame)


def main():
    dump, binary = sys.argv[1], sys.argv[2]
    files = int(sys.argv[3])

    with open(dump, "rb") as handle:
        data = handle.read()

    entries = list(records(data))
    marks = [index for index, (size, _) in enumerate(entries) if size == MARKER]
    if len(marks) < 2:
        print(f"found {len(marks)} window markers, need two")
        return
    window = [stack for size, stack in entries[marks[0] + 1 : marks[1]] if size != FREE]
    print(f"{len(window)} allocations in the window, {len(window) / files:.2f} per file")

    addresses = sorted({frame for stack in window for frame in stack})
    query = "\n".join(f"0x{address:x}" for address in addresses)
    output = subprocess.run(
        ["llvm-symbolizer", "--obj", binary, "--functions=linkage", "--demangle"],
        input=query,
        capture_output=True,
        text=True,
    ).stdout.splitlines()

    where = {}
    index = 0
    for address in addresses:
        name = output[index] if index < len(output) else "?"
        location = output[index + 1] if index + 1 < len(output) else "?"
        where[address] = (location.strip(), name.strip())
        while index < len(output) and output[index].strip():
            index += 1
        index += 1

    sites = collections.Counter()
    for stack in window:
        site = None
        for frame in stack:
            location, _name = where.get(frame, ("?", "?"))
            if "/lore-revision/src/" in location and "allocator" not in location:
                site = location.split("/data/Lore/")[-1]
                break
        sites[site or "(outside lore-revision)"] += 1

    for site, count in sites.most_common(40):
        print(f"{count:7}  {count / files:6.2f}/file  {site}")


main()
