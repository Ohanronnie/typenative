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

Use `BENCH_ITERATIONS` and `BENCH_SAMPLES` to change the workload. The runner validates matching checksums and malformed-input rejection before timing, then writes the latest machine-readable result to `results.json`. Executable timings include process startup; addon and Node.js timings are in-process.
