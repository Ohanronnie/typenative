#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
if [ -n "${TN_BIN:-}" ]; then
  tn=$TN_BIN
elif [ -x "$target_dir/release/tn" ]; then
  tn=$target_dir/release/tn
elif [ -x "$target_dir/debug/tn" ]; then
  tn=$target_dir/debug/tn
else
  cargo_target_dir=$(cargo metadata --no-deps --format-version 1 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  if [ -n "$cargo_target_dir" ] && [ -x "$cargo_target_dir/debug/tn" ]; then
    tn=$cargo_target_dir/debug/tn
  elif command -v tn >/dev/null 2>&1; then
    tn=$(command -v tn)
  else
    echo "tn compiler not found; set TN_BIN" >&2
    exit 2
  fi
fi

[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
compiler=$tn
tn_guard="$root/scripts/tn-guarded.sh"
export TYPENATIVE_TN_BIN="$compiler"

run_tn() {
  "$tn_guard" "$@"
}

time_stage() {
  stage=$1
  shift
  printf '%s\n' "verify-stage-start: category=$stage command=$*"
  if /usr/bin/time -p "$@"; then
    result=0
  else
    result=$?
  fi
  printf '%s\n' "verify-stage-result: category=$stage exit=$result"
  return "$result"
}

time_stage compiler scripts/verify-design.sh
time_stage freeze scripts/verify-selfhost-freeze.sh
time_stage freeze scripts/verify-ledger.sh docs/implementation-ledger.json docs/selfhost-debt.json
time_stage freeze scripts/verify-active-boundary.sh
time_stage compiler sh scripts/check-direct-llvm-backend.sh
time_stage compiler scripts/check-toolchain.sh
time_stage compiler scripts/check-native-sources.sh
time_stage compiler scripts/verify-hostile-paths.sh
time_stage compiler scripts/verify-strings.sh
time_stage compiler scripts/verify-collections.sh
time_stage compiler scripts/verify-channel.sh
time_stage compiler scripts/verify-ownership.sh
time_stage compiler scripts/verify-llvm-atomics.sh
time_stage compiler cargo fmt --all -- --check
time_stage compiler env TYPENATIVE_TN_BIN="$compiler" TYPENATIVE_RUNTIME_ROOT="$root" "$tn_guard" fmt --check runtime std validation benchmarks/json-parser benchmarks/redis-comparison benchmarks/http-log-analyzer benchmarks/performance
time_stage compiler scripts/check-foreign-syntax.sh
for source in "$root"/std/*.tn; do
  [ -f "$source" ] || continue
  time_stage compiler env TYPENATIVE_TN_BIN="$compiler" TYPENATIVE_RUNTIME_ROOT="$root" "$tn_guard" check "$source"
done

time_stage tests cargo test --workspace --all-targets
time_stage tests cargo clippy --workspace --all-targets -- -D warnings
time_stage tests env RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps

parallel_dir=$(mktemp -d "${TMPDIR:-/tmp}/typenative-verify-all.XXXXXX")
trap 'rm -rf -- "$parallel_dir"' EXIT

if [ -n "${TYPENATIVE_RUNTIME_SOURCE:-}" ]; then
  runtime_source=$TYPENATIVE_RUNTIME_SOURCE
else
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) runtime_source=$root/runtime/platform/darwin-arm64.tn ;;
    *) echo "unsupported host; set TYPENATIVE_RUNTIME_SOURCE" >&2; exit 2 ;;
  esac
fi
time_stage compiler env -u TYPENATIVE_RUNTIME_OBJECT TYPENATIVE_TN_BIN="$compiler" TYPENATIVE_RUNTIME_ROOT="$root" "$tn_guard" build "$runtime_source" --profile optimized --emit object --out "$parallel_dir/runtime.o"
export TYPENATIVE_RUNTIME_OBJECT="$parallel_dir/runtime.o"

time_log() {
  label=$1
  shift
  log="$parallel_dir/$label.log"
  if {
    printf '%s\n' "verify-stage-start: category=$label command=$*"
    if /usr/bin/time -p "$@"; then
      result=0
    else
      result=$?
    fi
    printf '%s\n' "verify-stage-result: category=$label exit=$result"
    exit "$result"
  } >"$log" 2>&1; then
    return 0
  fi
  return $?
}

printf '%s\n' 'verify-parallel: cli stdlib runtime time fs debug-info c-abi node redis-and-sanitizers'
(time_log tests-cli env TN_BIN="$compiler" scripts/verify-cli.sh) & cli_pid=$!
(time_log tests-stdlib env TN_BIN="$compiler" TN_SKIP_SOURCE_CHECKS=1 scripts/verify-stdlib.sh) & stdlib_pid=$!
(time_log tests-runtime env TN_BIN="$compiler" scripts/verify-runtime.sh) & runtime_pid=$!
(time_log tests-time env TN_BIN="$compiler" scripts/verify-time.sh) & time_pid=$!
(time_log tests-fs env TN_BIN="$compiler" scripts/verify-fs.sh) & fs_pid=$!
(time_log tests-debug-info env TN_BIN="$compiler" scripts/verify-debug-info.sh) & debug_info_pid=$!
(time_log tests-abi env TN_BIN="$compiler" scripts/verify-c-abi.sh) & c_abi_pid=$!
(time_log tests-node env TN_BIN="$compiler" scripts/verify-node.sh) & node_pid=$!
run_redis_checks() {
  (time_log benchmarks env TN_BIN="$compiler" scripts/verify-redis.sh)
  (time_log sanitizers env TN_BIN="$compiler" scripts/run-sanitizers.sh)
}

(run_redis_checks) & redis_pid=$!

parallel_failed=0
for job in \
  "$cli_pid:tests-cli" \
  "$stdlib_pid:tests-stdlib" \
  "$runtime_pid:tests-runtime" \
  "$time_pid:tests-time" \
  "$fs_pid:tests-fs" \
  "$debug_info_pid:tests-debug-info" \
  "$c_abi_pid:tests-abi" \
  "$node_pid:tests-node" \
  "$redis_pid:redis"; do
  pid=${job%%:*}
  name=${job#*:}
  if wait "$pid"; then
    :
  else
    parallel_failed=1
  fi
  if [ "$name" = redis ]; then
    cat "$parallel_dir/benchmarks.log" "$parallel_dir/sanitizers.log"
  else
    cat "$parallel_dir/$name.log"
  fi
done
rm -r -- "$parallel_dir"
trap - EXIT
[ "$parallel_failed" -eq 0 ] || exit 1

time_stage benchmarks env TN_BIN="$compiler" scripts/verify-performance-budgets.sh

if command -v cargo-fuzz >/dev/null 2>&1 && rustup toolchain list 2>/dev/null | rg -q '^nightly'; then
  fuzz_targets='lexer parser formatter hir_mir node_bridge resp utf8 collections'
  for fuzz_target in $fuzz_targets; do
    corpus_dir="$root/fuzz/corpus/$fuzz_target"
    if [ ! -d "$corpus_dir" ] || ! rg --files "$corpus_dir" | rg -q .; then
      printf '%s\n' "fuzz corpus missing for $fuzz_target" >&2
      exit 1
    fi
    time_stage tests sh -c "cd \"$root/fuzz\" && cargo +nightly fuzz run $fuzz_target -- -runs=10000 -max_len=4096 -timeout=5 -print_final_stats=1"
  done
else
  printf '%s\n' 'fuzzing=unavailable (cargo-fuzz/nightly not installed)' >&2
  exit 1
fi

printf '%s\n' 'verification-matrix=pass'
