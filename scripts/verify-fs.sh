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

empty=/tmp/typenative-fs-empty
one=/tmp/typenative-fs-one-byte
multi=/tmp/typenative-fs-multi-megabyte
sparse=/tmp/typenative-fs-sparse
directory=/tmp/typenative-fs-directory
unicode=/tmp/typenative-fs-雪
cleanup() {
  rm -f "$empty" "$one" "$multi" "$sparse" "$unicode"
  rmdir "$directory" 2>/dev/null || true
}
trap cleanup EXIT

: >"$empty"
printf 'x' >"$one"
dd if=/dev/zero of="$multi" bs=1m count=8 status=none
truncate -s 64m "$sparse"
mkdir -p "$directory"
printf 'unicode' >"$unicode"

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-fs.XXXXXX")
trap 'cleanup; rm -rf "$work"' EXIT
for profile in debug optimized; do
  "$tn" build "$root/validation/fs/main.tn" --profile "$profile" --out "$work/fs-$profile" >/dev/null
  "$work/fs-$profile"
done

printf '%s\n' 'filesystem-metadata-debug-optimized=pass'
