#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
benchmark="$root/benchmarks/redis-comparison"
build="$benchmark/build"
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn_bin=${TN_BIN:-$target_dir/debug/tn}
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(command -v tn || true)
fi
[ -x "$tn_bin" ] || { echo "tn compiler not found; set TN_BIN" >&2; exit 2; }
guard="$root/scripts/tn-guarded.sh"
[ -x "$guard" ] || { echo "TypeNative compiler guard not found: $guard" >&2; exit 2; }

tn() {
  TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" "$@"
}

mkdir -p "$build"
tn fmt --check "$benchmark/addon.tn" "$benchmark/native.tn"
tn check "$benchmark/addon.tn"
tn check "$benchmark/native.tn"
rm -f -- "$build/redis.node" "$build/redis.d.ts" "$build/redis-native"
/usr/bin/time -p -o "$build/build-addon-clean.time" \
  env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" \
  build "$benchmark/addon.tn" --profile optimized --emit node-addon --timings \
  --out "$build/redis.node" 2>"$build/build-addon-clean.phases"
/usr/bin/time -p -o "$build/build-addon-incremental.time" \
  env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" \
  build "$benchmark/addon.tn" --profile optimized --emit node-addon --timings \
  --out "$build/redis.node" 2>"$build/build-addon-incremental.phases"
/usr/bin/time -p -o "$build/build-native-clean.time" \
  env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" \
  build "$benchmark/native.tn" --profile optimized --timings \
  --out "$build/redis-native" 2>"$build/build-native-clean.phases"
/usr/bin/time -p -o "$build/build-native-incremental.time" \
  env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" \
  build "$benchmark/native.tn" --profile optimized --timings \
  --out "$build/redis-native" 2>"$build/build-native-incremental.phases"
node "$benchmark/benchmark.mjs" \
  --compiler-commit "$(git -C "$root" rev-parse HEAD)"
