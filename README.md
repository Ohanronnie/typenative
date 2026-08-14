# TypeNative

TypeNative is a statically compiled systems language with a TypeScript-inspired
surface and native semantics. It targets hosted command-line tools, servers,
databases, and native libraries without a garbage collector or JavaScript
runtime.

The hosted runtime and executable startup are ordinary TypeNative. Platform
services remain explicit `declare extern "C"` boundaries to external libc, pthread,
operating-system, LLVM, and Node-API implementations.

The language uses explicit native layouts, affine ownership, checked borrowing,
deterministic destruction, typed recoverable errors, and LLVM code generation.
Familiar syntax is a usability goal; TypeScript source compatibility is not.

## Design principles

- Safe code cannot create dangling references, use-after-free, mutable aliasing,
  or data races.
- Optimized and debug builds have the same observable language semantics.
- Allocation, dynamic dispatch, reference counting, copying, and unsafe
  operations are visible in source.
- The language specification is normative. An implementation that disagrees
  with it is incorrect until an explicit design change updates the specification.
- The grammar, formatter, compiler, documentation generator, and language server
  share one lossless syntax model.
- Platform and foreign interfaces are narrow, explicit, and testable.

## Scope

TypeNative currently supports hosted macOS ARM64 and Linux AMD64 targets. The
project includes the compiler, standard library, formatter, test runner,
documentation generator, language server, C interoperability, and generated
Node-API addons.

The following are outside the project:

- JavaScript, browser, or Node.js runtime compatibility
- garbage collection and implicit shared ownership
- bare-metal and kernel targets
- Windows targets
- dependency resolution, publishing, and a hosted package registry
- JSX, prototype mutation, dynamic properties, `eval`, and runtime code generation

## Example

```tn
import { run } from "std/async";
import { IOError } from "std/io";
import { TcpStream } from "std/net";

struct Endpoint {
  host: string;
  port: u16;
}

async function readGreeting(endpoint: &Endpoint): Promise<string, IOError> {
  const address: string = `${endpoint.host}:${endpoint.port}`;
  let stream = try await TcpStream.connect(address.asStr());
  return try await stream.readToString();
}

function main(): void throws IOError {
  const endpoint: Endpoint = { host: "127.0.0.1", port: 6379 };
  const greeting = try run(readGreeting(&endpoint));
  console.log(`server replied: ${greeting}`);
}
```

The example is intentionally not valid TypeScript. `struct`, borrowing,
fixed-width integers, typed errors, and deterministic ownership are TypeNative
semantics.

The `console` helpers are synchronous, best-effort output and do not add an
error effect. Programs that require confirmed delivery use the fallible
standard-stream writers in `std/io`.

## Canonical documents

| Document                                               | Authority                                                                                            |
| ------------------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| [Language specification](docs/language-spec.md)        | Grammar, types, ownership, execution semantics, modules, standard library surface, and CLI contract  |
| [Compiler architecture](docs/compiler-architecture.md) | Syntax infrastructure, HIR/MIR, safety analysis, LLVM lowering, runtime boundaries, and self-hosting |
| [Time and filesystem contract](docs/time-and-filesystem.md) | Calendar time, monotonic time, durations, metadata, and target-specific runtime boundaries |
| [Implementation plan](docs/implementation-plan.md)     | Ordered engineering gates, test matrices, and acceptance criteria                                    |
| [Redis acceptance](docs/redis-acceptance.md)           | Canonical end-to-end syntax, ownership, async, protocol, and native systems acceptance program       |
| [Design audit](docs/design-audit.md)                   | Problems found in the source plans and the canonical resolution for each                             |

When documents conflict, the language specification controls language behavior,
the compiler architecture controls implementation boundaries, and the
implementation plan controls sequencing. The design audit is explanatory rather
than normative.

## Tool contract

TypeNative source files use the `.tn` suffix. Projects use `typenative.json`.
The public command is `tn`:

```text
tn build
tn run
tn check
tn lint
tn test
tn fmt
tn doc
tn lsp
```

The compiler can emit an executable, object file, LLVM IR, LLVM bitcode,
assembly, shared library, or Node-API addon. A Node-API build produces a `.node`
binary and matching `.d.ts` declarations; it is distinct from a normal shared
library build.

## Repository state

The repository contains the Rust bootstrap compiler, LLVM backend, hosted
standard library, runtime, CLI tooling, C and Node-API generators, validation
fixtures, and the TypeNative compiler sources used by the bootstrap checks.
Implementation evidence is tracked in
[docs/implementation-status.md](docs/implementation-status.md); a gate remains
open there until its documented acceptance checks have run successfully.
