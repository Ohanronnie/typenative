# TypeNative Implementation Plan

## 1. Delivery model

This plan orders implementation work without changing the language between
ordinary implementation gates. The complete behavior is defined in the
[language specification](language-spec.md) before compiler construction begins.
A gate changes implementation coverage, not source semantics, except for the
dedicated language-surface convergence gate below. That gate may amend the
canonical language only by updating the specification, architecture, design
audit, fixtures, and implementation obligations together before implementation
continues.

Work may proceed in parallel within a gate when dependencies permit. A later
gate may begin only after every required acceptance check of its predecessors is
green on macOS ARM64.

Every gate follows the same completion loop:

1. implement the smallest complete vertical slice for the gate;
2. add positive, negative, recovery, and diagnostic fixtures before broadening
   the slice;
3. run formatters and static analysis;
4. run platform tests and the applicable sanitizers;
5. update canonical documents when an intentional design decision changes; and
6. record exact toolchain and artifact identities in CI.

### Current convergence record (2026-08-12)

The canonical TypeNative surface is being applied to the ordinary Rust
bootstrap path. The source-of-truth documents are synchronized with the
compiler fixtures and the Redis acceptance sources in `validation/redis/*.tn`.
The compiler-independent preparation records are:

- [`canonical-migration-manifest.md`](canonical-migration-manifest.md) for
  obsolete-spelling rejection, source traceability, and native dispositions;
- [`redis-acceptance.md`](redis-acceptance.md) for the canonical RESP/server
  behavior; and
- [`gate10-native-inventory.md`](gate10-native-inventory.md) and
  [`gate11-preparation.md`](gate11-preparation.md) for native-source retirement
  and independent-compiler entry evidence.

The four Redis sources pass ordinary bootstrap compiler checks. Native runtime
execution, sanitizer evidence, project-owned native-source retirement, and
independent self-hosting remain separate acceptance gates. The protected
`compiler-tn/**` source and self-hosting orchestration are not part of this
convergence pass.

## 2. Gate zero: canonical design

### Deliverables

- Normative grammar and language semantics.
- Compiler, runtime, ABI, and self-hosting architecture.
- This ordered implementation plan.
- A design audit mapping every source-plan conflict to one resolution.
- A repository README that links the canonical documents and states scope.

### Acceptance

- Every code example is accepted by the documented EBNF and semantic rules.
- Terminology and links are consistent across all documents.
- Source naming, CLI naming, configuration naming, targets, and emit products
  have one definition.
- Safety, overflow, panic, drop, and error behavior do not vary by build profile.
- A terminology scan finds no obsolete source, CLI, configuration, dependency
  command, deferred-safety, or profile-dependent-semantic claims outside the
  audit.
- The original Downloads documents retain their recorded hashes.

## 3. Gate one: syntax and tooling foundation

### Deliverables

- Rust workspace and crate boundaries from the architecture document.
- Verified LLVM 22.1.8 toolchain manifest, Inkwell 0.10.0 `llvm22-1` binding,
  and setup check.
- Logos lexer retaining comments and whitespace.
- Recovery-capable recursive-descent/Pratt parser producing a Rowan CST.
- Typed AST wrappers and stable source spans.
- Canonical formatter backed by the CST.
- Structured diagnostic records and text/JSON renderers.
- `tn fmt`, parse-only `tn check`, and syntax-aware `tn lsp` diagnostics.

### Test inventory

- One fixture for every lexical token and grammar production.
- Valid and invalid UTF-8, Unicode identifier, escape, comment nesting, and
  template interpolation cases.
- Missing delimiter, unexpected keyword, malformed generic, and nested recovery
  snapshots.
- Parse-format-parse equivalence and byte-idempotence corpora.
- Property tests generating balanced token trees.
- Coverage-guided lexer and parser fuzz targets with checked-in minimized seeds.

### Acceptance

- Every normative syntax example parses without error.
- Every excluded syntax form produces a localized diagnostic and recovery tree.
- Formatting the full fixture corpus twice produces no second diff.
- A one-token edit reparses only the affected syntax region in the language
  server test harness.
- No parser panic, unbounded recursion, or nontermination occurs under fuzzing.

## 4. Gate two: semantic core and ownership

### Deliverables

- Module graph, exact local/std import resolution, visibility, and constant
  initialization checks.
- HIR lowering with stable declaration identities and source origins.
- Primitive, array, slice, tuple, string, optional, struct, enum, interface,
  class, reference, raw-pointer, and function types.
- Local bidirectional inference and contextual literal/object/closure typing.
- Generic constraints, monomorphization planning, interface coherence, and
  operator-interface resolution.
- Class inheritance validation, overrides, explicit conformance, RTTI model, and
  checked cast typing.
- Closed error effects, explicit `try`, exhaustive catches, match exhaustiveness,
  and unreachable-pattern diagnostics.
- Generic MIR construction and validation.
- Definite initialization, affine moves, partial moves, non-lexical borrows,
  lifetime checking, and `Copy`/`Drop` rules.
- `Send`/`Sync` constraint analysis for thread and detached-task captures.

### Test inventory

- Compile-pass and compile-fail fixtures for every primitive and compound type.
- Inference tests with and without expected types; public annotation failures.
- Coherence, private access, import cycle, and constant initialization tests.
- Move-after-use, partial move, move-from-borrow, double mutable borrow, shared
  versus mutable overlap, returned local reference, and lifetime relation tests.
- Struct and enum patterns, optional narrowing, and exhaustiveness witnesses.
- Class construction, override variance, base/derived access, final methods,
  interface coercion, `instanceof`, and owned/borrowed downcasts.
- Throw-set propagation, missing `try`, redundant catch, missing catch, and error
  narrowing tests.
- Generic MIR validator mutation tests that deliberately corrupt one invariant.

### Acceptance

- `tn check` validates all language conformance fixtures without invoking LLVM.
- Safe fixtures containing known use-after-free, aliasing, or data-race patterns
  are rejected at their causal source spans.
- Every diagnostic contains a condition identifier, primary span, and actionable
  explanation.
- The semantic pipeline is deterministic under randomized source-file discovery
  and hash-map seeds.

## 5. Gate three: native execution

### Deliverables

- Drop elaboration for normal and typed-error control-flow edges.
- Tagged success/error lowering without native unwinding.
- Reachability and monomorphization engine.
- LLVM type, function, control-flow, checked arithmetic, and bounds lowering.
- Target machine, object emission, linker driver, and DWARF source information.
- Runtime startup, allocator hooks, class descriptors, panic/abort, and entry
  dispatch.
- `tn build`, `tn run`, and emit modes for object, LLVM IR, bitcode, assembly,
  executable, and shared library.
- TypeNative implementations of `std/core`, the single-threaded and allocator
  foundations of `std/alloc`, `std/fmt`, `std/console`, and the core of
  `std/testing`.

### Test inventory

- LLVM verifier tests for every generated module.
- MIR-to-IR structural snapshots for checked arithmetic, indexing, drops, enum
  switches, direct calls, virtual calls, and witness calls.
- Runtime tests for constructor failure, nested error propagation, partial moves,
  drop order, and process exit behavior.
- Numeric boundary tests for every integer type and operation.
- Debug-information smoke tests in LLDB and GDB.
- AddressSanitizer and UndefinedBehaviorSanitizer runs over runtime and allocation
  fixtures.
- Console calls compile without an error effect and remain observable in every
  build profile; injected destination failures are discarded without panic or
  hidden failure state.

### Acceptance

- A multi-module program using structs, classes, enums, generics, borrowing,
  typed errors, console output, and deterministic destruction builds and runs on
  macOS ARM64.
- Every emitted LLVM module verifies before optimization and after optimization.
- Debug and optimized binaries produce identical observable results for the
  conformance corpus.
- Runtime and standard-library core tests are clean under applicable sanitizers.

## 6. Gate four: hosted standard library

### Deliverables

- `string`, UTF-8 validation, scalar iteration, and checked slicing.
- `Bytes`, `BytesMut`, and binary cursor APIs.
- `Array<T>`, fixed arrays, borrowed slices, `Queue<T>`, `Deque<T>`, and `Heap<T>`.
- `Map<K, V>` and `Set<T>` with explicit `Equal` and `Hash` capability
  requirements, plus `OrderedMap<K, V>` and `OrderedSet<T>`.
- Buffered and unbuffered I/O, standard streams, files, directories, and paths.
- TCP, UDP, address parsing, and DNS.
- Monotonic and wall-clock time.
- Process arguments, environment, child processes, and exit handling.
- C strings, dynamic libraries, raw allocation helpers, and reviewed FFI
  wrappers.
- Documentation extraction and `tn doc` over resolved public APIs.

### Test inventory

- Property tests for collection operations against simple reference models.
- Allocation-failure injection and exact drop-count tests.
- UTF-8 malformed-sequence, boundary, slicing, and round-trip tests.
- Short read/write, interrupted syscall, EOF, broken pipe, and file-permission
  tests proving that reliable writers report `IOError` rather than discarding
  failures.
- Loopback TCP/UDP, DNS failure, path edge, environment, and child-process tests.
- C interoperability tests compiled by Clang on macOS ARM64.
- Sanitizer runs over every module containing unsafe code.

### Acceptance

- All safe public APIs document the invariant upheld by their unsafe core.
- Standard-library tests pass with allocator failure injection.
- File, network, and process resources close exactly once on success and every
  typed-error path.
- Generated API documentation resolves every public type and link.

## 7. Gate five: concurrency and async

### Deliverables

- Threads, builders, joins, parking, and sleep.
- `Arc`, `Weak`, mutexes, read-write locks, condition variables, barriers,
  atomics, and channels.
- Future protocol, generic MIR suspension points, borrow checking across
  suspension, pinned state-machine lowering, and cancellation drops.
- Explicit executor, platform reactor using kqueue/epoll, timers, async network
  and file adapters, tasks, task groups, and explicit detach.
- Async-aware debugger metadata and task diagnostics.

### Test inventory

- `Send`/`Sync` positive and negative compile fixtures.
- Mutex, read-write lock, condition variable, channel, and atomic stress tests.
- ThreadSanitizer runs over synchronization and shared-container tests.
- Async state-machine snapshots for zero, one, and multiple suspension points.
- Cancellation in every state with exact destructor-count assertions.
- Borrow-across-await acceptance and rejection tests.
- Structured task completion, error propagation, parent cancellation, and
  detached-task ownership tests.
- High-connection loopback echo and backpressure tests.

### Acceptance

- Safe concurrency fixtures are free of data races under ThreadSanitizer.
- A borrowed structured child cannot outlive its task group; an equivalent
  detached task is rejected.
- Dropping any in-progress promise cancels and destroys initialized state once.
- The async TCP echo server handles concurrent clients on macOS ARM64
  without a hidden global executor.

## 8. Gate six: C and Node-API interop

### Deliverables

- Complete C ABI signature validation and C header generation.
- Shared-library exports with explicit symbols and target layouts.
- Node-API wrapper MIR, module initialization, class finalizers, byte-buffer
  ownership, sync exception translation, and async promise bridging.
- `.d.ts` generation from the same resolved export model.
- `tn build --emit node-addon` producing exactly one `.node` and one `.d.ts`.

### Test inventory

- C calls into TypeNative and TypeNative calls into C for every legal ABI type.
- Rejection tests for strings, references, classes, optionals, generic types,
  async functions, and typed errors in raw C signatures.
- Node tests for every documented scalar mapping, optionals, arrays, strings,
  bytes, exceptions, async rejection, classes, and finalization.
- Borrowed-return and use-after-wrapper-return rejection tests.
- Repeated module load/unload and worker-context tests using only Node-API.
- AddressSanitizer and UndefinedBehaviorSanitizer runs across C and Node-API
  wrappers, plus ThreadSanitizer for worker-context concurrency fixtures.

### Acceptance

- Generated C headers compile without warnings under Clang on macOS ARM64.
- Exported C layouts match Clang `sizeof`, alignment, and offset probes.
- Generated Node declarations match runtime exports exactly.
- Native class owners and external byte buffers finalize exactly once under
  forced JavaScript garbage collection tests.
- The addon has no imports from V8, libuv, or Node.js C++ internals.

## 9. Gate seven: Redis systems validation

### Deliverables

- RESP2 parser and encoder over `Bytes` and `BytesMut`.
- Thread-safe key/value store with expiration.
- TCP server using the explicit async executor and structured per-connection
  tasks.
- Commands: PING, ECHO, SET with expiration, GET, DEL, EXISTS, INCR, EXPIRE,
  TTL, COMMAND, and QUIT.
- Operational metrics exposed through ordinary application code rather than
  compiler-special logging.

### Test inventory

- Parser property tests with fragmented and coalesced frames.
- Protocol malformed-input, size-limit, and incomplete-buffer tests.
- Store model tests, concurrent mutation tests, and deterministic clock tests for
  expiration.
- End-to-end command tests through the real `redis-cli`.
- Concurrent connection, slow-client, disconnect, cancellation, and shutdown
  tests.
- AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer runs.

### Acceptance

- Every listed command returns the documented RESP2 result through `redis-cli`.
- Expiration behavior is deterministic under an injected monotonic clock.
- Concurrent clients produce no data races, leaked tasks, leaked allocations, or
  double drops.
- Debug and optimized servers pass the same protocol suite.

## 10. Gate eight: self-hosting

### Deliverables

- TypeNative lexer/parser consuming the same grammar fixtures.
- TypeNative HIR, semantic checker, MIR, borrow checker, and diagnostics.
- TypeNative bindings to the pinned LLVM C API with a mandatory major check.
- TypeNative driver, CLI, formatter, documentation generator, and language
  server sharing the self-hosted syntax model.
- Deterministic compiler build profile and bootstrap orchestration.

Before the language-surface convergence work begins, Gate eight also includes
build-pipeline performance hardening. The self-hosting build must derive
whole-program ownership and drop facts once per compilation, reuse those facts
through body checking, MIR lowering, and drop elaboration, and read parsed
attribute data from HIR rather than re-lexing each source module for every
declaration query. Reachability roots must be minimal and output-aware: an
executable starts from `main`, while shared-library and Node-API builds start
from their validated explicit exports and required runtime/drop entry points.
The driver must expose opt-in phase timings for module loading/checking,
ownership analysis, MIR/drop preparation, monomorphization, LLVM emission, and
native linking so regressions can be diagnosed from the same command used by
the bootstrap harness.

### Test inventory

- Rust and TypeNative compiler differential results over the full conformance
  corpus.
- Normalized diagnostic-record comparisons rather than terminal text snapshots.
- Bootstrap compiler A, self-built compiler B, and repeated self-built compiler
  C.
- Deterministic artifact comparisons for B and C.
- Instrumented self-host builds with recorded phase timings and a regression
  check proving that ownership/drop derivation and source lexing are not
  repeated once per MIR body.
- Reachability fixtures containing unreachable non-generic functions and
  methods, plus shared-library and Node-API export fixtures proving that
  explicit exports remain emitted after root pruning.
- Complete platform, sanitizer, standard-library, interop, and Redis suites under
  B and C.

### Acceptance

- B and C artifacts match under identical declared inputs.
- B and C independently pass every test required by prior gates.
- The self-hosted compiler rejects a mismatched LLVM major before code
  generation.
- The optimized self-host build reports each requested phase when timing is
  enabled, and the reachability/analysis checks pass without changing emitted
  behavior or deterministic artifact identity.
- The Rust bootstrap remains able to reproduce compiler A from a clean checkout.

## 11. Gate nine: language-surface convergence

This is the controlled canonical-design convergence gate for the approved
TypeNative surface. It must not leave the implementation accepting two competing
spellings or semantics.

### Deliverables

- Update the language specification, compiler architecture, design audit,
  examples, and implementation-status ledger to the approved TypeNative surface.
- Make `struct` a nominal inline value declaration and `class` a uniquely owned
  heap identity declaration, with methods directly inside both declarations,
  class inheritance, `abstract`, `extends`, `override`, `super`, visibility,
  `readonly`, and `@Sealed`. Instance members use the contextual `this` name;
  `self` is not part of the canonical surface.
- Implement the ownership surface exactly as `T`, `&T`, `&mut T`, `mut`, and
  `move`, with inferred lifetimes, explicit receiver modes, partial-move rules,
  deterministic destruction, and `@Copy`, `@Clone`, `@Drop`, `@Send`, and `@Sync`.
- Implement object and array destructuring with TypeNative ownership semantics:
  copied inputs copy eligible fields, owned inputs move fields, borrowed inputs
  produce corresponding borrows, and partial moves remain visible to the borrow
  and drop checkers.
- Replace `Promise<T> throws E` with `Promise<T, E>` and retain
  `function f(): T throws E` for synchronous functions.
- Replace public `Result`, `Option`, `match`, `impl`, `extension`, `record`,
  `where`, `dyn`, `Vec`, `HashMap`, `HashSet`, `IntoIterator`, `use`, `mod`,
  `pub`, and `crate` forms with the canonical TypeScript-shaped alternatives.
- Implement direct capability decorators, `@Conform(...)`, `@Drop`,
  `@Intrinsic`, `@Sealed`, `@Layout("C")`, and `@Export(...)` with explicit
  compiler ownership of safety-sensitive decorators.
- Implement `T | undefined`, checked optional access, `!` force-unwrapping,
  typed `try`/`catch`, inline optional fallback, and exhaustive `switch` guards.
- Implement `unknown` narrowing, checked `as?` downcasts, strict equality,
  explicit numeric conversions, checked arithmetic, UTF-8 text views, `Bytes`,
  and explicit byte/scalar/grapheme string positions.
- Implement contextual numeric literals so assignments, arguments, returns, and
  generic instantiations infer the required numeric type. Unconstrained integer
  literals default to `number`, unconstrained decimal literals default to `f64`,
  and explicit suffixes such as `i32`, `usize`, and `f32` remain available when
  context is absent or the author intentionally selects a type.
- Implement `using`, `await using`, generators, async generators,
  `Iterable<T>`, `Iterator<T>`, `AsyncIterable<T>`, and `for await` lowering.
- Implement the complete collection surface: `Array<T>`, fixed arrays, borrowed
  slices, `Map<K, V>`, `Set<T>`, `OrderedMap<K, V>`, `OrderedSet<T>`, `Queue<T>`,
  `Deque<T>`, and `Heap<T>`, with explicit `Equal`/`Hash` requirements for keys.
  Fresh owned values use one construction form, `new Type(...)`, including
  `Arc`, `Mutex`, and collections. Collections accept an explicit constructor
  options object such as `{ capacity: 1024 }`; ordinary empty construction uses
  `new Array<T>()` or `new Map<K, V>()`. Static methods such as `Type.from(...)`
  are reserved for conversion from existing values rather than competing
  allocation constructors such as `withCapacity` or `Type.new`.
- Implement generic `extends` constraints, nominal interfaces, static and
  dynamic interface calls, `Send`/`Sync` capture checks, channels, mutexes,
  atomics, task groups, cancellation, and explicit detach semantics.
- Implement named `import`/`export` modules with no default exports, no `use`,
  `mod`, `pub`, or `crate`, and deterministic local/std path resolution.
- Implement `extern "C"`, raw pointers, `unsafe` blocks/functions, C layouts,
  exported symbols, and typed Node-API wrappers without hidden conversions.
- Complete the `tn` command surface, formatter, linter, documentation generator,
  language server, test runner, diagnostics, and deterministic package/build
  configuration against the same resolved syntax and semantic model.
- Implement typed user-defined declaration macros with deterministic,
  sandboxed, AST-level expansion and source-mapped diagnostics.
- Keep `&`, `&mut`, `mut`, `move`, `unsafe`, and `extern "C"` as the canonical
  low-level syntax.
- Remove obsolete parser, formatter, semantic, MIR, backend, standard-library,
  compiler-tn, documentation, and fixture spellings rather than accepting
  compatibility aliases.

### Test inventory

- Positive, negative, recovery, and formatter round-trip fixtures for every
  changed grammar production.
- Effect tests for synchronous `throws`, `Promise<T, E>`, `try await`, typed
  catches, optional fallbacks, and panic-producing `!` expressions.
- Ownership tests for `using`/`await using`, generator frames, yielded borrows,
  move captures, cancellation, destructuring copies, destructuring borrows,
  partial moves, and cleanup ordering.
- Collection model tests for insertion, lookup, deletion, iteration, collision,
  ordering, constructor capacity, automatic capacity growth, reservation,
  shrinking, missing values, and exact cleanup for `Array`, `Map`, and `Set`.
- Constructor tests proving that fresh `Array`, `Map`, `Set`, `Queue`, `Arc`,
  `Mutex`, and user-class values consistently use `new`, while `from` performs
  conversion and obsolete `withCapacity`, `Arc.new`, and `Mutex.new` forms are
  rejected.
- The complete native Redis program in
  [`redis-acceptance.md`](redis-acceptance.md), covering incremental RESP
  parsing, buffered encoding, contextual numeric literals, collection capacity,
  shared ownership, mutex-guard cleanup before suspension, typed async errors,
  structured client tasks, malformed input, pipelining, and concurrent clients.
- Macro expansion tests for methods, conformances, diagnostics, collisions,
  deterministic output, sandbox violations, and forbidden ABI/ownership changes.
- Differential Rust-bootstrap and TypeNative-compiler results over the complete
  syntax and semantic corpus.
- Explicit rejection tests proving every obsolete spelling is no longer parsed.

### Acceptance

- All canonical documents agree on one grammar and semantic model.
- The formatter emits only the canonical syntax.
- The parser rejects every obsolete spelling with a localized diagnostic.
- Generated macro code passes ordinary type, ownership, effect, and unsafe checks.
- `using`, generators, async generators, decorators, and `Promise<T, E>` work in
  both hosted and native execution fixtures.
- The canonical Redis acceptance program formats, compiles, links, passes its
  protocol and concurrency matrix, and runs cleanly under every required
  sanitizer on macOS ARM64 without project-owned C Redis logic.

## 12. Gate ten: TypeNative source migration and C-free repository

This gate makes the project-owned implementation TypeNative-first without
removing required interoperability with external native systems.

### Deliverables

- Maintain the native-source dispositions in
  [`gate10-native-inventory.md`](gate10-native-inventory.md) and do not retire
  a native boundary until its replacement has native protocol, ABI, and
  sanitizer evidence.
- Inventory every project-owned C, C++, and handwritten assembly source and
  assign it to port, delete, or externalize with a recorded reason.
- Port runtime logic, allocation helpers, strings, collections, Redis logic,
  validation services, and Node-API wrappers to TypeNative.
- Replace project-owned C shims with TypeNative `extern "C"` declarations and
  safe TypeNative wrappers.
- Keep libc, OS, LLVM, Node-API, and other external native libraries outside the
  repository; never copy their implementation sources into the project.
- Replace checked-in C test fixtures with TypeNative ABI tests and external
  compiler-generated probes that are not project-owned source artifacts.
- Add a deterministic source-tree check that rejects tracked `.c`, `.h`, `.cc`,
  `.cpp`, and handwritten assembly implementation files.
- Update build scripts, documentation, sanitizer commands, and validation
  manifests so normal TypeNative builds do not compile project-owned C.

### Test inventory

- C ABI layout, calling-convention, symbol, string, pointer, and ownership tests
  against external system libraries on macOS ARM64.
- Node-API addon and declaration tests with no C++ Node internals.
- Redis protocol, lifecycle, concurrency, and sanitizer tests using the
  TypeNative implementation.
- C-free source scans, clean-checkout builds, and dependency provenance checks.
- AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer coverage for
  every remaining FFI boundary.

### Acceptance

- No project-owned C/C++ source is required for `tn check`, `tn build`, `tn test`,
  standard-library validation, Redis validation, or Node-API validation.
- All external ABI tests pass on macOS ARM64.
- The repository contains no handwritten C implementation while retaining
  explicit `extern "C"` interoperability.
- TypeNative ownership and error rules remain visible at every foreign boundary.

## 13. Gate eleven: independent full self-hosting

This gate replaces the current bounded hosted self-hosting slice with a complete
TypeNative compiler pipeline.

### Deliverables

- Follow [`gate11-preparation.md`](gate11-preparation.md) for clean-checkout,
  normalized-diagnostic, discovery-order, artifact, and sanitizer evidence
  before beginning the independent compiler sequence.
- Complete TypeNative lexer, parser, CST/AST, HIR, type checker, ownership
  checker, MIR, macro expander, generator/async lowering, and LLVM backend.
- TypeNative implementations of the formatter, diagnostics, CLI, documentation
  generator, language server, test runner, and compiler macro host.
- Normal compiler builds that do not depend on the retained Rust compiler after
  bootstrap artifacts are established.
- Compiler A building compiler B, and compiler B building compiler C, with the
  same canonical language and source tree.

### Test inventory

- Full prior-gate syntax, semantic, native, standard-library, async, ABI,
  Node-API, Redis, sanitizer, and tooling suites run independently by B and C.
- Compiler differential tests over every valid and invalid conformance fixture.
- Macro, generator, `using`, async cancellation, and C-free migration suites
  executed by B and C.
- Clean-checkout bootstrap runs with randomized file discovery and hash seeds.

### Acceptance

- B and C are independently full LLVM-backed compilers, not bounded analysis
  wrappers or Rust-driver-assisted lowering slices.
- B and C pass every prior-gate acceptance check on macOS ARM64.
- Compiler and source fixed-point digests match under identical declared inputs.
- A clean checkout can reproduce the same compiler artifacts without hidden
  generated source or network access.

## 14. Gate twelve: final conformance and cross-host verification

This is the final evidence gate. It closes environmental blockers instead of
recording them as completion.

### Deliverables

- One complete verification matrix covering every command, language feature,
  standard-library API, sanitizer, ABI, Node-API, Redis, and bootstrap check.
- Native macOS ARM64 execution for every sanitizer and verification fixture.
- Final documentation, design-audit, source-tree, dependency, and terminology
  scans.
- Reproducible artifact and diagnostic-record manifests for compiler A, B, and C.

### Acceptance

- Every gate is complete on macOS ARM64.
- No required check is marked complete from an emulated or unavailable host.
- No genuine blocker remains in `docs/implementation-status.md`.
- The complete TypeNative source tree, compiler chain, runtime, standard library,
  tooling, interoperability, and validation projects pass the final matrix.

## 15. Continuous quality gates

These checks run throughout implementation:

AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer run on macOS
ARM64 for the runtime, every unsafe standard-library module, every FFI boundary,
threads, synchronization, async scheduling, Node worker contexts, and Redis.

| Area              | Required checks                                                              |
| ----------------- | ---------------------------------------------------------------------------- |
| Rust bootstrap    | `cargo fmt --check`, Clippy with warnings denied, unit and integration tests |
| TypeNative source | `tn fmt --check`, `tn check`, unit and integration tests                     |
| Syntax            | Fixture snapshots, idempotence, property tests, fuzzing                      |
| Semantics         | Compile-pass/fail corpus, MIR validator, diagnostic records                  |
| Native safety     | AddressSanitizer and UndefinedBehaviorSanitizer                              |
| Concurrency       | ThreadSanitizer and deterministic stress harnesses                           |
| LLVM              | Module verification before and after optimization                            |
| ABI               | Clang layout probes, symbol inspection, Node-API import inspection           |
| Docs              | Link check, example extraction, grammar coverage, terminology scan           |
| Determinism       | Randomized discovery-order test and repeated artifact digest                 |

No golden assembly is required: it is brittle across LLVM patches. Tests assert
MIR semantics, LLVM properties, executable behavior, and performance budgets
where a regression threshold has an explicit benchmark rationale.

## 16. Completion definition

TypeNative is complete against the canonical design when:

- all gates satisfy their acceptance criteria on macOS ARM64;
- every safe-language exclusion has a compile-fail test;
- every unsafe standard-library module has invariant documentation and sanitizer
  coverage;
- debug and optimized builds pass the same observable-behavior suite;
- C and Node-API products pass their complete ABI suites;
- the Redis system validation passes under concurrency and sanitizers; and
- the self-hosted compiler reaches a deterministic fixed point and passes the
  same conformance suite as the retained Rust bootstrap;
- the approved language surface, including macros, `using`, generators, and
  `Promise<T, E>`, is implemented without obsolete compatibility syntax;
- the project-owned implementation contains no handwritten C/C++ source; and
- final macOS ARM64 sanitizer and reproducibility evidence is complete.
