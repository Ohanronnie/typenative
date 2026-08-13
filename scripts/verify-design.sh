#!/bin/sh
set -eu

workspace=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace"

check_hash() {
  expected=$1
  path=$2
  actual=$(shasum -a 256 "$path" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    echo "DESIGN_SOURCE_HASH_MISMATCH: $path" >&2
    exit 1
  fi
}

check_hash 99db564e1e275fb99b6d7690ea69b474c11febe05655dd42de84df64ccc2895f \
  "/Users/ronnie/Downloads/TypeNative_Expanded_Plan.md"
check_hash 3c877e070611ace639caab967b538e5f19c23ca89b5fa7b064f85e37fb3d2f10 \
  "/Users/ronnie/Downloads/TypeNative_Expanded_Plan (1).md"
check_hash 05110b52ad4100cc4f6569448ce31dc10e3f635e5bc73ac2ab35690ae2ee3c57 \
  "/Users/ronnie/Downloads/TypeNative_Execution_Path.md"

if rg -n '\.tn\.ts|tnconfig\.json|\btype-native\b|assertions disappear|wraps in optimized|safety is opt-in' \
  README.md docs --glob '!docs/design-audit.md'; then
  echo "DESIGN_OBSOLETE_TERMINOLOGY: obsolete claims remain outside the audit" >&2
  exit 1
fi

bunx markdown-link-check README.md
bunx markdown-link-check docs/language-spec.md
bunx markdown-link-check docs/compiler-architecture.md
bunx markdown-link-check docs/implementation-plan.md
bunx markdown-link-check docs/design-audit.md

cargo test -p tn-syntax --test canonical_examples
echo "design_verification=pass"

