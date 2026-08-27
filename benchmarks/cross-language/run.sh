#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
directory=$(mktemp -d "${TMPDIR:-/tmp}/typenative-cross-language.XXXXXX")
trap 'rm -rf "$directory"' EXIT HUP INT TERM

tn_bin=${TN_BIN:-"$root/target/release/tn"}
if [ -n "${TN_BIN:-}" ]; then
  tn_bin=$TN_BIN
elif [ ! -x "$tn_bin" ]; then
  tn_bin="$root/target/debug/tn"
fi
if [ ! -x "$tn_bin" ]; then
  echo "TypeNative compiler not found; set TN_BIN" >&2
  exit 2
fi

"$tn_bin" build "$root/benchmarks/cross-language/workload.tn" --out "$directory/typenative" >/dev/null
cc -O3 -std=c11 "$root/benchmarks/cross-language/workload.c" -o "$directory/c" 2>/dev/null || true
rustc -O "$root/benchmarks/cross-language/workload.rs" -o "$directory/rust" 2>/dev/null || true
if command -v zig >/dev/null 2>&1; then
  zig build-exe -O ReleaseFast "$root/benchmarks/cross-language/workload.zig" --name zig --cache-dir "$directory/zig-cache" --global-cache-dir "$directory/zig-global" -- . 2>/dev/null || true
  [ -x "$directory/zig" ] || true
fi
if command -v go >/dev/null 2>&1; then
  (cd "$root/benchmarks/cross-language" && go build -trimpath -o "$directory/go" workload.go) 2>/dev/null || true
fi

run_product() {
  name=$1
  shift
  if [ "$#" -eq 0 ] || [ ! -x "$1" ] && [ "$name" != "node" ]; then
    printf '%-12s skipped\n' "$name"
    return 0
  fi
  command_path=$1
  shift
  i=0
  while [ "$i" -lt 2 ]; do
    "$command_path" "$@" >/dev/null
    i=$((i + 1))
  done
  samples=
  sample_order=$(python3 -c 'import random, sys; values=list(range(9)); random.Random(sum(map(ord, sys.argv[1]))).shuffle(values); print(" ".join(map(str, values)))' "$name")
  for sample in $sample_order; do
    start=$(python3 -c 'import time; print(time.perf_counter_ns())')
    if output=$("$command_path" "$@"); then
      code=0
    else
      code=$?
    fi
    end=$(python3 -c 'import time; print(time.perf_counter_ns())')
    if [ "$code" -ne 0 ] || [ "$output" != "checksum=899120682" ]; then
      echo "$name failed: status=$code output=$output" >&2
      return 1
    fi
    elapsed=$((end - start))
    samples="$samples $elapsed"
  done
  median=$(python3 -c 'import sys; values=sorted(map(int, sys.argv[1:])); print(values[len(values)//2]/1_000_000)' $samples)
  printf '%-12s median_ms=%s checksum=899120682\n' "$name" "$median"
}

run_product typenative "$directory/typenative"
run_product rust "$directory/rust"
run_product c "$directory/c"
if [ -x "$directory/zig" ]; then run_product zig "$directory/zig"; else printf '%-12s skipped\n' zig; fi
if [ -x "$directory/go" ]; then run_product go "$directory/go"; else printf '%-12s skipped\n' go; fi
run_product node node "$root/benchmarks/cross-language/workload.mjs"
