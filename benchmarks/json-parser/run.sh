#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
benchmark="$root/benchmarks/json-parser"

cargo build --release -p tn-cli
target_dir=$(cargo metadata --no-deps --format-version 1 | node -p 'JSON.parse(require("node:fs").readFileSync(0, "utf8")).target_directory')
tn_bin=${TN_BIN:-"$target_dir/release/tn"}
mkdir -p "$benchmark/build"
guard="$root/scripts/tn-guarded.sh"
tn() {
  TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$tn_bin" "$@"
}
tn fmt "$benchmark/parser.tn"
tn build "$benchmark/parser.tn" --profile optimized --emit executable --out "$benchmark/build/json-parser"
tn build "$benchmark/parser.tn" --profile optimized --emit node-addon --out "$benchmark/build/json-parser.node"
node "$benchmark/benchmark.mjs"
