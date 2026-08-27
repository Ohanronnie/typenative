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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-atomics.XXXXXX")
trap 'rm -rf "$work"' EXIT

rg -q 'export class AtomicI32' "$root/std/core.tn"
rg -q 'export class AtomicU64' "$root/std/core.tn"
rg -q 'export class AtomicUsize' "$root/std/core.tn"
rg -q 'private value: i32' "$root/std/core.tn"
rg -q 'private value: u64' "$root/std/core.tn"
rg -q 'private value: usize' "$root/std/core.tn"
if rg -n 'ChannelI32|atomicI32\(|atomicU64\(|atomicUsize\(' \
  "$root/std" "$root/runtime" "$root/validation"; then
  echo 'free-function or i32-specific atomic shortcut is still present' >&2
  exit 1
fi
rg -q 'index !== 250000' "$root/validation/sync/atomics.tn"
rg -q 'let first = startWorker' "$root/validation/sync/atomics.tn"
rg -q 'let fourth = startWorker' "$root/validation/sync/atomics.tn"

"$guard" "$compiler" fmt --check \
  "$root/std/core.tn" "$root/std/thread.tn" "$root/validation/sync/atomics.tn"
"$guard" "$compiler" check "$root/validation/sync/atomics.tn" >/dev/null

output="$work/atomics"
"$guard" "$compiler" build "$root/validation/sync/atomics.tn" --out "$output" >/dev/null
code=0
"$output" || code=$?
[ "$code" -eq 42 ] || {
  echo "atomic concurrency regression returned $code; expected 42" >&2
  exit 1
}

"$guard" "$compiler" build "$root/validation/sync/atomics.tn" \
  --emit llvm-ir --out "$work/atomics.ll" >/dev/null
rg -q 'load atomic i32' "$work/atomics.ll"
rg -q 'store atomic i32' "$work/atomics.ll"
rg -q 'atomicrmw add' "$work/atomics.ll"
rg -q 'cmpxchg ' "$work/atomics.ll"
rg -q 'tn_thread_spawn_task' "$work/atomics.ll"
rg -q '!dbg !' "$work/atomics.ll"

printf '%s\n' 'llvm-atomics=pass increments=1000000 workers=4'
