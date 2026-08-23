# JSON parser benchmark

This benchmark runs the same validating, checksum-producing recursive-descent JSON parser as:

- an optimized TypeNative executable;
- an optimized TypeNative Node-API addon;
- handwritten JavaScript in Node.js.

It also includes Node.js `JSON.parse` as a practical built-in baseline. `JSON.parse` constructs a JavaScript value tree, while the handwritten parsers validate and scan, so treat it as a useful reference rather than an algorithm-identical comparison.

Run it from the repository root:

```sh
benchmarks/json-parser/run.sh
```

The default measurement uses two shuffled warmup rounds and nine shuffled measured samples per product. Use `BENCH_ITERATIONS`, `BENCH_SAMPLES`, `BENCH_WARMUPS`, and `BENCH_SHUFFLE_SEED` to declare a workload. Set `BENCH_RESULTS` to keep fresh evidence outside the repository. The runner validates matching checksums and malformed-input rejection before timing. Executable timings include process startup; addon and Node.js timings are in-process.
