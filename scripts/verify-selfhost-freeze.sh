#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest=$root/docs/selfhost-freeze.json

[ -f "$manifest" ] || {
  echo "self-host freeze manifest is missing: $manifest" >&2
  exit 1
}

python3 - "$root" "$manifest" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
manifest_path = pathlib.Path(sys.argv[2])
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
files = manifest.get("files")
if manifest.get("format") != 1 or not isinstance(files, dict) or not files:
    raise SystemExit("invalid self-host freeze manifest")

expected = set(files)
actual = {
    path.relative_to(root).as_posix()
    for directory in (root / "compiler-tn",)
    for path in directory.rglob("*")
    if path.is_file()
}
actual.add("scripts/bootstrap-self-host.sh")

if actual != expected:
    added = sorted(actual - expected)
    removed = sorted(expected - actual)
    if added:
        print("self-host freeze additions:", *added, sep="\n  ", file=sys.stderr)
    if removed:
        print("self-host freeze removals:", *removed, sep="\n  ", file=sys.stderr)
    raise SystemExit(1)

for relative_path in sorted(expected):
    path = root / relative_path
    if not path.is_file():
        print(f"self-host freeze path is not a regular file: {relative_path}", file=sys.stderr)
        raise SystemExit(1)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != files[relative_path]:
        print(f"self-host freeze content changed: {relative_path}", file=sys.stderr)
        print(f"  expected {files[relative_path]}", file=sys.stderr)
        print(f"  actual   {digest}", file=sys.stderr)
        raise SystemExit(1)

print(f"self-host-freeze=pass checkpoint={manifest['checkpoint']} files={len(expected)}")
PY
