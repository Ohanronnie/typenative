#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cc=${CC:-clang}
out=$(mktemp -d "${TMPDIR:-/tmp}/typenative-runtime.XXXXXX")
trap 'rm -rf "$out"' EXIT

"$cc" -std=c11 -Wall -Wextra -Werror -pthread \
  "$root/runtime/runtime.c" "$root/validation/runtime/main.c" \
  -o "$out/runtime-test"
"$out/runtime-test"
printf '%s\n' 'runtime-collections-refcounts=pass'
