# TypeNative Language Specification

## 1. Status and conformance

This document is the normative definition of the canonical TypeNative language.
The Rust bootstrap compiler, formatter, standard library, tooling, and future
self-hosted compiler consume this same grammar. A conforming implementation must
accept every valid program described here, reject every excluded construct, and
report a stable machine-readable condition identifier with a primary source
span. Build profiles may change optimization and debug information only; they
must not change ownership, error, overflow, bounds, destruction, or panic
semantics.

The current implementation scope is the Rust bootstrap compiler through the
compiler-independent preparation for Gate 11. The `compiler-tn/**` source and
the A/B/C self-hosting chain are protected follow-up work and are not evidence
for this specification.

## 2. Source text and lexical rules

Source is UTF-8 and uses the `.tn` suffix. Invalid UTF-8 is rejected before
tokenization. Identifiers use Unicode XID start and continuation rules and are
not normalized. Locations are reported as byte offsets, lines, and Unicode
scalar columns. Whitespace, line comments, nested block comments, and
documentation comments are retained as trivia in the lossless CST.

Simple statements end in `;`; automatic semicolon insertion does not exist.
Strings use double quotes, characters use single quotes, and templates use
backticks with `${ expression }` interpolation. A template interpolation is
parsed with the ordinary expression grammar and is checked through `Display`.

The canonical keywords are:

```text
abstract  as  async  await  break  case  catch  class  const  constructor
continue  default  else  enum  export  extends  extern  false  final  for
from  function  if  implements  import  instanceof  interface  let  move
mut  new  of  override  private  protected  public  readonly  return  static
struct  super  switch  throw  throws  true  try  type  undefined  unknown
unsafe  using  while
```

`this` is a contextual receiver name. `super` is valid only in a derived
constructor or class method. The lexer reserves `null`, `self`, `impl`, `where`,
`match`, `dyn`, `use`, `mod`, `pub`, `crate`, `record`, and `extension` as
excluded keywords so each receives a localized rejection rather than becoming
an accidental user identifier. `Option`, `Result`, `Vec`, `HashMap`, and
`HashSet` are ordinary names but are rejected as obsolete public forms when
used in a TypeNative declaration or standard-library import.

Integer literals may be decimal, binary, octal, or hexadecimal, with single
underscores between digits. Suffixes are `i8`, `i16`, `i32`, `i64`, `i128`,
`isize`, `u8`, `u16`, `u32`, `u64`, `u128`, `usize`, and `number`. Decimal
floating literals accept `f32` or `f64`. Signs are operators, not part of a
literal. Literal spelling is preserved by the CST and formatter.

## 3. Canonical grammar

The following EBNF describes the public grammar. Recovery nodes may contain
missing-token expectations, but a recovered tree never invents a semantic
declaration or silently translates an excluded spelling.

```ebnf
source_file       = { item } ;
item              = import_declaration
                  | { attribute } [ "export" ] declaration ;
declaration       = const_declaration | static_declaration
                  | function_declaration | type_alias_declaration
                  | struct_declaration | class_declaration
                  | interface_declaration | enum_declaration
                  | extern_block ;

attribute         = "@" attribute_name [ "(" [ attribute_arguments ] ")" ] ;
attribute_name    = identifier | "export" ;
attribute_arguments = attribute_argument { "," attribute_argument } [ "," ] ;
attribute_argument = literal | type_path | identifier ;

import_declaration = "import" "{" import_names "}" "from" string_literal ";" ;
import_names      = import_name { "," import_name } [ "," ] ;
import_name       = identifier [ "as" identifier ] ;

const_declaration  = "const" identifier [ ":" type ] "=" expression ";" ;
static_declaration = "static" [ "mut" ] identifier ":" type
                     "=" const_expression ";" ;
type_alias_declaration = "type" identifier [ generic_parameters ] "=" type ";" ;

function_declaration = [ "unsafe" ] [ "async" ] "function" identifier
                       [ generic_parameters ] parameter_list ":" type
                       [ throws_clause ] block ;
throws_clause      = "throws" type_path { "|" type_path } ;

generic_parameters = "<" generic_parameter { "," generic_parameter } [ "," ] ">" ;
generic_parameter  = identifier [ "extends" generic_bound { "&" generic_bound } ] ;
generic_bound      = type_path | "static" ;
parameter_list     = "(" [ parameters ] ")" ;
parameters         = parameter { "," parameter } [ "," ] ;
parameter          = identifier ":" type ;

struct_declaration = "struct" identifier [ generic_parameters ] "{"
                     { struct_member } "}" ;
struct_member     = { attribute } [ member_visibility ]
                     ( field_declaration | method_declaration ) ;
class_declaration  = [ "abstract" | "final" ] "class" identifier
                     [ generic_parameters ] [ "extends" type_path ]
                     [ "implements" type_paths ] "{" { class_member } "}" ;
class_member       = { attribute } [ member_visibility ]
                     ( field_declaration | constructor_declaration
                     | method_declaration ) ;
interface_declaration = "interface" identifier [ generic_parameters ] "{"
                        { interface_member } "}" ;
interface_member   = { attribute } [ receiver_mode ] identifier
                      [ generic_parameters ] parameter_list ":" type
                      [ throws_clause ] ";" ;
enum_declaration   = "enum" identifier [ generic_parameters ] "{"
                     enum_variant { "," enum_variant } [ "," ] "}" ;
enum_variant       = identifier
                   | identifier "(" type_list ")"
                   | identifier "{" { field_declaration } "}"
                   | identifier "=" const_expression ;
extern_block       = "extern" string_literal "{" { extern_function } "}" ;
extern_function    = { attribute } "function" identifier
                     extern_parameter_list ":" type ";" ;

field_declaration  = [ "static" [ "mut" ] ] [ "readonly" ] identifier
                     [ "?" ] ":" type [ "=" expression ] ";" ;
constructor_declaration = "constructor" parameter_list
                          [ throws_clause ] block ;
method_declaration = [ "static" ] [ "abstract" | "final" | "override" ]
                     [ receiver_mode ] [ "unsafe" ] [ "async" ] identifier
                     [ generic_parameters ] parameter_list ":" type
                     [ throws_clause ] ( block | ";" ) ;
receiver_mode      = "mut" | "move" ;
member_visibility  = "public" | "protected" | "private" ;
type_paths         = type_path { "," type_path } ;

type               = primary_type [ "|" "undefined" ] ;
primary_type       = type_path | reference_type | raw_pointer_type
                   | array_type | slice_type | grouped_type | tuple_type
                   | function_type | foreign_function_type ;
type_path          = identifier { "." identifier } [ generic_arguments ] ;
generic_arguments  = "<" type_arguments ">" ;
type_arguments     = type { "," type } [ "," ] ;
reference_type     = "&" [ "mut" ] primary_type ;
raw_pointer_type   = "*" ( "const" | "mut" ) primary_type ;
array_type         = "[" type ";" const_expression "]" ;
slice_type         = "[" type "]" ;
grouped_type       = "(" type ")" ;
tuple_type         = "(" type "," [ type { "," type } [ "," ] ] ")" ;
function_type      = [ "async" ] "(" [ type_list ] ")" "=>" type ;
foreign_function_type = "extern" string_literal "function" "("
                        [ type_list ] ")" ":" type ;

block              = "{" { statement } "}" ;
statement          = block | local_declaration | using_statement
                   | expression_statement | return_statement | throw_statement
                   | if_statement | while_statement | for_statement
                   | try_statement | unsafe_statement | break_statement
                   | continue_statement ;
local_declaration  = ( "const" | "let" ) identifier [ ":" type ]
                     "=" expression ";" ;
using_statement    = [ "await" ] "using" identifier "=" expression ";" ;
expression_statement = expression ";" ;
return_statement   = "return" [ expression ] ";" ;
throw_statement    = "throw" expression ";" ;
if_statement       = "if" "(" expression ")" statement [ "else" statement ] ;
while_statement    = "while" "(" expression ")" statement ;
for_statement      = "for" "(" ( "const" | "let" ) identifier "of"
                     expression ")" statement ;
try_statement      = "try" block "catch" "(" identifier ":" type_path ")"
                     block { "catch" "(" identifier ":" type_path ")" block } ;
unsafe_statement   = "unsafe" block ;

expression         = assignment_expression ;
assignment_expression = conditional_expression
                      [ assignment_operator assignment_expression ] ;
conditional_expression = coalesce_expression
                         [ "?" expression ":" expression ] ;
coalesce_expression = logical_or_expression { "??" logical_or_expression } ;
logical_or_expression = logical_and_expression { "||" logical_and_expression } ;
logical_and_expression = equality_expression { "&&" equality_expression } ;
equality_expression = relational_expression
                      { ( "===" | "!==" ) relational_expression } ;
relational_expression = additive_expression
                        { ( "<" | "<=" | ">" | ">=" | "instanceof" )
                          additive_expression } ;
additive_expression = multiplicative_expression { ( "+" | "-" )
                      multiplicative_expression } ;
multiplicative_expression = unary_expression { ( "*" | "/" | "%" )
                            unary_expression } ;
unary_expression    = postfix_expression
                    | ( "!" | "-" | "~" | "move" | "await" ) unary_expression
                    | "&" [ "mut" ] unary_expression
                    | "*" unary_expression
                    | "try" [ "await" ] unary_expression ;
postfix_expression  = primary_expression { postfix_operation } ;
postfix_operation   = generic_arguments argument_list | argument_list
                    | "." identifier | "?." identifier
                    | "[" expression "]" | "as" type | "as?" type_path
                    | "!" ;
primary_expression  = literal | identifier | "this" | "super" | "undefined"
                    | "new" type_path argument_list | array_literal
                    | type_path object_literal | object_literal
                    | tuple_or_group | lambda_expression | switch_expression ;
switch_expression   = "switch" "(" expression ")" "{"
                      switch_arm { switch_arm } "}" ;
switch_arm          = "case" pattern [ "if" expression ] ":"
                      ( expression "," | block )
                    | "default" ":" ( expression "," | block ) ;
pattern             = "_" | literal | "undefined" | identifier
                    | type_path [ "(" [ patterns ] ")" ]
                    | type_path "{" [ pattern_fields ] "}" ;
```

`const_expression` is a side-effect-free expression containing literals,
constant references, aggregate construction, and compiler-known operations.
The Pratt precedence is: postfix, unary, multiplicative, additive, relational,
strict equality, logical AND, logical OR, nullish coalescing, conditional, and
assignment. Evaluation is left to right. Conditions must be `bool`; there is no
numeric, string, pointer, or optional truthiness.

## 4. Modules and declarations

Each file is one module. A relative specifier maps to exactly one `.tn` file;
`std/` maps to the bundled standard library. Directory fallback, package
registries, dynamic imports, default exports, namespace imports, `use`, `mod`,
`pub`, and `crate` are compile-time errors. Declaration cycles are allowed, but
top-level executable initialization is forbidden. `export` is the only way to
expose a top-level declaration; member visibility is explicit or private by
default.

`struct` is a nominal inline value. Its fields have declaration-order layout and
its methods are declared inside the struct. An expected struct type is required
for an object literal; unknown fields, duplicate fields, and spreads are
errors. Structs do not acquire identity or implicit heap allocation.

`class` is a nominal uniquely owned heap identity. `new C(arguments)` allocates
it, and assignment, passing, or returning moves the owning handle. Shared class
identity requires explicit `Rc`, `Arc`, or `Weak`. Classes have single
inheritance, optional `abstract`/`final`, direct constructors and methods,
`extends`, `implements`, `override`, `super`, `readonly`, and visibility. A
non-final instance method is virtual; an override must be marked `override`.
Derived constructors call `super(...)` exactly once as their first statement.

Interfaces are nominal contracts. A struct, enum, or class declares conformance
with `@Conform(Interface)` or `implements Interface`, and supplies the members
directly in its declaration. Conformance is explicit and coherent: at most one
implementation exists for an interface and nominal type, and either the
interface or nominal type's module must own the declaration. Interface values
use the compiler's dynamic witness representation; the public language does
not expose a `dyn` spelling.

Generic constraints are written on the parameter: `T extends Interface`. A
generic body is checked against its declared bounds and monomorphized for each
reachable concrete use. Generic types are invariant; only class upcasts,
explicit interface coercions, lifetime shortening, and `&mut T` to `&T`
reborrowing are implicit.

```tn
interface Display {
  display(out: &mut Formatter): void;
}

@Conform(Display)
struct Point {
  public x: f64;
  public y: f64;

  public distanceSquared(): f64 {
    return this.x * this.x + this.y * this.y;
  }

  display(out: &mut Formatter): void {
    out.write(`Point(${this.x}, ${this.y})`);
  }
}
```

## 5. Types, ownership, and destruction

The primitive types are `bool`, the fixed signed and unsigned integers,
`isize`, `usize`, `number` (an exact alias of `isize`), `f32`, `f64`, `char`,
`void`, `never`, `string`, `str`, `unknown`, and `undefined`. `T | undefined`
is the only union syntax. Optional fields `field?: T` are the same type.
General unions and intersections are excluded; use nominal enums for other sum
types.

`string` is the sole public owned UTF-8 text type. `str` is an unsized UTF-8
view and `&str` is its borrowed form. The uppercase name `String` is obsolete
and rejected. A string literal has type `&static str`; when an assignment,
argument, return, field, or enum payload requires owned `string`, the compiler
materializes an explicit owned conversion in HIR and MIR. This contextual rule
applies only from a string literal to `string` and is not a general implicit
conversion.

Canonical owned text operations are `string.from(view)`,
`string.fromUtf8(bytes)`, `text.toAsciiUppercase()`, `text.clone()`,
`text.asStr()`, and `text.bytes()`. Strict equality compares UTF-8 contents
without allocating. Text cannot be indexed by byte; callers use byte, scalar,
or grapheme views and checked UTF-8-boundary slicing.

Integer literals infer from assignments, parameters, returns, fields, enum
payloads, and generic instantiations. An unconstrained integer defaults to
`number`; an unconstrained decimal defaults to `f64`. A suffix selects an
explicit type. Numeric conversion is explicit through checked `from` or
fallible `tryFrom`; ordinary arithmetic never silently widens or truncates.

Every value is affine. Owned assignment, an owned argument, a return, aggregate
storage, or `move` capture moves the value. `@Copy` is valid only when all
stored fields are copyable and no destructor exists. `@Clone` provides an
explicit cloning operation; it is never inserted implicitly. `&T` is a shared
borrow and `&mut T` is an exclusive borrow. `mut` makes a binding or receiver
mutable. Borrows are non-lexical and cannot outlive, cross a move of, or escape
their referent. Object and array destructuring uses the same rules: copied
inputs copy eligible fields, owned inputs move fields, borrowed inputs create
corresponding borrows, and partial moves remain visible to drop analysis.

`@Drop` marks compiler-owned deterministic destruction. A destructor cannot
throw, suspend, move fields, or be called directly. Locals, temporaries,
partially initialized aggregates, mutex guards, and async state are destroyed
exactly once on normal exits, recoverable error exits, cancellation, and
constructor failure. `panic` aborts without unwinding; it never replaces a
cleanup-preserving typed error.

Raw pointers are `*const T` or `*mut T` and may be invalid. Dereference,
pointer arithmetic, uninitialized or unaligned access, manual allocation,
foreign calls, mutable statics, and manual `@Send`/`@Sync` conformance require
an `unsafe` block or function. Unsafe code does not disable ownership or
initialization checking.

```tn
struct Packet {
  public header: u32;
  public payload: string;
}

function split(packet: Packet): string {
  const payload = packet.payload;
  return payload;
}

function readFirst(values: &[u8]): u8 | undefined {
  return values.length > 0usize ? values[0] : undefined;
}
```

## 6. Optionals and narrowing

`?.` skips the remainder of an optional postfix chain when its receiver is
`undefined`; `??` evaluates a fallback only on the absent path. Postfix `!`
force-unwraps an optional after a runtime check and invokes `panic` if absent.
The checker accepts a force unwrap only where the value is optional and keeps
the resulting payload type. `as?` is a checked, non-consuming class/interface
downcast and returns a corresponding optional borrow. `unknown` can be used
only after a type guard, strict equality check, `instanceof`, or checked
downcast. `null` and unchecked assertions do not exist.

```tn
function choosePort(config: Config): u16 {
  const selected = config.port ?? 6379;
  if (config.port !== undefined) {
    return config.port!;
  }
  return selected;
}
```

## 7. Collections and construction

Fresh owned values use exactly `new Type(arguments)`. User classes, `Array`,
`Map`, `Set`, `OrderedMap`, `OrderedSet`, `Queue`, `Deque`, `Heap`, `Arc`, and
`Mutex` all use `new`. Empty construction uses `new Array<T>()` or
`new Map<K, V>()`. Constructor options may include `capacity`, as in
`new Array<i32>({ capacity: 1024 })`. `Type.from(existing)` is reserved for
conversion from existing data. `withCapacity`, `Arc.new`, `Mutex.new`, and
competing free constructors are rejected.

The required collection surface is `Array<T>`, fixed arrays, borrowed slices,
`Map<K,V>`, `Set<T>`, `OrderedMap<K,V>`, `OrderedSet<T>`, `Queue<T>`,
`Deque<T>`, and `Heap<T>`. Each growable collection exposes read-only
`length` and `capacity`, automatic growth, checked `reserve(minimumCapacity)`,
`shrinkToFit()`, and `clear()` that retains capacity. Allocation overflow is
checked, elements are destroyed exactly once, and map/set keys require explicit
`Equal` and `Hash` capabilities.

```tn
function makeUsers(): Array<string> {
  const users = new Array<string>({ capacity: 2 });
  users.push("ronnie");
  users.push("guest");
  users.reserve(8);
  return users;
}
```

## 8. Errors, async, generators, and tasks

Synchronous functions use `function f(): T throws E`. Every throwing call is
prefixed by `try`; a typed `catch` handles a closed set of nominal error types.
Catch clauses are ordered, exhaustive for the reaching effects, and may throw
only errors declared by the enclosing function. Errors are tagged return values
with explicit cleanup edges; native unwinding is never used.

Async functions return `Promise<T, E>`, where `E` is a nominal error type or
`never` for an empty set. Calling an async function creates a cold, move-only
promise and does not require `try`; consuming a fallible completion requires
`try await` or an explicit executor operation. A promise is pinned once polled,
and dropping it cancels and destroys initialized state exactly once.

`using` owns a synchronous disposable until scope exit. `await using` owns an
async disposable and awaits its cleanup. A non-suspendable resource, including
a mutex guard, must be destroyed before the enclosing `await`. Generators,
async generators, `Iterable<T>`, `Iterator<T>`, `AsyncIterable<T>`, `for of`,
and `for await of` lower through explicit state machines. `TaskGroup` owns
structured children, propagates cancellation, and waits before scope exit;
`detach` is explicit and requires process-lifetime owned `Send` captures.

```tn
async function loadConfig(path: &str): Promise<Config, IOError> {
  await using file = try await File.open(path);
  return try await file.readConfig();
}

function main(): i32 throws IOError {
  try {
    const config = try run(loadConfig("config.tn"));
    console.log(config.name);
    return 0;
  } catch (error: IOError) {
    console.error(error.message());
    return 1;
  }
}
```

## 9. Control flow and interfaces

`switch` is an expression and is exhaustive over enums, booleans, optionals,
and finite integer discriminants. Infinite domains require a binding or
`default`. Guards run only after a pattern matches; unreachable arms and
incompatible arm types are errors. `for of` selects `IntoIterator<Item, Iter>`
and `Iterator<Item>` witnesses with explicit methods; it does not introduce
implicit copies.

```tn
function encode(reply: Reply): string {
  return switch (reply) {
    case Reply.Simple(message): `+${message}\r\n`,
    case Reply.Integer(value): `:${value}\r\n`,
    case Reply.Missing: "$-1\r\n",
    default: "-ERR\r\n",
  };
}
```

## 10. Interoperability and compiler-owned metadata

`extern "C"` declares foreign functions. Foreign calls are unsafe. C-compatible
signatures contain only fixed-width integers, explicit `c_*` aliases, supported
floats, raw pointers, C function pointers, and `@Layout("C")` structs or
fieldless enums. They cannot contain classes, references, strings, slices,
optionals, generic values, promises, or typed error effects. Exported C
functions are non-generic, synchronous, non-throwing, and use only these types.

`@Export("symbol")` selects a supported C or Node-API export. Node-API
generation maps 32-bit values to JavaScript `number`, wider and pointer-width
integers to `bigint`, strings to `string`, bytes to `Uint8Array`, optionals to
optional values, and `Promise<T,E>` to a JavaScript promise with typed rejection.
Borrowed outputs are rejected; class wrappers run `Drop` exactly once.

Compiler-owned attributes are `@Copy`, `@Clone`, `@Drop`, `@Send`, `@Sync`,
`@Conform`, `@Sealed`, `@Layout("C")`, `@Export`, `@Intrinsic`, `@Inline`, and
`@Test`. Unknown or incorrectly targeted attributes are errors. Typed declaration
macros may expand only deterministic AST-level declarations in a sandbox with
no filesystem, network, environment, or unsafe ABI bypass. Expanded code goes
through ordinary name, type, ownership, effect, and ABI checks.

```tn
@Layout("C")
struct Pair {
  public left: i32;
  public right: i32;
}

extern "C" {
  function puts(text: *const c_char): c_int;
}

@Export("tn_pair_value")
function pairValue(pair: Pair): i32 {
  return pair.left + pair.right;
}
```

## 11. Standard library, layout, and tooling

The standard library is TypeNative over narrow reviewed OS, libc, LLVM, and
Node-API boundaries. Its public modules include `std/core`, `std/alloc`,
`std/fmt`, `std/console`, `std/collections`, `std/bytes`, `std/string`,
`std/io`, `std/fs`, `std/net`, `std/time`, `std/thread`, `std/sync`,
`std/async`, `std/process`, `std/path`, `std/env`, `std/ffi`, and
`std/testing`. `console.log`, `console.error`, and `console.write` are ordinary
synchronous best-effort library calls with no error effect; reliable output
uses fallible `std/io` writers.

Default layouts are compiler-private. `@Layout("C")` fixes target C field order
and alignment; packed fields require explicit unaligned operations. The only
supported target is `aarch64-apple-darwin`. The public command is `tn` with
`build`, `run`, `check`, `test`, `fmt`, `doc`, and `lsp` subcommands. Projects
use `typenative.json` and have no package-registry or dependency fields.

An executable has exactly one safe synchronous `main(): void` or `main(): i32`,
optionally with a closed `throws` set. An uncaught typed error is formatted and
returns status 1 after cleanup; `panic` aborts.

The formatter is lossless for trivia and literal spelling, uses two-space
indentation and deterministic import ordering, and is byte-idempotent. The
parser, formatter, documentation generator, language server, diagnostics, and
test runner share the same Rowan CST and resolved semantic model.

## 12. Explicit exclusions

The following are rejected rather than partially supported: default or
namespace imports, package specifiers, `null`, `self`, `impl`, `where`, `match`,
`dyn`, `use`, `mod`, `pub`, `crate`, `record`, `extension`, general union or
intersection value types, object spread, loose equality, unchecked assertions,
implicit numeric conversion, overload sets, multiple class inheritance,
executable decorators, unrestricted compile-time code, `eval`, runtime source
compilation, garbage collection, implicit class retention, native exception
unwinding, catching `panic`, and reflection over fields, methods, or
constructors. The rejection condition and replacement are listed in
[`canonical-migration-manifest.md`](canonical-migration-manifest.md).
