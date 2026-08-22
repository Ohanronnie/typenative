#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
compiler=${TYPENATIVE_TN_BIN:-}
if [ "$#" -gt 0 ] && [ "$1" = "$0" ]; then
  shift
fi
if [ -z "$compiler" ]; then
  [ "$#" -gt 0 ] || { echo "TypeNative compiler path is required" >&2; exit 2; }
  compiler=$1
  shift
fi
[ -x "$compiler" ] || { echo "TypeNative compiler is not executable: $compiler" >&2; exit 2; }

if [ -n "${TYPENATIVE_RUNTIME_OBJECT:-}" ]; then
  TYPENATIVE_RUNTIME_ROOT=${TYPENATIVE_RUNTIME_ROOT:-$root} \
    TYPENATIVE_RUNTIME_OBJECT="$TYPENATIVE_RUNTIME_OBJECT" \
    exec perl -e 'alarm 175; exec @ARGV' -- "$compiler" "$@"
fi

TYPENATIVE_RUNTIME_ROOT=${TYPENATIVE_RUNTIME_ROOT:-$root} \
  exec perl -e 'alarm 175; exec @ARGV' -- "$compiler" "$@"
