#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
benchmark="$root/benchmarks/http-log-analyzer"
build="$benchmark/build"
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn_bin=${TN_BIN:-"$target_dir/debug/tn"}
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(command -v tn || true)
fi
[ -x "$tn_bin" ] || { echo "tn compiler not found; set TN_BIN" >&2; exit 2; }
guard="$root/scripts/tn-guarded.sh"
tn() {
  TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" "$@"
}
fixture_mib=${BENCH_FIXTURE_MIB:-100}
fixture=${BENCH_FIXTURE:-"/tmp/typenative-http-log-${fixture_mib}MiB.log"}
bench_iterations=${BENCH_ITERATIONS:-1}
bench_samples=${BENCH_SAMPLES:-5}

if [ -n "${BENCH_FIXTURE:-}" ]; then
  bench_command="TN_BIN=$tn_bin BENCH_FIXTURE=$fixture BENCH_ITERATIONS=$bench_iterations BENCH_SAMPLES=$bench_samples benchmarks/http-log-analyzer/run.sh"
else
  bench_command="TN_BIN=$tn_bin BENCH_FIXTURE_MIB=$fixture_mib BENCH_ITERATIONS=$bench_iterations BENCH_SAMPLES=$bench_samples benchmarks/http-log-analyzer/run.sh"
fi
fixture_command="node $benchmark/generate-fixture.mjs $fixture $fixture_mib"

mkdir -p "$build"
node "$benchmark/generate-fixture.mjs" "$fixture" "$fixture_mib"

tn fmt "$benchmark/analyzer.tn"
tn fmt --check "$benchmark/analyzer.tn"
/usr/bin/time -p -o "$build/check.time" env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" check "$benchmark/analyzer.tn"
/usr/bin/time -p -o "$build/build-debug.time" env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" build "$benchmark/analyzer.tn" --emit executable --out "$build/http-log-analyzer-debug"
/usr/bin/time -p -o "$build/build-optimized.time" env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" build "$benchmark/analyzer.tn" --profile optimized --emit executable --out "$build/http-log-analyzer"
/usr/bin/time -p -o "$build/build-addon.time" env TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" build "$benchmark/analyzer.tn" --profile optimized --emit node-addon --out "$build/http-log-analyzer.node"

quick_fixture="/tmp/typenative-http-log-debug-$$.log"
node "$benchmark/generate-fixture.mjs" "$quick_fixture" 0.01 >/dev/null
"$build/http-log-analyzer-debug" "$quick_fixture" 1 >/dev/null
/bin/rm -f "$quick_fixture"

BENCH_FIXTURE="$fixture" TN_BIN="$tn_bin" BENCH_ITERATIONS="$bench_iterations" BENCH_SAMPLES="$bench_samples" \
  BENCH_COMMAND="$bench_command" BENCH_FIXTURE_COMMAND="$fixture_command" node "$benchmark/benchmark.mjs"
