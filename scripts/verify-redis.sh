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
  cargo_target_dir=$(cargo metadata --no-deps --format-version 1 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  if [ -n "$cargo_target_dir" ] && [ -x "$cargo_target_dir/debug/tn" ]; then
    tn="$cargo_target_dir/debug/tn"
  fi
fi
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

sanitizer=${REDIS_SANITIZER:-}
case "$sanitizer" in
  "") ;;
  address-undefined) ;;
  thread) ;;
  *)
    echo "REDIS_SANITIZER must be empty, address-undefined, or thread" >&2
    exit 2
    ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-redis.XXXXXX")
redis_pid=0
cleanup() {
  if [ "$redis_pid" -ne 0 ]; then
    kill "$redis_pid" 2>/dev/null || true
    wait "$redis_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

port=${REDIS_PORT:-6389}
build_redis() {
  profile=$1
  output=$2
  case "$sanitizer" in
    "")
      "$tn" build "$root/validation/redis/main-alt.tn" --profile "$profile" --out "$output"
      ;;
    address-undefined)
      "$tn" build "$root/validation/redis/main-alt.tn" --profile "$profile" --out "$output" \
        --sanitize address --sanitize undefined
      ;;
    thread)
      "$tn" build "$root/validation/redis/main-alt.tn" --profile "$profile" --out "$output" \
        --sanitize thread
      ;;
  esac
}
build_redis debug "$work/redis-debug"
build_redis optimized "$work/redis-optimized"
if [ -z "$sanitizer" ]; then
  "$tn" build "$root/validation/redis/allocation.tn" --profile optimized \
    --out "$work/redis-allocation"
  "$work/redis-allocation"
  printf '%s\n' 'redis-million-ping-runtime-allocations=0'
  printf '%s\n' 'redis-borrowed-get-runtime-allocations=0'
fi

case "$sanitizer" in
  address-undefined)
    ASAN_OPTIONS=${ASAN_OPTIONS:-detect_leaks=0:abort_on_error=1}
    UBSAN_OPTIONS=${UBSAN_OPTIONS:-halt_on_error=1}
    export ASAN_OPTIONS UBSAN_OPTIONS
    ;;
  thread)
    TSAN_OPTIONS=${TSAN_OPTIONS:-halt_on_error=1}
    export TSAN_OPTIONS
    ;;
esac

run_server() {
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "port $port is already in use" >&2
    return 1
  fi
  for _ in 1 2 3 4 5; do
    "$1" >"$work/server.out" 2>"$work/server.err" &
    redis_pid=$!
    for _ in $(seq 1 100); do
      listener_pid=$(lsof -t -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | head -1 || true)
      if [ "$listener_pid" = "$redis_pid" ] && redis-cli -h 127.0.0.1 -p "$port" ping >/dev/null 2>&1; then
        return 0
      fi
      if ! kill -0 "$redis_pid" 2>/dev/null; then
        break
      fi
      sleep 0.05
    done
    kill "$redis_pid" 2>/dev/null || true
    wait "$redis_pid" 2>/dev/null || true
    redis_pid=0
    sleep 1
  done
  echo "canonical Redis server did not start" >&2
  return 1
}

check_server() {
  python3 - "$port" <<'PY'
import socket
import sys
from concurrent.futures import ThreadPoolExecutor

PORT = int(sys.argv[1])


def frame(*arguments):
    result = [f"*{len(arguments)}\r\n".encode()]
    for argument in arguments:
        value = argument if isinstance(argument, bytes) else str(argument).encode()
        result.extend([f"${len(value)}\r\n".encode(), value, b"\r\n"])
    return b"".join(result)


def read_line(sock):
    result = bytearray()
    while not result.endswith(b"\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise AssertionError("server closed before a complete response")
        result.extend(chunk)
    return bytes(result)


def read_response(sock):
    line = read_line(sock)
    prefix = line[:1]
    if prefix in (b"+", b"-", b":"):
        return line
    if prefix == b"*" and line == b"*0\r\n":
        return line
    if prefix != b"$":
        raise AssertionError(f"unexpected RESP prefix: {line!r}")
    length = int(line[1:-2])
    if length < 0:
        return line
    body = bytearray()
    while len(body) < length + 2:
        chunk = sock.recv(length + 2 - len(body))
        if not chunk:
            raise AssertionError("server closed during a bulk response")
        body.extend(chunk)
    if body[-2:] != b"\r\n":
        raise AssertionError("bulk response lacks CRLF")
    return line + bytes(body)


def command(sock, *arguments):
    sock.sendall(frame(*arguments))
    return read_response(sock)


def assert_closes(payload):
    with socket.create_connection(("127.0.0.1", PORT), timeout=2) as sock:
        sock.settimeout(0.2)
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        for _ in range(20):
            try:
                if not sock.recv(64):
                    return
            except socket.timeout:
                continue
    raise AssertionError("malformed input did not close the client")


with socket.create_connection(("127.0.0.1", PORT), timeout=2) as sock:
    assert command(sock, "PING") == b"+PONG\r\n"
    assert command(sock, "SET", "user", "ronnie") == b"+OK\r\n"
    assert command(sock, "GET", "user") == b"$6\r\nronnie\r\n"
    assert command(sock, "DEL", "user") == b":1\r\n"
    assert command(sock, "GET", "user") == b"$-1\r\n"
    assert command(sock, "UNKNOWN") == b"-ERR unknown command\r\n"
    assert command(sock, "PING", "hello") == b"$5\r\nhello\r\n"
    assert command(sock, "ECHO", "echo") == b"$4\r\necho\r\n"
    assert command(sock, "SET", "counter", "1") == b"+OK\r\n"
    assert command(sock, "INCR", "counter") == b":2\r\n"
    assert command(sock, "EXISTS", "counter") == b":1\r\n"
    assert command(sock, "EXPIRE", "counter", "2") == b":1\r\n"
    ttl = command(sock, "TTL", "counter")
    assert ttl.startswith(b":") and int(ttl[1:-2]) >= 0
    assert command(sock, "COMMAND") == b"*0\r\n"

    pipelined = frame("SET", "pipeline", "ok") + frame("GET", "pipeline")
    sock.sendall(pipelined)
    assert read_response(sock) == b"+OK\r\n"
    assert read_response(sock) == b"$2\r\nok\r\n"

    ordered = b"".join(
        frame("SET", f"batch-{index}", index)
        + frame("GET", f"batch-{index}")
        + frame("DEL", f"batch-{index}")
        for index in range(256)
    )
    sock.sendall(ordered)
    for index in range(256):
        assert read_response(sock) == b"+OK\r\n"
        value = str(index).encode()
        assert read_response(sock) == b"$" + str(len(value)).encode() + b"\r\n" + value + b"\r\n"
        assert read_response(sock) == b":1\r\n"

    fragmented = frame("PING")
    for byte in fragmented:
        sock.send(bytes([byte]))
    assert read_response(sock) == b"+PONG\r\n"

    large_value = b"x" * 12000
    assert command(sock, "SET", "large", large_value) == b"+OK\r\n"
    assert command(sock, "GET", "large") == b"$12000\r\n" + large_value + b"\r\n"



assert_closes(b"*1\r\n$1\r\n\xff\r\n")
assert_closes(b"*1\r\n$536870913\r\n")
assert_closes(b"*1\r\n$4\r\nPI")


def concurrent_round(index):
    key = f"concurrent-{index}"
    with socket.create_connection(("127.0.0.1", PORT), timeout=2) as sock:
        assert command(sock, "SET", key, index) == b"+OK\r\n"
        assert command(sock, "GET", key) == f"${len(str(index))}\r\n{index}\r\n".encode()
        assert command(sock, "DEL", key) == b":1\r\n"


with ThreadPoolExecutor(max_workers=16) as executor:
    list(executor.map(concurrent_round, range(32)))

with socket.create_connection(("127.0.0.1", PORT), timeout=2) as sock:
    assert command(sock, "QUIT") == b"+OK\r\n"
PY
}

check_memory_and_benchmarks() {
  python3 - "$port" "$redis_pid" <<'PY'
import os
import random
import socket
import subprocess
import sys
import time

PORT = int(sys.argv[1])
PID = int(sys.argv[2])
PING = b"*1\r\n$4\r\nPING\r\n"


class RespReader:
    def __init__(self, sock):
        self.sock = sock
        self.buffer = bytearray()

    def fill(self, size=1):
        while len(self.buffer) < size:
            chunk = self.sock.recv(max(65536, size - len(self.buffer)))
            if not chunk:
                raise AssertionError("server closed before a complete response")
            self.buffer.extend(chunk)

    def line(self):
        while True:
            marker = self.buffer.find(b"\r\n")
            if marker >= 0:
                result = bytes(self.buffer[: marker + 2])
                del self.buffer[: marker + 2]
                return result
            self.fill(len(self.buffer) + 1)

    def response(self):
        line = self.line()
        if line[:1] in (b"+", b"-", b":"):
            return line
        if line[:1] != b"$":
            raise AssertionError(f"unexpected RESP prefix: {line!r}")
        length = int(line[1:-2])
        if length < 0:
            return line
        self.fill(length + 2)
        body = bytes(self.buffer[: length + 2])
        del self.buffer[: length + 2]
        if body[-2:] != b"\r\n":
            raise AssertionError("bulk response lacks CRLF")
        return line + body


def frame(*arguments):
    result = [f"*{len(arguments)}\r\n".encode()]
    for argument in arguments:
        value = argument if isinstance(argument, bytes) else str(argument).encode()
        result.extend([f"${len(value)}\r\n".encode(), value, b"\r\n"])
    return b"".join(result)


def rss_kib():
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(PID)],
        capture_output=True,
        text=True,
        check=False,
    )
    value = result.stdout.strip()
    if not value:
        raise AssertionError("could not sample Redis RSS")
    return int(value)


with socket.create_connection(("127.0.0.1", PORT), timeout=10) as sock:
    sock.settimeout(10)
    reader = RespReader(sock)
    for _ in range(1000):
        sock.sendall(PING)
        if reader.response() != b"+PONG\r\n":
            raise AssertionError("PING warmup failed")

    warmup_rss = rss_kib()
    samples = []
    started = time.monotonic()
    for batch in range(100):
        sock.sendall(PING * 10000)
        for _ in range(10000):
            if reader.response() != b"+PONG\r\n":
                raise AssertionError("PING response mismatch")
        if (batch + 1) % 10 == 0:
            samples.append(rss_kib())
    ping_elapsed = time.monotonic() - started
    ping_count = 1_000_000
    growth = samples[-1] - samples[0]
    tail_start = samples[len(samples) // 2]
    tail_growth = samples[-1] - tail_start
    growth_limit = 16384
    if tail_growth > growth_limit:
        raise AssertionError(
            f"Redis RSS grew by {tail_growth} KiB in the stabilized half of one million PINGs "
            f"(limit {growth_limit} KiB; total_growth={growth} KiB; samples={samples})"
        )
    print(
        "redis-canonical-million-ping-memory=pass "
        f"warmup_rss_kib={warmup_rss} final_rss_kib={samples[-1]} "
        f"growth_kib={growth} tail_growth_kib={tail_growth} samples={samples}",
        flush=True,
    )
    print(
        f"redis-benchmark-ping={ping_count / ping_elapsed:.0f}/s",
        flush=True,
    )

    rng = random.Random(0)
    keys = [f"benchmark-{index}" for index in range(128)]
    for index, key in enumerate(keys):
        sock.sendall(frame("SET", key, "warmup"))
        if reader.response() != b"+OK\r\n":
            raise AssertionError("SET warmup failed")

    started = time.monotonic()
    for index in range(100_000):
        key = keys[rng.randrange(len(keys))]
        value = f"value-{rng.randrange(64)}"
        sock.sendall(frame("SET", key, value))
        if reader.response() != b"+OK\r\n":
            raise AssertionError("random SET failed")
    set_elapsed = time.monotonic() - started
    print(f"redis-benchmark-random-set={100000 / set_elapsed:.0f}/s", flush=True)

    started = time.monotonic()
    for _ in range(100_000):
        key = keys[rng.randrange(len(keys))]
        sock.sendall(frame("GET", key))
        if not reader.response().startswith(b"$"):
            raise AssertionError("random GET failed")
    get_elapsed = time.monotonic() - started
    print(f"redis-benchmark-random-get={100000 / get_elapsed:.0f}/s", flush=True)
PY
}

for profile in debug optimized; do
  run_server "$work/redis-$profile"
  check_server
  check_memory_and_benchmarks
  kill "$redis_pid"
  wait "$redis_pid" 2>/dev/null || true
  redis_pid=0
done

if [ -n "$sanitizer" ]; then
  printf '%s\n' "redis-canonical-$sanitizer-sanitizers=pass"
else
  printf '%s\n' "redis-canonical-protocol=pass"
fi
