#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compiler=${TN_BIN:-${TYPENATIVE_TN_BIN:-}}
if [ -z "$compiler" ]; then
  target_dir=${CARGO_TARGET_DIR:-$root/target}
  case "$target_dir" in
    /*) ;;
    *) target_dir="$root/$target_dir" ;;
  esac
  compiler="$target_dir/debug/tn"
fi
[ -x "$compiler" ] || { echo "tn compiler not found: $compiler" >&2; exit 2; }

guard="$root/scripts/tn-guarded.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-ownership.XXXXXX")

rg -q 'dropValue<T>\(this\.pointer\)' "$root/std/alloc.tn"
rg -q 'tn_rc_destroy\(this\.pointer' "$root/std/alloc.tn"
if ! rg -q -U 'dropValue<T>\(this\.pointer\);\s*tn_runtime_free\(this\.pointer' "$root/std/alloc.tn"; then
  echo 'Box storage is not freed immediately after typed drop glue' >&2
  exit 1
fi

"$guard" "$compiler" fmt --check "$root/std/alloc.tn" "$root/validation/stdlib/alloc.tn"
"$guard" "$compiler" check "$root/std/alloc.tn" >/dev/null

output="$work/alloc"
"$guard" "$compiler" build "$root/validation/stdlib/alloc.tn" --out "$output" >/dev/null
code=0
"$output" || code=$?
[ "$code" -eq 42 ] || {
  echo "ownership regression returned $code; expected 42" >&2
  exit 1
}

"$guard" "$compiler" build "$root/validation/stdlib/alloc.tn" \
  --emit llvm-ir --out "$work/alloc.ll" >/dev/null
rg -q 'tn_string_free' "$work/alloc.ll"
rg -q 'tn_rc_retain' "$work/alloc.ll"
rg -q 'tn_rc_release' "$work/alloc.ll"
rg -q 'tn_rc_destroy' "$work/alloc.ll"
rg -q 'tn_arc_retain' "$work/alloc.ll"
rg -q 'tn_arc_release' "$work/alloc.ll"
rg -q 'tn_arc_destroy' "$work/alloc.ll"
rg -q 'tn_arc_downgrade' "$work/alloc.ll"
rg -q 'tn_arc_upgrade' "$work/alloc.ll"
rg -q 'tn_arc_release_weak' "$work/alloc.ll"
rg -q 'drop\.string' "$work/alloc.ll"

printf '%s\n' 'ownership=pass'
