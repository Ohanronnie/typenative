# Gate 10 Native Source Inventory

Gate 10 is closed for the ordinary hosted product. The repository contains no
project-owned C, C++, Objective-C, header, or handwritten-assembly
implementation source. Allocation, references, strings, bytes, collections,
synchronization, async promises, task groups, networking, filesystem,
processes, clocks, FFI handles, and executable startup are implemented in
ordinary TypeNative under `runtime/runtime.tn` and the standard library.

## Source boundary

The only native implementations used by a TypeNative product are external
system libraries and generated integration code. TypeNative declares those
boundaries explicitly with `extern "C"`; the linker supplies libc, pthreads,
socket/file/clock/process facilities, LLVM, and Node-API. The generated startup
module is TypeNative and links with the program and TypeNative runtime object.

`scripts/check-native-sources.sh` scans the present worktree for native source
suffixes, and the normal verification matrix runs that scan before building
the compiler and validation products.

## Replacements

| Surface | Ordinary TypeNative source | Evidence |
| --- | --- | --- |
| Runtime ABI and executable startup | `runtime/runtime.tn`, generated startup module in `crates/tn-driver/src/build.rs` | debug and optimized runtime fixtures; executable argument-count fixture |
| Allocation, references, strings, bytes, and collections | `std/alloc.tn`, `std/string.tn`, `std/bytes.tn`, `std/collections.tn` | standard-library and collection validation in both profiles |
| Sync, async, threads, promises, and task groups | `std/sync.tn`, `std/async.tn`, `std/thread.tn`, `runtime/runtime.tn` | async/generic/fallible fixtures; TypeNative lifecycle fixture; Redis concurrent clients |
| Networking, filesystem, process, time, environment, paths, and FFI | `std/net.tn`, `std/fs.tn`, `std/process.tn`, `std/time.tn`, `std/env.tn`, `std/path.tn`, `std/ffi.tn` | runtime fixture, IO fixture, and canonical Redis harness |
| Redis protocol and server | `validation/redis/resp.tn`, `validation/redis/redis-server.tn`, `validation/redis/main-alt.tn` | debug/optimized CRUD, fragmentation, pipelining, malformed input, concurrency, memory, and benchmark harness |
| C ABI validation | `validation/c/exports.tn`, `validation/c/extern.tn` | TypeNative shared-library to TypeNative executable ABI test; no checked-in C probe |

## Acceptance evidence

The captured compiler construction timings are:

```text
clean isolated cargo build -p tn-cli: 15.04 s
incremental cargo build -p tn-cli: 0.18 s
validation/collections/main.tn: check 7.91 s, debug 6.46 s, optimized 6.54 s
validation/redis/main-alt.tn: check 5.08 s, debug 5.45 s, optimized 5.58 s
validation/stdlib/main.tn: check 4.55 s, debug 4.84 s, optimized 4.82 s
```

The canonical Redis run passed one million PING requests without stabilized
RSS growth. The latest debug and optimized samples were respectively 0 KiB
and 0 KiB tail growth, with random SET/GET benchmark output emitted by
`scripts/verify-redis.sh`.

`scripts/verify-runtime.sh`, `scripts/verify-c-abi.sh`,
`scripts/run-sanitizers.sh`, and `scripts/verify-redis.sh` now exercise only
ordinary TypeNative products and explicit external ABI declarations. The
top-level `scripts/verify-all.sh` reports per-stage timing for compiler,
tests, sanitizers, and benchmarks.

## Protected paths

This migration does not edit or execute `compiler-tn/**`,
`scripts/bootstrap-self-host.sh`, or historical A/B/C/fixed-point artifacts.
Gate 10 evidence is independent of self-hosting and does not claim to replace
those protected follow-up paths.
