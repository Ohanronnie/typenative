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
elif command -v tn >/dev/null 2>&1; then
  tn=$(command -v tn)
else
  echo "tn compiler not found; set TN_BIN" >&2
  exit 2
fi

[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }

run() {
  printf '%s\n' "verify: $*"
  started=$(date +%s)
  if "$@"; then
    result=0
  else
    result=$?
  fi
  finished=$(date +%s)
  printf '%s\n' "verify-result: exit=$result elapsed=$((finished - started))s command=$*"
  return "$result"
}

run scripts/verify-design.sh
run scripts/check-toolchain.sh
run cargo fmt --all -- --check
run cargo test --workspace --all-targets
run cargo clippy --workspace --all-targets -- -D warnings
run env RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps

run "$tn" fmt --check std
run "$tn" fmt --check compiler-tn
run "$tn" fmt --check validation
for source in "$root"/std/*.tn; do
  [ -f "$source" ] || continue
  run "$tn" check "$source"
done
run "$tn" check "$root/compiler-tn/main.tn"

parallel_dir=$(mktemp -d "${TMPDIR:-/tmp}/typenative-verify-all.XXXXXX")
trap 'rm -rf -- "$parallel_dir"' EXIT
printf '%s\n' 'verify-parallel: cli stdlib runtime debug-info c-abi node redis sanitizers'
(run env TN_BIN="$tn" scripts/verify-cli.sh) >"$parallel_dir/cli.log" 2>&1 &
cli_pid=$!
(run env TN_BIN="$tn" TN_SKIP_SOURCE_CHECKS=1 scripts/verify-stdlib.sh) >"$parallel_dir/stdlib.log" 2>&1 &
stdlib_pid=$!
(run scripts/verify-runtime.sh) >"$parallel_dir/runtime.log" 2>&1 &
runtime_pid=$!
(run env TN_BIN="$tn" scripts/verify-debug-info.sh) >"$parallel_dir/debug-info.log" 2>&1 &
debug_info_pid=$!
(run env TN_BIN="$tn" scripts/verify-c-abi.sh) >"$parallel_dir/c-abi.log" 2>&1 &
c_abi_pid=$!
(run env TN_BIN="$tn" scripts/verify-node.sh) >"$parallel_dir/node.log" 2>&1 &
node_pid=$!
(run env TN_BIN="$tn" scripts/verify-redis.sh) >"$parallel_dir/redis.log" 2>&1 &
redis_pid=$!
(run env TN_BIN="$tn" scripts/run-sanitizers.sh) >"$parallel_dir/sanitizers.log" 2>&1 &
sanitizers_pid=$!

parallel_failed=0
for job in \
  "$cli_pid:cli" \
  "$stdlib_pid:stdlib" \
  "$runtime_pid:runtime" \
  "$debug_info_pid:debug-info" \
  "$c_abi_pid:c-abi" \
  "$node_pid:node" \
  "$redis_pid:redis" \
  "$sanitizers_pid:sanitizers"; do
  pid=${job%%:*}
  name=${job#*:}
  if wait "$pid"; then
    :
  else
    parallel_failed=1
  fi
  cat "$parallel_dir/$name.log"
done
rm -r -- "$parallel_dir"
trap - EXIT
[ "$parallel_failed" -eq 0 ] || exit 1

if ! command -v cargo-fuzz >/dev/null 2>&1; then
  echo "cargo-fuzz is required for the syntax fuzz gate" >&2
  exit 2
fi
run sh -c "cd \"$root/fuzz\" && cargo +nightly fuzz run lexer -- -runs=10000 -max_len=4096 -timeout=5"
run sh -c "cd \"$root/fuzz\" && cargo +nightly fuzz run parser -- -runs=10000 -max_len=4096 -timeout=5"

run env TN_BIN="$tn" scripts/bootstrap-self-host.sh
printf '%s\n' 'verification-matrix=pass'
