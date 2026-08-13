#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn_bin=${TN_BIN:-"$target_dir/debug/tn"}
if [ ! -x "$tn_bin" ]; then
  tn_bin="$target_dir/debug/tn"
fi
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(command -v tn || true)
fi
if [ -z "$tn_bin" ] || [ ! -x "$tn_bin" ]; then
  echo "tn compiler not found; set TN_BIN" >&2
  exit 2
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-c-abi.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) library="$work/libtn_c_exports.dylib" ;;
  *) echo "unsupported host for C ABI validation" >&2; exit 2 ;;
esac
for profile in debug optimized; do
  profile_library="$work/libtn_c_exports-$profile${library##*libtn_c_exports}"
  "$tn_bin" build "$root/validation/c/exports.tn" --profile "$profile" --emit shared-library --out "$profile_library"
  cp "${profile_library%.*}.h" "$work/exports-$profile.h"
done
library="$work/libtn_c_exports-debug${library##*libtn_c_exports}"
cp "$work/libtn_c_exports-debug.h" "$work/exports.h"
clang -std=c11 -Wall -Wextra -Werror "$root/validation/c/extern.c" -c -o "$work/extern.o"
"$tn_bin" build "$root/validation/c/extern.tn" --profile optimized --link-argument "$work/extern.o" --out "$work/extern"
if "$work/extern"; then
  extern_status=0
else
  extern_status=$?
fi
[ "$extern_status" -eq 42 ]
clang -std=c11 -Wall -Wextra -Werror \
  -I"$work" "$root/validation/c/caller.c" "$library" \
  -Wl,-rpath,"$work" -o "$work/caller"
"$work/caller"
for rejected in "$root"/validation/c/rejected-*.tn; do
  if "$tn_bin" build "$rejected" --emit shared-library --out "$work/rejected" >/dev/null 2>&1; then
    echo "C ABI rejection fixture unexpectedly compiled: $rejected" >&2
    exit 1
  fi
done
printf '%s\n' "c-abi-layout-and-call=pass"
