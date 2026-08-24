# TypeNative Active-Language Convergence Plan

## 1. Purpose and authority

This plan is the implementation sequence for the language defined in
[`language-spec.md`](language-spec.md). The active compiler is the Rust
implementation under `crates/tn-*`. The standard library, runtime, validation
programs, tooling, and benchmarks are active implementation surfaces.

The self-hosted compiler under `compiler-tn/**` and the orchestration in
`scripts/bootstrap-self-host.sh` are frozen. They are verified read-only and
are not modified, bootstrapped, compared, or used as acceptance evidence while
this plan is in progress. The protected unstaged change to
`benchmarks/json-parser/results.json` is also left untouched.

The former Gate 9–12 descriptions are superseded by this active convergence
program. Self-host migration becomes a separate program only after the active
language, runtime, standard library, tooling, and performance suite have
stopped moving and all active gates pass.

## 2. Non-negotiable design decisions

- One public grammar has one spelling for each feature. Obsolete syntax is
  rejected, not deprecated or aliased.
- `scope` is internal compiler bookkeeping only. Public code uses lifetime
  elision or named `lifetime` parameters.
- Compiler-owned decorators and source macros do not exist. User decorators
  are ordinary functions with ordinary declaration lookup and cannot claim
  compiler capabilities.
- Copyability, `Send`, and `Sync` are structural facts. Clone is an ordinary
  handwritten method. `implements` is the sole conformance spelling.
- `class` is uniquely owned heap identity and `struct` is an inline nominal
  value. `abstract`, `extends`, `implements`, `override`, visibility,
  `readonly`, and `static` are the supported class vocabulary.
- Memory destruction is automatic. External cleanup uses `Disposable`,
  `AsyncDisposable`, `using`, and `await using`.
- `Promise<T, E>` remains unchanged. Synchronous recoverable failures use
  `throws E`.
- `&[T]`, `&mut [T]`, `[T; N]`, `Bytes`, `BytesMut`, and primitive `string` are
  canonical. Public wrapper families and duplicate collection APIs are not.
- Threads, executor tasks, and reactor readiness are separate abstractions.
- RESP protocol logic belongs to Redis validation/application code, not the
  runtime or foundational byte library.
- Safe APIs hide pointers, descriptors, output parameters, integer event masks,
  and platform error codes behind typed wrappers.

## 3. Protected boundary checks

Before every implementation slice:

1. verify `docs/selfhost-freeze.json` with
   `scripts/verify-selfhost-freeze.sh`;
2. confirm no path under `compiler-tn/**` or the bootstrap script is staged or
   modified;
3. confirm `benchmarks/json-parser/results.json` retains its pre-existing
   worktree change; and
4. do not run compiler A/B/C/D, fixed-point, or self-host differential commands.

Any verification script that touches the frozen tree is out of scope until the
active program is complete and must be replaced with a read-only check.

## 4. Ordered delivery sequence

Each item is a reviewable commit or small coherent commit series. Grammar
removal, runtime redesign, and benchmark optimization are not combined into an
unreviewable change.

### 1. Lock the canonical design

Rewrite the six canonical documents together:

- `docs/language-spec.md`
- `docs/implementation-plan.md`
- `docs/canonical-migration-manifest.md`
- `docs/compiler-magic-audit.md`
- `docs/design-audit.md`
- `docs/implementation-status.md`

Record the active compiler, frozen boundary, rejected alternatives, ownership
and cleanup contracts, runtime layering, error behavior, and performance gates.
No source migration starts while these documents disagree.

### 2. Add rejection fixtures first

For every removed spelling, add a positive canonical fixture, an obsolete
negative fixture, a parser-recovery fixture, a condition identifier, and a
formatter assertion. The negative fixture must prove that a following
declaration still parses. A code action is emitted only where the replacement
is unambiguous. There is no compatibility mode.

### 3. Remove public `scope` and add lifetime elision

Remove `scope` from the lexer keyword table, parser grammar, type arguments,
formatter, normal diagnostics, generated documentation, and module metadata.
Keep internal lifetime categories private. Implement local lifetime inference,
borrowed aggregate output inference, conservative bodyless declaration elision,
named `lifetime` parameters, and exported lifetime contracts. Add diagnostics
that describe invalidation and escape relationships in source terms.

### 4. Replace compiler decorators with language behavior

Delete compiler-owned decorators from the lexer/parser registry and semantic
metadata. Implement structural Copy/Send/Sync inference, ordinary clone
methods, `implements`, `export extern "C"`, `extern struct`, and the private
trusted intrinsic manifest. Add negative coverage for forged ownership, ABI,
layout, export, intrinsic, and optimizer claims.

### 5. Remove macros and expansion infrastructure

Remove `macro` declarations, `@Expand`, token-template expansion, expansion
diagnostics, and all compatibility aliases from the active compiler and
tooling. Replace each supported use with functions, generics, interfaces, or
ordinary declarations. Preserve parser recovery and source-span diagnostics
for obsolete forms.

### 6. Add modern user-defined decorators

Implement TypeScript-shaped user decorators through ordinary callable
resolution. Support wrapping or initializing supported class elements and
`ClassMethodDecoratorContext`-style metadata. Enforce that decorators cannot
change ownership, ABI layout, `Send`, `Sync`, intrinsic identity, or unrelated
declarations. Exercise lexer, CST, AST, HIR, formatter, LSP, documentation,
Node declarations, and runtime fixtures.

### 7. Normalize classes and value types

Remove `sealed`, `final`, and derivation syntax. Remove receiver `mut` and
infer receiver mutability from writes to `this`. Implement `abstract`,
`extends`, `implements`, `override`, `readonly`, and visibility consistently
through parser, HIR, type checking, ownership, MIR dispatch, LLVM lowering,
formatter, and diagnostics. Closed dispatch is an optimizer proof rather than
a source claim.

### 8. Implement structural capabilities and ordinary Clone

Derive copyability and thread-safety recursively from fields and ownership
representation. Require explicit clone methods and preserve their type-specific
semantics. Add field-level and generic tests for files, sockets, locks, shared
pointers, arrays, maps, and nested aggregates. Ensure `Send + static` capture
checking is shared by threads and detached tasks.

### 9. Implement resource management

Separate automatic memory destruction from external-resource cleanup. Implement
`Disposable`, `AsyncDisposable`, `Symbol.dispose`, `Symbol.asyncDispose`,
`using`, and `await using` through ownership-aware HIR/MIR cleanup scopes.
Prove exactly-once cleanup for success, typed error, cancellation, constructor
failure, partial initialization, early `.close()`, and manual disposal calls.

### 10. Normalize strings, bytes, numerics, collections, and iterators

Converge the public standard library and compiler model on primitive `string`,
`&str`, `&[u8]`, `Bytes`, `BytesMut`, contextual numeric inference, and one
collection vocabulary. Remove public wrapper families, `StringMap`, competing
constructors, duplicate slice names, ownership-copying `Array.get`, and
suffix-heavy source workarounds. Make iteration O(n), non-consuming, and
borrow-aware. Add growth, hashing, order, cleanup, and allocation tests.

### 11. Remove public wrapper APIs

Complete the source migration for `ByteView`-family types, `Utf8View`-family
types, `StringMap`, hashed lookup methods, checked constructor suffixes, and
duplicate free functions. Keep private hash caches and low-level helpers only
behind ordinary public methods and reviewed unsafe/FFI boundaries.

### 12. Move protocol parsing out of the runtime

Keep generic byte primitives in the runtime and `std/bytes`. Move CRLF framing,
RESP marker validation, incomplete-line states, parsed Redis numbers, and
protocol error statuses to `validation/redis/resp.tn`. Remove Redis-shaped
runtime helpers and prove that the same byte primitives are exercised by
non-Redis validation.

### 13. Normalize filesystem, networking, threads, and errors

Expose typed `File`, `Directory`, `TcpListener`, `TcpStream`, `UdpSocket`,
`Thread`, and `JoinHandle` APIs. Hide descriptors, pointers, output parameters,
integer event masks, pthread result codes, and platform structures. Keep raw
variants explicitly unsafe and internal. Use nominal typed errors with optional
raw codes and remove public `Checked` suffixes.

### 14. Replace thread-backed tasks with executor tasks

Implement awaitable `Task<T, E>` values and structured `TaskGroup.spawn`.
Group exit waits for or cooperatively cancels children. Detached execution is
explicit and requires owned/static captures. Remove one-pthread-per-task code
and public task accounting methods.

### 15. Connect nonblocking I/O to the reactor

Implement one reactor watching many descriptors through `kqueue` or `epoll`,
typed `IoEvent` handles, registration ownership, deregistration, timers,
cancellation, readiness wakeups, and writer backpressure. Ensure no async
filesystem or networking operation performs indefinite blocking on an executor
thread. Test fragmented reads, partial writes, slow clients, cancellation,
descriptor closure, and task wake ordering.

### 16. Migrate Redis to the final surface

Use readable command expressions or ordinary string switches, not handwritten
numeric keys. Cover PING, ECHO, SET, GET, DEL, EXISTS, INCR, EXPIRE, TTL,
COMMAND, and QUIT; fragmented/coalesced and pipelined frames; malformed input
and limits; borrowed-data lifetime safety; zero-allocation one-million-PING and
existing-key GET paths; flat RSS; and address/undefined/thread sanitizers.

### 17. Build the cross-language performance suite

Implement equivalent TypeNative, Rust, C, Zig, Go, and meaningful Node
programs for JSON parsing, HTTP log analysis, HTTP serving, collections, text,
file processing, task scheduling, numeric kernels, trees/graphs, and a Node
addon. Use identical algorithms, inputs, checksums, observable output,
release optimization, two warmups, nine shuffled measured samples, medians,
bootstrap 95% confidence intervals, and p50/p95/p99 server latency.

Record CPU time, wall time, peak and steady-state RSS, allocations, copied
bytes, syscalls, context switches, startup time, compilation time, and binary
size. Separate cold and hot measurements. Profiles are evidence; benchmark-
specific compiler special cases are forbidden.

### 18. Enforce the compiler performance guard

Measure clean Rust builds, incremental builds, and every `tn` command used by
verification. Fail any single compiler invocation at 175 seconds. If a normal
verification command exceeds three minutes, stop feature work and profile it
before proceeding. Emit phase timings for parsing, HIR, ownership, MIR,
monomorphization, LLVM, linking, and test orchestration. Cache whole-program
ownership facts and keep output reachability minimal.

### 19. Run the active verification matrix

Run formatting, Rust tests, Clippy, Rust documentation, syntax fixtures,
parser recovery, formatter round trips, type/effect/ownership/lifetime tests,
MIR validation, LLVM verification, LSP/CLI/documentation tests, standard-
library validation, ABI and Node checks, reactor/task stress tests, Redis
protocol and sanitizer checks, fuzzing, and the complete performance suite.
Record command, host, inputs, result, duration, and artifact hashes. Every
failure remains an active blocker; historical Gate claims do not override it.

### 20. Keep self-hosting frozen, then plan its migration separately

After all active gates have passed and the language/runtime surface is stable,
create a new separately reviewed plan for migrating `compiler-tn/**`. Until
then, preserve its exact bytes and keep the freeze verification read-only.

## 5. Verification gates

| Gate | Required evidence |
| --- | --- |
| Design | Six canonical documents agree; rejected alternatives are recorded |
| Syntax | Canonical positive, obsolete negative, recovery, diagnostic, and formatter fixtures |
| Semantics | Ownership, lifetimes, effects, class dispatch, numerics, and cleanup are consistent across HIR/MIR/backend |
| Runtime | Generic bytes, executor, reactor, threads, tasks, timers, and resource ownership pass focused tests |
| Product | Filesystem, networking, standard library, Node, ABI, and Redis applications pass native checks |
| Safety | AddressSanitizer, UndefinedBehaviorSanitizer, ThreadSanitizer, fuzzing, and lifetime/ownership rejection pass |
| Performance | Reproducible cross-language suite and compiler-time guard pass without special cases |
| Boundary | Frozen self-host manifest and protected benchmark change remain unchanged |

## 6. Completion definition

Active convergence is complete only when the Rust compiler accepts one
canonical syntax, rejects all removed forms, exposes no compiler-owned source
decorators or macros, implements lifetime-safe borrowed APIs, automatic memory
cleanup plus typed resource disposal, one coherent collection/string/bytes
surface, distinct threads/tasks, a real reactor-backed async runtime, typed safe
system APIs, Redis validation without protocol logic in the runtime, and the
reproducible performance suite. The active verification matrix and all safety
gates must pass, while the frozen self-host tree and protected benchmark file
remain byte-for-byte unchanged.
