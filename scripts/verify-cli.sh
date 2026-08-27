#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn=${TN_BIN:-$target_dir/debug/tn}
if [ ! -x "$tn" ]; then
  tn=$(command -v tn || true)
fi
[ -x "$tn" ] || { echo "tn compiler not found; set TN_BIN or build with cargo" >&2; exit 2; }
tn_guard="$root/scripts/tn-guarded.sh"
if [ "$tn" = "$tn_guard" ]; then
  tn=${TYPENATIVE_TN_BIN:-}
fi
[ -x "$tn" ] || { echo "tn compiler is not executable: $tn" >&2; exit 2; }
compiler=$tn
export TYPENATIVE_TN_BIN="$compiler"
tn="$tn_guard"
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-cli.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target=aarch64-apple-darwin ;;
  *) echo "unsupported host for CLI validation" >&2; exit 2 ;;
esac

cat >"$work/main.tn" <<'EOF'
import { argumentCount } from "std/process";

function passes(): void {}
test("passes", () => passes());

export function exported(): i32 {
  return 42i32;
}

function countArguments(): i32 {
  return argumentCount();
}

function main(): i32 {
  return countArguments() + 6i32;
}
EOF
cat >"$work/typenative.json" <<EOF
{
  "entry": "main.tn",
  "outDir": "$work/build",
  "target": "$target",
  "profile": "optimized",
  "emit": "executable",
  "link": { "libraries": [], "searchPaths": [], "arguments": [] }
}
EOF

"$tn" fmt "$work/main.tn"
"$tn" fmt --check "$work/main.tn"
"$tn" check "$work/main.tn"
"$tn" check --json "$work/main.tn" >/dev/null
"$tn" doc "$work/main.tn" --out "$work/api.md"
grep -q '^# TypeNative API$' "$work/api.md"
"$tn" test "$work/main.tn" | grep -q '1 passed; 1 total'

"$tn" build "$work/main.tn" --emit object --out "$work/main.o" >/dev/null
"$tn" build "$work/main.tn" --emit llvm-ir --out "$work/main.ll" >/dev/null
"$tn" build "$work/main.tn" --emit llvm-ir --out "$work/main-timed.ll" --timings 2>"$work/timing.err" >/dev/null
for phase in module-check ownership mir-drop monomorphization llvm-link; do
  rg -q "tn-timing phase=$phase" "$work/timing.err"
done
"$tn" build "$work/main.tn" --emit bitcode --out "$work/main.bc" >/dev/null
"$tn" build "$work/main.tn" --emit assembly --out "$work/main.s" >/dev/null
"$tn" build "$work/main.tn" --emit shared-library --out "$work/main.dylib" >/dev/null
"$tn" build "$work/main.tn" --emit executable --out "$work/main" >/dev/null
set +e
"$work/main" >/dev/null
status=$?
set -e
[ "$status" -eq 7 ]
set +e
"$tn" run "$work/main.tn" -- >/dev/null
status=$?
set -e
[ "$status" -eq 7 ]
"$tn" build "$work" --out "$work/configured" >/dev/null

python3 - "$tn" <<'PY'
import json
import subprocess
import sys

process = subprocess.Popen([sys.argv[1], "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)

def send(message):
    payload = json.dumps(message, separators=(",", ":")).encode()
    process.stdin.write(b"Content-Length: " + str(len(payload)).encode() + b"\r\n\r\n" + payload)
    process.stdin.flush()

def receive():
    headers = {}
    while True:
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("language server closed before a response")
        line = line.strip()
        if not line:
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    length = int(headers["content-length"])
    return json.loads(process.stdout.read(length))

send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
assert receive()["id"] == 1
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
send({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
assert receive()["id"] == 2
send({"jsonrpc": "2.0", "method": "exit", "params": None})
process.stdin.close()
assert process.wait(timeout=5) == 0
PY

printf '%s\n' 'tn-cli-surface=pass'
