"""Future sizes of the lore crates, from a release build with `-Zprint-type-sizes`.

    python3 scripts/type-sizes.py build <label> [-p <package>]...
    python3 scripts/type-sizes.py compare <base-label> <new-label>
    python3 scripts/type-sizes.py show <label> <regex> [--limit 8]

`build` recompiles the crates of this working tree's workspace in release, non-incrementally, into
`target/type-sizes/build`: every workspace package, or those `-p` names. It records each future's
size and states in `target/type-sizes/<label>.json`, with the sources that name its async blocks,
and copies the `lore` binary to `target/type-sizes/<label>-lore`. The workspace crates are
compiled with `RUSTC_BOOTSTRAP=1`. Linux and macOS.

`compare` lists the futures whose size differs between two builds, and those in one build only.
An async block is named by its source span; a span in the base is mapped to the lines of the new
source, so that a block whose lines moved is matched with itself. A block that another crate
instantiates is also listed under its def path. A label that contains `/` is the path of a
build's JSON file.

`show` prints the futures whose name matches the regex, largest first: each state's size, the
future it awaits, and the locals and captures it holds.
"""

import argparse
import difflib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "target" / "type-sizes"

TYPE = re.compile(r"^print-type-size type: `(.*)`: (\d+) bytes")
STATE = re.compile(r"^print-type-size     variant `(\w+)`: (\d+) bytes")
FIELD = re.compile(
    r"^print-type-size         (upvar|local) `\.?([^`]*)`: (\d+) bytes(?:.*, type: (.*))?$"
)
COROUTINE = ("{async fn body of ", "{async block@", "{async closure body@")
CRATE = re.compile(r"\blore(?:_\w+)?::")
SPAN = re.compile(r"(lore[\w-]*/src/[\w/.-]+\.rs):(\d+):(\d+): (\d+):(\d+)")


def wrap(rustc, arguments):
    """The rustc wrapper: compiles a workspace crate with `-Zprint-type-sizes` into a file of its own.

    Cargo passes only workspace members through `RUSTC_WORKSPACE_WRAPPER`. Build scripts and the
    probes cargo runs to learn the target are compiled as they are.
    """
    crate = (
        arguments[arguments.index("--crate-name") + 1]
        if "--crate-name" in arguments
        else ""
    )
    if (
        crate
        and not crate.startswith("build_script_")
        and not any(argument.startswith("--print") for argument in arguments)
    ):
        output = (
            pathlib.Path(os.environ["TYPE_SIZES_OUT"]) / f"{crate}-{os.getpid()}.txt"
        )
        os.dup2(
            os.open(output, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644),
            sys.stdout.fileno(),
        )
        os.environ["RUSTC_BOOTSTRAP"] = "1"
        arguments = [*arguments, "-Zprint-type-sizes"]
    os.execvp(rustc, [rustc, *arguments])


def parse(paths):
    """The futures in rustc's type-size output, each at its largest size across crates."""
    futures = {}
    for path in paths:
        future = None
        with open(path, errors="replace") as lines:
            for line in lines:
                if line.startswith("print-type-size type:"):
                    future = None
                    match = TYPE.match(line)
                    if match and match.group(1).startswith(COROUTINE):
                        name, size = CRATE.sub("", match.group(1)), int(match.group(2))
                        if name not in futures or futures[name]["size"] < size:
                            future = futures[name] = {"size": size, "states": []}
                    continue
                if future is None:
                    continue
                match = STATE.match(line)
                if match:
                    future["states"].append(
                        {
                            "name": match.group(1),
                            "size": int(match.group(2)),
                            "awaitee": None,
                            "held": [],
                        }
                    )
                    continue
                match = FIELD.match(line)
                if match and future["states"]:
                    kind, name, size, awaitee = match.groups()
                    state = future["states"][-1]
                    if name == "__awaitee":
                        state["awaitee"] = [int(size), CRATE.sub("", awaitee or "?")]
                    else:
                        state["held"].append([kind, name, int(size)])
    return futures


def build(args):
    if not re.fullmatch(r"[\w.-]+", args.label):
        sys.exit(f"a label is a file name: {args.label}")
    OUT.mkdir(parents=True, exist_ok=True)
    wrapper = OUT / "rustc-wrapper"
    wrapper.write_text(
        f'#!/bin/sh\nexec "{sys.executable}" "{pathlib.Path(__file__).resolve()}" wrap "$@"\n'
    )
    wrapper.chmod(0o755)
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        capture_output=True,
        check=True,
        text=True,
    )
    members = [package["name"] for package in json.loads(metadata.stdout)["packages"]]
    environment = dict(
        os.environ, CARGO_TARGET_DIR=str(OUT / "build"), CARGO_INCREMENTAL="0"
    )
    clean = [
        "cargo",
        "clean",
        "--release",
        *(f"--package={member}" for member in members),
    ]
    if subprocess.run(clean, cwd=ROOT, env=environment, check=False).returncode != 0:
        sys.exit("cargo clean failed")
    with tempfile.TemporaryDirectory(prefix="lore-type-sizes-") as raw:
        environment.update(RUSTC_WORKSPACE_WRAPPER=str(wrapper), TYPE_SIZES_OUT=raw)
        packages = (
            [f"--package={package}" for package in args.package]
            if args.package
            else ["--workspace"]
        )
        result = subprocess.run(
            ["cargo", "build", "--release", *packages],
            cwd=ROOT,
            env=environment,
            check=False,
        )
        if result.returncode != 0:
            sys.exit("cargo build failed")
        futures = parse(sorted(pathlib.Path(raw).iterdir()))
    files = {span.group(1) for name in futures for span in SPAN.finditer(name)}
    sources = {
        file: (ROOT / file).read_text()
        for file in sorted(files)
        if (ROOT / file).is_file()
    }
    model = OUT / f"{args.label}.json"
    model.write_text(json.dumps({"futures": futures, "sources": sources}))
    binary = OUT / "build" / "release" / "lore"
    if binary.is_file():
        shutil.copy2(binary, OUT / f"{args.label}-lore")
    print(f"{len(futures)} futures in {model}")


def load(label):
    path = pathlib.Path(label) if "/" in label else OUT / f"{label}.json"
    return json.loads(path.read_text())


def remapper(base, new):
    """Maps the spans in a base future's name to the lines of the new source."""
    maps = {}

    def line_map(file):
        if file not in maps:
            if base.get(file) == new.get(file):
                maps[file] = None
            elif file in base and file in new:
                old_lines, new_lines = base[file].splitlines(), new[file].splitlines()
                maps[file] = {}
                matcher = difflib.SequenceMatcher(
                    None, old_lines, new_lines, autojunk=False
                )
                for block in matcher.get_matching_blocks():
                    for offset in range(block.size):
                        maps[file][block.a + offset + 1] = block.b + offset + 1
            else:
                maps[file] = {}
        return maps[file]

    def span(match):
        lines = line_map(match.group(1))
        if lines is None:
            return match.group(0)
        start, end = lines.get(int(match.group(2))), lines.get(int(match.group(4)))
        if start is None or end is None:
            return match.group(0) + " (changed)"
        return f"{match.group(1)}:{start}:{match.group(3)}: {end}:{match.group(5)}"

    return lambda name: SPAN.sub(span, name)


def compare(args):
    base, new = load(args.base), load(args.new)
    remap = remapper(base["sources"], new["sources"])
    before = {}
    for name, future in base["futures"].items():
        name = remap(name)
        before[name] = max(before.get(name, 0), future["size"])
    after = {name: future["size"] for name, future in new["futures"].items()}
    common = before.keys() & after.keys()
    changed = [
        (after[name] - before[name], name)
        for name in common
        if after[name] != before[name]
    ]
    smaller = sorted(change for change in changed if change[0] < 0)
    larger = sorted((change for change in changed if change[0] > 0), reverse=True)
    print(
        f"{len(common)} futures in both builds: {len(smaller)} smaller, {len(larger)} larger"
    )
    for title, rows in (("Smaller", smaller), ("Larger", larger)):
        if rows:
            print(f"{title}:")
        for delta, name in rows:
            print(f"  {before[name]:>7} -> {after[name]:>7} {delta:>+7}  {name}")
    for title, names, sizes in (
        (args.base, before.keys() - after.keys(), before),
        (args.new, after.keys() - before.keys(), after),
    ):
        if names:
            print(f"Only in {title}:")
        for name in sorted(names, key=lambda name: -sizes[name]):
            print(f"  {sizes[name]:>7}  {name}")


def show(args):
    futures = load(args.label)["futures"]
    pattern = re.compile(args.regex)
    matches = sorted(
        (
            (future["size"], name)
            for name, future in futures.items()
            if pattern.search(name)
        ),
        reverse=True,
    )
    for size, name in matches[: args.limit]:
        print(f"{size:>7}  {name}")
        for state in futures[name]["states"]:
            if state["name"] in ("Returned", "Panicked"):
                continue
            awaitee = (
                f"  awaits {state['awaitee'][0]}: {state['awaitee'][1]}"
                if state["awaitee"]
                else ""
            )
            print(f"    {state['name']} {state['size']}{awaitee}")
            for kind, title in (("local", "locals"), ("upvar", "captures")):
                held = sorted(
                    (field for field in state["held"] if field[0] == kind and field[2]),
                    key=lambda field: -field[2],
                )
                if held:
                    print(
                        f"      {title}: "
                        + ", ".join(f"{field[1]} {field[2]}" for field in held)
                    )
    if len(matches) > args.limit:
        print(f"{len(matches) - args.limit} more; raise --limit")


def main():
    if len(sys.argv) > 2 and sys.argv[1] == "wrap":
        wrap(sys.argv[2], sys.argv[3:])
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(required=True)
    command = commands.add_parser("build")
    command.add_argument("label")
    command.add_argument("-p", "--package", action="append")
    command.set_defaults(run=build)
    command = commands.add_parser("compare")
    command.add_argument("base")
    command.add_argument("new")
    command.set_defaults(run=compare)
    command = commands.add_parser("show")
    command.add_argument("label")
    command.add_argument("regex")
    command.add_argument("--limit", type=int, default=8)
    command.set_defaults(run=show)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
