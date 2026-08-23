# Canonical Redis Acceptance Program

This document is the acceptance contract for the Redis target. The executable
TypeNative sources are the source of truth:

- [`validation/redis/resp.tn`](../validation/redis/resp.tn) implements the
  incremental RESP parser, command model, reply encoder, and connection
  lifecycle.
- [`validation/redis/redis-server.tn`](../validation/redis/redis-server.tn)
  implements the database, locking boundary, listener, and client task
  orchestration.
- [`validation/redis/main.tn`](../validation/redis/main.tn) is the primary
  entrypoint; [`main-alt.tn`](../validation/redis/main-alt.tn) is the alternate
  port entrypoint.

The canonical target does not select the project-owned C protocol or server
implementation. The native runtime currently supplies only the reviewed
allocation, socket, string, mutex, promise, and task-group ABI functions used
by these sources. The canonical TypeNative executable harness now supplies the
protocol, concurrency, and sanitizer acceptance evidence. The remaining native
source inventory and retirement conditions are tracked separately.

## Canonical language surface

The target exercises direct class members, `this`, `switch`, typed catches,
`T | undefined`, postfix `!`, `try await`, `using`, `await using`, typed
`Promise<T, E>`, raw-pointer unsafe boundaries, and canonical collection and
network APIs. The parser and type checker must reject the removed spellings
listed in [`canonical-migration-manifest.md`](canonical-migration-manifest.md)
without compatibility aliases.

## RESP behavior

`resp.tn` must provide all of the following:

- Incremental reads into `BytesMut`, including fragmented frames and multiple
  pipelined frames already present in the input buffer.
- RESP arrays of bulk strings with an argument-count limit of 1024.
- Bulk payload limits of 512 MiB, checked length arithmetic, and CRLF checks.
- UTF-8 validation and ASCII integer parsing through typed standard-library
  errors mapped to `RedisError`.
- Prefix discard only after a complete command has been decoded.
- Safe simple-string and error replies that replace embedded CR/LF bytes.
- Complete writes, asynchronous disposal, abrupt disconnect handling, and
  explicit oversized, malformed, truncated, and invalid-UTF-8 outcomes.

`parseCommand` returns `ParsedCommand<scope>`. Its `ByteView`, `Utf8View`,
`AsciiKeyUtf8View`, and `HashedUtf8View` fields carry the input-buffer loan in
their nominal lifetime. The compiler rejects input growth, mutation, prefix
discard, and escape while any derived view remains live. Reply construction
does not retain that loan across `await`.

## Server behavior

`redis-server.tn` uses a `Database` class with a mutex-protected
`StringMap<string>`. SET crosses an explicit owned boundary for persistent keys
and values. GET and DEL pass lifetime-bound, prehashed UTF-8 views into the map,
and GET encodes the stored value through a shared reference. The command
contract is:

| Command         | Valid exchange                      | Invalid exchange                    |
| --------------- | ----------------------------------- | ----------------------------------- |
| `PING`          | `+PONG`                             | —                                   |
| `SET key value` | `+OK`                               | `-ERR SET requires a key and value` |
| `GET key`       | `$<n>\r\nvalue`                     | `$-1` or `-ERR GET requires a key`  |
| `DEL key`       | `:1` when removed, `:0` when absent | `-ERR DEL requires a key`           |
| unknown command | `-ERR unknown command`              | —                                   |

Each accepted stream is wrapped in `RespConnection` and submitted through the
`TaskGroup.spawn` boundary. The database is shared through
`Arc<Mutex<Database>>`, and every command operation holds a `MutexGuard` only
for the synchronous database operation, before the next socket await.

The server must bind `127.0.0.1:6379`; the alternate entrypoint binds
`127.0.0.1:6389`. Listener and accept failures map to `RedisError`, and client
errors are contained by the infallible task wrapper so the task group remains
structured.

## Compiler acceptance

Run these ordinary bootstrap-compiler checks from the repository root:

```sh
cargo run -q -p tn-cli -- check validation/redis/resp.tn --json
cargo run -q -p tn-cli -- check validation/redis/redis-server.tn --json
cargo run -q -p tn-cli -- check validation/redis/main.tn --json
cargo run -q -p tn-cli -- check validation/redis/main-alt.tn --json
```

All four commands must produce no diagnostics. The compiler must also reject
the negative fixtures for obsolete syntax, obsolete collection names,
non-canonical constructors, invalid promise arity, non-optional force unwraps,
and the ownership cases listed in the migration manifest.

## Runtime acceptance

The native executable acceptance harness must verify, at minimum:

```text
PING             -> PONG
SET user ronnie  -> OK
GET user         -> ronnie
DEL user         -> 1
GET user         -> nil
unknown command  -> RESP error
```

The harness must additionally cover fragmented frames, pipelining, malformed
lengths, invalid UTF-8, oversized frames, truncated input, abrupt disconnects,
concurrent clients, map and byte-buffer capacity growth, mutex cleanup, and
the absence of a borrow across `await`. Native verification includes
AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer runs on the
supported macOS ARM64 toolchain before any native implementation is retired.

The memory regression warms the server, sends one million PING requests, and
samples RSS after each 100,000-request batch. Its pass condition checks the
stabilized half of the run so sanitizer allocator quarantine is not confused
with live request retention. A genuine per-request leak continues to grow in
that tail window.

## Canonical native evidence

Recorded on 2026-08-13:

```sh
sh -n scripts/verify-redis.sh
cargo build -q -p tn-cli
./scripts/verify-redis.sh
REDIS_SANITIZER=address-undefined ./scripts/verify-redis.sh
REDIS_SANITIZER=thread ./scripts/verify-redis.sh
```

Both debug and optimized canonical TypeNative builds passed CRUD, unknown
commands, fragmented and pipelined frames, malformed/invalid UTF-8/oversized/
truncated input closure, 12,000-byte capacity growth, and 32 concurrent
mutex-protected clients. The result was `redis-canonical-protocol=pass`.
The harness builds `validation/redis/main-alt.tn` and has no project-owned
native server implementation to select.
The sanitizer runs produced `redis-canonical-address-undefined-sanitizers=pass`
and `redis-canonical-thread-sanitizers=pass`.

The active RESP parser returns `ParsedCommand<scope>` values whose byte and
UTF-8 views borrow the connection's `BytesMut`. Commands are matched directly
with case-insensitive ASCII comparison. `PING` encodes a static reply into the
retained output buffer; `GET` and `DEL` use borrowed `StringMap` keys; `GET`
encodes the stored value through a shared reference; and only `SET` crosses the
explicit `Utf8View.toOwned()` persistence boundary. Input prefix compaction
occurs after every command view in the batch has reached its last use.

`validation/redis/allocation.tn` resets the runtime allocator counters after
connection-equivalent input/output buffers and database state are initialized,
then runs the real parser, dispatch, and retained-buffer reply path for one
million PING commands. A nonzero allocation count or free count fails the
executable, proving that the measurement is not hiding per-command ownership
behind balanced allocation/free traffic.

The final focused regression output was:

```text
debug:     RSS 2464 -> 2464 KiB, growth 0 KiB, tail growth 0 KiB
optimized: RSS 2256 -> 2256 KiB, growth 0 KiB, tail growth 0 KiB
runtime:   1,000,000 PING allocations 0, frees 0
runtime:     100,000 GET allocations 0, frees 0
```

Under AddressSanitizer and UndefinedBehaviorSanitizer, both profiles also
passed. ASan retained a one-time quarantine plateau of about 233 MiB, while
the stabilized tail grew by only 16 KiB in one million PINGs. ThreadSanitizer
reported no race and both profiles had zero measured tail growth.

The sanitizer crash found before this run was an ownership/codegen defect:
generic drop selection matched only the callable, so the `Array<string>` drop
body could be selected for `Array<Bytes>` and free a borrowed RESP input slice.
Drop specialization now requires an exact receiver type; a 4,090-byte SET
boundary test passes under AddressSanitizer and UndefinedBehaviorSanitizer.

## Gate boundary

The TypeNative source checks and this acceptance contract close the hosted
Gate 10 migration. They do not claim self-hosting or replace the protected
self-hosting validation paths. The remaining protected work is tracked in
[`gate11-preparation.md`](gate11-preparation.md).
