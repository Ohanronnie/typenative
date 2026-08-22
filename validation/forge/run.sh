#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
compiler=${TN_BIN:-$root/target/release/tn}
guard="$root/scripts/tn-guarded.sh"
build=${FORGE_BUILD_DIR:-$root/validation/forge/build}

[ -x "$compiler" ] || {
  echo "TypeNative compiler is not executable: $compiler" >&2
  exit 2
}

tn() {
  TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$compiler" "$@"
}

llvm_config=${TYPENATIVE_LLVM_CONFIG:-}
if [ -z "$llvm_config" ] && command -v llvm-config >/dev/null 2>&1; then
  llvm_config=$(command -v llvm-config)
fi
if [ -z "$llvm_config" ] && [ -x /opt/homebrew/opt/llvm/bin/llvm-config ]; then
  llvm_config=/opt/homebrew/opt/llvm/bin/llvm-config
fi
llvm_link_arguments=""
if [ -n "$llvm_config" ]; then
  for argument in $("$llvm_config" --ldflags --libs --system-libs); do
    llvm_link_arguments="$llvm_link_arguments --link-argument=$argument"
  done
fi
[ -n "$llvm_link_arguments" ] || {
  echo "llvm-config not found; set TYPENATIVE_LLVM_CONFIG" >&2
  exit 2
}

tn_build() {
  tn build "$@" $llvm_link_arguments
}

mkdir -p "$build"
if [ -z "${TYPENATIVE_RUNTIME_OBJECT:-}" ]; then
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) runtime_source=$root/runtime/platform/darwin-arm64.tn ;;
    Linux:x86_64) runtime_source=$root/runtime/platform/linux-x86_64.tn ;;
    *) echo "unsupported host; set TYPENATIVE_RUNTIME_OBJECT" >&2; exit 2 ;;
  esac
  env -u TYPENATIVE_RUNTIME_OBJECT TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$compiler" build "$runtime_source" --profile optimized --emit object --out "$build/runtime.o" $llvm_link_arguments
  export TYPENATIVE_RUNTIME_OBJECT="$build/runtime.o"
fi

tn fmt --check std/ffi.tn validation/forge
for source in "$root"/validation/forge/*.tn; do
  tn check "$source"
done

tn check "$root/validation/forge/fixtures/positive-sealed.tn"
tn check "$root/validation/forge/fixtures/negative/sealed-base.tn"
tn check "$root/validation/forge/fixtures/negative/sealed-interface-base.tn"
tn check "$root/validation/forge/fixtures/negative/final-base.tn"
for source in \
  "$root"/validation/forge/fixtures/negative/sealed-child.tn \
  "$root"/validation/forge/fixtures/negative/sealed-interface-child.tn \
  "$root"/validation/forge/fixtures/negative/final-child.tn; do
  set +e
  TYPENATIVE_RUNTIME_ROOT="$root" "$guard" "$compiler" check --json "$source" >"$build/negative.out" 2>"$build/negative.err"
  code=$?
  set -e
  [ "$code" -ne 0 ]
  case "$source" in
    *sealed-child.tn) rg -q 'TYPE_EXTENDS_SEALED_CLASS' "$build/negative.out" ;;
    *sealed-interface-child.tn) rg -q 'TYPE_CONFORMS_TO_SEALED_INTERFACE' "$build/negative.out" ;;
    *final-child.tn) rg -q 'TYPE_EXTENDS_FINAL_CLASS' "$build/negative.out" ;;
  esac
done
node "$root/validation/forge/coverage.mjs" --check

tn_build "$root/validation/forge/main.tn" --profile debug --emit executable --out "$build/forge-debug"
tn_build "$root/validation/forge/main.tn" --profile optimized --emit executable --out "$build/forge-optimized"
tn_build "$root/validation/forge/native.tn" --profile optimized --emit object --out "$build/forge.o"
tn_build "$root/validation/forge/native.tn" --profile optimized --emit llvm-ir --out "$build/forge.ll"
tn_build "$root/validation/forge/native.tn" --profile optimized --emit bitcode --out "$build/forge.bc"
tn_build "$root/validation/forge/native.tn" --profile optimized --emit assembly --out "$build/forge.s"
tn_build "$root/validation/forge/native.tn" --profile optimized --emit shared-library --out "$build/forge.shared"
tn_build "$root/validation/forge/main.tn" --profile optimized --emit node-addon --out "$build/forge.node"

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) plugin=/usr/lib/libSystem.B.dylib ;;
  Linux:x86_64) plugin=/lib/x86_64-linux-gnu/libc.so.6 ;;
  *) echo "unsupported host for Forge FFI probe" >&2; exit 2 ;;
esac

FORGE_PLUGIN_PATH="$plugin" "$build/forge-debug"
FORGE_PLUGIN_PATH="$plugin" "$build/forge-optimized"

FORGE_PLUGIN_PATH="$plugin" node - "$build/forge.node" <<'NODE'
void async function () {
const addon = require(process.argv[2]);
if (addon.forge_health() !== 42 || addon.forge_probe() !== 42 || addon.forge_feature_probe() !== 0) process.exit(1);
for (const name of [
  "forge_model_probe",
  "forge_protocol_probe",
  "forge_scheduler_probe",
  "forge_metrics_probe",
  "forge_plugin_probe",
]) {
  if (addon[name]() !== 0) process.exit(1);
}
if (addon.forge_node_optional(9) !== 9 || addon.forge_node_optional(undefined) !== undefined) process.exit(1);
if (addon.forge_node_string("forge") !== "forge") process.exit(1);
if (addon.forge_node_bytes_length(new Uint8Array([1, 2, 3])) !== 3n) process.exit(1);
if (addon.forge_node_array_length([1, 2, 3, 4]) !== 4n) process.exit(1);
if (addon.forge_node_fixed_middle([10, 42, 30]) !== 42) process.exit(1);
const job = new addon.ForgeJob(5);
if (job.run(7) !== 12) process.exit(1);
if (await addon.forge_node_async(41) !== 42) process.exit(1);
await addon.forge_node_async_fail().then(() => process.exit(1), () => undefined);
}();
NODE

port=$((25000 + ($$ % 1000)))
FORGE_MODE=1 FORGE_PORT="$port" "$build/forge-debug" >"$build/server.out" 2>"$build/server.err" &
server_pid=$!
cleanup() {
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT
python3 - "$port" <<'PY'
import socket
import sys
import time

port = int(sys.argv[1])


def frame(command):
    payload = command.encode()
    return b"*1\r\n$" + str(len(payload)).encode() + b"\r\n" + payload + b"\r\n"


expected = {"PING": b"+PONG\r\n", "BUILD": b"+QUEUED\r\n", "STATUS": b"+READY\r\n"}
for _ in range(100):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5) as sock:
            command = "PING"
            sock.sendall(frame(command))
            if sock.recv(128) != expected[command]:
                raise SystemExit(1)
            break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit(1)

for command in ("BUILD", "STATUS"):
    with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
        sock.sendall(frame(command))
        if sock.recv(128) != expected[command]:
            raise SystemExit(1)
PY

printf '%s\n' 'forge-conformance=pass'
