#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

native_sources=$(
  git ls-files -co --exclude-standard |
    while IFS= read -r path; do
      [ -e "$path" ] && printf '%s\n' "$path"
    done |
    rg '\.(c|h|cc|cpp|cxx|m|mm|S|s|asm)$' |
    rg -v '^benchmarks/cross-language/' || true
)
if [ -n "$native_sources" ]; then
  printf '%s\n' "$native_sources" >&2
  echo 'project-owned native implementation sources are still tracked' >&2
  exit 1
fi

printf '%s\n' 'project-owned-native-source-scan=pass'
