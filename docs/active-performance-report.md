# Active compiler and Redis performance evidence

Evidence date: 2026-08-23. Platform: Darwin arm64, LLVM 22.1.8, Node.js
v24.14.0. All compiler commands used the Rust-bootstrap `tn` executable through
`scripts/tn-guarded.sh`; no self-hosted source or product was read by a compiler
or executed.

## Method

The Redis comparison uses two warmups and nine measured samples per
implementation. Both phases use a deterministic Fisher-Yates shuffle with seed
`324508639`. Every sample starts a fresh server and runs the same RESP2
correctness suite before timing. The measured workload is 100,000 pipelined
PINGs, 10,000 non-pipelined PINGs, 10,000 deterministic random SETs, and 10,000
deterministic random GETs. The pre-timing response checksum is
`3f48a4c4960554600b4d299426351331a1f46f05bce88f3d8b7f1537f39b25ad`
for all 27 measured processes.

Intervals are two-sided 95% Student-t intervals over nine samples. Dispersion is
the median absolute deviation (MAD). CPU and syscall counters come from
`PROC_PIDTASKINFO`; Mach absolute CPU ticks are converted with
`mach_timebase_info`.

## Reproduced baseline

| Implementation      | Startup median (ms) | Pipelined PING median (/s) | Pipelined mean 95% CI (/s) | Non-pipelined (/s) | SET (/s) | GET (/s) | RSS growth median (KiB) |
| ------------------- | ------------------: | -------------------------: | -------------------------: | -----------------: | -------: | -------: | ----------------------: |
| TypeNative native   |              26.484 |                    556,411 |            546,073–563,770 |             29,642 |   28,212 |   26,841 |                      16 |
| TypeNative addon    |              79.159 |                    617,976 |            603,414–620,453 |             28,201 |   27,597 |   27,254 |                      16 |
| Handwritten Node.js |              80.559 |                    656,759 |            644,056–667,901 |             22,630 |   20,735 |   21,120 |                   7,280 |

The native and addon pipelined intervals were both wholly below the handwritten
Node interval. All other acceptance metrics already passed.

## Profile-selected optimization

The baseline native PING sample collected 3,310 worker-thread samples. Of those,
1,725 stopped in `send`, 310 in runtime free paths, 213 in runtime allocation,
102 in async destruction, 81 in `recv`, and 43 in prefix-compaction `memmove`.
One reply was encoded into a new buffer and sent for every command, even when a
single read contained a full pipeline.

`RespConnection` now owns a reusable output buffer. The server drains up to
1,024 already-buffered commands, preserves execution and response order, encodes
their replies into that buffer, and flushes once. It never waits for another
command to fill a batch. Fragmented input and non-pipelined latency therefore
retain their prior behavior.

A fixed one-million-PING run isolates the server-side effect:

| Metric                  |   Baseline |    Batched | Change |
| ----------------------- | ---------: | ---------: | -----: |
| Throughput (/s)         |    549,508 |  2,258,190 |  +311% |
| Unix syscalls           |  1,004,007 |      8,007 | -99.2% |
| User CPU (s)            |      0.920 |      0.379 | -58.8% |
| System CPU (s)          |      0.816 |     0.0277 | -96.6% |
| Runtime allocations     | 18,013,057 | 10,050,060 | -44.2% |
| Allocations per command |     18.013 |     10.050 | -44.2% |
| Context switches        |     15,337 |      2,183 | -85.8% |

The fixed workload rebuilds the pre-change active Redis sources from `HEAD` in a
temporary tree containing only `benchmarks/redis-comparison/native.tn` and
`validation/redis/**`. It uses the same active compiler, runtime, and standard
library as the changed build. Runtime allocation totals are read from the
existing `tn_runtime_allocation_count` instrumentation with LLDB after the
workload.

## Accepted result

| Implementation      | Startup median (ms) | Pipelined PING median (/s) | MAD (/s) |    Mean 95% CI (/s) | Non-pipelined (/s) | SET (/s) | GET (/s) | Initial RSS median (KiB) | RSS growth median (KiB) |
| ------------------- | ------------------: | -------------------------: | -------: | ------------------: | -----------------: | -------: | -------: | -----------------------: | ----------------------: |
| TypeNative native   |              26.752 |                  1,027,485 |    7,790 | 1,017,712–1,034,973 |             29,079 |   28,415 |   27,354 |                    2,176 |                       0 |
| TypeNative addon    |              79.493 |                  1,039,353 |   11,883 | 1,016,759–1,050,442 |             30,184 |   27,679 |   27,781 |                   44,832 |                      16 |
| Handwritten Node.js |              80.511 |                    657,655 |    6,831 |     631,267–662,226 |             22,128 |   20,103 |   20,594 |                   51,184 |                   4,784 |

The paired native-minus-Node pipelined improvement is 381,150/s at the median;
its 95% interval is +365,949 to +393,243/s. The addon-minus-Node interval is
+365,550 to +408,158/s. Native and addon therefore exceed the Node median and
neither can be classified as slower at 95% confidence.

Median measured server resource counters for the complete timed workload are:

| Implementation      | User CPU (ms) | System CPU (ms) | Unix syscalls | Context switches | Artifact bytes |
| ------------------- | ------------: | --------------: | ------------: | ---------------: | -------------: |
| TypeNative native   |         108.2 |           241.9 |        60,200 |           43,434 |        116,064 |
| TypeNative addon    |         105.8 |           246.2 |        60,200 |           43,995 |        140,184 |
| Handwritten Node.js |         391.1 |           428.3 |       220,881 |           71,530 |            n/a |

## Remaining-cost investigation

- Socket I/O: after batching, the PING worker sample contains 179 `send` and 247
  `recv` stops; allocation/free paths are now larger than write calls. The
  benchmark syscall medians confirm a 3.7x advantage over handwritten Node.
- RESP parsing, UTF-8, strings, and bounds: the optimized PING sample still shows
  `tn_string_from_bytes`, `tn_utf8_validate`, `tn_bytes_read_u8`, uppercase
  conversion, and string free paths. These checks remain intact. They account
  for much of the remaining 10.05 allocations per command.
- Hash-map lookup: a separate pipelined SET/GET sample processed 6,266,000
  commands. Its leading map cost was 499 `tn_string_equals_slots` samples and
  359 `memcmp` samples; `tn_string_hash_slots` appeared 15 times. SET and GET
  nevertheless remain faster than handwritten Node in the shuffled acceptance
  run, so no map-specific change is justified by the acceptance target.
- Buffer growth and copying: input prefix compaction appears as 149
  `tn_bytes_move_at` samples in the PING profile and 123 platform `memmove`
  samples in the SET/GET profile. Output growth is amortized by the retained
  8-KiB reply buffer. Prefix-index parsing is the next candidate if a future
  target requires more throughput.
- Reference counting and drop glue: inspected optimized IR contains one ARC
  retain/release pair, no RC retain/release pair, 105 runtime allocation calls,
  121 runtime free calls, and 37 string-free calls. LLVM cannot remove the
  remaining heap traffic across the external runtime allocation boundary.
- Locking and scheduling: mutex operations are visible but do not lead either
  PING profile. The fixed workload's context-switch reduction follows from one
  async flush per batch instead of one async write per command.
- Node bridge: the addon main thread remains in `uv_run`; the TypeNative worker
  has the same allocation, parse, compaction, and socket shape as the native
  executable. The exported async `serve` call crosses Node-API once at startup;
  there is no per-command bridge crossing.
- LLVM optimization: an independent `default<O2>` pass inspection reports 195
  successful inlines and 62 `TooCostly` inline misses. GVN reports 1,256
  clobbered loads and LICM reports 295 loop-invariant addresses invalidated by
  possible stores. These are consistent with externally visible allocation,
  buffer, and runtime calls rather than a missing global optimization switch.
  The active pipeline already uses `default<O2>` and an aggressive target
  machine.
- Escape and dead allocation behavior: the optimized IR still contains the
  allocation/free calls above, and allocation instrumentation confirms they
  execute. They are not dead allocations hidden by the benchmark. Removing
  command-string ownership would require a validated borrowed-command lifetime
  design; it is not needed to meet the current targets.

Instruments `xctrace` was attempted in both attach and launch modes with the
Allocations template. This host denied target attachment in both modes and
saved failed traces. Process sampling, `PROC_PIDTASKINFO`, runtime allocation
instrumentation, LLDB, LLVM IR, and optimization records supply the reported
evidence without weakening the workload or protocol.

## Compiler phases

| Product | Mode            | Wall (s) | Module check (ms) | Ownership (ms) | MIR/drop (ms) | Monomorphization (ms) | LLVM/link (ms) |
| ------- | --------------- | -------: | ----------------: | -------------: | ------------: | --------------------: | -------------: |
| Addon   | clean           |    32.30 |             462.5 |            2.9 |       2,127.8 |                   6.0 |       25,746.3 |
| Addon   | unchanged-input |    31.68 |             482.5 |            3.0 |       2,036.2 |                   3.4 |       25,276.8 |
| Native  | clean           |    37.77 |             480.9 |            3.0 |       2,059.6 |                   3.2 |       31,227.5 |
| Native  | unchanged-input |    37.59 |             474.4 |            2.9 |       2,109.5 |                   3.0 |       31,056.5 |

Every measured active compiler invocation is below 38 seconds, well under the
175-second alarm. The active compiler does not currently persist an incremental
module cache, so the unchanged-input measurement records warm filesystem and
existing-output behavior without claiming skipped compiler phases.

Generated raw evidence is retained under
`target/performance-evidence/redis-batching/`; the directory is ignored by Git.
