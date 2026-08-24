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

`parseCommand` returns one aggregate `ParsedCommand`. Its `Bytes` parts and
borrowed `&str` command name carry the input-buffer loan through inferred
aggregate lifetimes; no public lifetime spelling or view-wrapper family is
required. The compiler rejects input growth, mutation, prefix discard, and
escape while any derived borrow remains live. Reply construction does not retain
that loan across `await`.

## Server behavior

`redis-server.tn` uses a `Database` class with a mutex-protected
`Map<string, RedisEntry>`. Each entry owns a reusable `BytesMut` value and an
expiration timestamp. SET crosses an explicit owned boundary for new persistent
keys and values; updates replace the existing value buffer in place. GET and
DEL use borrowed map lookup, and GET encodes the stored value through a shared
reference. The command contract is:

| Command         | Valid exchange                      | Invalid exchange                    |
| --------------- | ----------------------------------- | ----------------------------------- |
| `PING`          | `+PONG`                             | —                                   |
| `PING message`  | `$<n>\r\nmessage`                    | —                                   |
| `ECHO message`  | `$<n>\r\nmessage`                    | `-ERR ECHO requires a message`      |
| `SET key value` | `+OK`                               | `-ERR SET requires a key and value` |
| `GET key`       | `$<n>\r\nvalue`                     | `$-1` or `-ERR GET requires a key`  |
| `DEL key...`    | removed-key count                  | `-ERR DEL requires a key`           |
| `EXISTS key...` | present-key count                  | —                                   |
| `INCR key`      | incremented integer                | typed integer error                 |
| `EXPIRE key s`  | `:1` when present, `:0` otherwise  | typed integer error                 |
| `TTL key`       | remaining seconds, `-1`, or `-2`   | `-ERR TTL requires a key`           |
| `COMMAND`       | empty RESP array                  | —                                   |
| `QUIT`          | `+OK`, then connection close        | —                                   |
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

Recorded on 2026-08-24 with the active Rust-bootstrap compiler:

```sh
sh -n scripts/verify-redis.sh
TN_BIN=/path/to/tn scripts/verify-redis.sh
```

Both debug and optimized canonical TypeNative builds passed the complete command
matrix, fragmented and pipelined frames, malformed/invalid UTF-8/oversized/
truncated input closure, 12,000-byte capacity growth, QUIT disposal, and 32
concurrent mutex-protected clients. The result was
`redis-canonical-protocol=pass`. The harness builds
`validation/redis/main-alt.tn` and has no project-owned native server
implementation to select.

The final focused output was:

```text
redis-million-ping-runtime-allocations=0
redis-borrowed-get-runtime-allocations=0
redis-canonical-million-ping-memory=pass warmup_rss_kib=2784 final_rss_kib=2784 growth_kib=0 tail_growth_kib=0
redis-canonical-million-ping-memory=pass warmup_rss_kib=2480 final_rss_kib=2480 growth_kib=0 tail_growth_kib=0
redis-canonical-protocol=pass
```

Earlier AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer
regressions also passed for the canonical runtime and async paths. The drop
specialization regression and the 4,090-byte SET boundary test passed under
AddressSanitizer and UndefinedBehaviorSanitizer.

The active RESP parser returns `ParsedCommand` aggregates whose `Bytes` parts
borrow the connection's `BytesMut`. The aggregate caches the first three payload
ranges without copying and supports arbitrary additional arguments through the
validated frame. Commands are matched with case-insensitive ASCII comparison.
Stateless commands bypass the database mutex; GET and DEL use borrowed
`Map<string, RedisEntry>` keys; GET encodes the stored `BytesMut` value through a
shared reference; and only new SET entries cross the explicit `String(...)`
persistence boundary. Existing SET values are updated in place, and input prefix
compaction occurs after every command borrow in the batch has reached its last
use.

`validation/redis/allocation.tn` resets the runtime allocator counters after
connection-equivalent input/output buffers and database state are initialized,
then runs the real parser, dispatch, and retained-buffer reply path for one
million PING commands. A nonzero allocation count or free count fails the
executable, proving that the measurement is not hiding per-command ownership
behind balanced allocation/free traffic.

The sanitizer crash found during the earlier run was an ownership/codegen defect:
generic drop selection matched only the callable, so the `Array<string>` drop
body could be selected for `Array<Bytes>` and free a borrowed RESP input slice.
Drop specialization now requires an exact receiver type; a 4,090-byte SET
boundary test passes under AddressSanitizer and UndefinedBehaviorSanitizer.

## Gate boundary

The TypeNative source checks and this acceptance contract close the hosted
Gate 10 migration. They do not claim self-hosting or replace the protected
self-hosting validation paths. The remaining protected work is tracked in
[`gate11-preparation.md`](gate11-preparation.md).
