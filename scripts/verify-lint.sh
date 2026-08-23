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

"$tn" lint "$root/std/core.tn" --json >/dev/null
"$tn" lint "$root/std/collections.tn" --json >/dev/null
"$tn" lint "$root/validation/generators/main.tn" --json >/dev/null
"$tn" lint "$root/validation/generators/async.tn" --json >/dev/null
"$tn" lint "$root/validation/macros/main.tn" --json >/dev/null
printf '%s\n' 'tn-lint-canonical-sources=pass'
