# HTTP access-log analyzer benchmark

This benchmark compares one deterministic nginx-style access-log workload in
three forms:

- an optimized TypeNative executable;
- the same TypeNative analyzer compiled as a Node `.node` addon;
- a genuinely handwritten JavaScript parser operating directly on a `Buffer`.

Every implementation scans the complete byte input, parses numeric fields,
branches on method and status, counts routes through a hash map, finds the
busiest route, and calculates record count, status buckets, total response
bytes, total duration, per-route counts, and one stable checksum. The runner
requires identical checksums and separately verifies malformed-input rejection
before recording performance.

Run from the repository root:

```sh
benchmarks/http-log-analyzer/run.sh
```

The default fixture is approximately 100 MiB and is generated under `/tmp`, not
inside the repository. Configuration:

```sh
BENCH_FIXTURE_MIB=10 BENCH_ITERATIONS=2 BENCH_SAMPLES=7 \
  benchmarks/http-log-analyzer/run.sh
```

`BENCH_SAMPLES` must be at least five. Fixture generation and loading are
excluded from all core timings. The TypeNative executable loads the fixture
using `std/fs.metadata` and emits its parse duration from `Instant.now()`;
the runner keeps that duration separate from process wall time, which includes
startup, fixture loading, parsing, and shutdown. Addon and JavaScript
measurements run in separate Node processes and pass the entire `Buffer` once
per iteration.

Peak RSS is collected after loading the fixture. Node modes use isolated
`process.resourceUsage().maxRSS`; the native executable uses macOS
`/usr/bin/time -l` or Linux `/usr/bin/time -v`. The latest machine-readable
output is in `results.json`.
These numbers characterize only this workload and are not general claims about
either language.

## Latest recorded 100 MiB run

The checked-in `results.json` was produced on an Apple M1 Pro with Node 24.14.0
and `tn 0.1.0` using:

```sh
BENCH_FIXTURE_MIB=100 BENCH_ITERATIONS=1 BENCH_SAMPLES=5 \
  TN_BIN=/Users/ronnie/.cargo.target/release/tn \
  benchmarks/http-log-analyzer/run.sh
```

The 104,857,590-byte fixture was parsed once in each of five samples. All modes
returned checksum `1015466368115939`. Median throughput was 201.15 MiB/s for
the TypeNative executable, 203.52 MiB/s for the TypeNative addon, and 199.04
MiB/s for handwritten JavaScript. Peak RSS was 101.69 MiB, 146.22 MiB, and
154.70 MiB, respectively. The exact orchestration, fixture-generation, and
worker commands are recorded in `results.json`.

The smaller quick-run configuration remains available:

```sh
BENCH_FIXTURE_MIB=1 BENCH_ITERATIONS=20 BENCH_SAMPLES=5 \
  TN_BIN=/Users/ronnie/.cargo.target/release/tn \
  benchmarks/http-log-analyzer/run.sh
```
