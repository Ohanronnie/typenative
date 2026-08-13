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

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-sanitize.XXXXXX")
trap 'rm -rf "$work"' EXIT

build_and_run() {
  source=$1
  expected=$2
  output=$3
  "$tn" build "$root/$source" --profile debug --out "$work/$output" \
    --link-argument=-fsanitize=address,undefined >/dev/null
  set +e
  ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:abort_on_error=1} \
    UBSAN_OPTIONS=${UBSAN_OPTIONS:-halt_on_error=1} \
    "$work/$output" >/dev/null
  result=$?
  set -e
  [ "$result" -eq "$expected" ]
}

build_and_run validation/runtime/main.tn 0 runtime-address-undefined
build_and_run validation/stdlib/main.tn 42 stdlib-address-undefined
build_and_run validation/async/main.tn 43 async-address-undefined
build_and_run validation/collections/main.tn 42 collections-address-undefined
build_and_run validation/redis/lifecycle.tn 0 lifecycle-address-undefined
printf '%s\n' 'ordinary-typenative-address-undefined-sanitizers=pass'

TN_BIN="$tn" REDIS_SANITIZER=address-undefined \
  "$root/scripts/verify-redis.sh"

if "$tn" build "$root/validation/runtime/main.tn" --profile debug --out "$work/runtime-thread" \
  --link-argument=-fsanitize=thread >/dev/null 2>"$work/thread-build.err"; then
  TSAN_OPTIONS=${TSAN_OPTIONS:-halt_on_error=1} "$work/runtime-thread" >/dev/null
  printf '%s\n' 'ordinary-typenative-thread-sanitizer=pass'
  TN_BIN="$tn" REDIS_SANITIZER=thread "$root/scripts/verify-redis.sh"
else
  printf '%s\n' "ordinary-typenative-thread-sanitizer=unavailable: $(tr '\n' ' ' <"$work/thread-build.err")" >&2
fi
