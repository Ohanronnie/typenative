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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-collections.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

if rg -n 'tn_map_|mapCreate|mapInsert|mapFind|mapEnsureCapacity|MapState' \
  "$root/runtime/runtime.tn" "$root/std/collections.tn"; then
  echo 'legacy byte-comparison map path is still present' >&2
  exit 1
fi
if rg -n 'tn_string_(compare|hash|equals)_slots' "$root/std/collections.tn"; then
  echo 'collection source still uses string-only protocol shortcuts' >&2
  exit 1
fi
if rg -n 'public get\(index: usize\): T \| undefined' "$root/std/collections.tn"; then
  echo 'Array.get still exposes ownership-copying access' >&2
  exit 1
fi

"$guard" "$compiler" fmt --check "$root/std/collections.tn" \
  "$root/validation/collections"
"$guard" "$compiler" check "$root/std/collections.tn" >/dev/null

run_case() {
  expected=$1
  label=$2
  source=$3
  output="$work/$label"
  "$guard" "$compiler" build "$root/$source" --out "$output" >/dev/null
  if "$output" >/dev/null; then
    result=0
  else
    result=$?
  fi
  [ "$result" -eq "$expected" ] || {
    echo "$label returned $result; expected $expected" >&2
    exit 1
  }
}

run_case 42 collections validation/collections/main.tn
run_case 42 typed-map validation/collections/typed-map.tn
run_case 42 ordered-map validation/collections/ordered-map.tn
run_case 42 custom validation/collections/custom.tn
run_case 42 ownership validation/collections/ownership.tn
run_case 42 array-ownership validation/collections/array-ownership.tn

"$guard" "$compiler" build "$root/validation/collections/typed-map.tn" \
  --emit llvm-ir --out "$work/typed-map.ll" >/dev/null
if rg -n 'tn_map_|memcmp' "$work/typed-map.ll"; then
  echo 'typed table LLVM still contains legacy byte-comparison map calls' >&2
  exit 1
fi
rg -q 'tn_builtin_hash_' "$work/typed-map.ll"
rg -q 'tn_builtin_equal_' "$work/typed-map.ll"
rg -q 'getelementptr .*i32' "$work/typed-map.ll"

"$guard" "$compiler" build "$root/validation/collections/ownership.tn" \
  --emit llvm-ir --out "$work/ownership.ll" >/dev/null
rg -q 'drop.element.address' "$work/ownership.ll"
rg -q 'drop.element.initialized.address' "$work/ownership.ll"
rg -q 'store.element.drop' "$work/ownership.ll"

"$guard" "$compiler" build "$root/validation/collections/array-ownership.tn" \
  --emit llvm-ir --out "$work/array-ownership.ll" >/dev/null
rg -q 'borrowed.element' "$work/array-ownership.ll"
rg -q 'move.element.value' "$work/array-ownership.ll"

printf '%s\n' 'collections=pass'
