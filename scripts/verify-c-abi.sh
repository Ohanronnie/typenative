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
  cargo_target_dir=$(cargo metadata --no-deps --format-version 1 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  if [ -n "$cargo_target_dir" ] && [ -x "$cargo_target_dir/debug/tn" ]; then
    tn_bin="$cargo_target_dir/debug/tn"
  fi
fi
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(command -v tn || true)
fi
if [ -z "$tn_bin" ] || [ ! -x "$tn_bin" ]; then
  echo "tn compiler not found; set TN_BIN" >&2
  exit 2
fi
tn_guard="$root/scripts/tn-guarded.sh"
if [ "$tn_bin" = "$tn_guard" ]; then
  tn_bin=${TYPENATIVE_TN_BIN:-}
fi
[ -x "$tn_bin" ] || { echo "tn compiler is not executable: $tn_bin" >&2; exit 2; }
compiler=$tn_bin
export TYPENATIVE_TN_BIN="$compiler"
tn_bin="$tn_guard"
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-c-abi.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    library_suffix=.dylib
    nm_mode=darwin
    ;;
  *)
    echo "unsupported host for C ABI verification: $(uname -s) $(uname -m)" >&2
    exit 2
    ;;
esac

check_symbol() {
  symbol=$1
  library=$2
  if [ "$nm_mode" = darwin ]; then
    nm -gU "$library"
  else
    nm -g "$library"
  fi | grep -q "$symbol"
}

for profile in debug optimized; do
  profile_library="$work/libtn_c_exports-$profile$library_suffix"
  "$tn_bin" build "$root/validation/c/exports.tn" --profile "$profile" --emit shared-library --out "$profile_library"
  check_symbol tn_add "$profile_library"
  check_symbol tn_pair_value "$profile_library"
  check_symbol tn_kind_value "$profile_library"
done
library="$work/libtn_c_exports-debug$library_suffix"
"$tn_bin" build "$root/validation/c/extern.tn" --profile optimized --link-argument "$library" --out "$work/extern"
if "$work/extern"; then
  extern_status=0
else
  extern_status=$?
fi
[ "$extern_status" -eq 0 ]
for rejected in "$root"/validation/c/rejected-*.tn; do
  if "$tn_bin" build "$rejected" --emit shared-library --out "$work/rejected" >/dev/null 2>&1; then
    echo "C ABI rejection fixture unexpectedly compiled: $rejected" >&2
    exit 1
  fi
done
printf '%s\n' "c-abi-layout-and-call=pass"
