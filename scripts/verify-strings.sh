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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-strings.XXXXXX")
trap 'rm -rf "$work"' EXIT

if rg -n '(^|[[:space:]])strlen[[:space:]]*\(' \
  "$root/runtime" "$root/std" "$root/validation" "$root/benchmarks" \
  --glob '*.tn'; then
  echo 'active TypeNative source still calls strlen for an ordinary value' >&2
  exit 1
fi

"$guard" "$compiler" fmt --check \
  "$root/runtime/runtime.tn" "$root/std/string.tn" "$root/std/bytes.tn" \
  "$root/std/ffi.tn" "$root/validation/string"
"$guard" "$compiler" check "$root/std/string.tn" >/dev/null
"$guard" "$compiler" check "$root/std/bytes.tn" >/dev/null

for source in main embedded-nul cstring; do
  output="$work/$source"
  "$guard" "$compiler" build "$root/validation/string/$source.tn" --out "$output" >/dev/null
  code=0
  "$output" || code=$?
  [ "$code" -eq 42 ] || {
    echo "string regression $source returned $code; expected 42" >&2
    exit 1
  }
done

"$guard" "$compiler" build "$root/validation/string/embedded-nul.tn" \
  --emit llvm-ir --out "$work/embedded-nul.ll" >/dev/null
if rg -n '@strlen|call .*strlen|declare .*strlen' "$work/embedded-nul.ll"; then
  echo 'embedded-NUL product imports or calls strlen' >&2
  exit 1
fi
rg -q 'tn_string_length' "$work/embedded-nul.ll"
rg -q 'tn_string_scalar_length' "$work/embedded-nul.ll"
rg -q 'tn_string_equals' "$work/embedded-nul.ll"
rg -q 'tn_string_hash_slots|tn_builtin_hash_' "$work/embedded-nul.ll"
rg -q 'tn_string_from_bytes' "$work/embedded-nul.ll"
rg -q '!dbg !' "$work/embedded-nul.ll"

printf '%s\n' 'strings=pass embedded-nul hash utf8 append length-aware'
