#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

obsolete=$(
  rg -n -P '^[[:space:]]*extern[[:space:]]+"[^"]+"[[:space:]]*\{' \
    --glob '*.tn' --glob 'README.md' --glob 'docs/*.md' \
    --glob '!compiler-tn/**' --glob '!tests/syntax/invalid/**' . || true
)

if [ -n "$obsolete" ]; then
  printf '%s\n' "$obsolete" >&2
  printf '%s\n' 'obsolete foreign declaration block syntax remains in active sources or documentation' >&2
  exit 1
fi

pointer_types=$(
  rg -n -P 'extern[[:space:]]+"C"[[:space:]]+function[[:space:]]*\(' \
    --glob '*.tn' --glob 'README.md' --glob 'docs/*.md' \
    --glob '!compiler-tn/**' --glob '!tests/syntax/invalid/**' . || true
)

if [ -z "$pointer_types" ]; then
  printf '%s\n' 'foreign-function-pointer-syntax=missing' >&2
  exit 1
fi

printf '%s\n' 'foreign-declaration-block-scan=pass'
printf '%s\n' 'foreign-function-pointer-syntax=allowed'
