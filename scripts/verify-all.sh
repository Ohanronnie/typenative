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
time_stage compiler scripts/check-toolchain.sh
time_stage compiler scripts/check-native-sources.sh
time_stage compiler cargo fmt --all -- --check
time_stage compiler "$tn" fmt --check runtime std validation benchmarks/http-log-analyzer
time_stage compiler scripts/check-foreign-syntax.sh
for source in "$root"/std/*.tn; do
  [ -f "$source" ] || continue
  time_stage compiler "$tn" check "$source"
done

time_stage tests cargo test --workspace --all-targets
time_stage tests cargo clippy --workspace --all-targets -- -D warnings
time_stage tests env RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps

parallel_dir=$(mktemp -d "${TMPDIR:-/tmp}/typenative-verify-all.XXXXXX")
trap 'rm -rf -- "$parallel_dir"' EXIT

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
(time_log tests-cli env TN_BIN="$tn" scripts/verify-cli.sh) & cli_pid=$!
(time_log tests-stdlib env TN_BIN="$tn" TN_SKIP_SOURCE_CHECKS=1 scripts/verify-stdlib.sh) & stdlib_pid=$!
(time_log tests-runtime env TN_BIN="$tn" scripts/verify-runtime.sh) & runtime_pid=$!
(time_log tests-time env TN_BIN="$tn" scripts/verify-time.sh) & time_pid=$!
(time_log tests-fs env TN_BIN="$tn" scripts/verify-fs.sh) & fs_pid=$!
(time_log tests-debug-info env TN_BIN="$tn" scripts/verify-debug-info.sh) & debug_info_pid=$!
(time_log tests-abi env TN_BIN="$tn" scripts/verify-c-abi.sh) & c_abi_pid=$!
(time_log tests-node env TN_BIN="$tn" scripts/verify-node.sh) & node_pid=$!
run_redis_checks() {
  (time_log benchmarks env TN_BIN="$tn" scripts/verify-redis.sh)
  (time_log sanitizers env TN_BIN="$tn" scripts/run-sanitizers.sh)
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

if command -v cargo-fuzz >/dev/null 2>&1 && rustup toolchain list 2>/dev/null | rg -q '^nightly'; then
  time_stage tests sh -c "cd \"$root/fuzz\" && cargo +nightly fuzz run lexer -- -runs=10000 -max_len=4096 -timeout=5"
  time_stage tests sh -c "cd \"$root/fuzz\" && cargo +nightly fuzz run parser -- -runs=10000 -max_len=4096 -timeout=5"
else
  printf '%s\n' 'fuzzing=unavailable (cargo-fuzz/nightly not installed)'
fi

printf '%s\n' 'verification-matrix=pass'
