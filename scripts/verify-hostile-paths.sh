#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac

if [ -n "${TYPENATIVE_TN_BIN:-}" ]; then
  compiler=$TYPENATIVE_TN_BIN
elif [ -x "$target_dir/debug/tn" ]; then
  compiler=$target_dir/debug/tn
elif [ -x "$target_dir/release/tn" ]; then
  compiler=$target_dir/release/tn
else
  compiler=$(command -v tn || true)
fi
[ -n "$compiler" ] && [ -x "$compiler" ] || {
  echo "tn compiler not found; set TYPENATIVE_TN_BIN" >&2
  exit 2
}

guard="$root/scripts/tn-guarded.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-hostile-paths.XXXXXX")
trap 'rm -rf "$work"' EXIT
export TYPENATIVE_TN_BIN="$compiler"
export TYPENATIVE_RUNTIME_ROOT="$root"

if rg -n 'tn_process_spawn\(|system\(|popen\(' \
  "$root/runtime" "$root/std" "$root/validation" "$root/crates/tn-driver/src/build.rs"; then
  echo "active process path contains a forbidden shell or legacy spawn call" >&2
  exit 1
fi

"$guard" fmt "$root/validation/process/main.tn"
"$guard" check "$root/validation/process/main.tn"
"$guard" build "$root/validation/process/main.tn" --profile debug --out "$work/process"

set +e
"$work/process" >"$work/argv.out"
run_code=$?
set -e
[ "$run_code" -eq 42 ] || {
  echo "hostile argv regression returned $run_code" >&2
  exit 1
}

{
  printf '%s' "space path|apostrophe'path|semicolon;path|dollar\$path|tab"
  printf '\t%s|line\npath|' path
} >"$work/expected"
cmp -s "$work/expected" "$work/argv.out" || {
  echo "argv boundaries or hostile bytes were not preserved" >&2
  exit 1
}

printf '%s\n' 'hostile-argv=pass'
