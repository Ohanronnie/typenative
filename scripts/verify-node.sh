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
case "$tn_bin" in
  */*) ;;
  *) echo "TN_BIN must resolve to an executable path" >&2; exit 2 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-node.XXXXXX")
trap 'rm -rf "$work"' EXIT
runtime_object="$work/runtime.o"

guarded() {
  started=$(date +%s)
  if [ -s "$runtime_object" ]; then
    if TYPENATIVE_RUNTIME_ROOT="$root" TYPENATIVE_RUNTIME_OBJECT="$runtime_object" perl -e 'alarm 175; exec @ARGV' -- "$@"; then
      status=0
    else
      status=$?
    fi
  elif TYPENATIVE_RUNTIME_ROOT="$root" perl -e 'alarm 175; exec @ARGV' -- "$@"; then
    status=0
  else
    status=$?
  fi
  finished=$(date +%s)
  elapsed=$((finished - started))
  if [ "$elapsed" -ge 175 ]; then
    echo "TypeNative compiler command reached the 175-second guard: $*" >&2
    return 124
  fi
  return "$status"
}

if [ -n "${TYPENATIVE_RUNTIME_SOURCE:-}" ]; then
  runtime_source=$TYPENATIVE_RUNTIME_SOURCE
else
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) runtime_source=$root/runtime/platform/darwin-arm64.tn ;;
    Linux:x86_64) runtime_source=$root/runtime/platform/linux-x86_64.tn ;;
    *) echo "unsupported host; set TYPENATIVE_RUNTIME_SOURCE" >&2; exit 2 ;;
  esac
fi

llvm_config=${TYPENATIVE_LLVM_CONFIG:-}
if [ -z "$llvm_config" ] && command -v llvm-config >/dev/null 2>&1; then
  llvm_config=$(command -v llvm-config)
fi
if [ -z "$llvm_config" ] && [ -x /opt/homebrew/opt/llvm/bin/llvm-config ]; then
  llvm_config=/opt/homebrew/opt/llvm/bin/llvm-config
fi
llvm_link_arguments=""
if [ -n "$llvm_config" ]; then
  for argument in $($llvm_config --ldflags --libs --system-libs); do
    llvm_link_arguments="$llvm_link_arguments --link-argument=$argument"
  done
fi
if [ -z "$llvm_link_arguments" ]; then
  echo "llvm-config not found; set TYPENATIVE_LLVM_CONFIG" >&2
  exit 2
fi

guarded "$tn_bin" build "$runtime_source" --profile optimized --emit object --out "$runtime_object" $llvm_link_arguments

guarded "$tn_bin" build "$root/validation/node/exports.tn" --emit node-addon --out "$work/exports.node" $llvm_link_arguments
guarded "$tn_bin" build "$root/validation/node/classes.tn" --emit node-addon --out "$work/classes.node" $llvm_link_arguments
guarded "$tn_bin" build "$root/validation/node/classes-fallible.tn" --emit node-addon --out "$work/classes-fallible.node" $llvm_link_arguments
guarded "$tn_bin" build "$root/validation/node/async.tn" --emit node-addon --out "$work/async.node" $llvm_link_arguments

for declaration in "$work/exports.d.ts" "$work/classes.d.ts" "$work/classes-fallible.d.ts" "$work/async.d.ts"; do
  [ -s "$declaration" ] || { echo "missing Node declaration $declaration" >&2; exit 1; }
done

node --expose-gc "$root/validation/node/check.mjs" \
  "$work/exports.node" "$work/classes.node" "$work/classes-fallible.node" "$work/async.node"

for addon in "$work/exports.node" "$work/classes.node" "$work/classes-fallible.node" "$work/async.node"; do
  if nm -u "$addon" | grep -E 'tn_runtime_(alloc|free)(\.|$)' >/dev/null; then
    echo "Node addon has unresolved TypeNative allocator symbols: $addon" >&2
    exit 1
  fi
  if nm -u "$addon" | grep -E '(^|[[:space:]])(_?v8|_?uv_|_?node::)' >/dev/null; then
    echo "Node addon imports V8, libuv, or C++ Node internals: $addon" >&2
    exit 1
  fi
done
