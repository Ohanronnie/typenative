# Gate 10 Native Source Inventory

This inventory records every project-owned native source found during the
compiler-independent preparation pass on 2026-08-12. It separates native
runtime boundaries from application implementation and records what must be
proven before a source can be retired. The inventory does not close Gate 10.

## Scope and scan result

The source scan covered C, C++, Objective-C, headers, and handwritten assembly
under the repository root, while treating `build/` and `target/` as generated
output. The project-owned native implementation files currently present are:

| Path | Role | Current disposition | Evidence still required |
| --- | --- | --- | --- |
| `runtime/runtime.c` | Native ABI for allocation, strings, sockets, synchronization, async promises, task groups, process support, and compiler products | Retained as the current reviewed native boundary | TypeNative wrappers must exercise the reviewed ABI in native builds and sanitizers before the implementation is externalized or retired |
| `runtime/redis.c` | Legacy C Redis protocol/server implementation | Retained only as a legacy validation baseline; not the canonical Redis source | Canonical native protocol, concurrency, and sanitizer evidence is recorded; source retirement remains |
| `runtime/startup.c` | Native executable startup and entry-status adapter | Retained for the current generated executable product | A generated startup contract and clean-checkout product build that no longer requires a project-owned startup implementation |
| `runtime/selfhost_module.c` | Self-hosted LLVM/backend support included by the runtime | Protected follow-up boundary; unchanged in this goal | Independent self-hosting work; it is outside the current preparation scope |
| `validation/c/extern.c` | C-side provider for the C ABI fixture | Retained validation source pending external generated-probe migration | Generate the provider outside the repository and compare layout, calling convention, and symbol results |
| `validation/c/caller.c` | C-side caller for the C ABI fixture | Retained validation source pending external generated-probe migration | Generate the caller outside the repository and run the same ABI assertions |
| `validation/redis/lifecycle.c` | C lifecycle/concurrency harness for the legacy Redis path | Retained legacy harness pending a TypeNative or external generated harness | TypeNative lifecycle/drop fixture, sanitizer coverage, and retirement evidence |
| `validation/runtime/main.c` | C runtime ownership, allocation, socket, and threading harness | Retained validation source pending TypeNative or external generated migration | Native TypeNative runtime fixture plus sanitizer and leak evidence |

No checked-in C++ source, header, Objective-C source, or handwritten assembly
source was found outside generated output. Generated files under
`build/bootstrap/` include assembly and diagnostic JSON from prior ordinary
compiler products; they are artifacts and must not be treated as source
implementation or hidden compiler inputs.

## Canonical TypeNative replacements

The Redis application implementation now lives in:

- [`validation/redis/resp.tn`](../validation/redis/resp.tn)
- [`validation/redis/redis-server.tn`](../validation/redis/redis-server.tn)
- [`validation/redis/main.tn`](../validation/redis/main.tn)
- [`validation/redis/main-alt.tn`](../validation/redis/main-alt.tn)

All four files pass the ordinary bootstrap compiler with JSON diagnostics
enabled and no emitted diagnostics. The target uses explicit standard-library
ABI declarations for allocation, strings, sockets, mutexes, promises, and task
groups. The C Redis implementation remains in the inventory because source
presence and runtime retirement are separate acceptance conditions.

## Required Gate 10 evidence

Gate 10 can advance only when the following records are attached to this
inventory:

1. A clean-checkout source scan showing no project-owned implementation source
   is required by ordinary `tn check`, `tn build`, `tn test`, standard-library,
   Redis, C ABI, or Node-API validation.
2. A native Redis executable run covering the exchanges in
   [`redis-acceptance.md`](redis-acceptance.md), fragmented and pipelined
   frames, malformed input, disconnects, concurrent clients, and capacity
   growth.
3. AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer results
   for the native Redis and runtime fixtures on the supported macOS ARM64
   toolchain. The canonical Redis results are attached below; the remaining
   runtime/native-source retirement records are separate.
4. External generated C ABI probes in a temporary directory, with no generated
   probe source copied into the repository.
5. A dependency/provenance record showing that libc, OS, LLVM, Node-API, and
   other system implementations remain external dependencies.

The canonical native execution record and canonical Redis sanitizer records are
attached below. External generated probes and source-retirement evidence remain
open. No self-hosting script, bootstrap check, or fixed-point check is an
acceptable substitute for them.

## Attached canonical execution record

Recorded on 2026-08-12:

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
redis-canonical-address-undefined-sanitizers=pass

REDIS_SANITIZER=thread ./scripts/verify-redis.sh
redis-canonical-thread-sanitizers=pass
```

The harness builds `validation/redis/main-alt.tn`, verifies that each listener
belongs to the process it launched, and does not execute the legacy
`runtime/redis.c` application. This closes the native protocol, concurrency,
and canonical Redis sanitizer records, but does not close the native-source
retirement requirements above.

## Protected boundaries

This inventory does not authorize edits to `compiler-tn/**`,
`scripts/bootstrap-self-host.sh`, or historical A/B/C/fixed-point artifacts.
Those paths remain follow-up work for the independent self-hosting gate.
