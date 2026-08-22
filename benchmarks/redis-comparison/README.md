# Redis comparison

This benchmark compares three servers implementing the same RESP2 subset. Each
run allocates separate loopback ports for the addon, native executable,
and handwritten Node server, and each sample gets a fresh port. This prevents
TCP close-state conflicts between different socket implementations:

- a handwritten Node.js server;
- the TypeNative Redis server compiled as a Node-API addon and loaded by Node;
- the TypeNative Redis server compiled as a native executable.

All implementations are checked for PING, SET, GET, DEL, missing keys, unknown
commands, pipelining, fragmented frames, malformed frames, large values, and
concurrent clients before measurement. Each implementation is started fresh for
every sample. The benchmark separately measures pipelined and non-pipelined
PING, randomized SET, and randomized GET, then reports min/median/max across
repeated samples. RSS is sampled before and after the measured work.

Run it with:

```sh
TN_BIN=/path/to/tn benchmarks/redis-comparison/run.sh
```

The default workload is five samples of 100,000 pipelined PINGs, 10,000
non-pipelined PINGs, and 10,000 deterministic randomized SET and GET commands.
Use `BENCH_SAMPLES`, `BENCH_PING_COUNT`, `BENCH_NONPIPE_PING_COUNT`,
`BENCH_OPERATION_COUNT`, `BENCH_CONCURRENT_CLIENTS`, and `BENCH_LARGE_VALUE`
to declare a different workload. The runner writes the machine-readable result
to `results.json`, including the exact workload, platform, architecture, Node
version, compiler commit, compilation times, artifact sizes, RSS, startup
latency, per-sample measurements, and aggregate statistics.
