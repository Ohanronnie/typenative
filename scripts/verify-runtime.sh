#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn=${TN_BIN:-$target_dir/debug/tn}
if [ ! -x "$tn" ]; then
  cargo_target_dir=$(cargo metadata --no-deps --format-version 1 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  if [ -n "$cargo_target_dir" ] && [ -x "$cargo_target_dir/debug/tn" ]; then
    tn="$cargo_target_dir/debug/tn"
  fi
fi
if [ ! -x "$tn" ]; then
  tn=$(command -v tn || true)
fi
[ -x "$tn" ] || { echo "tn compiler not found; set TN_BIN or build with cargo" >&2; exit 2; }
tn_guard="$root/scripts/tn-guarded.sh"
if [ "$tn" = "$tn_guard" ]; then
  tn=${TYPENATIVE_TN_BIN:-}
fi
[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
compiler=$tn
export TYPENATIVE_TN_BIN="$compiler"
tn="$tn_guard"
out=$(mktemp -d "${TMPDIR:-/tmp}/typenative-runtime.XXXXXX")
trap 'rm -rf "$out"' EXIT

for profile in debug optimized; do
  "$tn" build "$root/validation/runtime/main.tn" --profile "$profile" --out "$out/runtime-$profile" >/dev/null
  "$out/runtime-$profile"
done
printf '%s\n' 'runtime-ordinary-typenative=pass'
