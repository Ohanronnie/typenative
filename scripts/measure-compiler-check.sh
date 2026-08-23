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
elif command -v tn >/dev/null 2>&1; then
  tn=$(command -v tn)
else
  metadata_target=$(cargo metadata --no-deps --format-version 1 2>/dev/null | sed -n 's/.*"target_directory":"\([^"\\]*\)".*/\1/p')
  if [ -x "$metadata_target/release/tn" ]; then
    tn=$metadata_target/release/tn
  elif [ -x "$metadata_target/debug/tn" ]; then
    tn=$metadata_target/debug/tn
  else
    echo "tn compiler not found; set TN_BIN" >&2
    exit 2
  fi
fi

[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
tn_guard="$root/scripts/tn-guarded.sh"
if [ "$tn" = "$tn_guard" ]; then
  tn=${TYPENATIVE_TN_BIN:-}
fi
[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
compiler=$tn
export TYPENATIVE_TN_BIN="$compiler"

runtime_root=${TYPENATIVE_RUNTIME_ROOT:-$root/runtime}
source=${TYPENATIVE_CHECK_SOURCE:-$root/runtime/runtime.tn}
case "$source" in
  "$root/compiler-tn/"*|*/compiler-tn/*)
    echo "active compiler timing cannot target frozen self-host sources: $source" >&2
    exit 1
    ;;
esac
timeout_seconds=${TN_CHECK_TIMEOUT_SECONDS:-175}
started=$(date +%s)
set +e
env TYPENATIVE_RUNTIME_ROOT="$runtime_root" TYPENATIVE_TN_BIN="$compiler" \
  "$tn_guard" check "$source" --timings
status=$?
set -e
finished=$(date +%s)
elapsed=$((finished - started))
printf '%s\n' "compiler-check-regression: seconds=$elapsed timeout=$timeout_seconds status=$status driver=$tn"

[ "$status" -eq 0 ] || exit "$status"
[ "$elapsed" -lt 180 ] || {
  echo "compiler check exceeded the 180-second regression budget" >&2
  exit 1
}
