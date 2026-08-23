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
# The shared verification runtime object is intentionally unsanitized. Sanitizer products must
# compile their own runtime and startup support through the compiler-owned instrumentation path.
unset TYPENATIVE_RUNTIME_OBJECT
tn="$tn_guard"

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-sanitize.XXXXXX")
trap 'rm -rf "$work"' EXIT

build_and_run() {
  source=$1
  expected=$2
  output=$3
  "$tn" build "$root/$source" --profile debug --out "$work/$output" \
    --sanitize address --sanitize undefined >/dev/null
  set +e
  ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:abort_on_error=1} \
    UBSAN_OPTIONS=${UBSAN_OPTIONS:-halt_on_error=1} \
    "$work/$output" >/dev/null
  result=$?
  set -e
  [ "$result" -eq "$expected" ]
}

build_ir() {
  source=$1
  output=$2
  shift 2
  "$tn" build "$root/$source" --profile debug --emit llvm-ir --out "$work/$output" "$@" >/dev/null
}

build_ir validation/string/embedded-nul.tn address-undefined.ll \
  --sanitize address --sanitize undefined
rg -q '__asan_' "$work/address-undefined.ll" || {
  echo "AddressSanitizer hooks are missing from emitted TypeNative LLVM" >&2
  exit 1
}
rg -q 'llvm\.ubsantrap' "$work/address-undefined.ll" || {
  echo "UBSan traps are missing from emitted TypeNative LLVM" >&2
  exit 1
}
rg -q '!dbg' "$work/address-undefined.ll" || {
  echo "sanitized TypeNative LLVM has no debug locations" >&2
  exit 1
}
rg -q 'source_filename = .*validation/string/embedded-nul\.tn' "$work/address-undefined.ll" || {
  echo "sanitized LLVM lost the TypeNative source location" >&2
  exit 1
}
TN_NODE_BRIDGE_IR="$work/node-bridge.ll" "$tn" build "$root/validation/node/exports.tn" \
  --profile debug --emit node-addon --sanitize address --sanitize undefined \
  --out "$work/node-exports.node" >/dev/null
rg -q '__asan_' "$work/node-bridge.ll" || {
  echo "AddressSanitizer hooks are missing from the direct-LLVM Node bridge" >&2
  exit 1
}
rg -q 'filename: "exports\.tn"' "$work/node-bridge.ll" || {
  echo "direct-LLVM Node bridge lost its TypeNative source filename" >&2
  exit 1
}
rg -q 'directory: ".*validation/node"' "$work/node-bridge.ll" || {
  echo "direct-LLVM Node bridge lost TypeNative source locations" >&2
  exit 1
}
rg -q '!dbg' "$work/node-bridge.ll" || {
  echo "direct-LLVM Node bridge has no debug locations" >&2
  exit 1
}

build_and_run validation/runtime/main.tn 0 runtime-address-undefined
build_and_run validation/stdlib/main.tn 42 stdlib-address-undefined
build_and_run validation/async/main.tn 43 async-address-undefined
build_and_run validation/collections/main.tn 42 collections-address-undefined
build_and_run validation/redis/lifecycle.tn 0 lifecycle-address-undefined
printf '%s\n' 'ordinary-typenative-address-undefined-sanitizers=pass'

TN_BIN="$compiler" REDIS_SANITIZER=address-undefined \
  "$root/scripts/verify-redis.sh"

build_ir validation/sync/atomics.tn thread.ll --sanitize thread
rg -q '__tsan_' "$work/thread.ll" || {
  echo "ThreadSanitizer hooks are missing from emitted TypeNative LLVM" >&2
  exit 1
}
rg -q '!dbg' "$work/thread.ll" || {
  echo "ThreadSanitizer LLVM has no debug locations" >&2
  exit 1
}
if "$tn" build "$root/validation/sync/atomics.tn" --profile debug --sanitize thread \
  --out "$work/runtime-thread" >/dev/null 2>"$work/thread-build.err"; then
  set +e
  TSAN_OPTIONS=${TSAN_OPTIONS:-halt_on_error=1} "$work/runtime-thread" >/dev/null
  thread_result=$?
  set -e
  [ "$thread_result" -eq 42 ]
  printf '%s\n' 'ordinary-typenative-thread-sanitizer=pass'
  TN_BIN="$compiler" REDIS_SANITIZER=thread "$root/scripts/verify-redis.sh"
else
  thread_error=$(tr '\n' ' ' < "$work/thread-build.err")
  printf '%s\n' "ordinary-typenative-thread-sanitizer=unavailable: $thread_error" >&2
  exit 1
fi
