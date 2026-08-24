# Redis comparison

This benchmark compares four servers implementing the same RESP2 command matrix.
Each run allocates separate loopback ports for the addon, TypeNative native
executable, handwritten Node server, and Rust native executable, and each sample
gets a fresh port. This prevents TCP close-state conflicts between different
socket implementations:

- a handwritten Node.js server;
- the TypeNative Redis server compiled as a Node-API addon and loaded by Node;
- the TypeNative Redis server compiled as a native executable.
- a Rust server compiled as an optimized native executable.

The TypeNative and Rust native servers both begin with a 4 KiB connection input
buffer, retain an 8 KiB response buffer, and flush no more than 1,024 commands
per batch. The Rust server validates UTF-8 and implements the same RESP length
limits rather than using a PING-specific parser.

The command matrix is PING, ECHO, SET, GET, DEL, EXISTS, INCR, EXPIRE, TTL,
COMMAND, QUIT, and unknown-command handling. All implementations are checked
for missing keys, pipelining, fragmented frames, malformed frames, large values,
and concurrent clients before measurement. Each implementation is started fresh
for every sample. The benchmark separately measures pipelined and non-pipelined
PING, randomized SET, and randomized GET, then reports min/median/max across
repeated samples. Every metric also includes standard deviation, median absolute
deviation, and a deterministic percentile-bootstrap 95% confidence interval. A
paired comparison uses the same sample number across implementations; for
fixed-work rate comparisons, its evidence interval is bootstrapped from the
ratio of summed paired durations while the per-sample median remains visible.
A deterministic response checksum is verified before timing. RSS, user and system
CPU time, Mach and Unix system calls, and context switches are sampled before
and after the measured work.

On Darwin hosts with `taskpolicy`, the runner applies the same throughput and
latency scheduling tier to the benchmark process and every server child so
background UI work does not decide the comparison.

Run it with:

```sh
TN_BIN=/path/to/tn benchmarks/redis-comparison/run.sh
```

The default workload is two shuffled warmup rounds and nine shuffled measured
samples of 100,000 pipelined PINGs, 10,000 non-pipelined PINGs, and 10,000
deterministic randomized SET and GET commands per trial. Each measured sample
contains three fixed-work trials, aggregated by elapsed time. Use `BENCH_WARMUPS`,
`BENCH_SAMPLES`, `BENCH_SHUFFLE_SEED`, `BENCH_PING_COUNT`,
`BENCH_NONPIPE_PING_COUNT`, `BENCH_OPERATION_COUNT`,
`BENCH_CONCURRENT_CLIENTS`, and `BENCH_LARGE_VALUE` to declare a different
workload. Set `BENCH_RESULTS` to keep fresh evidence outside the repository.
The runner writes the machine-readable result to that path, or `results.json` by
default, including the exact workload, platform, architecture, Node version,
compiler commit, compilation times, artifact sizes, RSS, startup latency, CPU
and system-call counters, response checksum, per-sample measurements, paired
comparisons, and aggregate statistics.
