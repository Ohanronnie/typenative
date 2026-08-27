# TypeNative Language Specification

## 1. Status and authority

This document is the normative definition of the active TypeNative language.
The Rust compiler in `crates/tn-*`, the standard library, formatter, linter,
language server, documentation generator, test runner, diagnostics, and
generated Node declarations must implement this surface together. A feature is
not canonical until its parser, semantic model, ownership model, runtime
behavior, tooling, and validation fixtures agree.

The self-hosted sources in `compiler-tn/**` and their bootstrap orchestration
are frozen historical inputs. Self-hosting is not an active acceptance
requirement for this design convergence. The freeze is verified read-only by
`scripts/verify-selfhost-freeze.sh`; active verification never edits or
executes the frozen bootstrap chain.

Build profiles may change optimization and debug information. They must not
change ownership, lifetime, error, overflow, bounds, cleanup, cancellation, or
observable program behavior.

The following compact example is part of the executable specification. The CLI
test suite extracts every `tn-executable` block, type-checks it, and runs the
native product so the documentation cannot drift from the active compiler:

```tn-executable
function answer(): i32 {
  const base: i32 = 40;
  return base + 2;
}

function main(): i32 {
  return answer();
}
```

## 2. Source text and lexical rules

Source is UTF-8 and uses the `.tn` suffix. Invalid UTF-8 is rejected before
tokenization. Identifiers use Unicode XID start and continuation rules and are
not normalized. Locations are reported as byte offsets, lines, and Unicode
scalar columns. The lossless CST retains whitespace, comments, and
documentation comments as trivia.

Simple statements end in `;`; automatic semicolon insertion does not exist.
Strings use double quotes, characters use single quotes, and templates use
backticks with `${ expression }` interpolation. Template interpolation uses
ordinary expression typing and the declared `Display` interface.

The public keyword set is:

```text
abstract  as  async  await  break  case  catch  class  const  constructor
continue  declare  default  else  enum  export  extends  extern  false  for
from  function  if  implements  import  instanceof  interface  let  lifetime
move  new  of  override  private  protected  public  readonly  return  static
struct  super  switch  throw  throws  true  try  type  undefined  unknown
unsafe  using  while
```

`this` is a contextual receiver name. `super` is valid only in a derived
constructor or class method. `mut` is part of reference syntax (`&mut T` and
`&mut [T]`), not a receiver modifier or a binding modifier. The public grammar
has no `scope` lifetime keyword. `sealed`, `final`, `derives`, `macro`, and
`Expand` are excluded spellings and receive localized obsolete-syntax
diagnostics.

The lexer also reserves excluded historical words such as `null`, `self`,
`impl`, `where`, `match`, `dyn`, `use`, `mod`, `pub`, `crate`, `record`, and
`extension` so that they cannot become accidental user identifiers.

Integer literals may be decimal, binary, octal, or hexadecimal, with single
underscores between digits. Suffixes are `i8`, `i16`, `i32`, `i64`, `i128`,
`isize`, `u8`, `u16`, `u32`, `u64`, `u128`, and `usize`. Decimal floating
literals accept `f32` or `f64`. Signs are operators, not part of a literal.

## 3. Canonical grammar

The following EBNF describes the public grammar. Recovery nodes may contain
missing-token expectations, but a recovered tree never invents a declaration
or silently translates an excluded spelling.

```ebnf
source_file       = { item } ;
item              = import_declaration
                  | { attribute } [ "export" ] declaration ;
declaration       = const_declaration | static_declaration
                  | function_declaration | type_alias_declaration
                  | struct_declaration | foreign_struct_declaration
                  | class_declaration | interface_declaration
                  | enum_declaration | foreign_declaration_block
                  | exported_foreign_function ;

attribute         = "@" identifier [ argument_list ] ;

import_declaration = "import" "{" import_names "}" "from" string_literal ";" ;
import_names      = import_name { "," import_name } [ "," ] ;
import_name       = identifier [ "as" identifier ] ;

const_declaration  = "const" identifier [ ":" type ] "=" expression ";" ;
static_declaration = "static" identifier ":" type
                     "=" const_expression ";" ;
type_alias_declaration = "type" identifier [ generic_parameters ] "=" type ";" ;

function_declaration = [ "unsafe" ] [ "async" ] "function" identifier
                       [ generic_parameters ] parameter_list ":" type
                       [ throws_clause ] block ;
throws_clause      = "throws" type_path { "|" type_path } ;

generic_parameters = "<" generic_parameter { "," generic_parameter }
                     [ "," ] ">" ;
generic_parameter  = lifetime_parameter
                   | type_parameter ;
lifetime_parameter = "lifetime" identifier ;
type_parameter     = identifier [ "extends" generic_bound
                     { "&" generic_bound } ] ;
generic_bound      = type_path | "static" ;
parameter_list     = "(" [ parameters ] ")" ;
parameters         = parameter { "," parameter } [ "," ] ;
parameter          = identifier ":" type ;

struct_declaration = "struct" identifier [ generic_parameters ] "{"
                     { struct_member } "}" ;
struct_member     = { attribute } [ member_visibility ]
                     ( field_declaration | method_declaration ) ;

foreign_struct_declaration = "extern" "struct" identifier
                             [ generic_parameters ] "{"
                             { foreign_field_declaration } "}" ;
foreign_field_declaration = identifier ":" foreign_type ";" ;

class_declaration  = [ "abstract" ] "class" identifier
                     [ generic_parameters ] [ "extends" type_path ]
                     [ "implements" type_paths ] "{"
                     { class_member } "}" ;
class_member       = { attribute } [ member_visibility ]
                     ( field_declaration | constructor_declaration
                     | method_declaration ) ;
interface_declaration = "interface" identifier [ generic_parameters ] "{"
                        { interface_member } "}" ;
interface_member   = { attribute } [ member_visibility ] identifier
                      [ generic_parameters ] parameter_list ":" type
                      [ throws_clause ] ";" ;
enum_declaration   = "enum" identifier [ ":" integer_type ]
                     [ generic_parameters ] "{"
                     enum_variant { "," enum_variant } [ "," ] "}" ;
enum_variant       = identifier
                   | identifier "(" type_list ")"
                   | identifier "{" { field_declaration } "}"
                   | identifier "=" const_expression ;

foreign_declaration_block = "declare" "extern" string_literal "{"
                            { extern_function } "}" ;
extern_function    = "function" identifier extern_parameter_list
                     ":" type ";" ;
exported_foreign_function = "extern" string_literal "function" identifier
                            extern_parameter_list ":" type
                            [ throws_clause ] block ;

field_declaration  = [ "static" ] [ "readonly" ] identifier
                     [ "?" ] ":" type [ "=" expression ] ";" ;
constructor_declaration = "constructor" parameter_list
                          [ throws_clause ] block ;
method_declaration = [ "static" ] [ "abstract" ] [ "override" ]
                     [ "unsafe" ] [ "async" ] identifier
                     [ generic_parameters ] parameter_list ":" type
                     [ throws_clause ] ( block | ";" ) ;
member_visibility  = "public" | "protected" | "private" ;
type_paths         = type_path { "," type_path } ;

type               = primary_type [ "|" "undefined" ] ;
primary_type       = type_path | reference_type | raw_pointer_type
                   | array_type | slice_type | grouped_type | tuple_type
                   | function_type | foreign_function_type ;
type_path          = identifier { "." identifier } [ generic_arguments ] ;
generic_arguments   = "<" type_arguments ">" ;
type_arguments      = type { "," type } [ "," ] ;
reference_type     = "&" [ lifetime_name ] [ "mut" ] primary_type ;
lifetime_name      = identifier | "static" ;
raw_pointer_type   = "*" ( "const" | "mut" ) primary_type ;
array_type         = "[" type ";" const_expression "]" ;
slice_type         = "[" type "]" ;
grouped_type       = "(" type ")" ;
tuple_type         = "(" type "," [ type { "," type } [ "," ] ] ")" ;
function_type      = [ "async" ] "(" [ type_list ] ")" "=>" type ;
foreign_function_type = "extern" string_literal "function" "("
                        [ type_list ] ")" ":" type ;
foreign_type       = integer_type | "bool" | "char" | raw_pointer_type
                   | array_type ;
integer_type       = "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
                   | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" ;

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
if_statement       = "if" "(" expression ")" statement
                     [ "else" statement ] ;
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

`extern "C" function(...)` is a function-pointer type. A foreign declaration
block has exactly one spelling: `declare extern "C" { ... }`. A C-layout
declaration has exactly one spelling: `extern struct Name { ... }`. A public C
symbol uses `export extern "C" function name(...)`. These are grammar
productions, not decorators.

## 4. Modules, classes, interfaces, and value types

Each file is one module. Relative import specifiers map to one `.tn` file and
`std/` maps to the bundled standard library. Package registries, dynamic
imports, default exports, namespace imports, `use`, `mod`, `pub`, and `crate`
are not part of the language. `export` is the only top-level visibility
boundary; members are private by default unless their visibility is written.

`struct` is a nominal inline value with declaration-order fields. It has no
implicit identity or heap allocation. `class` is a nominal uniquely owned heap
identity. `new C(...)` creates it, and assignment, passing, and returning move
the owning handle. Shared identity requires an explicit shared-pointer type.

Classes support `abstract`, `extends`, `implements`, `override`, `readonly`,
`public`, `protected`, `private`, `static`, and `super`. Instance dispatch is
virtual by default; `override` is required for an override. There is no
source-level `sealed` or `final`. Closed dispatch is an optimizer proof, not a
user assertion. A derived constructor calls `super(...)` exactly once before
using the derived instance.

Interfaces are nominal contracts. Conformance is declared with `implements`
and members are supplied directly in the implementing declaration. Interface
values use an internal witness representation; no public `dyn` spelling
exists. Generic constraints use `T extends Interface`.

```ts
interface Display {
  display(out: &mut Formatter): void;
}

struct Point implements Display {
  public x: f64;
  public y: f64;

  public distanceSquared(): f64 {
    return this.x * this.x + this.y * this.y;
  }

  public display(out: &mut Formatter): void {
    out.write(`Point(${this.x}, ${this.y})`);
  }
}

abstract class Transport implements AsyncDisposable {
  public abstract read(buffer: &mut BytesMut): Promise<number, IOError>;

  public abstract [Symbol.asyncDispose](): Promise<void, never>;
}

class TcpTransport extends Transport {
  public override async read(
    buffer: &mut BytesMut,
  ): Promise<number, IOError> {
    return try await this.readInto(buffer);
  }
}
```

User-defined decorators use ordinary declaration lookup and the `@name` syntax.
They may wrap or initialize supported class elements, but they cannot claim ABI
layout, copyability, `Send`, `Sync`, ownership exemptions, compiler intrinsics,
or unrelated declarations.

```ts
function logged(
  method: () => void,
  context: ClassMethodDecoratorContext,
): () => void {
  return () => {
    console.log(`Calling ${context.name}`);
    method();
  };
}

class Worker {
  @logged
  public run(): void {}
}
```

## 5. Types, ownership, and lifetimes

The primitive types are `bool`, the fixed signed and unsigned integers,
`isize`, `usize`, `f32`, `f64`, `char`, `void`, `never`, `string`, `str`,
`unknown`, and `undefined`. `T | undefined` is the optional form. General
unions and intersections are excluded; use nominal enums for other sum types.

Every value is affine. An owned assignment, owned argument, return, aggregate
store, or `move` capture transfers ownership. `const` bindings are immutable;
`let` bindings may be reassigned. The compiler infers whether a method writes
to `this`; there is no receiver mutability modifier.

Copyability is inferred recursively from fields and representation. A value is
copyable only when every stored field is copyable and no external resource or
custom cleanup makes copying ambiguous. Clone is explicit: a type may provide a
normal handwritten `.clone()` method with its own error and ownership contract.
`Send` and `Sync` are inferred recursively from fields. There are no source
attributes that assert any of these properties.

`&T` is a shared borrow and `&mut T` is an exclusive borrow. Named lifetimes
are available only when an exported relationship cannot be inferred:

```ts
struct View<lifetime source> {
  bytes: &source [u8];
}

declare function find<lifetime values>(
  values: &values [User],
  name: &str,
): &values User | undefined;
```

The compiler infers local lifetimes, output relationships from function bodies,
and the lifetime carried by returned aggregates. Exported inferred contracts
are stored in module metadata. Bodyless declarations use conservative documented
elision rules. A named lifetime is required only when several inputs make the
relationship ambiguous. `static` remains available for process-lifetime data
but is uncommon.

Public source never uses an internal lifetime category. In particular, the
compiler may classify a non-escaping temporary borrow internally, but that
category is not a keyword, type argument, formatter output, diagnostic term, or
generated documentation item.

Ordinary elision makes common borrowed functions concise:

```ts
function first(values: &[User]): &User | undefined {
  return values.length === 0 ? undefined : &values[0];
}

function parseCommand(
  input: &BytesMut,
  start: usize,
): ParsedCommand | undefined throws RedisError {
  // ParsedCommand's inferred borrow is tied to input.
}
```

Borrows are non-lexical and cannot outlive, cross a move of, or escape their
referent. Mutation, reallocation, and buffer compaction are rejected while a
derived borrow remains live. Diagnostics describe the source value and the
operation that would invalidate it rather than exposing internal equations.

## 6. Cleanup and resource management

Memory destruction is automatic. The compiler recursively destroys owned values
and initialized fields exactly once on normal return, typed error, cancellation,
constructor failure, and partial initialization. Users do not define or call a
`drop()` operation.

External resources use the standard resource-management protocols:

```ts
class File implements Disposable {
  public [Symbol.dispose](): void {
    this.close();
  }
}

using file = try File.open(path);
```

```ts
class TcpStream implements AsyncDisposable {
  public async [Symbol.asyncDispose](): Promise<void, NetworkError> {
    return try await this.flushAndClose();
  }
}

await using stream = try await TcpStream.connect(endpoint);
```

`using` and `await using` create managed bindings whose disposal runs exactly
once. A manual call to a disposal symbol is safe and marks the resource closed;
scope cleanup becomes harmless. `.close()` remains available for deliberate
early closure and follows the same idempotent rule. Managed resources cannot be
silently rebound as ordinary unmanaged locals.

## 7. Strings and bytes

The canonical public representations are:

```ts
&[u8]      // borrowed bytes
&str       // borrowed valid UTF-8
Bytes      // owned immutable bytes
BytesMut   // owned mutable bytes
string     // owned UTF-8 text
```

`string` is the primitive owned UTF-8 type. `String(value)` converts to owned
text; `new String(...)`, `string.from(...)`, `OwnedString.from(...)`, and
equivalent competing constructors are rejected. `String.fromUtf8(bytes)` is
fallible, while `String.fromUtf8Lossy(bytes)` is explicit lossy conversion.

For a `string` value `text`, `text.length` is the Unicode scalar count and
`text.byteLength` is the UTF-8 byte count. `startsWith`, `includes`, `slice`,
and `toUpperCase` are ordinary methods. Byte positions and scalar positions
use distinct APIs and never silently share an index meaning.

`Bytes` and `BytesMut` expose `.length`, `.slice(start, end)`, and
`.trySlice(start, end)`. The checked operations return typed absence or a typed
error according to their declaration. There is no duplicate `view()` or
`subview()` public vocabulary and no handwritten maximum integer sentinel.
Private collection hash caches are representation details, not public string
wrapper types. `ByteView`, `Utf8View`, `HashedUtf8View`, and
`AsciiKeyUtf8View` are not public types.

## 8. Numeric and collection semantics

Expected types drive numeric literal inference:

```ts
let index: usize = 0;
const port: u16 = 6379;
return 0;

const mask = 255u8;
const maximum = usize.MAX;
index += 1;
```

Unconstrained integer literals use the smallest declared context or the
documented default `number`; explicit suffixes select a width. Overflow,
conversion, ABI width, generic inference, and diagnostics are identical in
debug and optimized profiles.

The canonical collection vocabulary is:

```ts
const values = new Array<User>({ capacity: 1_024 });
const jobs = new Queue<Job>({ capacity: 256 });
const cache = new Map<string, User>({ capacity: 1_024 });
const seen = new Set<string>();
```

The supported collection types are `Array<T>`, `[T; N]`, `&[T]`, `&mut [T]`,
`Queue<T>`, `Deque<T>`, `Map<K, V>`, `Set<T>`, `OrderedMap<K, V>`,
`OrderedSet<T>`, and `Heap<T>`. `Vector`, `FixedArray`, `ReadonlySlice`,
`MutableSlice`, and `StringMap` are not public types.

`values.at(index)` returns `&T | undefined`; assignment through an index uses
the borrow inferred by context; `values.removeAt(index)` returns `T | undefined`.
Iteration advances by index and borrow and does not consume the collection or
turn repeated front removal into an accidental O(n²) algorithm.

`Queue` uses `enqueue` and `dequeue`. `Deque` uses `pushFront`, `pushBack`,
`popFront`, and `popBack`. Maps use `set`, `get`, `has`, and `delete`; sets use
`add`, `has`, and `delete`. `Map.set` returns the map for chaining. Borrowed
lookup of `Map<string, V>` uses the ordinary `get` and `delete` operations;
hash caching and borrowed equality are implementation details.

## 9. Errors, async, threads, and tasks

Recoverable failures use typed effects:

```ts
function readConfig(path: &str): string throws LoadError;
async function fetchUser(id: u64): Promise<User, NetworkError>;

const user = try await fetchUser(id);
```

`Promise<T, E>` is the only asynchronous result shape and remains unchanged.
Synchronous functions may use `throws E`. Public APIs use nominal typed errors
with optional platform `rawCode` fields. `Checked` suffixes are not part of
the public API. Raw or unchecked variants are explicitly `unsafe`, end in
`Raw` where appropriate, and live in internal or FFI modules.

An OS thread and an async task are different abstractions:

```ts
const worker = Thread.spawn(move () => calculate());
const result = try worker.join();

using tasks = new TaskGroup();
const first = tasks.spawn(fetchUser());
const second = tasks.spawn(fetchPosts());
const user = try await first;
const posts = try await second;
```

`Thread.spawn` returns `JoinHandle<T>`. `join`, explicit `detach`,
`currentId`, `yield`, and `sleep` have typed errors and enforce `Send + static`
capture requirements. `TaskGroup.spawn` returns an awaitable `Task<T, E>`, not
a status flag. Group exit waits for or cooperatively cancels children. Detached
tasks require explicit ownership and process-lifetime captures. There is no
one-thread-per-task implementation and no public `enter`/`leave` accounting
protocol.

The executor schedules suspended task values. A reactor watches many
nonblocking descriptors using the host readiness mechanism, wakes ready tasks,
and shares one timeout/cancellation path with timers. `accept`, read, and write
operations never perform indefinite blocking inside an `async` function;
backpressure suspends the writer and later resumes it. Registration and
deregistration are tied to resource ownership.

## 10. Filesystem, networking, and foreign boundaries

Safe filesystem and networking APIs hide descriptors, pointers, output
parameters, integer event masks, and platform structures:

```ts
using file = try File.open(path, { read: true });
const text = try file.readText();
try File.writeText(outputPath, text);
const metadata = try File.metadata(outputPath);
const entries = try Directory.read(path);
```

```ts
await using listener = try await new TcpListener({
  host: "127.0.0.1",
  port: 8080,
});

using tasks = new TaskGroup();
while (true) {
  const stream = try await listener.accept();
  tasks.spawn(handle(move stream));
}
```

The only public foreign block is `declare extern "C"`. The only public C
function export is `export extern "C" function`. Raw pointers, OS handles,
manual allocation, and platform codes are confined to `unsafe` or approved FFI
modules. A safe wrapper translates platform failures into nominal TypeNative
errors.

## 11. Runtime layering and Redis

The runtime and foundational `std/bytes` layer provide generic byte operations:
`find`, `startsWith`, `equals`, `equalsIgnoreAsciiCase`, `hash`, `validateUtf8`,
and `parseUnsigned`. They do not know RESP markers, CRLF framing, incomplete
RESP lines, or Redis error states.

RESP framing and command parsing live in `validation/redis/resp.tn` and ordinary
Redis application modules. The compiler and runtime have no Redis-specific
branch, intrinsic, or parser helper. Redis validation covers fragmented and
coalesced frames, pipelining, malformed input, size limits, all required
commands, borrowed-data lifetime safety, one-million-PING allocation behavior,
existing-key GET allocation behavior, and stable memory use.

## 12. Compiler ownership, tooling, and conformance

All syntax changes are implemented through the lexer, parser/CST, AST wrappers,
formatter, HIR, type checker, ownership/lifetime checker, MIR, LLVM lowering,
linter, LSP, documentation generator, Node declaration generator, diagnostic
JSON, fuzz targets, and fixtures. The formatter emits only this grammar.

Compiler-owned intrinsics are identified by a private trusted manifest using
declaration identity and approved module location. The manifest is the only
place that may bind a declaration to representation, ABI, allocation, or
lowering primitives. Source code cannot forge an intrinsic with a decorator or
attribute.

Every removed spelling has:

1. a canonical positive fixture;
2. an obsolete negative fixture with a localized condition identifier;
3. recovery coverage proving following declarations still parse;
4. formatter coverage proving the spelling is never emitted; and
5. a machine-applicable replacement only when the transformation is unambiguous.

There is no compatibility mode or deprecated alias. The Rust compiler must
reject old syntax before any self-host migration is considered.

## 13. Rejected alternatives

The following decisions are closed and must not be revived:

| Rejected source or design | Canonical decision |
| --- | --- |
| public `scope` lifetime category | Internal lifetime bookkeeping only; use elision or named `lifetime` parameters |
| compiler-owned ownership/ABI decorators | Structural inference, ordinary methods, `implements`, `export extern "C"`, and the private intrinsic manifest |
| `macro` declarations and `@Expand` | Functions, generics, interfaces, and ordinary declarations |
| `sealed`, `final`, and derivation syntax | Remove all three; optimizer proofs handle closed dispatch |
| receiver `mut` | Infer receiver mutability from writes to `this` |
| public `ByteView`/`Utf8View` families | `&[u8]`, `&str`, `Bytes`, and `BytesMut` |
| boxed or competing string constructors | primitive `string`, `String(value)`, and explicit UTF-8 conversion methods |
| `StringMap` and hashed lookup methods | ordinary `Map<string, V>` APIs |
| one-thread-per-task scheduling | executor-backed tasks and reactor readiness |
| RESP parsing in runtime or foundational bytes | generic byte primitives plus Redis-owned protocol code |
| public pointers, descriptors, output parameters, and numeric OS codes | typed safe wrappers with narrow unsafe/FFI boundaries |
| self-hosting as a current gate | frozen `compiler-tn/**`; migrate only after active convergence completes |

## 14. Completion criteria

This specification is implemented when the Rust compiler accepts exactly this
surface, rejects obsolete syntax without aliases, preserves lifetime-safe
zero-allocation borrowed parsing, exposes one coherent standard-library API,
executes async I/O through the reactor and executor, keeps threads and tasks
distinct, passes tooling/ABI/sanitizer/fuzz/Redis checks, and supports the
reproducible cross-language performance suite. The frozen self-host tree and
the protected `benchmarks/json-parser/results.json` change remain untouched.
