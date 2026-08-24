# TypeNative Active Design Audit

## 1. Audit status

This document records the decisions that make the active language coherent.
The Rust compiler, standard library, runtime, tooling, validation sources, and
benchmarks must follow these decisions together. Any implementation that
requires a competing public spelling is a design failure and must stop for
review.

The self-hosted compiler and bootstrap orchestration are frozen. Historical
self-host claims are evidence about an earlier checkpoint, not active design
authority or acceptance evidence.

## 2. Closed decisions

| Area | Decision | Rejected alternative and reason |
| --- | --- | --- |
| Compiler authority | The Rust compiler under `crates/tn-*` is the only active compiler | Editing two compiler implementations at once creates grammar and semantic drift |
| Public lifetimes | Use elision, named `lifetime` parameters, and rare `static` | Public `scope` exposes an internal category and makes signatures harder to understand |
| Returned borrows | Infer relationships from bodies and publish contracts in module metadata | Requiring explicit lifetime syntax on every ordinary function creates needless surface noise |
| Classes | `class` is uniquely owned heap identity; `struct` is inline nominal value | Treating both as interchangeable hides allocation and ownership behavior |
| Inheritance | Support `abstract`, `extends`, `implements`, `override`, visibility, `readonly`, and `static` | `sealed`, `final`, and derivation claims create competing hierarchy controls; closed dispatch is an optimizer proof |
| Conformance | Use nominal `implements` in the declaration | Compiler conformance decorators make ownership of the relationship unclear |
| Copy and thread safety | Infer recursively from fields and representation | Source assertions can lie about files, locks, sockets, and shared state |
| Clone | Handwritten ordinary `.clone()` method | Implicit clone has type-specific resource semantics that cannot be inferred safely |
| Destruction | Compiler automatically destroys owned memory and initialized fields | A user `drop()` hook can double free, skip partial cleanup, or throw from cleanup |
| External cleanup | `Disposable`, `AsyncDisposable`, `using`, `await using`, and idempotent close | Memory destruction and external resource closure have different contracts |
| Decorators | Keep only ordinary user-defined decorators for supported class elements | Compiler-owned decorators create a second hidden type/ABI language |
| Macros | No source macro or expansion system | A token-template language duplicates ordinary declarations and complicates tooling |
| Compiler intrinsics | Private manifest by declaration identity and approved module | A forgeable source `@Intrinsic` attribute is not a trusted boundary |
| Strings | Primitive owned UTF-8 `string`, borrowed `&str`, explicit conversion methods | Boxed strings and wrapper families create competing ownership and indexing models |
| Bytes | `&[u8]`, `Bytes`, and `BytesMut` with one slice vocabulary | Public `ByteView`/`Utf8View` layers leak implementation caching and lifetime categories |
| Numeric literals | Bidirectional contextual inference plus explicit suffixes | Requiring suffixes everywhere is noisy; silent widening/truncation is unsafe |
| Collections | One TypeScript-shaped vocabulary with borrowed lookup and O(n) iteration | `Vector`, `StringMap`, hashed public methods, and ownership-copying getters split semantics |
| Errors | `Promise<T, E>` plus synchronous `throws E` and nominal error types | Public `Result`/`Option` wrappers and `Checked` suffixes duplicate effects and APIs |
| Threads and tasks | OS threads expose join/detach; tasks are executor values | One-pthread-per-task hides scheduling cost and defeats structured cancellation |
| Reactor | One readiness reactor wakes many nonblocking tasks and owns timers/cancellation | Blocking inside async functions stalls unrelated tasks and makes backpressure invisible |
| Filesystem/networking | Typed safe wrappers hide descriptors, pointers, masks, and platform structures | Public OS-shaped APIs move unsafe invariants into every application |
| Protocol layering | RESP belongs to Redis validation/application code; bytes remain generic | Redis-shaped runtime helpers make foundational APIs application-specific |
| Performance | Use equivalent implementations, identical inputs, statistical samples, and resource counters | A single throughput number or benchmark-specific compiler path is not evidence |
| Self-hosting | Frozen until active convergence and all active gates pass | Treating self-hosting as a current gate forces migration while the language is still changing |

## 3. Ownership, error, and runtime contracts

Every public API must state:

- whether inputs are borrowed, moved, copied, or cloned;
- the lifetime relationship of returned references or aggregates;
- the typed error and cancellation behavior;
- the cleanup behavior on success, error, cancellation, and partial failure;
- whether the operation may suspend or block; and
- the ABI/FFI boundary, if any.

The implementation must reject APIs that cannot express those contracts. A
runtime optimization is valid only after the ordinary contract is checked and
must be reusable outside a single application protocol.

## 4. Rejected spelling inventory

These spellings are intentionally listed so future work cannot revive them as
aliases:

```text
scope
@Copy  @Clone  @Drop  @Send  @Sync  @Conform  @Sealed
@Layout  @Export  @Intrinsic  @Inline  @Test  @Expand
macro
sealed  final  derives
ByteView  Utf8View  HashedUtf8View  AsciiKeyUtf8View  StringMap
Vector  FixedArray  ReadonlySlice  MutableSlice
getHashed  removeHashed  setBorrowed
Checked
```

The parser, formatter, diagnostics, LSP, documentation generator, and Node
declaration generator must agree that these are not current public features.
Intentional negative fixtures and historical evidence are the only places where
the spellings may remain visible.

## 5. Active audit checklist

- [ ] Six canonical documents agree on grammar and ownership semantics.
- [ ] Every rejected spelling has a negative, recovery, diagnostic, and
      formatter fixture.
- [ ] The active source tree has no compiler-owned decorator or macro usage.
- [ ] Public lifetime signatures contain no internal category names.
- [ ] Public bytes, strings, and collections use one vocabulary.
- [ ] Runtime and foundational bytes contain no RESP state machine.
- [ ] Tasks suspend through the executor/reactor rather than spawning an OS
      thread per task.
- [ ] Safe system APIs hide raw OS representation.
- [ ] Benchmark claims include statistical and resource evidence.
- [ ] The self-host freeze manifest and protected benchmark change are intact.
