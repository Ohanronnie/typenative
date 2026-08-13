#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
work=$(mktemp -d "${TMPDIR:-/tmp}/typenative-sanitize.XXXXXX")
trap 'rm -rf "$work"' EXIT

cat >"$work/runtime_probe.c" <<'EOF'
#include <stddef.h>
#include <stdint.h>
extern void *tn_runtime_alloc(size_t);
extern void tn_runtime_free(void *);
extern uint64_t tn_clock_monotonic_ns(void);
int main(void) {
  void *memory = tn_runtime_alloc(64);
  tn_runtime_free(memory);
  return tn_clock_monotonic_ns() == 0 ? 1 : 0;
}
EOF

cat >"$work/redis_probe.c" <<'EOF'
#include <stdint.h>
extern int tn_redis_main(int32_t port);
int main(void) { return tn_redis_main(6391); }
EOF

clang -fsanitize=address,undefined -fno-omit-frame-pointer \
  "$work/runtime_probe.c" "$root/runtime/runtime.c" -o "$work/runtime_probe"
ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=halt_on_error=1 "$work/runtime_probe"
printf '%s\n' "address-undefined-sanitizers=pass"

tn=${TN_BIN:-$target_dir/debug/tn}
if [ ! -x "$tn" ]; then
  tn=$(command -v tn || true)
fi
if [ -z "$tn" ] || [ ! -x "$tn" ]; then
  echo "TypeNative compiler not found; set TN_BIN for standard-library sanitizer checks" >&2
  exit 2
fi
for fixture in stdlib async; do
  case "$fixture" in
    stdlib) expected=42; source="$root/validation/stdlib/main.tn" ;;
    async) expected=43; source="$root/validation/async/main.tn" ;;
  esac
  "$tn" build "$source" --profile debug --out "$work/$fixture-asan" \
    --link-argument=-fsanitize=address,undefined >/dev/null
  set +e
  ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=halt_on_error=1 "$work/$fixture-asan" >/dev/null
  result=$?
  set -e
  [ "$result" -eq "$expected" ]
done
printf '%s\n' "hosted-address-undefined-sanitizers=pass"

clang -fsanitize=address,undefined -fno-omit-frame-pointer -pthread \
  "$work/redis_probe.c" "$root/runtime/redis.c" -o "$work/redis_probe"
clang -fsanitize=address,undefined -fno-omit-frame-pointer -pthread \
  "$root/runtime/redis.c" "$root/validation/redis/lifecycle.c" -o "$work/redis_lifecycle"
ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=halt_on_error=1 "$work/redis_lifecycle"
printf '%s\n' "redis-lifecycle-address-undefined-sanitizers=pass"
"$work/redis_probe" >/dev/null 2>&1 &
redis_pid=$!
redis_cleanup() { kill "$redis_pid" 2>/dev/null || true; wait "$redis_pid" 2>/dev/null || true; }
trap 'redis_cleanup; rm -rf "$work"' EXIT
sleep 0.2
python3 - <<'PY'
import socket
import time

def frame(*args):
    payload = b'*%d\r\n' % len(args)
    for arg in args:
        value = str(arg).encode()
        payload += b'$%d\r\n%s\r\n' % (len(value), value)
    return payload

def response(sock):
    line = b''
    while not line.endswith(b'\r\n'):
        chunk = sock.recv(1)
        if not chunk:
            raise RuntimeError('redis probe closed')
        line += chunk
    if line[:1] == b'$':
        size = int(line[1:-2])
        if size >= 0:
            body = b''
            while len(body) < size + 2:
                body += sock.recv(size + 2 - len(body))
            return line + body
    return line

for _ in range(40):
    try:
        sock = socket.create_connection(('127.0.0.1', 6391), timeout=0.2)
        break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit('Redis sanitizer server did not start')
with sock:
    for command, expected in [
        (('PING',), b'+PONG\r\n'),
        (('SET', 'sanitize-key', 'value'), b'+OK\r\n'),
        (('GET', 'sanitize-key'), b'$5\r\nvalue\r\n'),
        (('QUIT',), b'+OK\r\n'),
    ]:
        sock.sendall(frame(*command))
        if response(sock) != expected:
            raise SystemExit('unexpected Redis response')
PY
redis_cleanup
printf '%s\n' "redis-address-undefined-sanitizers=pass"

TN_BIN="$tn" REDIS_SANITIZER=address-undefined \
  "$root/scripts/verify-redis.sh"

if clang -fsanitize=thread -fno-omit-frame-pointer \
  "$work/runtime_probe.c" "$root/runtime/runtime.c" -o "$work/runtime_probe_tsan" 2>"$work/tsan.err"; then
  TSAN_OPTIONS=halt_on_error=1 "$work/runtime_probe_tsan"
  printf '%s\n' "thread-sanitizer=pass"
  for fixture in stdlib async; do
    case "$fixture" in
      stdlib) expected=42; source="$root/validation/stdlib/main.tn" ;;
      async) expected=43; source="$root/validation/async/main.tn" ;;
    esac
    "$tn" build "$source" --profile debug --out "$work/$fixture-tsan" \
      --link-argument=-fsanitize=thread >/dev/null
    set +e
    TSAN_OPTIONS=halt_on_error=1 "$work/$fixture-tsan" >/dev/null
    result=$?
    set -e
    [ "$result" -eq "$expected" ]
  done
  printf '%s\n' "hosted-thread-sanitizer=pass"
  if clang -fsanitize=thread -fno-omit-frame-pointer -pthread \
    "$work/redis_probe.c" "$root/runtime/redis.c" -o "$work/redis_probe_tsan" 2>"$work/redis_tsan.err"; then
    if clang -fsanitize=thread -fno-omit-frame-pointer -pthread \
      "$root/runtime/redis.c" "$root/validation/redis/lifecycle.c" -o "$work/redis_lifecycle_tsan" 2>"$work/redis_lifecycle_tsan.err"; then
      TSAN_OPTIONS=halt_on_error=1 "$work/redis_lifecycle_tsan"
      printf '%s\n' "redis-lifecycle-thread-sanitizer=pass"
    else
      printf '%s\n' "redis-lifecycle-thread-sanitizer=unavailable: $(tr '\n' ' ' <"$work/redis_lifecycle_tsan.err")" >&2
      exit 2
    fi
    "$work/redis_probe_tsan" >/dev/null 2>&1 &
    redis_tsan_pid=$!
    sleep 0.4
    python3 - <<'PY'
import socket
import time
for _ in range(40):
    try:
        sock = socket.create_connection(('127.0.0.1', 6391), timeout=0.2)
        break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit('Redis TSan server did not start')
with sock:
    sock.sendall(b'*1\r\n$4\r\nPING\r\n')
    if sock.recv(64) != b'+PONG\r\n':
        raise SystemExit('unexpected Redis TSan response')
PY
    kill "$redis_tsan_pid" 2>/dev/null || true
    wait "$redis_tsan_pid" 2>/dev/null || true
    printf '%s\n' "redis-thread-sanitizer=pass"
    TN_BIN="$tn" REDIS_SANITIZER=thread \
      "$root/scripts/verify-redis.sh"
  else
    printf '%s\n' "redis-thread-sanitizer=unavailable: $(tr '\n' ' ' <"$work/redis_tsan.err")" >&2
    exit 2
  fi
else
  printf '%s\n' "thread-sanitizer=unavailable: $(tr '\n' ' ' <"$work/tsan.err")" >&2
  exit 2
fi
