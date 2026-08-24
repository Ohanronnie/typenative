# Active compiler and Redis performance evidence

Evidence date: 2026-08-24. Platform: Darwin arm64, LLVM 22.1.8, Node.js
v24.14.0. The measurements use the active Rust-bootstrap `tn` through
`scripts/tn-guarded.sh`; protected self-host paths were not compiled, read by a
compiler, executed, or benchmarked.

## Method

The Redis comparison runs two warmups and nine measured samples for the
TypeNative executable, TypeNative addon, Rust executable, and handwritten Node
server. Warmups and samples use a deterministic Fisher-Yates shuffle with seed
`324508639`. Each fresh server passes the same RESP correctness matrix and
response checksum before timing. A sample contains three internal fixed-work
trials of 100,000 pipelined PINGs, 10,000 non-pipelined PINGs, 10,000
deterministic random SETs, and 10,000 deterministic random GETs, with eight
persistent concurrent clients and a 12,000-byte value check.

The command matrix covers PING, ECHO, SET, GET, DEL, EXISTS, INCR, EXPIRE, TTL,
COMMAND, QUIT, unknown commands, fragmented frames, pipelining, malformed
frames, invalid UTF-8, oversized input, truncation, and abrupt disconnects.
The response checksum for the final run was
`3f48a4c4960554600b4d299426351331a1f46f05bce88f3d8b7f1537f39b25ad`.

Aggregate values below are medians over nine samples. The benchmark also emits
deterministic percentile-bootstrap 95% intervals, median absolute deviation, CPU
time, system calls, context switches, artifact size, and the complete per-sample
record.

## Final result

| Implementation | Startup (ms) | Pipelined PING (/s) | Non-pipelined latency (µs) | SET (/s) | GET (/s) | Initial RSS (KiB) | RSS growth (KiB) | Artifact (bytes) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| TypeNative native | 27.439 | 1,932,996 | 32.767 | 29,565 | 29,470 | 2,176 | 0 | 118,384 |
| TypeNative addon | 81.969 | 1,907,146 | 33.220 | 29,673 | 29,579 | 44,640 | 0 | 145,960 |
| Rust native | 27.441 | 1,956,744 | 31.623 | 30,243 | 30,986 | 2,016 | 16 | 450,376 |
| Handwritten Node | 83.431 | 665,571 | 41.715 | 23,007 | 22,960 | 50,928 | 9,728 | n/a |

The TypeNative native executable reaches 98.92% of Rust's paired median
pipelined-PING throughput, with a paired aggregate ratio of 98.82% and a
98.20–99.28% percentile-bootstrap interval. The addon reaches 97.19% by paired
median, with a 97.10% aggregate ratio and a 95.59–98.54% interval. Native SET
is 98.21% of Rust by paired median and GET is 95.69%; their aggregate ratios
are 97.71% and 95.35%, respectively. Native non-pipelined latency is 1.038×
Rust, within the 1.05× budget. The complete performance verifier passed.

TypeNative native is approximately 2.90× faster than handwritten Node on
pipelined PING, 1.29× faster on SET, and 1.28× faster on GET. Both TypeNative
artifacts had zero median RSS growth; Node grew by 9,728 KiB and Rust by 16 KiB.
The complete paired comparisons remain available in the machine-readable
result at `/tmp/typenative-performance.EwraBo/redis.json` for this run.

## Resource counters

| Implementation | User CPU (ms) | System CPU (ms) | Unix syscalls | Context switches |
| --- | ---: | ---: | ---: | ---: |
| TypeNative native | 157.6 | 674.1 | 180,600 | 144,123 |
| TypeNative addon | 155.3 | 676.1 | 180,600 | 143,362 |
| Rust native | 57.6 | 640.1 | 182,400 | 134,388 |
| Handwritten Node | 1,023.6 | 1,097.7 | 662,228 | 218,577 |

The TypeNative server uses one shared executor and a reactor-backed async
network boundary. Stateless commands bypass the database mutex; stateful
commands hold the mutex only for the synchronous map operation, before the next
socket await. The executor grows workers when queued tasks have no idle worker,
up to its configured bound, so persistent clients do not starve behind a fixed
worker count.

## Allocation and memory proof

`validation/redis/allocation.tn` resets the runtime allocator counters after
connection-equivalent input/output buffers and database state are initialized.
The real parser, dispatch, and retained-buffer reply path then processes one
million PING commands with zero runtime allocations. Existing-key GET also
records zero runtime allocations. The canonical RSS sampler passed in both
profiles:

```text
debug:     warmup 2784 KiB, final 2784 KiB, growth 0 KiB, tail growth 0 KiB
optimized: warmup 2480 KiB, final 2480 KiB, growth 0 KiB, tail growth 0 KiB
```

`ParsedCommand` carries borrowed `Bytes` ranges into the connection buffer and
caches the first three payload ranges without copying. Input compaction occurs
only after the aggregate's last use. SET is the explicit owned persistence
boundary: new entries retain a `BytesMut` value and existing entries update that
buffer in place. GET encodes the stored value through a shared reference.

## Compiler timings

| Product | Check/build | Wall (s) |
| --- | --- | ---: |
| Compiler check | clean | 6.94 |
| Debug executable | clean | 39.15 |
| Optimized executable | clean | 42.10 |
| Optimized Node addon | clean | 35.70 |

## Reproduction

```sh
TN_BIN=/path/to/tn \
BENCH_RESULTS=/tmp/typenative-redis-benchmark-final.json \
benchmarks/redis-comparison/run.sh
```

The result file records the workload, shuffle plan, platform, compiler metadata,
build timings, artifacts, checksum, individual samples, paired comparisons,
and aggregate statistics.
