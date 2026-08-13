#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
benchmark="$root/benchmarks/json-parser"

cargo build --release -p tn-cli
target_dir=$(cargo metadata --no-deps --format-version 1 | node -p 'JSON.parse(require("node:fs").readFileSync(0, "utf8")).target_directory')
tn_bin=${TN_BIN:-"$target_dir/release/tn"}
mkdir -p "$benchmark/build"
"$tn_bin" fmt "$benchmark/parser.tn"
"$tn_bin" build "$benchmark/parser.tn" --profile optimized --emit executable --out "$benchmark/build/json-parser"
"$tn_bin" build "$benchmark/parser.tn" --profile optimized --emit node-addon --out "$benchmark/build/json-parser.node"
node "$benchmark/benchmark.mjs"
