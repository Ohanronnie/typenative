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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-stdlib.XXXXXX")
trap 'rm -rf "$work"' EXIT

if [ "${TN_SKIP_SOURCE_CHECKS:-0}" != 1 ]; then
  "$tn" fmt --check "$root/std"
  for file in "$root"/std/*.tn; do
    "$tn" check "$file" >/dev/null
  done
fi

run_status() {
  expected=$1
  shift
  set +e
  "$@" >/dev/null 2>"$work/program.err"
  result=$?
  set -e
  [ "$result" -eq "$expected" ]
}

cat >"$work/helper-call.tn" <<'EOF'
function add(value: i32): i32 {
  return value + 5i32 + 2i32;
}

function main(): i32 {
  return add(2i32 + 3i32);
}
EOF
"$tn" build "$work/helper-call.tn" --emit llvm-ir --out "$work/helper-call.ll" >/dev/null
grep -q 'define i32' "$work/helper-call.ll"
run_status 12 "$tn" run "$work/helper-call.tn"

for profile in debug optimized; do
  "$tn" build "$root/validation/stdlib/main.tn" --profile "$profile" --out "$work/stdlib-$profile" >/dev/null
  run_status 42 "$work/stdlib-$profile"
  "$tn" build "$root/validation/async/main.tn" --profile "$profile" --out "$work/async-$profile" >/dev/null
  run_status 43 "$work/async-$profile"
  "$tn" build "$root/validation/async/generic.tn" --profile "$profile" --out "$work/async-generic-$profile" >/dev/null
  run_status 42 "$work/async-generic-$profile"
  "$tn" build "$root/validation/async/generic-struct.tn" --profile "$profile" --out "$work/async-generic-struct-$profile" >/dev/null
  run_status 42 "$work/async-generic-struct-$profile"
  "$tn" build "$root/validation/collections/main.tn" --profile "$profile" --out "$work/collections-$profile" >/dev/null
  run_status 42 "$work/collections-$profile"
  "$tn" build "$root/validation/stdlib/alloc.tn" --profile "$profile" --out "$work/alloc-$profile" >/dev/null
  run_status 42 "$work/alloc-$profile"
  "$tn" build "$root/validation/io/main.tn" --profile "$profile" --out "$work/io-$profile" >/dev/null
  run_status 0 "$work/io-$profile"
done

printf '%s\n' 'hosted-stdlib-and-async=pass'
