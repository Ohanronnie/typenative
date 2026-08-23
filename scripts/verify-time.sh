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
  tn=$(command -v tn || true)
fi
[ -x "$tn" ] || { echo "tn compiler not found; set TN_BIN" >&2; exit 2; }
tn_guard="$root/scripts/tn-guarded.sh"
if [ "$tn" = "$tn_guard" ]; then
  tn=${TYPENATIVE_TN_BIN:-}
fi
[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
compiler=$tn
export TYPENATIVE_TN_BIN="$compiler"
tn="$tn_guard"

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-time.XXXXXX")
trap 'rm -rf "$work"' EXIT

for profile in debug optimized; do
  "$tn" build "$root/validation/time/main.tn" --profile "$profile" --out "$work/time-$profile" >/dev/null
  output=$("$work/time-$profile")
  epoch=$(printf '%s\n' "$output" | sed -n '1p')
  current=$(printf '%s\n' "$output" | sed -n '2p')
  [ "$epoch" = '1970-01-01T00:00:00.000Z' ]
  node - "$current" <<'NODE'
const value = Date.parse(process.argv[2]);
if (!Number.isFinite(value) || Math.abs(value - Date.now()) > 60_000) {
  process.exit(1);
}
NODE
done

printf '%s\n' 'clock-date-debug-optimized=pass'
