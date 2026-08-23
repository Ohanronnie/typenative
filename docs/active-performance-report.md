# Active compiler and Redis performance evidence

Evidence date: 2026-08-23. Platform: Darwin arm64, LLVM 22.1.8, Node.js
v24.14.0. Every compiler command used the active Rust-bootstrap `tn` through
`scripts/tn-guarded.sh`; no frozen self-host source or product was compiled,
read by a compiler, executed, or benchmarked.

## Method

The Redis comparison runs two warmups and nine measured samples for the
TypeNative executable, TypeNative addon, Rust executable, and handwritten Node
server. Warmups and samples use a deterministic Fisher-Yates shuffle with seed
`324508639`. Each fresh server passes the same RESP2 correctness and response
checksum suite before timing. A sample contains 100,000 pipelined PINGs, 10,000
non-pipelined PINGs, 10,000 deterministic random SETs, and 10,000 deterministic
random GETs.

Intervals are two-sided 95% Student-t intervals over nine paired samples. The
equivalence gate requires the lower bound of the paired native/Rust PING ratio
to be at least 0.95. CPU, syscall, and context-switch counters come from
`PROC_PIDTASKINFO`.

## Reproduced baseline

| Implementation    | Startup (ms) | PING (/s) | SET (/s) | GET (/s) | Initial RSS (KiB) | RSS growth (KiB) | Artifact (bytes) |
| ----------------- | -----------: | --------: | -------: | -------: | ----------------: | ---------------: | ---------------: |
| TypeNative native |       26.662 | 1,017,679 |   26,358 |   27,848 |             2,272 |                0 |          116,064 |
| TypeNative addon  |       78.827 | 1,042,205 |   29,240 |   29,080 |            44,752 |                0 |          140,184 |
| Rust native       |       26.637 | 1,833,144 |   30,624 |   30,075 |             2,016 |               16 |          447,752 |
| Handwritten Node  |       81.122 |   656,625 |   23,173 |   23,062 |            51,072 |            4,784 |              n/a |

The baseline parser created owned part and argument arrays, copied every RESP
payload into a string, uppercased the command into another string, constructed
owned command/reply values, cloned GET values for encoding, and executed about
10.05 runtime allocations per PING. LLVM IR and runtime counters confirmed that
the allocation/free calls survived optimization.

## Borrowed architecture

The active compiler now carries named lifetime arguments through parsing,
semantic types, MIR substitution, ownership analysis, ABI matching, and
monomorphization. `scope` ties returned aggregate views to the current source
loan. Returning a view of a local owner, mutating or compacting a borrowed
buffer, or using a view after compaction is rejected causally by ownership
tests. Lifetime arguments have no runtime layout.

`ByteView<a>` is a private pointer/length view that can only be created from a
borrowed owner or another checked view. `Utf8View<a>` proves validation without
ownership. `HashedUtf8View<a>` and `AsciiKeyUtf8View<a>` retain reusable hash or
case-folded key evidence beside the safe UTF-8 view. `StringMap<V>` owns keys on
insertion and accepts borrowed or prehashed UTF-8 views for lookup, removal, and
the explicit persistent insertion boundary.

The RESP parser returns `ParsedCommand<scope>`. It validates every part, keeps
compact ranges for arbitrary arguments up to the 1,024-part limit, and caches
the common command/key evidence without arrays or copies. Dispatch writes into
the retained output buffer. PING uses a static response, GET and DEL borrow
their keys, GET borrows the stored value, and SET alone creates persistent owned
key/value storage.

General runtime changes fuse CRLF/unsigned-line parsing, UTF-8 proof generation
with hashing or a short ASCII key, case-insensitive comparison, managed-string
append, and exact CRLF checks. Unsigned decimal reply encoding now writes digits
backwards in place instead of allocating a scratch buffer.

## Accepted result

| Implementation    | Startup (ms) | PING (/s) | Latency (µs) | SET (/s) | GET (/s) | Initial RSS (KiB) | RSS growth (KiB) | Artifact (bytes) |
| ----------------- | -----------: | --------: | -----------: | -------: | -------: | ----------------: | ---------------: | ---------------: |
| TypeNative native |       26.531 | 1,863,402 |       34.014 |   27,824 |   28,439 |             2,112 |                0 |           99,648 |
| TypeNative addon  |       79.084 | 1,860,992 |       33.681 |   28,517 |   28,600 |            44,688 |                0 |          137,976 |
| Rust native       |       26.569 | 1,914,387 |       32.780 |   28,752 |   29,878 |             2,000 |               16 |          447,752 |
| Handwritten Node  |       80.492 |   685,369 |       42.132 |   22,024 |   22,422 |            51,024 |            4,736 |              n/a |

Native/Rust median ratios are 97.34% for pipelined PING, 103.76% for latency
(lower is better), 96.77% for SET, and 95.19% for GET. The paired PING ratio
median is 97.88%; its 95% confidence interval is 95.39–100.32%, establishing
equivalence within the required 5% margin. Native PING improved 83.1% from the
reproduced baseline while the executable became 14.1% smaller.

The addon reaches 97.21% of Rust PING throughput and remains faster than
handwritten Node for PING, SET, and GET. Its maximum measured RSS growth was 48
KiB versus Node's 5,056 KiB, more than the required 20× separation.

Median complete-workload resource counters are:

| Implementation    | User CPU (ms) | System CPU (ms) | Unix syscalls | Context switches |
| ----------------- | ------------: | --------------: | ------------: | ---------------: |
| TypeNative native |          51.2 |           233.4 |        60,200 |           48,516 |
| TypeNative addon  |          49.5 |           231.3 |        60,200 |           48,230 |
| Rust native       |          18.8 |           224.7 |        60,800 |           46,188 |
| Handwritten Node  |         376.2 |           385.8 |       220,872 |           71,929 |

Compared with the reproduced native baseline, user CPU fell from 111.0 ms to
51.2 ms and system CPU fell from 244.6 ms to 233.4 ms. The fixed socket workload
keeps the same 60,200 syscall count; the performance gain comes from removing
owned parser/dispatch work rather than moving cost into I/O.

## Allocation and memory proof

`validation/redis/allocation.tn` initializes input/output buffers, persistent
database state, and map capacity before resetting both runtime counters. The
actual parser and `Database.execute` path then processes one million PINGs with
exactly zero allocations and zero frees. A second reset followed by 100,000
existing-key GETs also records zero allocations and zero frees, proving that
lookup, stored-value encoding, and unsigned length encoding do not hide balanced
heap traffic. The million-PING RSS sampler is flat after warmup in both debug
and optimized profiles.

SET retains only the allocations required for persistent owned key/value state
and map growth. GET never clones the stored value.

## Compiler timings

| Product | Build           | Wall (s) | Module check (ms) | Ownership (ms) | MIR/drop (ms) | Monomorphization (ms) | LLVM/link (ms) |
| ------- | --------------- | -------: | ----------------: | -------------: | ------------: | --------------------: | -------------: |
| Addon   | clean           |    35.02 |             553.1 |            2.9 |       2,472.8 |                   2.2 |       27,296.9 |
| Addon   | unchanged input |    35.22 |             581.2 |            3.0 |       2,509.4 |                   2.2 |       27,368.7 |
| Native  | clean           |    43.01 |             564.0 |            2.9 |       2,523.2 |                   2.4 |       35,176.6 |
| Native  | unchanged input |    42.36 |             562.8 |            2.9 |       2,523.1 |                   2.3 |       34,542.0 |

Every measured active compiler invocation is below 44 seconds, well below the
175-second guard. Generated raw evidence remains under the ignored
`target/performance-evidence/borrowed-redis/` directory until final cleanup.
