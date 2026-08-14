#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export LC_ALL=C
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
if [ -n "${TN_BIN:-}" ]; then
  tn=$TN_BIN
elif [ -x "$target_dir/release/tn" ]; then
  tn=$target_dir/release/tn
elif command -v tn >/dev/null 2>&1; then
  tn=$(command -v tn)
else
  tn=$target_dir/debug/tn
fi

guarded() {
  started=$(date +%s)
  if perl -e 'alarm 175; exec @ARGV' -- "$@"; then
    status=0
  else
    status=$?
  fi
  finished=$(date +%s)
  elapsed=$((finished - started))
  if [ -n "${run:-}" ]; then
    {
      printf '%s\t%s\t%s' "$started" "$finished" "$status"
      for argument in "$@"; do
        printf '\t%s' "$argument"
      done
      printf '\n'
    } >>"$run/command-trace.tsv"
  fi
  if [ "$elapsed" -ge 180 ]; then
    echo "TypeNative compiler command exceeded the 180-second budget: $*" >&2
    return 124
  fi
  return "$status"
}

out=${TYPENATIVE_BOOTSTRAP_DIR:-$root/build/bootstrap}
mkdir -p "$out"
run="$out/run-$(date +%s)-$$"
mkdir -p "$run"
printf 'started_epoch\tfinished_epoch\texit_status\tcommand\targuments...\n' >"$run/command-trace.tsv"

if [ -n "${TYPENATIVE_RUNTIME_SOURCE:-}" ]; then
  runtime_source=$TYPENATIVE_RUNTIME_SOURCE
else
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) runtime_source=$root/runtime/platform/darwin-arm64.tn ;;
    Linux:x86_64) runtime_source=$root/runtime/platform/linux-x86_64.tn ;;
    *)
      echo "unsupported host; set TYPENATIVE_RUNTIME_SOURCE" >&2
      exit 2
      ;;
  esac
fi
runtime_object="$run/runtime.o"

build_compiler() {
  name=$1
  guarded "$tn" build "$root/compiler-tn/main.tn" --profile optimized --out "$run/$name"
  guarded "$tn" build "$runtime_source" --profile optimized --emit object --out "$runtime_object"
  export TYPENATIVE_RUNTIME_OBJECT="$runtime_object"
}

rewrite_sources() {
  compiler=$1
  source_dir=$2
  output_dir=$3
  mkdir -p "$output_dir"
  for source in "$source_dir"/*.tn; do
    name=${source##*/}
    guarded "$compiler" "$source" "$output_dir/$name"
  done
}

check_frontend() {
  compiler=$1
  printf '%s\n' 'function main(): void {}' >"$run/frontend-valid.tn"
  guarded "$compiler" "$run/frontend-valid.tn" "$run/frontend-valid.out.tn"
  [ -s "$run/frontend-valid.out.tn" ]
  cmp "$run/frontend-valid.tn" "$run/frontend-valid.out.tn"
  printf '%s\n' 'self-hosted-llvm-c-api=pass'
  guarded "$compiler" check "$run/frontend-valid.tn"
  guarded "$compiler" fmt --check "$run/frontend-valid.tn"
  guarded "$compiler" check "$root/compiler-tn"
  guarded "$compiler" fmt --check "$root/std"
  for source in "$root/tests/semantics/pass"/*.tn; do
    guarded "$compiler" check "$source"
  done
  for source in "$root/tests/semantics/fail"/*.tn; do
    [ "${source##*/}" = "resolution-helper.tn" ] && continue
    set +e
    guarded "$compiler" check "$source" >/dev/null 2>"$run/semantic-fail.err"
    status=$?
    set -e
    [ "$status" -ne 0 ]
  done
  printf '%s\n' 'self-hosted-semantic-corpus=pass'
  printf '%s\n' 'function main(): void {' >"$run/frontend-invalid.tn"
  set +e
  guarded "$compiler" "$run/frontend-invalid.tn" "$run/frontend-invalid.out.tn"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  [ ! -e "$run/frontend-invalid.out.tn" ]
  set +e
  guarded "$compiler" check --json "$run/frontend-invalid.tn" >"$run/frontend-invalid.json"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  rg -q 'SYNTAX_SELFHOST' "$run/frontend-invalid.json"
  printf '%s\n' 'function duplicate(): void {} function duplicate(): void {}' >"$run/frontend-duplicate.tn"
  set +e
  guarded "$compiler" "$run/frontend-duplicate.tn" "$run/frontend-duplicate.out.tn"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  [ ! -e "$run/frontend-duplicate.out.tn" ]
  printf '%s\n' 'self-hosted-hir-declarations=pass'
  printf '%s\n' 'function borrowed(value: i32): void { if (true) { const shared = &value; shared; } }' >"$run/mir-borrow-valid.tn"
  guarded "$compiler" check "$run/mir-borrow-valid.tn"
  printf '%s\n' 'function moved(value: string): void { const movedValue = move value; movedValue; }' >"$run/mir-move-valid.tn"
  guarded "$compiler" check "$run/mir-move-valid.tn"
  printf '%s\n' 'async function waiting(value: Promise<i32, never>): Promise<i32, never> { return await value; }' >"$run/mir-await-valid.tn"
  guarded "$compiler" check "$run/mir-await-valid.tn"
  printf '%s\n' 'function conflict(value: string): void { const movedValue = move value; value; }' >"$run/mir-ownership-invalid.tn"
  set +e
  guarded "$compiler" check "$run/mir-ownership-invalid.tn" >/dev/null 2>"$run/mir-ownership-invalid.err"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  printf '%s\n' 'self-hosted-mir-borrow-move-await=pass'
  printf '%s\n' 'export function answer(): i32 { return 7i32; } function main(): i32 { return 7i32; }' >"$run/frontend-build.tn"
  guarded "$compiler" build "$run/frontend-build.tn" --emit llvm-ir --out "$run/frontend-build.ll"
  rg -q 'define i32 @tn_user_main' "$run/frontend-build.ll"
  rg -q 'ret i32 7' "$run/frontend-build.ll"
  printf '%s\n' 'function answer(value: i32): i32 { return value + 5i32 + 2i32; } function main(): i32 { return answer(2i32 + 3i32); }' >"$run/frontend-helper.tn"
  guarded "$compiler" build "$run/frontend-helper.tn" --emit llvm-ir --out "$run/frontend-helper.ll"
  rg -q 'define i32' "$run/frontend-helper.ll"
  set +e
  guarded "$compiler" run "$run/frontend-helper.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 12 ]
  guarded "$compiler" build "$run/frontend-build.tn" --emit bitcode --out "$run/frontend-build.bc"
  guarded "$compiler" build "$run/frontend-build.tn" --emit assembly --out "$run/frontend-build.s"
  guarded "$compiler" build "$run/frontend-build.tn" --emit object --out "$run/frontend-build.o"
  [ -s "$run/frontend-build.bc" ]
  [ -s "$run/frontend-build.s" ]
  [ -s "$run/frontend-build.o" ]
  guarded "$compiler" build "$run/frontend-build.tn" --emit executable --out "$run/frontend-build"
  set +e
  "$run/frontend-build" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  guarded "$compiler" build "$run/frontend-build.tn" --out "$run/frontend-build-default"
  set +e
  "$run/frontend-build-default" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  guarded "$compiler" build "$run/frontend-build.tn" --emit shared-library --out "$run/frontend-build.dylib"
  [ -s "$run/frontend-build.dylib" ]
  printf '%s\n' 'self-hosted-native-link-products=pass'
  guarded "$compiler" build "$run/frontend-build.tn" --emit node-addon --out "$run/frontend-build.node"
  [ -s "$run/frontend-build.node" ]
  [ -s "$run/frontend-build.d.ts" ]
  node -e 'const addon = require(process.argv[1]); if (addon.main() !== 7) process.exit(1);' "$run/frontend-build.node"
  printf '%s\n' 'self-hosted-node-addon=pass'
  set +e
  guarded "$compiler" run "$run/frontend-build.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  printf '%s\n' 'function main(): i32 { return 0x20i32 + 0b10i32 + 0o1i32; }' >"$run/frontend-radix.tn"
  guarded "$compiler" build "$run/frontend-radix.tn" --emit llvm-ir --out "$run/frontend-radix.ll"
  rg -q 'ret i32 35' "$run/frontend-radix.ll"
  set +e
  guarded "$compiler" run "$run/frontend-radix.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 35 ]
  printf '%s\n' 'self-hosted-radix-literals=pass'
  printf '%s\n' 'function main(): i32 { return 1foo; }' >"$run/frontend-radix-invalid.tn"
  set +e
  guarded "$compiler" run "$run/frontend-radix-invalid.tn" >/dev/null 2>"$run/frontend-radix-invalid.err"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  printf '%s\n' 'function main(): void {}' >"$run/frontend-void.tn"
  guarded "$compiler" build "$run/frontend-void.tn" --emit llvm-ir --out "$run/frontend-void.ll"
  rg -q 'define void @tn_user_main' "$run/frontend-void.ll"
  guarded "$compiler" build "$run/frontend-void.tn" --emit bitcode --out "$run/frontend-void.bc"
  guarded "$compiler" build "$run/frontend-void.tn" --emit assembly --out "$run/frontend-void.s"
  guarded "$compiler" build "$run/frontend-void.tn" --emit object --out "$run/frontend-void.o"
  [ -s "$run/frontend-void.bc" ]
  [ -s "$run/frontend-void.s" ]
  [ -s "$run/frontend-void.o" ]
  set +e
  guarded "$compiler" run "$run/frontend-void.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 0 ]
  printf '%s\n' 'function helper(): void {} function main(): void { helper(); }' >"$run/frontend-void-call.tn"
  guarded "$compiler" build "$run/frontend-void-call.tn" --emit llvm-ir --out "$run/frontend-void-call.ll"
  rg -q 'call void @tn_m0_helper' "$run/frontend-void-call.ll"
  guarded "$compiler" build "$run/frontend-void-call.tn" --emit executable --out "$run/frontend-void-call"
  set +e
  "$run/frontend-void-call" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 0 ]
  printf '%s\n' 'function consume(value: i32): void {} function main(): void { consume(42i32); }' >"$run/frontend-parameterized-void-call.tn"
  guarded "$compiler" build "$run/frontend-parameterized-void-call.tn" --emit llvm-ir --out "$run/frontend-parameterized-void-call.ll"
  rg -Fq 'define void @tn_m0_consume(i32' "$run/frontend-parameterized-void-call.ll"
  rg -q 'call void @tn_m0_consume\(i32 .*42\)' "$run/frontend-parameterized-void-call.ll"
  guarded "$compiler" build "$run/frontend-parameterized-void-call.tn" --emit executable --out "$run/frontend-parameterized-void-call"
  set +e
  "$run/frontend-parameterized-void-call" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 0 ]
  printf '%s\n' 'self-hosted-parameterized-void-call=pass'
  printf '%s\n' 'import { argumentCount } from "std/process"; function countArguments(): i32 { return argumentCount(); } function main(): i32 { return countArguments() + 6i32; }' >"$run/frontend-cli.tn"
  guarded "$compiler" build "$run/frontend-cli.tn" --emit llvm-ir --out "$run/frontend-cli.ll"
  rg -q 'call i32 @tn_process_argc' "$run/frontend-cli.ll"
  set +e
  guarded "$compiler" run "$run/frontend-cli.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  set +e
  guarded "$compiler" run "$run/frontend-cli.tn" -- >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  set +e
  guarded "$compiler" run "$run/frontend-cli.tn" -- extra >/dev/null
  status=$?
  set -e
  [ "$status" -eq 8 ]
  printf '%s\n' 'import { argumentCount } from "std/process"; function main(): i32 { return argumentCount() !== 0i32 ? 7i32 : 9i32; }' >"$run/frontend-conditional.tn"
  guarded "$compiler" build "$run/frontend-conditional.tn" --emit llvm-ir --out "$run/frontend-conditional.ll"
  rg -q 'select i1' "$run/frontend-conditional.ll"
  set +e
  guarded "$compiler" run "$run/frontend-conditional.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  printf '%s\n' 'function main(): i32 { if (true) { return 7i32; } else { return 9i32; } }' >"$run/frontend-if-return.tn"
  guarded "$compiler" build "$run/frontend-if-return.tn" --emit llvm-ir --out "$run/frontend-if-return.ll"
  rg -q 'ret i32 7' "$run/frontend-if-return.ll"
  set +e
  guarded "$compiler" run "$run/frontend-if-return.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  printf '%s\n' 'import { argumentCount } from "std/process"; function main(): i32 { if (argumentCount() !== 0i32) { return 7i32; } else { return 9i32; } }' >"$run/frontend-if-conditional.tn"
  guarded "$compiler" build "$run/frontend-if-conditional.tn" --emit llvm-ir --out "$run/frontend-if-conditional.ll"
  rg -q 'br i1' "$run/frontend-if-conditional.ll"
  set +e
  guarded "$compiler" run "$run/frontend-if-conditional.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 7 ]
  printf '%s\n' 'self-hosted-conditional-selection=pass'
  printf '%s\n' 'function main(): i32 { return 1i32 / 0i32; }' >"$run/frontend-divzero.tn"
  set +e
  guarded "$compiler" run "$run/frontend-divzero.tn" >/dev/null
  status=$?
  set -e
  [ "$status" -eq 1 ]
  printf '%s\n' 'function main(): i32 { return 1i32 + unknown; }' >"$run/frontend-unsupported.tn"
  set +e
  guarded "$compiler" run "$run/frontend-unsupported.tn" >/dev/null 2>"$run/frontend-unsupported.err"
  status=$?
  set -e
  [ "$status" -ne 0 ]
  guarded "$compiler" doc "$run/frontend-build.tn" --out "$run/frontend-api.md"
  rg -q '^# TypeNative API$' "$run/frontend-api.md"
  python3 - "$compiler" <<'PY'
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
    return json.loads(process.stdout.read(int(headers["content-length"])))

send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
assert receive()["id"] == 1
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
send({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
assert receive()["id"] == 2
send({"jsonrpc": "2.0", "method": "exit", "params": None})
process.stdin.close()
assert process.wait(timeout=5) == 0
PY
  printf '%s\n' 'self-hosted-cli-extended=pass'
  printf '%s\n' 'self-hosted-frontend=pass'
  printf '%s\n' 'self-hosted-cli=pass'
  printf '%s\n' 'self-hosted-semantic-duplicate=pass'
}

check_syntax_differential() {
  compiler=$1
  valid_output="$run/syntax-valid"
  invalid_output="$run/syntax-invalid"
  mkdir -p "$valid_output" "$invalid_output"
  for source in "$root/tests/syntax/valid"/*.tn; do
    name=${source##*/}
    rust_output=$(
      guarded "$tn" check --json "$source" 2>&1 || true
    )
    if printf '%s\n' "$rust_output" | rg -q '"condition":"SYNTAX_'; then
      echo "Rust parser rejected valid syntax fixture: $source" >&2
      return 1
    fi
    guarded "$compiler" "$source" "$valid_output/$name.out"
    [ -s "$valid_output/$name.out" ]
  done
  for source in "$root/tests/syntax/invalid"/*.tn; do
    name=${source##*/}
    rust_output=$(
      guarded "$tn" check --json "$source" 2>&1 || true
    )
    if ! printf '%s\n' "$rust_output" | rg -q '"condition":"SYNTAX_'; then
      echo "Rust parser accepted invalid syntax fixture: $source" >&2
      return 1
    fi
    set +e
    guarded "$compiler" "$source" "$invalid_output/$name.out"
    self_host_status=$?
    set -e
    [ "$self_host_status" -ne 0 ]
    [ ! -e "$invalid_output/$name.out" ]
  done
  numeric_source="$root/tests/syntax/invalid/numeric-literal.tn"
  rust_numeric=$(
    guarded "$tn" check --json "$numeric_source" 2>&1 || true
  )
  self_numeric=$(
    guarded "$compiler" check --json "$numeric_source" 2>&1 || true
  )
  printf '%s\n' "$rust_numeric" | rg -q '"condition":"SYNTAX_INVALID_INTEGER_SUFFIX"'
  printf '%s\n' "$self_numeric" | rg -q '"condition":"SYNTAX_INVALID_INTEGER_SUFFIX"'
  printf '%s\n' "$rust_numeric" | rg -q '"byte_start":31,"byte_end":34'
  printf '%s\n' "$self_numeric" | rg -q '"byte_start":31,"byte_end":34'
  printf '%s\n' 'self-hosted-diagnostic-records=pass'
  printf '%s\n' 'self-hosted-syntax-differential=pass'
}

build_compiler compiler-a
check_frontend "$run/compiler-a"
check_syntax_differential "$run/compiler-a"
rewrite_sources "$run/compiler-a" "$root/compiler-tn" "$run/compiler-src"
if ! guarded "$run/compiler-a" build "$run/compiler-src/main.tn" --profile optimized --timings --out "$run/compiler-b"; then
  echo "independent self-hosting failed: compiler A could not build compiler B" >&2
  exit 1
fi
guarded "$run/compiler-b" >/dev/null
check_frontend "$run/compiler-b"
rewrite_sources "$run/compiler-b" "$run/compiler-src" "$run/compiler-src"
if ! guarded "$run/compiler-b" build "$run/compiler-src/main.tn" --profile optimized --timings --out "$run/compiler-c"; then
  echo "independent self-hosting failed: compiler B could not build compiler C" >&2
  exit 1
fi
guarded "$run/compiler-c" >/dev/null
check_frontend "$run/compiler-c"
rewrite_sources "$run/compiler-c" "$run/compiler-src" "$run/compiler-src"
if ! guarded "$run/compiler-c" build "$run/compiler-src/main.tn" --profile optimized --timings --out "$run/compiler-d"; then
  echo "independent self-hosting failed: compiler C could not build compiler D" >&2
  exit 1
fi
guarded "$run/compiler-d" >/dev/null
check_frontend "$run/compiler-d"

artifact_digest() {
  python3 - "$1" <<'PY'
import hashlib
import struct
import sys

data = open(sys.argv[1], "rb").read()
digest = hashlib.sha256()


def add_section(name, size, payload):
    digest.update(struct.pack("<Q", len(name)))
    digest.update(name)
    digest.update(struct.pack("<Q", size))
    digest.update(payload)


if data[:4] == b"\xcf\xfa\xed\xfe" and len(data) >= 32:
    add_section(b"mach-o-header", 32, data[:32])
    command_count = struct.unpack_from("<I", data, 16)[0]
    offset = 32
    for _ in range(command_count):
        if offset + 8 > len(data):
            break
        command, command_size = struct.unpack_from("<II", data, offset)
        if command_size < 8 or offset + command_size > len(data):
            break
        if command == 0x19 and command_size >= 72:
            segment_name = data[offset + 8 : offset + 24].split(b"\0", 1)[0]
            section_count = struct.unpack_from("<I", data, offset + 64)[0]
            section_offset = offset + 72
            for _ in range(section_count):
                if section_offset + 80 > offset + command_size:
                    break
                section_name = data[section_offset : section_offset + 16].split(b"\0", 1)[0]
                section_size = struct.unpack_from("<Q", data, section_offset + 40)[0]
                file_offset = struct.unpack_from("<I", data, section_offset + 48)[0]
                payload = b""
                if file_offset != 0 and file_offset + section_size <= len(data):
                    payload = data[file_offset : file_offset + section_size]
                if segment_name != b"__LINKEDIT":
                    add_section(segment_name + b"/" + section_name, section_size, payload)
                section_offset += 80
        offset += command_size
elif data[:4] == b"\x7fELF" and len(data) >= 64 and data[4] == 2 and data[5] == 1:
    add_section(b"elf-header", 64, data[:64])
    section_table_offset = struct.unpack_from("<Q", data, 40)[0]
    section_entry_size = struct.unpack_from("<H", data, 58)[0]
    section_count = struct.unpack_from("<H", data, 60)[0]
    string_table_index = struct.unpack_from("<H", data, 62)[0]
    if section_entry_size >= 64 and section_table_offset + section_entry_size * section_count <= len(data):
        string_table = b""
        string_table_offset = section_table_offset + section_entry_size * string_table_index
        if string_table_index < section_count:
            string_table_size = struct.unpack_from("<Q", data, string_table_offset + 32)[0]
            string_table_file_offset = struct.unpack_from("<Q", data, string_table_offset + 24)[0]
            if string_table_file_offset + string_table_size <= len(data):
                string_table = data[string_table_file_offset : string_table_file_offset + string_table_size]
        for index in range(section_count):
            section_offset = section_table_offset + section_entry_size * index
            name_offset = struct.unpack_from("<I", data, section_offset)[0]
            section_type = struct.unpack_from("<I", data, section_offset + 4)[0]
            flags = struct.unpack_from("<Q", data, section_offset + 8)[0]
            file_offset = struct.unpack_from("<Q", data, section_offset + 24)[0]
            section_size = struct.unpack_from("<Q", data, section_offset + 32)[0]
            name_end = string_table.find(b"\0", name_offset)
            section_name = string_table[name_offset:name_end] if name_end >= 0 else b""
            payload = b""
            if section_type != 8 and file_offset + section_size <= len(data):
                payload = data[file_offset : file_offset + section_size]
            if flags & 2 and not section_name.startswith(b".debug"):
                add_section(section_name, section_size, payload)
else:
    digest.update(data)

print(digest.hexdigest())
PY
}

sha_a=$(artifact_digest "$run/compiler-a")
sha_b=$(artifact_digest "$run/compiler-b")
sha_c=$(artifact_digest "$run/compiler-c")
sha_d=$(artifact_digest "$run/compiler-d")
guarded "$run/compiler-a" build "$run/compiler-src/main.tn" --profile optimized --out "$run/compiler-b-repeat"
sha_b_repeat=$(artifact_digest "$run/compiler-b-repeat")
[ "$sha_b" = "$sha_c" ]
[ "$sha_c" = "$sha_d" ]
[ "$sha_b" = "$sha_b_repeat" ]
source_c=$(cd "$run/compiler-src" && find . -type f -name '*.tn' -print0 | sort -z | xargs -0 shasum -a 256 | shasum -a 256 | awk '{print $1}')
source_manifest="$run/source-manifest.txt"
(
  cd "$root"
  find compiler-tn runtime std tests/syntax tests/semantics -type f -name '*.tn' -print |
    sort |
    while IFS= read -r source; do
      shasum -a 256 "$source"
    done
) >"$source_manifest"
manifest="$run/bootstrap-manifest.txt"
{
  printf '%s\n' 'format=gate11-bootstrap-evidence'
  printf '%s\n' "host=$(uname -s)-$(uname -m)"
  printf '%s\n' "compiler-driver=$tn"
  printf '%s\n' "compiler-driver-sha256=$(shasum -a 256 "$tn" | awk '{print $1}')"
  printf '%s\n' "clang=$(clang --version | head -1)"
  printf '%s\n' "node=$(node --version)"
  printf '%s\n' "runtime-source=$runtime_source"
  printf '%s\n' "runtime-source-sha256=$(shasum -a 256 "$runtime_source" | awk '{print $1}')"
  printf '%s\n' "runtime-object=$runtime_object"
  printf '%s\n' "runtime-object-sha256=$(artifact_digest "$runtime_object")"
  printf '%s\n' "source-manifest=$source_manifest"
  printf '%s\n' "source-fixed-point=$source_c"
  printf '%s\n' "compiler-a-sha256=$sha_a"
  printf '%s\n' "compiler-b-sha256=$sha_b"
  printf '%s\n' "compiler-c-sha256=$sha_c"
  printf '%s\n' "compiler-d-sha256=$sha_d"
  printf '%s\n' "compiler-b-repeat-sha256=$sha_b_repeat"
  printf '%s\n' 'fixed-point=compiler-b=compiler-c=compiler-d=compiler-b-repeat'
  printf '%s\n' 'discovery-order=LC_ALL=C sorted source paths'
  printf '%s\n' 'hash-seed=deterministic source and artifact hashing'
  printf '%s\n' "command-trace=$run/command-trace.tsv"
} >"$manifest"
printf '%s\n' "bootstrap-fixed-point=$sha_c"
printf '%s\n' "bootstrap-digest=$sha_d"
printf '%s\n' "bootstrap-repeatable=$sha_b_repeat"
printf '%s\n' "bootstrap-source-fixed-point=$source_c"
printf '%s\n' "bootstrap-manifest=$manifest"
