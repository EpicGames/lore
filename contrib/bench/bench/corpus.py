"""The corpus: an archive fetched by URL, unpacked once onto the client's data disk, and described
by manifests. Its identity is the hash of every file's path, size, mode and SHA-256, so it doesn't
depend on how the archive was packed.

Before each run, restore() puts the tree back as the manifests record it: tools change modes (P4
makes submitted files read-only) and leave metadata (Lore's .lore). A file whose size changed means
the tree was damaged, and the run stops.
"""
import os
import shutil
import subprocess

from . import fetch, util

MANIFESTS = ("filelist.txt", "stat.tsv", "sha256.txt")


class CorpusError(Exception):
    pass


def prepare(c, data, log):
    """Fetch, check and unpack corpus C (the spec's [corpus]) under DATA once, and return its
    description. A corpus already unpacked from the same archive with the same root is reused."""
    base = os.path.join(data, "corpus", f"{c['sha256'][:16]}-{util.digest(c['root'])[:8]}")
    info_path = os.path.join(base, "corpus.json")
    if os.path.exists(info_path):
        info = util.read_json(info_path)
        if info.get("archive_sha256") == c["sha256"] and info.get("archive_root") == c["root"]:
            log(f"corpus {info['id'][:12]} already unpacked: {info['files']} files, {info['bytes'] / 1e9:.2f} GB")
            return info
    name = os.path.basename(c["url"].split("?")[0]) or "corpus.tar"
    archive = fetch.fetch(c["url"], c["sha256"], os.path.join(data, "cache", "corpus"), name, log)
    tree = os.path.join(base, "tree")
    shutil.rmtree(tree, ignore_errors=True)
    os.makedirs(tree)
    log(f"unpacking {name}")
    unpack(archive, tree, c["format"] if c["format"] != "auto" else _format(name))
    root = os.path.join(tree, c["root"]) if c["root"] else tree
    if not os.path.isdir(root):
        raise CorpusError(f"corpus.root {c['root']!r} is not a directory in the archive")
    log("writing manifests")
    info = describe(root, base)
    info["archive_sha256"] = c["sha256"]
    info["archive_root"] = c["root"]
    info["url"] = c["url"]
    if not c["url"].startswith("file://"):
        os.remove(archive)  # the unpacked tree is what runs use; the archive is only for fetching
    util.write_json(info_path, info)
    log(f"corpus {info['id'][:12]}: {info['files']} files, {info['bytes'] / 1e9:.2f} GB")
    return info


def _format(name):
    for suffix, fmt in ((".tar.zst", "tar.zst"), (".tzst", "tar.zst"), (".tar.gz", "tar.gz"), (".tgz", "tar.gz"),
                        (".tar.xz", "tar.xz"), (".tar", "tar")):
        if name.endswith(suffix):
            return fmt
    raise CorpusError(f"cannot tell the archive format of {name}; set corpus.format")


def unpack(archive, dest, fmt):
    tar = ["tar", "-x", "--no-same-owner", "-C", dest]
    if fmt == "tar.zst":
        zstd = subprocess.Popen(["zstd", "-dcq", "-T0", archive], stdout=subprocess.PIPE)
        subprocess.run(tar, stdin=zstd.stdout, check=True)
        if zstd.wait() != 0:
            raise CorpusError(f"zstd failed on {archive}")
    else:
        flag = {"tar": [], "tar.gz": ["-z"], "tar.xz": ["-J"]}[fmt]
        subprocess.run([*tar, *flag, "-f", archive], check=True)


def describe(root, base):
    """Write the manifests for the tree at ROOT into BASE and return the corpus description:
    filelist.txt (every regular file, in byte order), stat.tsv (path, size, mtime in ns, mode) and
    sha256.txt (as sha256sum prints it)."""
    paths = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in filenames:
            full = os.path.join(dirpath, name)
            if os.path.isfile(full) and not os.path.islink(full):
                paths.append(os.path.relpath(full, root))
    paths.sort(key=util.sort_key)
    with open(os.path.join(base, "filelist.txt"), "w", encoding="utf-8", errors="surrogateescape") as f:
        f.writelines(p + "\n" for p in paths)
    total, stats = 0, {}
    with open(os.path.join(base, "stat.tsv"), "w", encoding="utf-8", errors="surrogateescape") as f:
        for p in paths:
            st = os.lstat(os.path.join(root, p))
            total += st.st_size
            stats[p] = (st.st_size, st.st_mode & 0o7777)
            f.write(f"{p}\t{st.st_size}\t{st.st_mtime_ns}\t{st.st_mode & 0o7777:o}\n")
    sums = sha256_tree(root, os.path.join(base, "filelist.txt"))
    with open(os.path.join(base, "sha256.txt"), "w", encoding="utf-8", errors="surrogateescape") as f:
        f.writelines(f"{sums[p]}  {p}\n" for p in paths)
    ident = util.digest([[p, stats[p][0], f"{stats[p][1]:o}", sums[p]] for p in paths])
    return {"id": ident, "root": root, "base": base, "files": len(paths), "bytes": total,
            **{m.split(".")[0]: os.path.join(base, m) for m in MANIFESTS}}


def sha256_tree(root, filelist, jobs=16):
    """path -> SHA-256 for every file FILELIST names under ROOT, hashed in parallel."""
    with open(filelist, "rb") as f:
        names = f.read()
    proc = subprocess.run(["xargs", "-d", "\\n", "-P", str(jobs), "-n", "512", "sha256sum", "--"],
                          input=names, cwd=root, capture_output=True, check=True)
    out = {}
    for line in proc.stdout.decode("utf-8", "surrogateescape").splitlines():
        digest, _, path = line.partition("  ")
        if digest.startswith("\\"):  # sha256sum escapes a name holding a backslash or newline
            digest, path = digest[1:], path.replace("\\n", "\n").replace("\\\\", "\\")
        out[path] = digest
    missing = [p for p in names.decode("utf-8", "surrogateescape").splitlines() if p not in out]
    if missing:
        raise CorpusError(f"no SHA-256 for {len(missing)} files, such as {missing[0]!r}")
    return out


def restore(info):
    """Put the tree back as stat.tsv records it: remove .lore, restore modes and modification times.
    Raises CorpusError if a file is missing or its size changed."""
    root = info["root"]
    shutil.rmtree(os.path.join(root, ".lore"), ignore_errors=True)
    missing = resized = restored = 0
    with open(info["stat"], encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            path, size, mtime_ns, mode = line.rstrip("\n").rsplit("\t", 3)  # a name may hold a tab
            full = os.path.join(root, path)
            try:
                st = os.lstat(full)
            except FileNotFoundError:
                missing += 1
                continue
            if st.st_size != int(size):
                resized += 1
            if st.st_mode & 0o7777 != int(mode, 8):
                os.chmod(full, int(mode, 8))
                restored += 1
            if st.st_mtime_ns != int(mtime_ns):
                os.utime(full, ns=(st.st_atime_ns, int(mtime_ns)))
                restored += 1
    if missing or resized:
        raise CorpusError(f"the corpus tree changed: {missing} files missing, {resized} resized")
    return {"restored": restored}
