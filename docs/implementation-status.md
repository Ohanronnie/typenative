# TypeNative Implementation Status

This ledger records only checks that have actually run. A gate remains open until every acceptance
criterion in `docs/implementation-plan.md` has evidence on macOS ARM64.

Support-target decision recorded on 2026-08-11:

- macOS ARM64 is the only active TypeNative target.
- Linux and its sanitizer/toolchain matrix are not required support targets.
- Linux results below are retained as historical evidence only and cannot close
  or block a current gate.

The driver, CLI target enum, toolchain manifest, host checks, and code-generation test target
selection now enforce that decision: `aarch64-apple-darwin` is the only accepted target spelling.
After this target-surface cleanup, `cargo fmt --all -- --check`, `cargo test --workspace
--all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, `RUSTDOCFLAGS=-Dwarnings
cargo doc --workspace --no-deps`, and `sh -n scripts/*.sh` pass on macOS ARM64.

## Current controlled convergence record (2026-08-12)

The ordinary Rust bootstrap path now carries the canonical source convergence
work. The current source and evidence records are:

- [`language-spec.md`](language-spec.md) is the normative canonical surface;
- [`canonical-migration-manifest.md`](canonical-migration-manifest.md) records
  obsolete-spelling rejection and native-source dispositions;
- [`redis-acceptance.md`](redis-acceptance.md) defines the canonical RESP and
  server contract;
- [`gate10-native-inventory.md`](gate10-native-inventory.md) inventories every
  project-owned native file found in the repository; and
- [`gate11-preparation.md`](gate11-preparation.md) records clean-checkout,
  diagnostic, determinism, and sanitizer prerequisites without claiming the
  independent compiler gate.

The current documentation corpus contains eight `.tn` examples, and the
canonical example test parses all eight. The four Redis TypeNative entrypoints
pass ordinary `tn check --json` with no diagnostics. The canonical Redis
executable now also passes the protocol, concurrency, and canonical sanitizer
harnesses; native-source retirement and independent self-hosting remain open
acceptance work.

Ordinary verification completed on 2026-08-12:

```text
cargo fmt --all -- --check
PASS
cargo test --workspace --all-targets
PASS: all workspace unit, integration, fixture, property, CLI, LSP, semantic, ownership, MIR, and pipeline tests
cargo clippy --workspace --all-targets -- -D warnings
PASS
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
PASS
cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/runtime.c
cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/redis.c
PASS: strict runtime translation-unit compilation
cargo run -q -p tn-cli -- fmt --check std
cargo run -q -p tn-cli -- fmt --check validation
PASS: active TypeNative standard-library and validation sources are formatter-clean
cargo run -q -p tn-cli -- check validation/redis/resp.tn --json
cargo run -q -p tn-cli -- check validation/redis/redis-server.tn --json
cargo run -q -p tn-cli -- check validation/redis/main.tn --json
cargo run -q -p tn-cli -- check validation/redis/main-alt.tn --json
PASS: all four canonical Redis sources produced no diagnostics
```

Canonical native Redis acceptance completed on 2026-08-12:

```text
sh -n scripts/verify-redis.sh
PASS
cargo build -q -p tn-cli
./scripts/verify-redis.sh
PASS: builds the canonical TypeNative Redis server in debug and optimized profiles
PASS: CRUD, unknown commands, fragmented frames, pipelining, malformed input,
      invalid UTF-8, oversized and truncated frames, capacity growth, and 32 concurrent clients
redis-canonical-protocol=pass

REDIS_SANITIZER=address-undefined ./scripts/verify-redis.sh
PASS: redis-canonical-address-undefined-sanitizers=pass
REDIS_SANITIZER=thread ./scripts/verify-redis.sh
PASS: redis-canonical-thread-sanitizers=pass
```

The harness targets `validation/redis/main-alt.tn`; it does not run the legacy
`runtime/redis.c` application. Native-source retirement and the independent
self-hosting gate remain open.

The Gate zero and earlier matrix paragraphs below preserve historical evidence
from the previous document corpus. Their former fifteen-example count and
self-hosting results must not be read as current evidence for the rewritten
canonical corpus or as a final independent compiler result.

## Gate zero: canonical design

Status: complete.

Verified on 2026-08-09 on macOS ARM64:

- The five canonical documents and repository README were read completely before implementation.
- The workspace contains no filesystem `AGENTS.md`; the task-supplied workspace instructions are in
  force.
- The three original Downloads inputs retain their recorded SHA-256 hashes.
- LLVM 22.1.8 is installed through Homebrew.
- All 15 canonical `tn` examples parse in their documented top-level or function-body context.
- Canonical internal/external links, obsolete terminology, naming, and profile-semantics claims pass
  the automated design verification.

Commands and results:

```text
find /Users/ronnie/Downloads -maxdepth 2 -type f \( ... \) -print0 | sort -z | xargs -0 shasum -a 256
PASS: 05110b...ee3c57 TypeNative_Execution_Path.md
PASS: 3c877e...3d2f10 TypeNative_Expanded_Plan (1).md
PASS: 99db56...2895f TypeNative_Expanded_Plan.md

brew list --versions llvm
PASS: llvm 22.1.8

scripts/verify-design.sh
PASS: 13 Markdown links checked
PASS: 15 canonical TypeNative examples parsed
PASS: source hashes and terminology scan
PASS: design_verification=pass
```

Implementation decision recorded on 2026-08-09:

- Resolved the `@export` keyword/identifier grammar contradiction by adding `attribute_name` to the
  normative grammar and design-audit entry 104. The parser fixture exercises the resolved form.

## Gate one: syntax and tooling foundation

Status: complete.

Implemented:

- Rust workspace with every architecture-defined crate boundary, the exact LLVM/Inkwell manifest,
  strict project configuration, and host setup verification.
- Lossless Logos lexer with retained trivia, nested comments, Unicode XID identifiers, literal
  validation, pre-tokenization UTF-8 rejection, reserved excluded keywords, and delimiter-aware
  template interpolation using normal tokenization.
- Recovery-capable recursive-descent/Pratt parser, Rowan CST, checked AST wrappers, stable spans,
  bounded recursion and diagnostics, and smallest-top-level-region incremental reparsing.
- Idempotent CST-backed formatter with two-space indentation, deterministic import sorting, comment
  and literal preservation, and parse-format-parse validation over the fixture corpus.
- Structured text/JSON diagnostics, parse-only `tn check`, `tn fmt --check`, strict project input
  resolution, and incremental syntax diagnostics through `tn lsp`.
- Broad grammar/token fixtures, localized invalid snapshots, template nesting/expression fixtures,
  property tests, and checked-in lexer/parser fuzz targets with minimized regression seeds.

Verification on 2026-08-09:

```text
scripts/verify-design.sh
PASS: 13 Markdown links and all 15 canonical examples

scripts/check-toolchain.sh
PASS: LLVM 22.1.8; host_target=aarch64-apple-darwin

cargo test --workspace --all-targets
PASS: all workspace unit, integration, fixture, property, CLI, and LSP tests

cargo clippy --workspace --all-targets -- -D warnings
PASS

RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
PASS: 10 workspace crates documented

cargo +nightly fuzz run lexer -- -runs=10000 -max_len=4096 -timeout=5
PASS: 10,000 executions; no crash, timeout, or sanitizer finding

cargo +nightly fuzz run parser /tmp/typenative-fuzz.../parser -- -runs=10000 -max_len=4096 -timeout=5
PASS: 10,000 executions; no crash, timeout, or sanitizer finding

podman run --platform linux/amd64 rust:1.96-bookworm ... \
  cargo test -p tn-syntax -p tn-driver -p tn-cli --all-targets
PASS on x86_64 Linux: all 33 Gate 1 tests
```

Implementation decisions recorded on 2026-08-09:

- Reserved `null` as an excluded keyword and recorded design-audit entry 105, because otherwise the
  explicit exclusion was indistinguishable from an ordinary identifier.
- Template literals remain one lossless outer CST token, while each discovered interpolation range
  is lexed and parsed normally with absolute source spans. This preserves literal spelling and gives
  nested interpolation expressions the same grammar and recovery behavior as ordinary expressions.

## Gate two: semantic core and ownership

Status: complete.

Verified work in progress on 2026-08-09:

- Deterministic exact-file local/std module graph loading, import cycles, export binding, duplicate
  namespace detection, package-specifier rejection, and stable SHA-256-derived semantic identities.
- Resolved HIR signatures for primitives, generics, optionals, references, raw pointers, arrays,
  slices, tuples, functions, structs, enums, interfaces, classes, implementations, and extern blocks.
- Interface coherence/orphan checks, class-cycle validation, override/interface signature checks,
  public annotation rules, closed attributes, constant-initializer restrictions, and exact error
  effect checks are connected to `tn check` without loading LLVM.
- Generic MIR now models all architecture-defined statement/terminator families, has deterministic
  text, definite-initialization/type/control-flow validation, and mutation tests that corrupt block,
  local, type, and initialization invariants.
- Ownership analysis includes affine moves, partial-move restrictions for `Drop`, non-lexical loan
  ends, mutable/shared conflict detection, returned-local-reference rejection, suspension checks,
  and structural `Copy`/`Drop`/`Send`/`Sync` facts. Both MIR-level causal tests and source-level CLI
  diagnostics pass.
- `tn check` now runs deterministic body checking with contextual integer, float, array, object,
  call, and return typing; strict boolean conditions; raw-pointer safety checks; closed synchronous
  and asynchronous error-effect checks; and enum match witnesses. Focused tests cover missing `try`,
  `try await`, undeclared propagation, and incomplete catch sets.
- Nominal and function generic parameters now retain resolved `where` constraints in HIR. Generic
  calls use result-to-argument bidirectional inference and validate explicit interface bounds;
  arithmetic and comparison operators select either compiler-provided primitive behavior or a
  named operator-interface witness.
- Class checking now retains constructors, exact method body ranges, field-initializer facts,
  abstract/final rules, covariant class returns, override safety/effect substitutability,
  constructor synthesis constraints, first-statement `super`, required field initialization,
  member visibility, class/interface upcasts, checked downcasts, and abstract construction checks.
- Match analysis now covers closed enums, booleans, optionals, infinite-domain catch-all witnesses,
  payload bindings, guarded coverage, incompatible arm results, and unreachable patterns.
- Body checking records deterministic concrete generic function/method instances for downstream
  reachability and monomorphization. Source ownership checks now connect move-from-borrow,
  returned-local-reference, and live-borrow-across-await failures to causal source spans in addition
  to the MIR ownership mutation suite.
- Nominal interface arguments and lifetime arguments are retained through HIR, checked for arity
  and namespace agreement, and validated against interface declaration kinds. A regression test
  also fixed module scanning so declaration-like keywords inside generic arguments cannot become
  false top-level declarations.
- Enum record-field names and origins are retained in HIR. Struct, tuple-variant, and record-variant
  patterns now bind substituted generic field types and diagnose unknown fields, duplicate fields,
  unknown constructors, and payload arity mismatches.
- Exported reference signatures enforce the documented output-lifetime elision rules, named output
  lifetimes must be related to an input, and interface/outlives bounds are checked in their proper
  generic namespaces.
- Closure analysis infers shared, mutable, and move captures, preserves captures through local
  closure bindings, and applies structural `Send`/`Sync` plus process-lifetime checks at spawn and
  detach boundaries. Structural thread-safety derivation handles generic, enum, and recursive
  aggregate fields while explicit unsafe marker implementations remain authoritative.
- Body checking now materializes stable body owners, local/expression/statement identities, complete
  parent-child expression and statement structure, body roots, typed origins, resolved
  local/declaration/member values, pattern constructors, and typed pattern bindings in HIR.
- Constructor initialization is path-sensitive across branches, loops, early returns, and escaping
  `self`; mutable statics require unsafe access and immutable statics require structurally derived
  `Sync`.
- HIR-to-MIR lowering now gives methods unique member identities, maps HIR locals independently of
  compiler temporaries, and constructs validated CFGs for `if`, `while`, `break`, `continue`,
  suspension, typed returns, borrows, moves, assignments, casts, checked binary expressions, and
  typed calls with explicit success/error edges, tuple/array aggregates, and checked indexing.
  Resolved struct and inherited-class fields lower to layout-indexed typed place projections.
  Local initializers use delimiter-aware statement boundaries and preserve complete expressions
  instead of reducing them to their first token.
- Built-in array/slice iteration lowers to explicit length, index, increment, and back-edge CFG;
  enum and optional matches lower to discriminant switches with guarded fallbacks and typed payload
  projections; typed catches lower to an explicit closed error-union dispatch CFG.
- Unary operations, short-circuit boolean operations, nullish coalescing, conditional expressions,
  checked downcasts, compound assignments, and declaration-ordered struct aggregates lower to
  validated generic MIR. String and character constants are decoded to Unicode values before MIR,
  and enum switches use declared integer discriminants while retaining separate layout indices.
- Signature checking rejects payload/discriminant enum mixing and duplicate effective integer
  discriminants, with focused MIR coverage for explicit discriminant switch values.
- Class construction lowers to typed constructor calls with normal success/error edges. Resolved
  class calls use hierarchy-stable vtable slots (after the destruction slot), and dynamic-interface
  calls use declaration-stable witness slots. Interface operations are externally callable without
  an invalid class-style visibility requirement.
- Typed HIR now records every intermediate postfix and chained-operator node rather than collapsing
  member, call, index, cast, conditional, type-test, and binary transformations into only the outer
  expression; method lowering consumes the retained resolved member identity.
- Empty, payload, and context-specialized generic enum variants lower to typed aggregates followed
  by explicit layout discriminants. `instanceof` lowers to a dedicated typed runtime test rather
  than an opaque raw operation.
- Normal lexical block exits now end nested-local storage in reverse declaration order and restore
  shadowed name bindings, with validated MIR proving that a post-block return reads the outer local.
- Promise types retain their closed completion error sets through inference and local storage.
  Cold async calls have only creation success edges; awaiting consumes the promise through a typed
  `Suspend` terminator with separate completion, error-payload, and cancellation successors. The
  MIR validator mutation suite checks suspension result/error destinations and edge invariants.
- Static methods lower to symbolic method constants without runtime receiver evaluation. Concrete
  struct/enum implementation methods lower to bound direct-method values, while class and dynamic
  calls retain vtable/witness dispatch. Receiver mode is explicit in MIR, and move receivers consume
  their owner for ownership analysis; static/instance qualifier misuse has causal diagnostics.
- Tuple literals are distinguished from parenthesized lambdas with bounded lookahead, including
  empty-parameter closures, and local body annotations now resolve tuple and function types.
- `for ... of` now selects the canonical explicit `IntoIterator<Item, Iter>` and `Iterator<Item>`
  implementations during body checking, records both implementation and method identities in HIR,
  and lowers user-defined iteration to direct `intoIterator`/`next` calls, optional-tag switching,
  typed payload projection, and ownership-checked loop CFG. Malformed and ambiguous protocol
  selections are rejected rather than deferred to code generation.
- Typed HIR retains both the optional result and active present-path type for every node in an
  optional postfix chain. MIR lowers fields, calls, indexes, and repeated `?.` segments into nested
  present/absent CFG, evaluates call arguments only on present paths, projects typed payloads, and
  constructs normalized optional results. Non-optional `?.` receivers have a causal diagnostic;
  semantic, MIR-validator, and ownership regressions cover full and nested chains.
- Closures now have stable HIR identities, parameter-local mappings, capture-local mappings and
  modes, complete source/body origins, and typed function signatures. MIR materializes shared and
  mutable capture borrows, move captures, typed environments, and recursively validated closure
  bodies; nested closures reborrow captured environments through explicit dereference places.
  Ownership loan propagation follows references into closure and aggregate destinations so a
  captured loan remains live until the stored closure's last use. Positive, mutation, nested, and
  causal write-during-capture regressions pass.
- Template literals now lower as anonymous typed formatting values rather than allocated strings.
  HIR retains decoded literal segments, stable interpolation identities and origins, Display
  conformance, and owned-versus-shared-borrow storage. MIR evaluates each interpolation exactly
  once from left to right, stores temporaries by value, borrows lvalue places for the template
  lifetime, validates capture and part types, and propagates captured loans through stored template
  values. Escapes, nested templates, invalid Display values, malformed capture indices, and
  ownership-safe lowering have focused regressions.
- Optional fields reached through shared or mutable owners now produce optional reborrows for
  non-`Copy` payloads. Checked class/interface downcasts likewise produce non-consuming optional
  references with the input access mode, and MIR keeps their source loans live through storage.
  Mutable references are affine rather than `Copy`, and method receiver loans remain active through
  argument evaluation.
- Ordinary bindings, arguments, returns, and field extraction now move non-`Copy` values without an
  explicit keyword. Partial moves from `Drop` owners and subsequent uses are rejected. Ownership
  state is propagated over actual CFG successors with union joins and fixed-point loop handling, so
  a move on a returning branch no longer contaminates an unreachable join path.
- The checked-in semantic corpus runs every pass fixture twice, asserts causal structured records
  for every fail fixture, rejects leaked internal MIR errors, and verifies identical results across
  reversed filesystem creation order and fresh subprocess hash seeds.

Commands currently passing:

```text
cargo test -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --all-targets
cargo clippy -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --all-targets -- -D warnings
```

The final Gate 2 inventory contains 95 tests across HIR, MIR, type checking, driver, and CLI,
including 21 body-semantic, 46 ownership/MIR-lowering, eight signature, six MIR-validator, and four
semantic-corpus tests. The expanded command also passes all 25 syntax/parser tests. A dedicated
inventory fixture proves a causal type failure on every one of 35 primitive and compound type
declarations. `@derive(Copy)`, invalid Copy/Drop combinations, unsafe Send/Sync implementations,
thread captures, class access, and borrowed downcasts are represented in checked-in conformance
fixtures.

Final verification on 2026-08-10:

```text
# macOS ARM64
cargo test -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --all-targets
PASS: all 120 syntax and Gate 2 tests

cargo clippy -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --all-targets -- -D warnings
PASS

RUSTDOCFLAGS='-D warnings' cargo doc -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --no-deps
PASS

scripts/verify-design.sh
PASS: 13 links; all canonical examples; design_verification=pass

scripts/check-toolchain.sh
PASS: LLVM 22.1.8; host_target=aarch64-apple-darwin

# Linux x86_64 under Podman, exact Rust 1.96.0
podman run --rm --platform linux/amd64 ... rust:1.96.0-bookworm ... \
  cargo test -p tn-syntax -p tn-hir -p tn-mir -p tn-typecheck -p tn-driver -p tn-cli --all-targets
PASS: all 120 syntax and Gate 2 tests

... cargo clippy ... --all-targets -- -D warnings
NOT RUN: the official Rust 1.96.0 Linux image has no installed `cargo-clippy`
component; `rustup component add clippy` did not complete under the emulated
container. This remains open evidence, not a pass.

... RUSTDOCFLAGS='-D warnings' cargo doc ... --no-deps
PASS
```

Implementation decision recorded on 2026-08-10:

- Resolved the unnameable iterator-result contradiction by specifying
  `IntoIterator<Item, Iter> where Iter: Iterator<Item>` with exact infallible
  `move intoIterator(): Iter` and `mut next(): Item | undefined` operations. Language-spec,
  architecture, and design-audit entry 106 agree, and semantic plus MIR/ownership regressions pass.

## Gate three: native execution

Status: complete on the active macOS ARM64 target.

Work in progress verified on 2026-08-10:

- Generic MIR drop elaboration now computes drop-requiring structural types, initializes and updates
  per-place drop flags across assignments and moves, recursively elaborates closure bodies, and
  inserts reverse-completion-order conditional `Drop` chains before function return and throw
  exits. MIR validation understands post-borrow-check conditional drops and flag updates.
- Cleanup dataflow now covers lexical `StorageDead`, branch and match-arm joins, call success and
  typed-error edges, and suspension resume, error, and cancellation edges. Edge-specific result
  flags are initialized only on the edge that produced the value; owned error unions participate in
  structural destruction.
- Typed returns and throws lower to explicit success/error tagged completions. Recoverable calls
  retain separate success and error successors, so downstream lowering requires no landing pads,
  personality routines, or native unwinding.
- Mutation-style MIR tests prove move-flag transitions, reverse cleanup order, branch scope exit,
  destruction before `StorageDead`, and owned payload cleanup on a typed-error edge. Tagged-return
  tests validate both completion variants and pass idempotently.
- Deterministic reachability and monomorphization now use a sorted work queue, specialize every MIR
  type occurrence, assign stable instance keys, and terminate for recursive generic call graphs.
  Missing roots, generic arity errors, and unresolved concrete parameters are rejected before LLVM.
- The LLVM 22 adapter now establishes the requested target data layout, lowers primitive function
  bodies and explicit control flow, uses signed/unsigned overflow intrinsics, checks division and
  shift failure cases, lowers recoverable calls to tag tests, and verifies every module. Optimized
  emission runs `default<O2>` and verifies the resulting module again.
- Verified LLVM IR, bitcode, assembly, and object emission are connected to `tn build`. Native
  startup maps the four valid `main` signatures to process status, provides the panic/abort hook,
  and `tn run` preserves the executed program's status. A CLI integration fixture builds every
  implemented backend product and executes an `i32` entry returning status 7.
- Tuple, optional, and array values now have concrete LLVM aggregate layouts. Array and slice
  indexing emit a dominating unsigned bounds check before a deliberately non-`inbounds` GEP;
  borrowed places and tuple/optional/array projections lower through typed addresses. Debug and
  optimized array executables both return the same in-range value and both abort through the runtime
  panic hook for an out-of-range index.
- The checked-integer verifier inventory covers signed and unsigned 8-, 16-, 32-, 64-, 128-, and
  pointer-width operations for addition, subtraction, multiplication, division, remainder, shifts,
  bitwise operators, equality, and ordering in both profiles. Every generated module passes LLVM
  verification; signed minimum divided or reduced by minus one, zero division, invalid shift counts,
  and lossy left shifts branch to panic.
- Resolved nominal layout metadata now reaches the LLVM boundary without exposing LLVM types to
  upstream crates. Struct fields use declaration order, enums use a private tagged layout with
  variant-specific payload offsets, discriminant switches load the tag explicitly, and generic
  layout fields are substituted from concrete nominal arguments. Native fixtures return 42 through
  reordered struct literals, enum payload matching, and an inferred generic identity instance.
- MIR aggregates now retain their optional variant identity, so enum payload placement remains
  explicit through monomorphization instead of being inferred from neighboring statements. Generic
  direct-call signatures are concretized during MIR lowering, fixing validation and allowing the
  reachability engine and backend to agree on specialized callable identities.
- Native reachability now keeps only the executable entry for executable/object/IR/assembly products,
  and only validated explicit C/Node exports (plus public exported Node class members) for foreign
  products. An integration regression proves an unreferenced non-exported function is absent from
  emitted LLVM IR; the full workspace and Clippy suites remain green after the root-pruning change.

Commands currently passing:

```text
cargo test -p tn-mir -p tn-typecheck --all-targets
cargo clippy -p tn-mir -p tn-typecheck --all-targets -- -D warnings
cargo test -p tn-codegen-llvm -p tn-driver -p tn-cli --all-targets
cargo clippy -p tn-codegen-llvm -p tn-driver -p tn-cli --all-targets -- -D warnings
cargo test --workspace --all-targets
PASS: complete macOS workspace suite after native array and numeric lowering

cargo clippy --workspace --all-targets -- -D warnings
PASS
```

The macOS ARM64 native, debug-information, and sanitizer acceptance evidence
is complete. The Linux records in this section are historical only.

## Gate three: native execution — current evidence

Status: complete on macOS ARM64. The native, LLVM, debug-information, and
AddressSanitizer/UndefinedBehaviorSanitizer matrix is green on the active target.
The later self-hosting acceptance criteria remain open.

Verified on 2026-08-10 (macOS ARM64):

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
PASS: verification-matrix=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-debug-info.sh
PASS: debug-information=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/run-sanitizers.sh
PASS: address-undefined-sanitizers=pass
PASS: hosted-address-undefined-sanitizers=pass
PASS: redis-lifecycle-address-undefined-sanitizers=pass
PASS: redis-address-undefined-sanitizers=pass
PASS: thread-sanitizer=pass
PASS: hosted-thread-sanitizer=pass
PASS: redis-lifecycle-thread-sanitizer=pass
PASS: redis-thread-sanitizer=pass
```

The matrix also exercises debug and optimized native products, LLVM IR/bitcode/assembly/object
emission, checked arithmetic and bounds failures, DWARF lookup in LLDB, C layout probes, Node-API,
standard-library and async executables, Redis, and bootstrap. Generic `run<i32>`, generic
`run<struct>`, and effectful `try run` now execute successfully; async completion payloads are
separated from raw completion records in the runtime ABI, with target-data-derived payload offsets.

Verified on 2026-08-10 in an isolated Linux x86-64 Podman container using the official LLVM
22.1.8 Debian packages (LLVM, Polly, compiler-rt, and Z3 artifacts were SHA-256 checked against
the apt.llvm.org package index):

```text
scripts/check-toolchain.sh
PASS: LLVM 22.1.8; host_target=x86_64-unknown-linux-gnu

cargo test --workspace --all-targets
PASS: every workspace test, including LLVM lowering, native execution, semantic corpus, and
      syntax/fixture/property suites

scripts/verify-debug-info.sh
PASS: debug-information=pass
scripts/verify-stdlib.sh
PASS: hosted-stdlib-and-async=pass
scripts/verify-c-abi.sh
PASS: c-abi-layout-and-call=pass
scripts/verify-node.sh
PASS: node-api-validation=pass (Node 18.20.4, Node-API headers)
scripts/verify-redis.sh
PASS: redis-lifecycle=pass; redis-cli-protocol=pass
scripts/bootstrap-self-host.sh
PASS (historical hosted-boundary run): compiler A/B/C source and artifact fixed points
```

The Linux runtime portability fixes exercised by that run define the required POSIX feature
surface in `runtime/runtime.c`, use `PRId64` for portable Redis integer formatting, select the
host target in the CLI validation fixture, and accept both Mach-O and ELF TypeNative symbols in
the debug-information check.

## Gate four: hosted standard library

Status: in progress. The hosted standard-library and allocation/refcount/UTF-8/runtime suite passes
on macOS ARM64; the remaining work is API completeness and self-hosted execution.

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-stdlib.sh
PASS: hosted-stdlib-and-async=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-runtime.sh
PASS: runtime-collections-refcounts=pass
```

The hosted collection validation fixture now exercises the canonical `Array`, FIFO `Queue`,
two-ended `Deque`, `Heap` storage, `Map`, `Set`, `OrderedMap`, `OrderedSet`, and borrowed
`Slice` constructors and operations (including growth, removal, ordered lookup, and length
updates) in addition to the allocation-checked APIs. The complete hosted-standard-library
script, including both debug and optimized native builds of that fixture, was rerun after the
change:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-stdlib.sh
PASS: hosted-stdlib-and-async=pass

/Users/ronnie/.cargo.target/debug/tn build validation/collections/main.tn \
  --profile optimized --out /tmp/typenative-collections-canonical
PASS: native build
/tmp/typenative-collections-canonical
PASS: exit=42

/Users/ronnie/.cargo.target/debug/tn build validation/collections/main.tn \
  --profile optimized --out /tmp/typenative-collections-canonical2
PASS: native build; canonical ordered/slice and FIFO/two-ended/map-set regression fixture
/tmp/typenative-collections-canonical2
PASS: exit=42

TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime \
  /Users/ronnie/.cargo.target/debug/tn check compiler-tn/main.tn --timings
PASS: exit=0; module-check=820919us; ownership=11686117us; mir-drop=6515567us
```

The workspace quality suite was rerun after the collection changes:

```text
cargo fmt --all -- --check
PASS
cargo test --workspace --all-targets
PASS: all workspace tests, including reachability and timing regressions
cargo clippy --workspace --all-targets -- -D warnings
PASS
RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps
PASS
scripts/verify-design.sh
PASS: design_verification=pass
```

The checked-in TypeNative modules pass `tn fmt --check` and `tn check` on macOS ARM64. The remaining
acceptance items are the full documented hosted API inventory and independent self-host execution.

Post-reachability-root-pruning checks on macOS ARM64:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-cli.sh
PASS: tn-cli-surface=pass
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-c-abi.sh
PASS: c-abi-layout-and-call=pass
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-node.sh
PASS: node-api-validation=pass
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-runtime.sh
PASS: runtime-collections-refcounts=pass
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-redis.sh
PASS: redis-lifecycle=pass
PASS: redis-cli-protocol=pass
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/run-sanitizers.sh
PASS: address-undefined-sanitizers=pass
PASS: hosted-address-undefined-sanitizers=pass
PASS: redis-lifecycle-address-undefined-sanitizers=pass
PASS: redis-address-undefined-sanitizers=pass
PASS: thread-sanitizer=pass
PASS: hosted-thread-sanitizer=pass
PASS: redis-lifecycle-thread-sanitizer=pass
PASS: redis-thread-sanitizer=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-debug-info.sh
PASS: debug-information=pass
```

## Gate five: concurrency and async

Status: complete on macOS ARM64. Cold promises, single-await completion,
generic results, cancellation/drop paths, channels, reference counts, reactor
calls, synchronization stress, and ThreadSanitizer pass on the active target.
Linux records below are historical only.

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/run-sanitizers.sh
PASS: hosted-thread-sanitizer=pass
PASS: redis-thread-sanitizer=pass

# Linux x86-64, official LLVM 22.1.8 container
scripts/run-sanitizers.sh
PASS: address-undefined-sanitizers=pass
PASS: hosted-address-undefined-sanitizers=pass
PASS: redis-lifecycle-address-undefined-sanitizers=pass
PASS: redis-address-undefined-sanitizers=pass
BLOCKED BY HOST: ThreadSanitizer reports an incompatible Rosetta memory layout before executing
                the runtime probe, even with seccomp unconfined and SYS_PTRACE.
```

## Gate six: C and Node-API interoperability

Status: complete on macOS ARM64. ABI, shared-library, Node-API, declaration,
and sanitizer checks pass on the active target. Linux records below are
historical only.

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-c-abi.sh
PASS: c-abi-layout-and-call=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-node.sh
PASS: node-api-validation=pass

# Linux x86-64, official LLVM 22.1.8 container
scripts/verify-c-abi.sh
PASS: c-abi-layout-and-call=pass
scripts/verify-node.sh
PASS: node-api-validation=pass
```

## Gate seven: Redis systems validation

Status: complete on macOS ARM64. The RESP2 protocol, fragmented input,
expiration, concurrent clients, shutdown, debug/optimized servers, and
ASan/UBSan/TSan lifecycle checks pass on the active target. Linux records below
are historical only.

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn scripts/verify-redis.sh
PASS: redis-lifecycle=pass
PASS: redis-cli-protocol=pass

# Linux x86-64, official LLVM 22.1.8 container
scripts/verify-redis.sh
PASS: redis-lifecycle=pass
PASS: redis-cli-protocol=pass
```

During this gate the lifecycle sanitizer exposed and fixed two real defects: a listener-close race
on shutdown and double-free cleanup after a partial RESP command. Both regression paths are now
covered by the lifecycle and sanitizer checks.

## Gate eight: self-hosting

Status: in progress. The checked-in TypeNative compiler sources pass formatter and semantic checks,
the pinned LLVM-major guard and dynamically loaded LLVM C API context/module create-dispose-roundtrip
smoke test are exercised, and the bootstrap driver is structured for source rewriting plus an
independent compiler A → B → C artifact/source fixed-point check. The current independent run stops
at A → B because the lowerer is still bounded; the older hosted-boundary run is retained separately.
The self-hosted lexer now tokenizes a
length-aware source buffer in TypeNative itself, recursively validates nested templates/comments,
and the parser performs bounded nested-delimiter and excluded-syntax validation in TypeNative. The
self-hosted HIR records declaration/function/type/value/block and ownership-operation inventories;
the semantic pass checks duplicate declarations, typed initializer/condition and ownership hazards,
trait/class/lifetime/static/effect/catch/pattern rules; and the formatter independently revalidates
token and template ranges before preserving source spelling for the writer. Parser errors carry
stable condition codes and primary spans through HIR, semantic, driver, tooling, and diagnostic
records. The self-hosted driver exposes file and recursive-directory `check`, JSON diagnostics,
file/tree `fmt`, `test`, `doc`, and LSP operations, with local-import visibility validation and
nonzero process status on rejected input. Its LLVM C API binding performs the mandatory major check,
module smoke test, and a verified integer-return IR emission. Bootstrap compares the full
valid/invalid syntax corpus against Rust parser diagnostics and requires both compilers to agree on
syntax acceptance. The runtime self-host boundary writes the validated source buffer directly
through a bounded writer; it no longer injects a synthetic artifact header or a second C-side
tokenization pass. Local-import validation is lexical-state aware: quoted strings, templates, line
comments, and nested block comments cannot be mistaken for import declarations. This is not marked
complete because the TypeNative sources still provide a hosted analysis boundary rather than an
independent LLVM-backed compiler. The previous fixed-point digests are determinism evidence for the
hosted boundary rather than independent self-hosting evidence; B/C also have not independently rerun
every prior-gate suite on macOS ARM64. The HIR boundary now owns
a declaration table with declaration kind,
source/name/body spans, and a stable identity word for each top-level declaration; semantic duplicate
checks consume those records and compare name spans through the hosted byte-span primitive. MIR now
performs a single token pass, maintains nested active-block state, and emits a deterministic eight-word
record for every block (source span, function-body flag, successor, borrow/move/await counts, and total
operations). Borrow validation consumes that record stream through bounded runtime byte access,
fails closed on null, zero-sized, or short-capacity record streams, validates record shape and block
successors, and reports checked block/operation totals without
rejecting valid scopes merely because unrelated operations occur in the same function. Implementation
and repeated extern-block names are excluded from namespace duplicate checks, matching the language's
coherence model. The TypeNative lowering boundary now owns a bounded postfix operation stream for
`main(): i32` integer literals, unary negation, parentheses, checked `+`, `-`, `*`, `/`, and `%`, plus
the hosted `argumentCount()` process primitive, and a separate empty `main(): void` path. Runtime
evaluation uses the process argument base appropriate to `tn run`; the LLVM C API emits verified
integer, process-call, and void-return modules as LLVM IR, bitcode, assembly, and object products.
Executable and shared-library products use temporary target-machine output and a checked `clang`
invocation with the canonical startup/runtime sources; executable entry symbols are kept distinct
from the startup wrapper's `main`. Node-addon emission additionally compiles a Node-API-only wrapper,
links a `.node` bundle, and writes the matching `main(): number|void` declaration file. The stream
rejects malformed expressions, arithmetic overflow, division by zero, and unsupported expression
forms rather than silently accepting them. Numeric source spans are validated against the runtime
literal grammar before self-hosted lowering, including radix prefixes and integer/float suffixes;
the self-hosted CLI reports malformed literals as ordinary nonzero diagnostics. Error-path source
buffers use a runtime-provided null pointer sentinel, so rejected `build`/`run` inputs do not pass
static string addresses to `free`.
The hosted source boundary now also materializes every top-level function declaration into a
deterministic fourteen-word backend record: name/body spans, parameter count, result-type span and
kind, statement/return/call counts, lowering validity and operation count, plus an explicit entry
index. `compiler-tn/backend.tn` validates
the record shape, source bounds, and the aggregate typed operation stream before the driver accepts
the module, and `lowerFunction` shares
the expression lowerer across named functions instead of hard-coding the `main` search. This is a
real reusable source-to-MIR boundary, but it remains an intermediate slice: it does not yet emit
all discovered functions or lower their control flow, ownership, effects, aggregates, or imports
to LLVM.

The hosted boundary now also has a verified multi-function integer module emitter. It serializes
the aggregate operation stream separately from the fourteen-word function table, emits parameterized
`i32` functions, direct typed calls, `argumentCount()` calls, void direct calls, and an executable
entry wrapper, and renames a source `main` only when the native startup product requires that
wrapper. Focused
TypeNative-built compiler checks cover constant helper calls, parameter arithmetic, direct calls,
argument-count calls, LLVM verification, executable linking, and exit-code behavior. This remains a
backend slice: non-`i32` nominal returns, general control flow, ownership/drop edges, effects,
aggregates, and the compiler's own full `Compiler` entry function are still outside this emitter.

The integer operation stream now also has checked conditional selection and signed comparison
operations. The runtime evaluator returns canonical `0`/`1` comparison values, while the LLVM path
uses `LLVMBuildICmp`, zero-extension, and `LLVMBuildSelect`; constant selections are folded only
when the condition is proven. TypeNative source regressions cover `argumentCount() !== 0i32 ?
7i32 : 9i32` and a literal `if`/`else` return, verify the emitted `icmp`/`select` or folded IR,
and run the native products. This extends the bounded expression backend but does not close the
unresolved general statement-level control-flow or aggregate/module bootstrap requirements.

HIR declaration identities now combine one precomputed source identity with each declaration's
exact span through `tn_selfhost_hash_declaration_with_source_identity`; the strict runtime
validation checks repeatability, distinction of two declarations, and malformed-span rejection.
This replaces the previous source-offset identity without rehashing the complete source once per
declaration, while retaining the original span fields for diagnostics.
Named lowering also records simple parameter references as explicit MIR operation kind `8`; the
runtime evaluates those streams through `tn_selfhost_eval_i32_program_with_parameters` (the runtime
validation exercises `40 + 2` and the missing-parameter rejection), and the LLVM product emitter now
loads `LLVMGetParam`, constructs the parameterized function type, and emits those references as
actual arguments. A strict runtime regression reads the resulting IR and verifies the parameter
addition. Direct typed calls and void calls now have a verified multi-function LLVM path; aggregate
values, general control flow, ownership, effects, and complete module lowering remain open.
The lowering boundary also propagates checked integer constants through local bindings, simple
literal-returning helper calls, nested `run`/`runI32` calls, `await`/`try` initializers,
object-literal field projections, and the reference-shaped `mapInsert`/`mapGet` collection
validation path. Constant records use their complete 24-byte layout, and constant folding uses
the runtime's checked 64-bit arithmetic helper, including an overflow regression. The hosted
standard-library/async validation suite passes under the resulting self-hosted compiler A in both
debug and optimized profiles. This remains a bounded lowering slice, not a replacement for the
complete HIR/MIR/LLVM self-hosted pipeline.

The generic-call lowering repair now runs binary-expression recognition before unary recognition
and skips `<...>` generic-call ranges while searching for operators. This prevents a generic call
from being emitted as a function-pointer comparison when it appears under unary negation and
short-circuit boolean control flow. The regression is checked by
`lowers_unary_short_circuit_generic_calls_without_function_pointer_operands`; the focused
TypeNative suite reports 21 body, 48 ownership, one pipeline, and eight signature tests passing.
The expanded collection fixture reaches exit 42 through this corrected lowering path.

The bootstrap harness now enforces `compiler-b == compiler-c == compiler-b-repeat` for each run.
Module and declaration identities use stable project-relative paths, and Mach-O UUID, signature
payload, signature metadata, and segment-size noise are excluded from the digest. The repeated
build produced the same normalized digest as B and C (`26fbdfdc...`), so this reproducibility
criterion is verified for the current macOS toolchain.

The bootstrap orchestration now follows the canonical independent order: Rust builds A, A builds B,
and B builds C. A fresh macOS attempt reaches every existing A frontend check, and the focused
multi-function emitter passes its independent sample programs, but the compiler's own `Compiler`
entry still falls back to the bounded expression lowerer because its nominal return and general
control-flow backend are not implemented:

```text
TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime \
  build/bootstrap/run-1786513461-91263/compiler-a \
  build build/bootstrap/run-1786513461-91263/compiler-src/main.tn --profile optimized --timings \
  --out /tmp/typenative-compiler-b-repro
FAIL: exit=2
PASS: tn-timing phase=module-check nanos=6675000
PASS: tn-timing phase=ownership nanos=6794000
PASS: tn-timing phase=mir-drop nanos=1000
PASS: tn-timing phase=monomorphization nanos=18000
FAIL: error[SYNTAX_SELFHOST](1014) at bytes 20229..20240
FAIL: bootstrap message: independent self-hosting failed: compiler A could not build compiler B
```

Fresh full-matrix verification on 2026-08-12 (macOS ARM64) used the built compiler explicitly:

````text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
PASS: design, toolchain, workspace tests, Clippy, docs, TypeNative format/check, CLI, hosted
      standard library, runtime, debug information, C ABI, Node-API, Redis, sanitizers, and fuzzing
FAIL: bootstrap-self-host.sh at compiler A -> compiler B with SYNTAX_SELFHOST(1014) at bytes
      20229..20240; no B/C artifact or prior-gate evidence was recorded as complete

The focused post-emitter regressions passed:

```text
cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/runtime.c -o /tmp/runtime-clean-final.o
PASS: strict runtime compilation
scripts/verify-runtime.sh
PASS: runtime-collections-refcounts=pass
tn fmt --check compiler-tn/backend.tn
tn fmt --check compiler-tn/lowering.tn
tn fmt --check compiler-tn/main.tn
PASS
tn check compiler-tn/backend.tn
tn check compiler-tn/lowering.tn
tn check compiler-tn/main.tn
PASS
tn build compiler-tn/main.tn --profile optimized --out /tmp/typenative-compiler-a-multi-final
PASS
/tmp/typenative-compiler-a-multi-final build /tmp/tn-module-sample1-nonl.tn --emit llvm-ir ...
/tmp/typenative-compiler-a-multi-final build /tmp/tn-module-sample2-nonl.tn --emit llvm-ir ...
/tmp/typenative-compiler-a-multi-final build /tmp/tn-module-sample4.tn --emit llvm-ir ...
PASS: selfhost-module-samples=pass
scripts/verify-runtime.sh
PASS: module parameter/call LLVM regression
/tmp/typenative-compiler-a-multi-final build /tmp/tn-module-sample1-nonl.tn --emit executable ...
PASS: executable returned 40
runtime conditional/comparison regression
PASS: evaluator comparison and conditional select; single-function and multi-function LLVM
`icmp`/zero-extension/`select`; strict runtime compile
````

The combined post-change rerun (parameter-aware LLVM emission, the aggregate typed backend
operation stream, content-stable HIR declaration identities, and the active-target cleanup) used
the same command and reached the same first independent boundary. Its completed output includes
`tn-cli-surface=pass`, `hosted-stdlib-and-async=pass`, `runtime-collections-refcounts=pass`,
`debug-information=pass`, `c-abi-layout-and-call=pass`, `node-api-validation=pass`,
`redis-lifecycle=pass`, `redis-cli-protocol=pass`, all sanitizer probes, both 10,000-run fuzz
targets, and every existing A frontend/differential check. It then reports:

```text
error[SYNTAX_SELFHOST](1014) at bytes 20229..20240
independent self-hosting failed: compiler A could not build compiler B
```

No B or C artifact was produced by that run. The captured log is
`/tmp/verify-all-final-current.log`; the focused post-cleanup Rust and TypeNative quality loop
remains green as recorded at the top of this ledger.

The latest full rerun after the verified multi-function emitter, runtime source-buffer cleanup, and
parameterized module regression used `/tmp/verify-all-post-module.log`. It again passed design,
toolchain, workspace tests, Clippy, rustdoc, TypeNative checks, CLI, hosted standard library,
runtime, debug information, C ABI, Node-API, Redis, all sanitizer probes, and both 10,000-run fuzz
targets. The only nonzero phase was the independent A-to-B bootstrap boundary:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
PASS: all pre-bootstrap phases
FAIL: error[SYNTAX_SELFHOST](1014) at bytes 20229..20240
FAIL: independent self-hosting failed: compiler A could not build compiler B
```

The resolver-aware rerun reached the same boundary with
`module-check=5381000ns`, `ownership=5390000ns`, `mir-drop=0ns`, and
`monomorphization=4000ns`, then exited before B was emitted.

````

The standalone CLI validation also passes with the explicit compiler path on this checkout:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-cli.sh
PASS: tn-cli-surface=pass
````

The verification scripts now resolve an absolute `CARGO_TARGET_DIR` before falling back to the
compiler binary, so the external target directory is handled without a repository-local symlink.

The resolver was exercised without `TN_BIN`:

```text
env -u TN_BIN CARGO_TARGET_DIR=/Users/ronnie/.cargo.target \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-cli.sh
PASS: tn-cli-surface=pass
```

The complete verification matrix was rerun after the backend-record/named-function change on
macOS ARM64. Rust formatting, workspace tests (including 11 CLI project tests), Clippy, rustdoc,
design verification, all TypeNative formatting/check passes, CLI, hosted standard library, runtime,
debug information, C ABI, Node-API, Redis, AddressSanitizer, UndefinedBehaviorSanitizer,
ThreadSanitizer, and both 10,000-run fuzz targets passed. The matrix exits nonzero only at the
independent bootstrap boundary:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
PASS: address-undefined-sanitizers=pass
PASS: hosted-address-undefined-sanitizers=pass
PASS: redis-lifecycle-address-undefined-sanitizers=pass
PASS: redis-address-undefined-sanitizers=pass
PASS: thread-sanitizer=pass
PASS: hosted-thread-sanitizer=pass
PASS: redis-lifecycle-thread-sanitizer=pass
PASS: redis-thread-sanitizer=pass
PASS: fuzz lexer=10000 runs
PASS: fuzz parser=10000 runs
FAIL: bootstrap-self-host.sh at A -> B with SYNTAX_SELFHOST(1014) at bytes 19175..19186
EXIT: 1
```

The self-hosted CLI now reports the same opt-in phase labels as the Rust driver. The direct helper
lowering regression folds `add(2i32 + 3i32)` through `value + 5i32 + 2i32` to 12 in both LLVM IR
and process execution. These are verified slices; they do not close Gate 8 or Gates 9–12.

After the reusable backend-record and named-function lowering change, a fresh A frontend/bootstrap
run still passes the LLVM-C-API, semantic, HIR, MIR, native-link, Node-API, CLI, diagnostic, and
syntax-differential slices, then fails at the same first independent A-to-B boundary:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/bootstrap-self-host.sh
PASS: self-hosted-llvm-c-api=pass
PASS: self-hosted-hir-declarations=pass
PASS: self-hosted-mir-borrow-move-await=pass
PASS: self-hosted-native-link-products=pass
PASS: self-hosted-node-addon=pass
PASS: self-hosted-cli=pass
PASS: self-hosted-diagnostic-records=pass
PASS: self-hosted-syntax-differential=pass
PASS: tn-timing phase=module-check nanos=5299000
PASS: tn-timing phase=ownership nanos=5325000
PASS: tn-timing phase=mir-drop nanos=0
PASS: tn-timing phase=monomorphization nanos=6000
FAIL: error[SYNTAX_SELFHOST](1014) at bytes 19175..19186
FAIL: independent self-hosting failed: compiler A could not build compiler B
```

Fresh compiler-A slice verification after the lowering fix:

```text
TN_BIN=/tmp/typenative-selfhost-fixed.MyCJ85/compiler-a \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-stdlib.sh
PASS: hosted-stdlib-and-async=pass

TN_BIN=/tmp/typenative-selfhost-fixed.MyCJ85/compiler-a \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-cli.sh
PASS: tn-cli-surface=pass

TN_BIN=/Users/ronnie/.cargo.target/debug/tn \
  TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-cli.sh
PASS: tn-cli-surface=pass (zero-argument helper forwarding to argumentCount)

compiler-A build ... --emit llvm-ir --timings
PASS: module-check, ownership, mir-drop, monomorphization, llvm-link timing records
```

```text
TN_BIN=/tmp/typenative-selfhost-helper scripts/verify-stdlib.sh
PASS: hosted-stdlib-and-async=pass
```

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/bootstrap-self-host.sh
# Historical pre-independent-order run; B/C were Rust-driver-built.
PASS: self-hosted-llvm-c-api=pass
PASS: self-hosted-semantic-corpus=pass
PASS: self-hosted-hir-declarations=pass
PASS: self-hosted-cli-extended=pass
PASS: self-hosted-frontend=pass
PASS: self-hosted-cli=pass
PASS: self-hosted-semantic-duplicate=pass
PASS: self-hosted-mir-borrow-move-await=pass
PASS: self-hosted-syntax-differential=pass
PASS: self-hosted-native-link-products=pass
PASS: self-hosted-node-addon=pass
PASS: self-hosted-radix-literals=pass
PASS: self-hosted-diagnostic-records=pass
PASS: bootstrap-fixed-point=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-repeatable=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-source-fixed-point=4a882a717d32b36a72d66c1a5bbec6106a1889a0f17cfc3585b1bda308db65ee

# Linux x86-64, official LLVM 22.1.8 container (historical hosted-boundary run)
scripts/bootstrap-self-host.sh
PASS: bootstrap-fixed-point=4880a55fb07ca4abfb43b2ebe0d675b577f81cb1921b62fa94cd65fdd0ea1291
PASS: bootstrap-source-fixed-point=56c85b9c9e370b2515faefe308381536d0b7cd4524db34dea4ece7d98ae62b89
```

The final pre-independent-order macOS run on 2026-08-12 also exercised the TypeNative source-buffer,
numeric-literal, normalized-diagnostic-record, semantic-corpus, declaration-table, documentation,
test, LSP, local-import, LLVM C API, default executable emission, constant propagation,
helper-call lowering, object-field projection, and collection mapInsert/mapGet regressions:

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
PASS (pre-independent-order correction): verification-matrix=pass
PASS: self-hosted-llvm-c-api=pass
PASS: self-hosted-semantic-corpus=pass
PASS: self-hosted-hir-declarations=pass
PASS: self-hosted-cli-extended=pass
PASS: self-hosted-frontend=pass
PASS: self-hosted-cli=pass
PASS: self-hosted-semantic-duplicate=pass
PASS: self-hosted-mir-borrow-move-await=pass
PASS: self-hosted-syntax-differential=pass
PASS: self-hosted-native-link-products=pass
PASS: self-hosted-node-addon=pass
PASS: self-hosted-radix-literals=pass
PASS: self-hosted-diagnostic-records=pass
PASS: bootstrap-fixed-point=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-repeatable=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-source-fixed-point=4a882a717d32b36a72d66c1a5bbec6106a1889a0f17cfc3585b1bda308db65ee
```

## Gate nine: language-surface convergence

Status: in progress. Canonical source and ordinary bootstrap convergence
evidence is recorded; the final gate remains open until native acceptance and
the remaining canonical obligations pass.

Current evidence:

- the canonical specification, migration manifest, architecture, implementation
  plan, Redis acceptance contract, and preparation records are synchronized;
- uppercase public `String` has been removed from the non-protected standard
  library and is rejected as `TYPE_OBSOLETE_PUBLIC_TYPE`; owned-returning
  string, environment, and path APIs use lowercase `string`;
- contextual string literals required as owned `string` are represented by a
  typed `StringLiteralToOwned` HIR conversion and an explicit
  `string_from_static` MIR operation for annotated locals, arguments, and
  returns; `validation/string/main.tn` exercises the conversion and strict
  UTF-8 equality in native debug and optimized products;
- the bundled `@Intrinsic("string")` declaration is loaded as a prelude and
  binds the predeclared owned representation to ordinary standard-library
  members; `from`, `fromUtf8`, `toAsciiUppercase`, `clone`, `asStr`, and
  `bytes` resolve as declared member identities rather than compiler-known
  method names;
- `&[T]` uses a fat borrowed-slice representation in LLVM, and
  `string.bytes()` exercises checked indexing without a temporary descriptor;
  focused syntax, prelude, HIR, MIR, intrinsic-fraud rejection, and native
  debug/optimized tests pass;
- legacy procedural functions in `std/string` are private implementation
  details and cannot be imported as the canonical user API;
- the last HIR `BuiltinValue`, previously used for `usize.parseAscii`, has been
  removed; the operation is a declared static member of the bundled
  `@Intrinsic("usize")` definition, with user-defined binding fraud rejected;
- private intrinsic functions select reviewed MIR operations through explicit
  `@Intrinsic("operation")` arguments rather than compiler-known helper names;
  operation/module pairs are closed and user-defined intrinsic functions are
  rejected;
- generic `is_copy` queries remain typed MIR operations until monomorphization,
  so `Array<i32>` and other concrete copy types select the correct storage path;
  the collection native fixture now returns 42 in both debug and optimized
  products instead of its prior `Array.pop()` exit 6;
- `Queue<T>` and `Deque<T>` now use ordinary class constructors and declared
  `push`, `pop`, `pushFront`, `popBack`, `reserve`, `shrinkToFit`, and `clear`
  methods as appropriate. Element layout is derived by the private `size_of`
  intrinsic, storage fields are private, `length` and `capacity` are externally
  read-only, and initialized-slot tracking moves and destroys non-copy elements
  exactly once. The old exported `queue*` and `deque*` procedural families are
  absent and covered by missing-export regressions; circular copy storage and
  growing/clearing `string` elements return 42 in debug and optimized native
  collection products;
- readonly field assignment is rejected outside the declaring type with
  `TYPE_READONLY_FIELD_ASSIGNMENT`, while constructors and declared methods can
  maintain their own public read-only state;
- the canonical parser example corpus is green;
- canonical `validation/redis/*.tn` sources pass ordinary compiler checks; and
- focused syntax, semantic, ownership, MIR, CLI, and full workspace regressions
  are being rerun after the convergence changes.

Remaining scope:

- Canonicalize `Promise<T, E>`, synchronous `throws`, `T | undefined`, `!`, typed
  `try`/`catch`, direct capability decorators, `@Conform`, `@Drop`, `@Intrinsic`,
  `@Sealed`, `using`, generators, async generators, and typed user macros.
- Complete the remaining collection surface: fixed arrays, slices,
  `Map<K, V>`, `Set<T>`, `OrderedMap<K, V>`, `OrderedSet<T>`, and `Heap<T>`,
  including `Equal`/`Hash` key constraints; finish Queue/Deque borrowing and
  iteration together with the canonical iterator protocol.
- Remove obsolete `Result`, `Option`, `match`, `impl`, `extension`, `record`,
  `where`, `dyn`, collection, module, and compatibility spellings from every
  compiler layer and fixture.
- Re-run parser, formatter, semantic, ownership, MIR, native, hosted, and
  bootstrap differential inventories after the canonical documents are updated.

This gate cannot be completed until Gates 3–8 are complete and the canonical
documents, audit, grammar, implementation, and fixtures agree.

## Gate ten: TypeNative source migration and C-free repository

Status: preparation recorded; implementation and retirement evidence remain
open. See [`gate10-native-inventory.md`](gate10-native-inventory.md).

Planned scope:

- Port project-owned runtime, standard-library, Redis, validation, and Node-API
  implementation logic to TypeNative.
- Remove handwritten project-owned C/C++ implementation sources and replace
  them with TypeNative code plus explicit `extern "C"` declarations for external
  OS, libc, LLVM, Node-API, and other native libraries.
- Add source-tree, dependency-provenance, ABI, sanitizer, and clean-checkout
  evidence proving that normal TypeNative builds do not require project-owned C.

## Gate eleven: independent full self-hosting

Status: not started. Entry requirements are recorded in
[`gate11-preparation.md`](gate11-preparation.md); no final Gate 11 result is
claimed.

Planned scope:

- Replace the current bounded TypeNative hosted analysis/lowering slice with a
  complete TypeNative compiler, macro host, generator lowering, LLVM backend,
  formatter, CLI, documentation generator, test runner, and language server.
- Build compiler B from TypeNative compiler A and compiler C from B without a
  Rust-driver-assisted compiler pipeline.
- Run every prior-gate suite independently under B and C on macOS ARM64.

## Gate twelve: final conformance and cross-host verification

Status: planned; not started. No verification evidence exists for this gate.

Planned scope:

- Execute the complete matrix on macOS ARM64.
- Re-run all formatter, lint, build, test, sanitizer, ABI, Node-API, Redis,
  macro, generator, `using`, C-free, and A/B/C reproducibility checks.
- Record exact commands, toolchains, artifact digests, results, and genuine
  blockers before any completion claim.

## Consolidated verification ledger

```text
TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/verify-all.sh
FAIL on macOS ARM64 (2026-08-12, post-TypeNative lowering, constant/helper-call propagation,
collection validation, native product-linking, and Node-API emission): prior matrix phases pass;
bootstrap-self-host.sh fails at independent compiler A -> B with SYNTAX_SELFHOST(1014) at bytes
20229..20240
The latest focused bootstrap rerun on 2026-08-12 additionally passes the source-level conditional
selection and literal `if`/`else` return regressions before reaching the same independent A -> B
boundary:
  TN_BIN=/Users/ronnie/.cargo.target/debug/tn TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative/runtime scripts/bootstrap-self-host.sh
  PASS: self-hosted-conditional-selection=pass
  PASS: all existing A frontend, semantic, native-link, Node-API, diagnostic, and syntax-differential checks
  PASS: tn-timing phases module-check, ownership, mir-drop, monomorphization
  FAIL: error[SYNTAX_SELFHOST](1014) at bytes 20229..20240
  FAIL: independent self-hosting failed: compiler A could not build compiler B

The strict runtime regression now also covers evaluator comparisons and LLVM zero-extended
comparison plus `select` emission:
  cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/runtime.c -o /tmp/runtime-select-compare.o
  scripts/verify-runtime.sh
  PASS: runtime-collections-refcounts=pass

Parameterized void-call source lowering now recognizes a simple `i32` argument, records the
argument operation before the void-call operation, and resolves the declared result type after the
parameter list. The independent bootstrap fixture emits `call void @consume(i32 42)`, links, runs,
and reports `self-hosted-parameterized-void-call=pass`. The verification driver now checks the
compiler import graph once through `compiler-tn/main.tn`, avoids repeating standard-library source
checks inside the hosted suite, records per-command elapsed time, and runs eight independent suites
concurrently with isolated logs. `/tmp/verify-all-parallel.log` completed in 93 seconds versus 183
seconds for `/tmp/verify-all-select.log`; every phase before bootstrap passed, and bootstrap retained
the known A -> B `SYNTAX_SELFHOST(1014)` failure at bytes `20229..20240`.

Full matrix rerun after the comparison and `if` lowering on 2026-08-12 captured
`/tmp/verify-all-select.log`. Every design, toolchain, Rust workspace, TypeNative format/check,
CLI, hosted standard-library, runtime, debug-info, C ABI, Node-API, Redis, sanitizer, and 10,000-run
fuzz phase passed; the command exited `1` only when the independent bootstrap reached A -> B with
`SYNTAX_SELFHOST(1014)` at bytes `20229..20240`.
The clean-state bootstrap confirmation captured `/tmp/bootstrap-select-final.log`; it reports
`self-hosted-conditional-selection=pass` (including the literal `if`/`else` return fixture) and
the same A -> B failure, with `module-check=7129000ns`, `ownership=7166000ns`, `mir-drop=0ns`,
and `monomorphization=8000ns`; no B or C artifact was produced.
FAIL on macOS ARM64 (2026-08-12, after parameter-aware LLVM emission): the same prior phases,
strict C compilation, runtime parameter evaluation, and parameterized LLVM IR regression pass;
independent compiler A -> B still fails with SYNTAX_SELFHOST(1014) at bytes 20229..20240.
The captured run is `/tmp/verify-all-parameter-emitter.log`; its completed phases include
`runtime-collections-refcounts=pass`, all four sanitizer variants, and both 10,000-run fuzz
targets before the bootstrap failure.
After adding the aggregate typed backend operation stream, the focused quality loop also passed:
`cargo fmt --all -- --check`, `cargo test --workspace --all-targets` (all workspace tests),
`cargo clippy --workspace --all-targets -- -D warnings`, `RUSTDOCFLAGS=-Dwarnings cargo doc
--workspace --no-deps`, all TypeNative format/check passes, `scripts/verify-design.sh`, and
`scripts/verify-runtime.sh`.
PASS (pre-independent-order correction): CLI, hosted standard-library, runtime, debug-info, C ABI,
      Node-API, Redis, sanitizer, fuzzing, and hosted-boundary bootstrap checks
PASS: self-hosted-native-link-products=pass
PASS: self-hosted-node-addon=pass
PASS: self-hosted-radix-literals=pass
PASS: self-hosted-diagnostic-records=pass
PASS: bootstrap-fixed-point=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-repeatable=26fbdfdc551f1b86a80f2f8cb97c4383af76d475e2e59b4b78f1707c5bfc5844
PASS: bootstrap-source-fixed-point=4a882a717d32b36a72d66c1a5bbec6106a1889a0f17cfc3585b1bda308db65ee

cargo fmt --all -- --check
tn fmt --check compiler-tn
tn fmt --check std
tn fmt --check validation
cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/runtime.c
cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/redis.c
PASS: final-format-and-strict-c=pass

(cd fuzz && cargo +nightly fuzz run lexer -- -runs=10000 -max_len=4096 -timeout=5)
PASS: 10000 runs

(cd fuzz && cargo +nightly fuzz run parser -- -runs=10000 -max_len=4096 -timeout=5)
PASS: 10000 runs
```

Linux x86-64 verification completed as far as the host permits on 2026-08-10 with Podman,
Rust 1.96.0, official LLVM 22.1.8 Debian packages, Clang 22, Node-API headers/runtime, GDB,
and redis-cli. The full Rust workspace, native LLVM suite, hosted standard library, debug info,
C ABI, Node-API, Redis protocol/lifecycle, and the historical hosted-boundary bootstrap A/B/C checks
pass. AddressSanitizer and
UndefinedBehaviorSanitizer pass. ThreadSanitizer cannot start under the current Rosetta-backed
emulation because it exits before executing the probe with
`FATAL: ThreadSanitizer: memory layout is incompatible, even though ASLR is disabled.`; a native
Linux x86-64 host is still required for that check. A post-change strict Linux C compile of the runtime and Redis
sources also passes after the self-host diagnostic/CLI runtime additions:

```text
podman run --rm --platform linux/amd64 ... cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/runtime.c
podman run --rm --platform linux/amd64 ... cc -std=c11 -Wall -Wextra -Werror -pthread -c runtime/redis.c
PASS: linux-runtime-c-portability=pass
```

The last complete macOS matrix before the independent-order correction passed the hosted-boundary
checks, but its B/C artifacts were Rust-driver-built and therefore cannot be used as independent
self-hosting evidence. The current independent-order attempt fails at A→B as recorded above. The
full Linux compiler/hosted matrix recorded above is historical evidence only and is not an active
support requirement. Gate eight remains open because `compiler-tn` is not yet an independent
full LLVM-backed compiler. Gate 9 is currently active in the ordinary bootstrap
path with preparation evidence recorded; Gates 10–12 remain open, and no final
independent self-hosting result is claimed.
