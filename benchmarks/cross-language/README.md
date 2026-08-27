# Cross-language kernel benchmark

This is the repository's small, reproducible cross-language suite.  It keeps
one deterministic workload and one checksum in equivalent TypeNative, Rust, C,
Zig, Go, and Node.js programs.  The loop combines a modular numeric recurrence,
an adjacency-style graph recurrence, and a text-byte recurrence; all arithmetic
is bounded so every implementation follows the same algorithm without a
compiler-specific fast path.

Run it from the repository root:

```sh
benchmarks/cross-language/run.sh
```

The runner compiles release products when the corresponding toolchain is
installed, performs two warmups followed by nine shuffled measured runs, checks the
`checksum=899120682` output from every product, and reports median wall time.
Missing optional toolchains are reported as `skipped` rather than changing the
workload or silently comparing a different algorithm.  Set `TN_BIN` to select
the active TypeNative compiler binary.
