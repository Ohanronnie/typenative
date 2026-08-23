#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
forbidden='compiler-tn|bootstrap-self-host|fixed-point|self-host differential|self-hosted fuzz'
scan_roots='scripts crates runtime std validation tests benchmarks'
scan_log=$(mktemp "${TMPDIR:-/tmp}/typenative-boundary.XXXXXX")
trap 'rm -f "$scan_log"' EXIT

matches=$(rg -n "$forbidden" $scan_roots \
  --glob '!scripts/bootstrap-self-host.sh' \
  --glob '!scripts/verify-selfhost-freeze.sh' \
  --glob '!scripts/check-direct-llvm-backend.sh' \
  --glob '!scripts/check-foreign-syntax.sh' \
  --glob '!scripts/measure-compiler-check.sh' \
  --glob '!scripts/verify-active-boundary.sh' \
  --glob '!benchmarks/json-parser/results.json' || true)
if [ -n "$matches" ]; then
  printf '%s\n' "$matches" >&2
  echo 'active-boundary=fail frozen self-host references found in active paths' >&2
  exit 1
fi

if rg -n '(^|[[:space:]])tn[[:space:]]+(fmt|check|build|test|run)' scripts \
  --glob '!scripts/tn-guarded.sh' \
  --glob '!scripts/verify-active-boundary.sh' \
  --glob '!scripts/verify-selfhost-freeze.sh' \
  --glob '!scripts/check-direct-llvm-backend.sh' \
  --glob '!scripts/check-foreign-syntax.sh' \
  --glob '!scripts/measure-compiler-check.sh' \
  --glob '!scripts/bootstrap-self-host.sh' >"$scan_log" 2>/dev/null; then
  echo 'active-boundary=fail unguarded TypeNative compiler invocation found' >&2
  cat "$scan_log" >&2
  exit 1
fi

printf '%s\n' 'active-boundary=pass frozen sources excluded from active command paths'
