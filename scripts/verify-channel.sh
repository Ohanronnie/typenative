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
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-channel.XXXXXX")

if rg -n 'ChannelI32|channelI32|sendI32|receiveI32|tn_channel_send\(|tn_channel_receive\(' \
  "$root/std/sync.tn" "$root/runtime/runtime.tn" "$root/validation"; then
  echo 'i32-specific channel API is still present' >&2
  exit 1
fi

"$guard" "$compiler" fmt --check \
  "$root/std/sync.tn" "$root/validation/sync" "$root/validation/forge/scheduler.tn"
"$guard" "$compiler" check "$root/std/sync.tn" >/dev/null

output="$work/channel"
"$guard" "$compiler" build "$root/validation/sync/channel.tn" --out "$output" >/dev/null
code=0
"$output" || code=$?
[ "$code" -eq 42 ] || {
  echo "typed channel regression returned $code; expected 42" >&2
  exit 1
}

"$guard" "$compiler" build "$root/validation/sync/channel.tn" \
  --emit llvm-ir --out "$work/channel.ll" >/dev/null
if rg -n 'tn_channel_send\(|tn_channel_receive\(' "$work/channel.ll"; then
  echo 'channel LLVM still calls the removed byte-copy API' >&2
  exit 1
fi
rg -q 'tn_channel_send_index_begin' "$work/channel.ll"
rg -q 'tn_channel_receive_index_begin' "$work/channel.ll"
rg -q 'store\.element' "$work/channel.ll"
rg -q 'move\.element' "$work/channel.ll"
rg -q 'drop\.elements' "$work/channel.ll"

printf '%s\n' 'channel=pass'
