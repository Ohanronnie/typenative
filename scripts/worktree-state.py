#!/usr/bin/env python3
"""Capture and compare the exact non-ignored state of a Git worktree."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def git(*args: str) -> bytes:
    return subprocess.run(
        ["git", *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    ).stdout


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def untracked_entry(root: Path, encoded_path: bytes) -> dict[str, str | int]:
    path = root / os.fsdecode(encoded_path)
    metadata = path.lstat()
    entry: dict[str, str | int] = {
        "path": os.fsdecode(encoded_path),
        "mode": stat.S_IMODE(metadata.st_mode),
    }
    if path.is_symlink():
        entry["kind"] = "symlink"
        entry["sha256"] = digest(os.fsencode(os.readlink(path)))
    else:
        entry["kind"] = "file"
        entry["sha256"] = digest(path.read_bytes())
    return entry


def capture() -> dict[str, object]:
    root = Path(os.fsdecode(git("rev-parse", "--show-toplevel").rstrip(b"\n")))
    untracked = git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0")
    return {
        "head": os.fsdecode(git("rev-parse", "HEAD").strip()),
        "status_sha256": digest(
            git("status", "--porcelain=v2", "-z", "--untracked-files=all")
        ),
        "tracked_sha256": digest(
            git("diff", "--binary", "--full-index", "--no-ext-diff", "HEAD", "--")
        ),
        "index_sha256": digest(
            git(
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "HEAD",
                "--",
            )
        ),
        "untracked": [untracked_entry(root, path) for path in untracked if path],
    }


def write_snapshot(path: Path) -> None:
    path.write_text(json.dumps(capture(), indent=2, sort_keys=True) + "\n")


def compare_snapshot(path: Path) -> bool:
    before = json.loads(path.read_text())
    after = capture()
    if before == after:
        return True
    print("worktree state changed during verification", file=sys.stderr)
    for key in sorted(before.keys() | after.keys()):
        if before.get(key) != after.get(key):
            print(f"  changed: {key}", file=sys.stderr)
    return False


def main() -> int:
    if len(sys.argv) != 3 or sys.argv[1] not in {"capture", "compare"}:
        print(f"usage: {sys.argv[0]} capture|compare SNAPSHOT", file=sys.stderr)
        return 2
    snapshot = Path(sys.argv[2])
    if sys.argv[1] == "capture":
        write_snapshot(snapshot)
        return 0
    return 0 if compare_snapshot(snapshot) else 1


if __name__ == "__main__":
    raise SystemExit(main())
