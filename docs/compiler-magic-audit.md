# TypeNative Compiler-Magic Audit

## Purpose

This ledger records every source-visible name or category that the Rust
bootstrap compiler recognizes without ordinary lexical lookup. Each entry is
either a fundamental language mechanism or an identified migration obligation.
Internal MIR operation tags are also listed so backend behavior remains
traceable even though those tags are not source names.

## Fundamental type and syntax names

Foreign declaration blocks use the grammar `declare extern "C" { ... }`. The
separate `extern "C" function(...)` spelling remains a foreign function-pointer
type and does not take `declare`.

| Compiler-known item                                                                                               | Boundary               | Justification                                                                                                                         |
| ----------------------------------------------------------------------------------------------------------------- | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `bool`, fixed integers, `isize`, `usize`, `number`, `f32`, `f64`, `char`, `void`, `never`, `unknown`, `undefined` | lexer and type parser  | Closed primitive type grammar with target-independent semantics.                                                                      |
| `string`, `str`                                                                                                   | type parser and layout | Fundamental owned UTF-8 and unsized UTF-8-view representations. Members resolve through the declared standard-library intrinsic type. |
| `Promise`                                                                                                         | type parser            | Built-in effect-bearing async result constructor; its two generic arguments participate directly in async typing and MIR suspension.  |
| `this`, `super`                                                                                                   | expression resolver    | Contextual receiver names supplied by method and constructor scopes.                                                                  |
| `static`, `scope`                                                                                                 | lifetime normalization | Canonical lifetime categories used by literal and lexical borrows.                                                                    |

## Compiler attributes

| Name                                         | Boundary                                 | Justification                                                                                                                                                                                                              |
| -------------------------------------------- | ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `@Copy`, `@Clone`, `@Drop`, `@Send`, `@Sync` | ownership derivation                     | Closed ownership and capability markers validated structurally.                                                                                                                                                            |
| `@Conform`, `@Sealed`                        | nominal type checking                    | Closed conformance and hierarchy metadata consumed before body checking.                                                                                                                                                   |
| `@Layout`, `@Export`                         | ABI lowering                             | Explicit layout and symbol contracts required at foreign boundaries.                                                                                                                                                       |
| `@Inline`, `@Test`                           | optimization and test discovery          | Closed compiler directives with validated targets and arguments.                                                                                                                                                           |
| `@Intrinsic`                                 | standard-library implementation boundary | Declares reviewed compiler primitives. `@Intrinsic("string")` binds the bundled string member declaration; `@Intrinsic("slice_from_raw_parts")` constructs a fat borrowed slice. Both bindings reject user-defined claims. |
| `macro`, `@Expand`                           | declaration expansion                    | A local typed token-template boundary. It adds only declaration members/validated conformance attributes, rejects native and ownership capabilities, and re-enters the ordinary parser and semantic pipeline.              |

## Declared protocol names still inspected by semantic rules

| Name                                                            | Current use                       | Disposition                                                                                                                                                                                       |
| --------------------------------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Display`                                                       | Template interpolation capability | Ordinary conformance lookup selects implementations, but the capability name is still compiler-known. Keep only if the language specification defines template display as a fundamental protocol. |
| `IntoIterator`, `Iterator`, `Iterable`, `AsyncIterable`, `next` | iteration protocol                | The declarations and method identities are ordinary; loop lowering selects the recorded interface witnesses and is justified by `for of`/`for await of` syntax.                                   |
| `Array`, `ArrayIterator`, `AsyncArrayIterator`                  | generator runtime protocol        | Generator lowering uses the declared array and iterator constructors/methods as the ordinary storage and witness protocol; no collection-specific semantic type checker exists.                   |
| `Drop`                                                          | Destructor implementation         | Fundamental ownership hook; prevents recursive implicit destruction inside its own body.                                                                                                          |
| `run`, `detach`                                                 | async effect and lifetime checks  | Migration obligation: replace name tests with declared callable metadata.                                                                                                                         |

## Private intrinsic functions

Private helper names are not compiler-known. Standard-library functions bind
to reviewed operations with explicit attribute arguments such as
`@Intrinsic("size_of")`, `@Intrinsic("borrow_shared")`, and
`@Intrinsic("arc_clone")`. Renaming a helper preserves its compiler behavior;
moving an operation outside its approved standard module or forging the
attribute in user source is rejected. These operations implement representation
or ownership behavior that TypeNative source cannot express safely.

## Internal MIR operation tags

These strings never participate in source resolution: `size_of`, `is_string`,
`element_initialized`, `move_element`, `store_element`, `borrow_element`, `dereference`,
`store_raw`, `drop_initialized_elements`, `borrow_mut`, `borrow_shared`,
`arc_clone`, `string_from_static`, and `slice_from_raw_parts`. Each is produced
by typed MIR lowering and consumed by LLVM lowering. `string_from_static` is the
explicit contextual literal-to-owned conversion; `slice_from_raw_parts` is the
fat borrowed-slice constructor used privately by string and collection views.
`borrow_element` derives a typed shared
reference from an already bounds-checked standard-library storage slot; it is
restricted to `std/collections` because safe TypeNative cannot express the
required raw-pointer offset while preserving the borrow's referent type.

## String conclusion

No canonical string method name is compiler-known. `from`, `fromUtf8`,
`toAsciiUppercase`, `clone`, `asStr`, and `bytes` are methods of the private
bundled intrinsic declaration and follow ordinary member resolution, receiver
checking, HIR identity, MIR direct dispatch, and native calling conventions.

`usize.parseAscii` likewise resolves through the private bundled
`@Intrinsic("usize")` declaration. The HIR has no `BuiltinValue` variant and
contains no compiler-enumerated standard-library method names.
