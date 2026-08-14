# Canonical TypeNative Migration Manifest

This manifest is the Gate 9/10 traceability ledger for the Rust bootstrap
compiler and all non-protected TypeNative sources. `compiler-tn/**` and the
self-hosting scripts are deliberately outside this manifest's edit and test
scope. A source construct is complete only when the parser rejects the old
spelling, the formatter never emits it, the semantic layer reports the listed
condition, and the corresponding positive and negative fixtures pass.

## Public-language convergence

| Obsolete construct or spelling                                       | Canonical replacement                                                                                                                                               | Compiler layers                                                                                                                   | Non-protected source locations                                                           | Formatter / diagnostic / rejection evidence                                                                                                       |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `impl Type`, `impl Interface for Type`                               | Direct members inside `struct`, `class`, or `interface`; `@Conform(Interface)` declares conformance                                                                 | lexer keyword table, parser declaration/member grammar, HIR conformance lowering, method lookup, ownership facts, MIR dispatch    | `std/**/*.tn`, `validation/**/*.tn`, `tests/**/*.tn`, documentation examples             | Formatter prints direct members; `SYNTAX_OBSOLETE_CONSTRUCT`; `tests/syntax/invalid/obsolete-impl.tn`                                             |
| obsolete foreign declaration block `extern "C" { ... }`            | `declare extern "C" { ... }`; `extern "C" function(...)` remains the foreign function-pointer type                         | lexer keyword table, parser recovery, lossless CST, HIR lowering, formatter, LSP, diagnostics, source scans                     | runtime, `std/**/*.tn`, `validation/**/*.tn`, benchmarks, fixtures, and current docs      | `SYNTAX_OBSOLETE_EXTERN_BLOCK` with a machine-applicable `declare ` insertion; canonical-source scan and focused parser tests                         |
| `where T: Interface`                                                 | `T extends Interface` in the generic parameter list                                                                                                                 | parser generic parameters, HIR generic bounds, signature and inference checking                                                   | all `.tn` sources and Rust source fixtures containing `where`                            | Formatter prints `extends`; `SYNTAX_OBSOLETE_CONSTRUCT`; `obsolete-where.tn`                                                                      |
| `self`, `&self`, `&mut self`                                         | contextual `this`, `&this`, `&mut this` only where a receiver mode is required                                                                                      | lexer contextual-name handling, parser expressions/types, constructor analysis, body checker, diagnostics                         | class/struct methods in `std`, `validation`, and fixtures                                | Formatter prints `this`; `SYNTAX_SELF_NOT_ALLOWED`; `obsolete-self.tn`                                                                            |
| `match`                                                              | exhaustive `switch`                                                                                                                                                 | lexer/parser expression and pattern grammar, HIR/MIR switch lowering, exhaustiveness diagnostics                                  | `validation/c/exports.tn`, semantic fixtures, docs                                       | Formatter prints `switch`; `SYNTAX_OBSOLETE_CONSTRUCT`; `obsolete-match.tn`                                                                       |
| `dyn Interface` / `&dyn Interface`                                   | named interface value or compiler-selected dynamic interface representation; no `dyn` token                                                                         | type grammar, HIR type resolution, cast and witness checking, Node-API validation                                                 | standard-library and conformance signatures                                              | `SYNTAX_OBSOLETE_CONSTRUCT`; `obsolete-dyn.tn`                                                                                                    |
| `Promise<T> throws E`, `Task<T> throws E`                            | `Promise<T, E>` and `Task<T, E>`; `never` denotes an empty completion error set                                                                                     | parser/type parser, HIR promise error arguments, async effect checking, MIR suspend/error edges, LLVM ABI, docs/Node declarations | `std/async.tn`, `validation/async/*.tn`, Redis fixtures                                  | Formatter emits two promise arguments; `TYPE_PROMISE_ARITY`; `promise-arity.tn`                                                                   |
| `Option<T>`, `Result<T, E>` as public types                          | `T                                                                                                                                                                  | undefined`for absence; closed`throws` effects for recoverable errors                                                              | predeclared names, type parser, effect checker, optional narrowing, diagnostics          | `std/core.tn`, `std/async.tn`, validation and fixtures                                                                                            | `TYPE_OBSOLETE_PUBLIC_TYPE`; `obsolete-option-result.tn` |
| Uppercase `String`, `.toString()`, and procedural string helpers     | owned `string`, borrowed `&str`, declared canonical methods, and contextual literal-to-owned conversion                                                             | prelude loading, intrinsic-type binding, ordinary member resolution, ownership, HIR/MIR conversion, fat-slice ABI, LLVM lowering  | `std/string.tn`, `std/env.tn`, `std/path.tn`, validation, documentation examples         | `TYPE_OBSOLETE_PUBLIC_TYPE`; intrinsic-binding rejection; canonical string debug/optimized native validation                                      |
| `Vec`, `VecDeque`, `HashMap`, `HashSet`, `BTreeMap`, `BTreeSet`      | `Array`, `Queue`, `Deque`, `Map`, `Set`, `OrderedMap`, `OrderedSet`, `Heap`                                                                                         | HIR nominal names, collection constructors and capability checks, Node mappings, docs                                             | `std/collections.tn`, `validation/collections/main.tn`, Redis sources                    | Formatter preserves only canonical names; `TYPE_OBSOLETE_COLLECTION`; `obsolete-collections.tn`                                                   |
| `withCapacity`, `Arc.new`, `Mutex.new`, free allocation constructors | `new Type({ capacity: n })` or `new Type()`; `Type.from(value)` is conversion only                                                                                  | parser `new`, constructor typing, capacity/overflow APIs, diagnostics                                                             | collections, sync, async, Redis, validation                                              | `TYPE_COMPETING_CONSTRUCTOR`; `obsolete-constructors.tn`                                                                                          |
| `?`-only optional access and unchecked assertions                    | `?.`, `??`, postfix force-unwrapping `!`, checked `as?`, explicit narrowing                                                                                         | postfix parser, HIR optional-chain state, body checker, MIR branch/projection lowering                                            | Redis and optional fixtures                                                              | Formatter emits postfix `!`; `TYPE_FORCE_UNWRAP_NON_OPTIONAL`; `force-unwrap.tn`                                                                  |
| `use`, `mod`, `pub`, `crate`, default exports                        | named `import` / `export`; deterministic relative and bundled `std/` modules                                                                                        | lexer excluded-keyword table, module scanner/resolver, formatter, CLI diagnostics                                                 | all source and docs                                                                      | `RESOLVE_OBSOLETE_MODULE_SYNTAX`; `obsolete-modules.tn`                                                                                           |
| implicit numeric widening and suffix-heavy contextual code           | local bidirectional numeric inference; integer default `number`, decimal default `f64`, explicit suffixes remain                                                    | literal lexer, HIR/type inference, constructor and generic argument checking, LLVM numeric lowering                               | Redis, collections, async, native validation                                             | `TYPE_NUMERIC_CONTEXT_REQUIRED` only where ambiguous; `numeric-context.tn`                                                                        |
| executable decorators and unrestricted source macros                 | compiler-owned `@Copy`, `@Clone`, `@Drop`, `@Send`, `@Sync`, `@Conform`, `@Sealed`, `@Layout`, `@Export`, `@Intrinsic`, plus typed deterministic declaration macros | attribute registry, HIR metadata, expansion boundary, safety/effect/ABI validation, diagnostics                                   | `validation/decorators/*.tn`, `validation/macros/main.tn`, and all non-protected sources | formatter canonicalizes names; `TYPE_UNKNOWN_ATTRIBUTE`, `MACRO_SANDBOX_VIOLATION`, `MACRO_NAME_COLLISION`, and direct positive/negative fixtures |

## Redis acceptance traceability

| Acceptance requirement                                      | Canonical source location                              | Rust bootstrap ownership                                                                                                                     |
| ----------------------------------------------------------- | ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Incremental RESP reads and fragmented frames                | `validation/redis/resp.tn`                             | ordinary Rust-bootstrap checks, canonical byte-buffer methods, and the native protocol harness pass; canonical ASan/UBSan and TSan runs pass |
| RESP encoding, pipelining, malformed frames, size limits    | `validation/redis/resp.tn`, `docs/redis-acceptance.md` | `switch` exhaustiveness, checked arithmetic, `BytesMut` methods, typed errors, native protocol evidence, and sanitizer runs pass             |
| Shared database and mutex guard cleanup before suspension   | `validation/redis/redis-server.tn`                     | `Map<string, string>`, `Mutex`, `using`, `await using`, borrow-across-await checks, drop lowering, native execution, and sanitizer runs pass |
| Concurrent structured clients                               | `validation/redis/redis-server.tn`                     | `TaskGroup.spawn` and structured worker cleanup are represented; canonical concurrency and sanitizer evidence passes                         |
| Required PING/SET/GET/DEL/unknown exchanges                 | `validation/redis/main.tn`, `docs/redis-acceptance.md` | all four Redis entrypoints pass ordinary compiler checks; canonical debug/optimized build and protocol harness pass                          |
| No project-controlled native implementation in the canonical target | `docs/gate10-native-inventory.md`                   | `scripts/check-native-sources.sh`, ordinary TypeNative runtime, and the complete verification matrix pass                                  |

## Gate 10 native-source disposition

Every previously inventoried project-owned native implementation has been
retired from the worktree. External system implementations remain dependencies
behind explicit TypeNative `declare extern "C"` declarations.

| File                           | Disposition                                                                          | Replacement / evidence                                                                              |
| ------------------------------ | ------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| Project runtime and startup implementation | retired | `runtime/runtime.tn` and generated TypeNative startup module |
| Redis implementation and lifecycle fixture | retired | `validation/redis/*.tn` |
| C ABI provider and caller fixtures | retired | `validation/c/exports.tn`, `validation/c/extern.tn` |
| Runtime ownership fixture | retired | `validation/runtime/main.tn` |

The source scan found no present project-owned C, header, C++, Objective-C, or
handwritten assembly file. Generated assembly and JSON under `build/` remain
artifacts, not source implementation; they are excluded from the source-free
migration scan and must never become implicit compiler inputs.

## Current preparation evidence

On 2026-08-12 the ordinary Rust bootstrap compiler checks for the canonical Redis
sources passed with no diagnostics:

```text
cargo run -q -p tn-cli -- check validation/redis/resp.tn --json
cargo run -q -p tn-cli -- check validation/redis/redis-server.tn --json
cargo run -q -p tn-cli -- check validation/redis/main.tn --json
cargo run -q -p tn-cli -- check validation/redis/main-alt.tn --json
PASS: all four commands produced no diagnostics
```

The canonical native executable and protocol harness now also pass in both
debug and optimized profiles via `scripts/verify-redis.sh`. Its explicit
AddressSanitizer/UndefinedBehaviorSanitizer and ThreadSanitizer Redis runs
also pass. The project-owned native implementation files are now retired. The protected `compiler-tn/**`,
`scripts/bootstrap-self-host.sh`, and historical A/B/C/fixed-point artifacts
were not inspected as implementation targets or executed by this goal.

## Required rejection inventory

Every obsolete spelling above has a positive canonical fixture, a localized
negative fixture, and a formatter assertion. The Rust compiler tests must also
exercise recovery after each rejected construct and assert the structured
condition identifier, primary span, and replacement guidance. No compatibility
alias may be added to make an old fixture pass.
