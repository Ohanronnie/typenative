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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-debug.XXXXXX")
trap 'rm -rf "$work"' EXIT

"$tn" build "$root/validation/native/class-virtual.tn" --profile debug --emit object --out "$work/program.o" >/dev/null

dump=${DWARF_DUMP:-}
if [ -z "$dump" ]; then
  if command -v llvm-dwarfdump >/dev/null 2>&1; then
    dump=$(command -v llvm-dwarfdump)
  elif command -v dwarfdump >/dev/null 2>&1; then
    dump=$(command -v dwarfdump)
  else
    echo "no DWARF dumper available" >&2
    exit 2
  fi
fi

info=$($dump --debug-info "$work/program.o")
printf '%s\n' "$info" | grep -q 'DW_TAG_compile_unit'
printf '%s\n' "$info" | grep -q 'DW_TAG_subprogram'
if "$dump" --verify "$work/program.o" >/dev/null 2>&1; then
  :
fi

"$tn" build "$root/validation/native/class-virtual.tn" --profile debug --out "$work/program" >/dev/null
if command -v lldb >/dev/null 2>&1; then
  symbol=$(nm "$work/program" | awk '/tn_[0-9]+_[0-9]+$/ {sub(/^_/, "", $3); print $3; exit}')
  test -n "$symbol"
  lldb --batch \
    -o "target modules lookup -n $symbol" \
    "$work/program" 2>&1 | grep -q 'class-virtual.tn'
elif command -v gdb >/dev/null 2>&1; then
  symbol=$(nm "$work/program" | awk '/tn_[0-9]+_[0-9]+$/ {sub(/^_/, "", $3); print $3; exit}')
  test -n "$symbol"
  gdb -batch \
    -ex "info line $symbol" \
    "$work/program" 2>&1 | grep -q 'class-virtual.tn'
else
  echo "no LLDB or GDB available" >&2
  exit 2
fi

printf '%s\n' 'debug-information=pass'
