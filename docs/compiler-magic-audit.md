# TypeNative Compiler-Magic Audit

## 1. Purpose

This audit defines the boundary between ordinary TypeNative declarations and
the small set of language mechanisms that the Rust compiler must lower
directly. A source name is not compiler-owned merely because it is convenient
for the standard library. The default is ordinary lookup, ordinary typing, and
ordinary user-visible diagnostics.

The audit applies to the active Rust compiler only. `compiler-tn/**` is frozen
and is not a source target for this program.

## 2. Accepted compiler-owned mechanisms

| Mechanism | Compiler boundary | Required reason |
| --- | --- | --- |
| Primitive scalar types and fixed-width integer literals | Lexer, type parser, constant evaluator, ABI lowering | Representation and overflow semantics are part of the language |
| References, named lifetimes, lifetime elision, and borrow regions | Type representation, ownership checker, HIR/MIR | Safety facts must be checked before code generation |
| `Promise<T, E>`, `try`, `try await`, and async suspension | Effect checking, state-machine lowering, executor ABI | Suspension and typed error edges require compiler control flow |
| `using`, `await using`, and disposal scope cleanup | Ownership/HIR/MIR cleanup elaboration | Exactly-once cleanup must cover every exit path |
| Class inheritance, `super`, interface witnesses, and `override` | Nominal checking and dispatch lowering | The compiler owns initialization and dispatch invariants |
| Array/slice/reference ABI construction | Type checking and MIR/LLVM lowering | Safe fat-slice and bounds representations need invariant lowering |
| `declare extern "C"`, `extern struct`, and `export extern "C"` | Grammar, ABI checking, symbol/export lowering | Foreign ABI contracts must be explicit and validated |
| `unsafe` operations | Effect checking and FFI boundary validation | Unsafe capability must be visible at the call site |
| Contextual numeric inference and checked arithmetic | Type inference, constant evaluation, MIR validation | Width, overflow, and ABI behavior must be deterministic |

These mechanisms do not create source-level attributes. Their syntax and
semantics are specified directly in [`language-spec.md`](language-spec.md).

## 3. Trusted intrinsic manifest

Representation-level operations that cannot be expressed safely in TypeNative
are bound by a private compiler-owned manifest. Each record uses:

- the exact declaration identity, including its resolved module;
- the approved standard-library or runtime module location;
- the operation category (representation, allocation, ABI, or lowering);
- the expected type and ownership contract; and
- the MIR operation emitted by the binding.

The manifest is not TypeNative source, is not importable, and is not user
configurable. A same-named declaration in another module is ordinary code. A
source attribute cannot create, copy, rename, or move a manifest binding.

Manifest review must prove that the operation is generic and reusable. RESP
framing, Redis command markers, Redis error states, and other application
protocol behavior are not eligible intrinsic categories.

## 4. Ordinary public declarations

The following are ordinary library or application declarations and must resolve
through the normal module/type/member pipeline:

- `string` methods (`String`, `fromUtf8`, `fromUtf8Lossy`, `startsWith`,
  `includes`, `slice`, and `toUpperCase`);
- generic byte operations (`find`, `equals`, `hash`, `validateUtf8`, and
  `parseUnsigned`);
- collection methods and constructors;
- `Disposable`, `AsyncDisposable`, and their symbol-named methods;
- `Thread`, `JoinHandle`, `Task`, `TaskGroup`, `IoEvent`, `File`, and network
  wrappers; and
- user-defined decorators and `ClassMethodDecoratorContext` values.

The compiler may optimize these declarations after ordinary type checking, but
optimization must preserve their declared ownership, error, cleanup, ABI, and
observable behavior.

## 5. Forbidden source-owned compiler claims

These forms are rejected everywhere in active source, including the standard
library. They cannot be reintroduced as aliases:

| Forbidden claim | Reason for rejection | Canonical source model |
| --- | --- | --- |
| Ownership or thread-safety decorator | A declaration could forge a safety fact | Structural inference and capture checking |
| Conformance decorator | Duplicates nominal interface syntax | `implements` |
| Layout decorator | Hides ABI and field-order decisions | `enum Kind: u8` or `extern struct` |
| Export decorator | Hides symbol visibility | `export` and `export extern "C" function` |
| Intrinsic decorator | Makes compiler capabilities forgeable | Private trusted manifest |
| Inline or test decorator | Couples ordinary source to tooling/optimization directives | Optimizer decisions and `test(...)` registration |
| Destructor decorator or direct `drop()` | Allows duplicate or bypassed cleanup | Automatic destruction plus disposal symbols |
| `macro` or expansion decorator | Adds a second declaration language | Functions, generics, interfaces, and declarations |
| `scope` source lifetime category | Leaks implementation terminology | Elision, named lifetimes, and `static` |

User-defined `@decorator` syntax remains valid only when the name resolves to an
ordinary callable decorator. The checker rejects a decorator that changes ABI,
ownership, capability facts, intrinsic identity, or unrelated declarations.

## 6. Internal MIR operation vocabulary

Internal operation tags never participate in source resolution. Examples
include `size_of`, `move_element`, `store_element`, `borrow_element`,
`dereference`, `store_raw`, `drop_initialized_elements`, `borrow_mut`,
`borrow_shared`, `string_from_static`, `slice_from_raw_parts`, and executor
state-machine operations.

Each tag must be produced from a typed HIR/MIR fact and validated before LLVM
lowering. The tag list must contain no RESP, Redis, command-marker, or
application-protocol operation. A generic byte accelerator is allowed only when
the same operation is independently useful outside Redis.

## 7. Audit tests

The Rust test suite and validation fixtures must prove:

1. ordinary declarations with intrinsic-like names are not privileged;
2. manifest identity and approved-module checks reject forged bindings;
3. user decorators cannot alter safety or ABI facts;
4. every forbidden construct receives a localized condition identifier and
   parser recovery; and
5. the formatter, LSP, documentation generator, and Node declaration generator
   never emit compiler-owned source decorators or macros.
