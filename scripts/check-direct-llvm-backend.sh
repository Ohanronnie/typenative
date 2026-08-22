#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
artifact_root=${1:-}

fail_source_scan() {
  pattern=$1
  if rg -n --glob '*.tn' "$pattern" "$root/compiler-tn"; then
    echo "direct LLVM regression scan matched forbidden source: $pattern" >&2
    exit 1
  fi
  if rg -n "$pattern" "$root/scripts/bootstrap-self-host.sh"; then
    echo "direct LLVM regression scan matched forbidden bootstrap path: $pattern" >&2
    exit 1
  fi
}

fail_source_scan 'generic_codegen'
fail_source_scan 'materializeSource'
fail_source_scan 'normalizeCompoundAssignments'
fail_source_scan 'TYPENATIVE_C_SOURCE_DUMP'
fail_source_scan '\.tn\.c'
fail_source_scan 'clang[^[:cntrl:]]*(\.c| -c |--language=c|-std=gnu11)'
fail_source_scan 'function [A-Za-z0-9_]*(CStatement|CExpression|CType|CRenderer|renderC|emitC)'

if [ -n "$artifact_root" ]; then
  generated=$(find "$artifact_root" -type f \( -name '*.tn.c' -o -name '*.tn.debug.c' \) -print -quit)
  if [ -n "$generated" ]; then
    echo "direct LLVM regression scan found generated C source: $generated" >&2
    exit 1
  fi
fi

printf '%s\n' 'direct-llvm-regression-scan=pass'
