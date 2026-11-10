"""Fetching a file by URL into a cache, pinned by its SHA-256.

Sources: http:// and https:// (with resume), file://, s3:// (the aws CLI), gs:// (the gcloud CLI)
and Google Drive, as gdrive://FILE_ID or a drive.google.com share link. Google Drive serves large
files through a confirmation page and limits how often a file can be downloaded, so large corpora
are better served over plain HTTPS or from object storage.
"""
import os
import re
import shutil
import subprocess

from . import util


class FetchError(Exception):
    pass


def fetch(url, sha256, cache_dir, name, log=lambda m: None):
    """The path of a file with content SHA256, fetched from URL into CACHE_DIR/SHA256/NAME unless it
    is there already. A file:// URL is used in place after its hash is checked."""
    if url.startswith("file://"):
        path = url[len("file://"):]
        _check(path, sha256)
        return path
    dest_dir = os.path.join(cache_dir, sha256)
    dest = os.path.join(dest_dir, name)
    stamp = f"{dest}.sha256"
    if os.path.exists(dest) and os.path.exists(stamp) and open(stamp).read().strip() == sha256:
        return dest
    os.makedirs(dest_dir, exist_ok=True)
    part = f"{dest}.part"
    log(f"fetching {url}")
    for attempt in (1, 2):
        try:
            _download(_normalize(url), part)
            break
        except subprocess.CalledProcessError as e:
            if attempt == 2:
                raise FetchError(f"{url}: download failed ({e})") from None
            log(f"download failed ({e}); retrying from the start")
            if os.path.exists(part):
                os.remove(part)
    _check(part, sha256)
    os.replace(part, dest)
    with open(stamp, "w") as f:
        f.write(sha256 + "\n")
    return dest


def _normalize(url):
    m = re.match(r"https://drive\.google\.com/(?:file/d/|open\?id=|uc\?id=)([\w-]+)", url)
    if m:
        url = f"gdrive://{m.group(1)}"
    if url.startswith("gdrive://"):
        return f"https://drive.usercontent.google.com/download?id={url[len('gdrive://'):]}&export=download&confirm=t"
    return url


def _download(url, part):
    if url.startswith(("http://", "https://")):
        resume = ["-C", "-"] if os.path.exists(part) and os.path.getsize(part) > 0 else []
        # A transfer slower than 1 kB/s for 5 minutes has stalled: curl gives up, and fetch() tries again.
        subprocess.run(["curl", "-fsSL", "--retry", "5", "--retry-delay", "5", "--connect-timeout", "30",
                        "--speed-limit", "1000", "--speed-time", "300", *resume, "-o", part, url], check=True)
    elif url.startswith("s3://"):
        if not shutil.which("aws"):
            raise FetchError("s3:// needs the aws CLI")
        subprocess.run(["aws", "s3", "cp", "--only-show-errors", url, part], check=True)
    elif url.startswith("gs://"):
        if not shutil.which("gcloud"):
            raise FetchError("gs:// needs the gcloud CLI")
        subprocess.run(["gcloud", "storage", "cp", url, part], check=True)
    else:
        raise FetchError(f"{url}: unsupported scheme")


def _check(path, sha256):
    if not os.path.exists(path):
        raise FetchError(f"{path} does not exist")
    actual = _sha256(path)
    if actual != sha256:
        raise FetchError(f"{path}: SHA-256 is {actual}, the spec pins {sha256}")


def _sha256(path):
    """sha256sum where available: it uses the CPU's SHA extensions and is several times faster."""
    if shutil.which("sha256sum"):
        return subprocess.run(["sha256sum", path], capture_output=True, text=True, check=True).stdout.split()[0]
    return util.sha256_file(path)
