#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn_bin=${TN_BIN:-"$target_dir/debug/tn"}
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(command -v tn || true)
fi
if [ -z "$tn_bin" ] || [ ! -x "$tn_bin" ]; then
  echo "tn compiler not found; set TN_BIN" >&2
  exit 2
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-node.XXXXXX")
trap 'rm -rf "$work"' EXIT

node_include=${NODE_INCLUDE_DIR:-}
if [ -z "$node_include" ]; then
  for candidate in \
    /opt/homebrew/opt/node@24/include/node \
    /usr/local/include/node \
    /usr/include/node \
    "$(dirname "$(dirname "$(command -v node)")")/include/node"; do
    if [ -f "$candidate/node_api.h" ]; then
      node_include=$candidate
      break
    fi
  done
fi
[ -f "$node_include/node_api.h" ] || { echo "Node-API headers not found; set NODE_INCLUDE_DIR" >&2; exit 2; }

TN_NODE_WRAPPER_DUMP="$work/exports.c" "$tn_bin" build "$root/validation/node/exports.tn" --emit node-addon --out "$work/exports.node" >/dev/null
"$tn_bin" build "$root/validation/node/classes.tn" --emit node-addon --out "$work/classes.node" >/dev/null
"$tn_bin" build "$root/validation/node/classes-fallible.tn" --emit node-addon --out "$work/classes-fallible.node" >/dev/null
"$tn_bin" build "$root/validation/node/async.tn" --emit node-addon --out "$work/async.node" >/dev/null
for declaration in "$work/exports.d.ts" "$work/classes.d.ts" "$work/classes-fallible.d.ts" "$work/async.d.ts"; do
  [ -s "$declaration" ] || { echo "missing Node declaration $declaration" >&2; exit 1; }
done
clang -std=c11 -Wall -Wextra -Werror -fPIC -I"$node_include" -c "$work/exports.c" -o "$work/exports-wrapper.o"

node --expose-gc "$root/validation/node/check.mjs" \
  "$work/exports.node" "$work/classes.node" "$work/classes-fallible.node" "$work/async.node"
if nm -u "$work/exports.node" | grep -E 'tn_runtime_(alloc|free)(\.|$)' >/dev/null; then
  echo "Node addon has unresolved TypeNative allocator symbols" >&2
  exit 1
fi
if nm -u "$work/exports.node" | grep -E '(^|[[:space:]])(_?v8|_?uv_|_?node::)' >/dev/null; then
  echo "Node addon imports V8, libuv, or C++ Node internals" >&2
  exit 1
fi
