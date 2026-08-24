# TypeNative Canonical Migration Manifest

## 1. Purpose and boundary

This is the active traceability ledger for the Rust compiler, standard
library, runtime, validation programs, tooling, and benchmarks. It records the
one accepted spelling, the rejected alternative, the implementation layers,
and the evidence required before a migration is complete.

The tree `compiler-tn/**`, `scripts/bootstrap-self-host.sh`, and the exact
contents recorded by `docs/selfhost-freeze.json` are protected. They are not
migration targets in this program. The pre-existing worktree modification to
`benchmarks/json-parser/results.json` is also protected and must remain
untouched.

A row is complete only when the active compiler rejects the obsolete spelling,
the formatter never emits it, the listed condition identifier is stable, the
positive and negative fixtures pass, and every listed consumer uses the same
canonical representation. A historical document may mention an old spelling
as evidence, but it must not describe it as current language behavior.

## 2. Public-language convergence

| Rejected construct | Canonical replacement | Implementation layers | Required evidence |
| --- | --- | --- | --- |
| Public `scope` lifetime category | Lifetime elision, named `lifetime` parameters, and rare `static` borrows | Lexer, parser, type representation, lifetime inference, module metadata, formatter, diagnostics, docs | Positive elision and named-lifetime fixtures; obsolete `scope` rejection; returned-borrow and recovery tests |
| `@Copy`, `@Clone`, `@Drop`, `@Send`, `@Sync` | Structural copyability and thread-safety inference; handwritten `.clone()`; automatic destruction and resource protocols | Attribute registry removal, HIR facts, ownership checker, MIR cleanup, diagnostics, docs | Forged-attribute negatives; nested field inference; exact cleanup and capture tests |
| `@Conform(Type)` | `implements Type` | Parser, HIR conformance, method lookup, generic bounds, diagnostics, formatter | Canonical class/struct/interface fixtures and obsolete decorator recovery |
| `@Sealed`, `final`, and derivation syntax | No replacement; optimizer proves closed dispatch | Lexer exclusion, parser recovery, class HIR, dispatch optimization, diagnostics | Rejection fixtures; open hierarchy and optimizer correctness tests |
| `@Layout("u8")` | `enum Kind: u8` | Enum grammar, layout checking, ABI lowering, formatter, docs | Enum-width and C-layout probes; obsolete decorator negatives |
| `@Layout("C")` | `extern struct Name { ... }` | Foreign-layout grammar, type checking, ABI lowering, symbol/layout probes | Struct size/alignment/offset checks and parser recovery |
| `@Export(...)` | `export` or `export extern "C" function ...` | Top-level grammar, symbol table, ABI lowering, Node declarations, diagnostics | Export symbol and declaration parity tests; forged-attribute negatives |
| `@Intrinsic(...)` | Private trusted compiler manifest bound by declaration identity and approved module | Intrinsic registry, module validation, HIR identity, MIR/LLVM lowering | User-forgery rejection; manifest identity and module-boundary tests |
| `@Inline`, `@Test` | Optimizer decision; `test("name", () => {})` | Optimization, test discovery, CLI, formatter, docs | No source directive tests; ordinary test registration and optimization tests |
| `macro` declarations and `@Expand(...)` | Functions, generics, interfaces, and ordinary declarations | Lexer/parser removal, HIR expansion removal, diagnostics, tooling | Obsolete macro negatives, recovery fixtures, no expansion artifacts |
| Receiver `mut` | Receiver mutability inferred from writes to `this` | Parser, body checker, ownership, method metadata, formatter | Read-only/write receiver fixtures and obsolete receiver recovery |
| `Promise<T>` with a separate async error clause | `Promise<T, E>` | Type parser, async effects, HIR/MIR suspension, LLVM ABI, Node declarations | Arity, propagation, cancellation, and formatter fixtures |
| `Option<T>` and `Result<T, E>` as public wrappers | `T | undefined` for absence and typed `throws E` effects | Type grammar, optional narrowing, effect checker, diagnostics | Optional, catch, and obsolete-wrapper fixtures |
| Uppercase/boxed/competing string constructors | Primitive `string`, `String(value)`, `String.fromUtf8`, and `String.fromUtf8Lossy` | Prelude, type model, member resolution, ownership, HIR/MIR, LLVM ABI | UTF-8, scalar/byte length, conversion, allocation, and obsolete-constructor tests |
| `ByteView`, `Utf8View`, and hashed public wrappers | `&[u8]`, `&str`, `Bytes`, and `BytesMut` | Std types, borrow/lifetime checker, parser, docs, Node declarations | Borrowed parsing, mutation invalidation, and public-name rejection |
| `view`, `subview`, and handwritten maximum sentinels | `.slice`, `.trySlice`, `.length`, typed absence, or typed errors | Bytes API, diagnostics, formatter, validation | Bounds, overflow, recovery, and naming scans |
| `Vector`, `FixedArray`, `ReadonlySlice`, `MutableSlice`, `StringMap` | `Array`, `[T; N]`, `&[T]`, `&mut [T]`, and `Map<string, V>` | Collections HIR, standard library, ownership, Node mappings, docs | Collection construction, lookup, iteration, ownership, and obsolete-name fixtures |
| Ownership-copying `Array.get()` | `at` for borrowed lookup, indexing by context, `removeAt` for ownership transfer | Array methods, borrow checker, MIR projection, formatter | No-copy lookup and O(n) iteration tests |
| `getHashed`, `removeHashed`, `setBorrowed` | Ordinary `Map<string, V>` lookup and mutation | Hashing internals, map API, borrowed equality, ownership | Borrowed-key lookup without misleading allocation/ownership claims |
| `withCapacity`, `Arc.new`, `Mutex.new`, and duplicate constructors | `new Type({ capacity: n })` or `new Type()`; `Type.from` only converts | Parser, constructor typing, HIR, diagnostics, docs | Capacity/growth and obsolete-constructor tests |
| `Checked` public suffixes | Typed safe methods such as `TcpStream.connect` and explicit unsafe `Raw` internals | Std API, errors, FFI wrappers, docs, Node declarations | Typed error and unsafe-boundary tests |
| Public descriptors, pointers, output parameters, integer event masks, and platform codes | Typed filesystem/network/thread/reactor wrappers | FFI declarations, safe wrappers, ownership, diagnostics, ABI tests | Clang/OS probes, sanitizers, raw-boundary negatives |
| One-thread-per-task and public `enter`/`leave` accounting | Awaitable executor `Task<T, E>` and structured `TaskGroup` | Async HIR/MIR, executor, cancellation, task APIs, diagnostics | Task result/error, group exit, cancellation, detach, and scheduler tests |
| Blocking async I/O | Nonblocking operations registered with a shared reactor | Runtime, net/fs std modules, task wakeups, timers, cancellation | Slow-client, partial-I/O, readiness, descriptor-close, and backpressure tests |
| RESP framing and Redis parser helpers in runtime/std | Generic byte primitives plus Redis-owned `validation/redis/resp.tn` | Runtime, std/bytes, Redis validation, benchmarks, source scans | Non-Redis byte tests, Redis protocol matrix, and no-RESP runtime scan |

## 3. Source-consumer matrix

Every grammar or API row is checked across the same consumer set:

- lexer, parser, lossless CST, AST wrappers, and parser recovery;
- formatter, linter, LSP, documentation generator, and diagnostics JSON;
- HIR, type/effect checking, ownership/lifetime checking, MIR, and LLVM;
- standard-library declarations, runtime entry points, Node declarations, and
  FFI wrappers;
- positive, negative, recovery, property, fuzz, sanitizer, and integration
  fixtures; and
- validation applications, Redis, compiler-time checks, and benchmarks.

The migration is incomplete if any consumer retains a compatibility alias or
prints an obsolete spelling. Source scans must distinguish intentional negative
fixtures and historical evidence from active implementation sources.

## 4. Redis traceability

| Requirement | Canonical location | Evidence |
| --- | --- | --- |
| Fragmented/coalesced RESP parsing | `validation/redis/resp.tn` | Incremental parser tests and fuzzing |
| Pipelining, malformed frames, and size limits | `validation/redis/resp.tn` and `docs/redis-acceptance.md` | Protocol harness and typed-error assertions |
| PING, ECHO, SET, GET, DEL, EXISTS, INCR, EXPIRE, TTL, COMMAND, QUIT | `validation/redis/*.tn` | End-to-end client matrix |
| Borrowed command data and input-buffer invalidation | Redis parser/application modules | Lifetime rejection and sanitizer tests |
| Zero-allocation one-million-PING and existing-key GET paths | Redis benchmark sources | Allocation counters, RSS samples, and checksum parity |
| No Redis-specific compiler/runtime behavior | Rust sources and runtime/bytes scan | Source scan plus independent generic-byte validation |

## 5. Performance traceability

The benchmark suite covers JSON parsing, HTTP log analysis, HTTP serving,
collections, text processing, file processing, task scheduling, numeric
kernels, trees/graphs, and a Node addon in TypeNative, Rust, C, Zig, Go, and
meaningful Node implementations. Each workload records the algorithm, input,
checksum, warmups, shuffled samples, compiler flags, host, and tool versions.

The accepted summary is median plus bootstrap 95% confidence interval. Server
workloads also record p50/p95/p99 latency. CPU, wall time, RSS, allocations,
copied bytes, syscalls, context switches, startup, compilation, and binary size
are recorded separately for cold and hot runs.

## 6. Active status

The manifest is a design and traceability contract, not evidence that a row has
already passed. `docs/implementation-status.md` records only commands that
have actually run. Historical Gate records remain historical and cannot mark a
row complete in this active program.
