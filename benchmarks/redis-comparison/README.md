# Redis comparison

This benchmark compares four servers implementing the same RESP2 subset. Each
run allocates separate loopback ports for the addon, TypeNative native executable,
handwritten Node server, and Rust native executable, and each sample gets a fresh port. This prevents
TCP close-state conflicts between different socket implementations:

- a handwritten Node.js server;
- the TypeNative Redis server compiled as a Node-API addon and loaded by Node;
- the TypeNative Redis server compiled as a native executable.
- a Rust server compiled as an optimized native executable.

The TypeNative and Rust native servers both begin with a 4 KiB connection input
buffer, retain an 8 KiB response buffer, and flush no more than 1,024 commands
per batch. The Rust server validates UTF-8 and implements the same RESP length
limits rather than using a PING-specific parser.

All implementations are checked for PING, SET, GET, DEL, missing keys, unknown
commands, pipelining, fragmented frames, malformed frames, large values, and
concurrent clients before measurement. Each implementation is started fresh for
every sample. The benchmark separately measures pipelined and non-pipelined
PING, randomized SET, and randomized GET, then reports min/median/max across
repeated samples. Every metric also includes standard deviation, median absolute
deviation, and a 95% Student-t confidence interval. A deterministic response
checksum is verified before timing. RSS, user and system CPU time, Mach and Unix
system calls, and context switches are sampled before and after the measured work.

Run it with:

```sh
TN_BIN=/path/to/tn benchmarks/redis-comparison/run.sh
```

The default workload is two shuffled warmup rounds and nine shuffled measured samples of 100,000 pipelined PINGs, 10,000
non-pipelined PINGs, and 10,000 deterministic randomized SET and GET commands.
Use `BENCH_WARMUPS`, `BENCH_SAMPLES`, `BENCH_SHUFFLE_SEED`, `BENCH_PING_COUNT`, `BENCH_NONPIPE_PING_COUNT`,
`BENCH_OPERATION_COUNT`, `BENCH_CONCURRENT_CLIENTS`, and `BENCH_LARGE_VALUE`
to declare a different workload. Set `BENCH_RESULTS` to keep fresh evidence outside the repository. The runner writes the machine-readable result
to that path, or `results.json` by default, including the exact workload, platform, architecture, Node
version, compiler commit, compilation times, artifact sizes, RSS, startup
latency, CPU and system-call counters, response checksum, per-sample measurements,
paired comparisons, and aggregate statistics.
