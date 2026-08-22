# Gate 11 Preparation Record (Historical)

This document records the evidence boundary that was defined before independent
full self-hosting was completed. It is retained as historical preparation
material, not as the current Gate 11 status. The completion evidence is recorded
in [`gate11-parity-ledger.md`](gate11-parity-ledger.md).

## Entry contract

An eventual independent run must start from a clean checkout with:

- the exact committed TypeNative source, standard library, runtime ABI
  declarations, fixtures, and documentation;
- one of the supported hosted targets (`aarch64-apple-darwin` or
  `x86_64-unknown-linux-gnu`) and the pinned LLVM toolchain;
- no generated source, cached compiler, temporary probe, absolute workspace
  path, timestamp, network response, or host-randomized value included as an
  undeclared input;
- a recorded environment manifest containing compiler, linker, SDK, and
  sanitizer versions; and
- deterministic file discovery, declaration ordering, symbol ordering, and
  diagnostic serialization.

The clean-checkout procedure must verify the source tree before building and
must fail if an ignored `build/` or `target/` artifact is silently consumed as
compiler input. Native ABI probes, when needed, are generated in a temporary
directory and their inputs and outputs are recorded without becoming repository
sources.

## Evidence lanes

| Lane | Preparation evidence now available | Gate 11 evidence still required |
| --- | --- | --- |
| Canonical language | [`language-spec.md`](language-spec.md), [`canonical-migration-manifest.md`](canonical-migration-manifest.md), and canonical parser fixtures are synchronized; the current documentation corpus contains eight `.tn` examples and they parse | Independent compiler B and C must consume the same canonical grammar and reject every obsolete spelling with equivalent structured diagnostics |
| Rust bootstrap | `cargo test --workspace --all-targets` and focused syntax, HIR, typecheck, ownership, CLI, and MIR suites are the ordinary compiler baseline | The Rust bootstrap must build compiler A from the clean checkout, then the independent TypeNative compilers must reproduce the required artifacts and suites |
| Redis source | The four canonical Redis sources pass ordinary compiler checks; the acceptance contract is in [`redis-acceptance.md`](redis-acceptance.md) | Protected self-hosted compilers must independently reproduce the same native protocol/concurrency/sanitizer result |
| Determinism | Existing semantic tests cover randomized discovery and hash-seed independence; canonical source scans and diagnostics are recorded | Repeat the complete discovery-order and hash-seed matrix for B and C, compare normalized diagnostics, and compare declared artifact digests |
| Native safety | The runtime ABI boundary is explicitly documented in [`gate10-native-inventory.md`](gate10-native-inventory.md) | Run native products under AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer on macOS ARM64, including async cleanup, mutexes, sockets, Redis, and FFI boundaries |
| Protected self-hosting boundary | Protected paths are named and excluded from this pass | Complete the independent A-to-B-to-C sequence only in the dedicated self-hosting work, then attach its clean-checkout logs and artifact/source comparisons |

## Diagnostic equivalence

The comparison unit is a normalized diagnostic record, not terminal prose. Each
record must retain:

- condition identifier and severity;
- source path relative to the clean checkout;
- byte range, line, and Unicode scalar column;
- primary and secondary labels;
- notes and machine-applicable edits; and
- stable ordering by source span, condition, and message key.

The ordinary bootstrap checks already exercise JSON diagnostic output. The
independent compiler comparison must additionally prove that formatting,
recovery, invalid syntax, type errors, ownership errors, effect errors, and
unsafe-boundary errors preserve these fields under randomized discovery order.

## Randomized discovery protocol

The deterministic test protocol should run each corpus with at least these
declared variations:

```text
file-discovery-seed=0
file-discovery-seed=1
file-discovery-seed=0x5eed
hash-seed=0
hash-seed=1
hash-seed=0x5eed
```

The seed is an input to discovery and serialization only; it must not affect
semantic identities, monomorphization order, diagnostics, generated symbols,
or native behavior. A mismatch is a failure even when the emitted executable
happens to run correctly.

## Native acceptance prerequisites

Before independent compiler runs are meaningful, Gate 10 must attach:

1. the canonical Redis native executable exchange matrix;
2. fragmented-frame, pipelining, malformed-frame, invalid-UTF-8,
   oversized-frame, truncated-input, disconnect, and concurrent-client tests;
3. cleanup evidence for `using`, `await using`, mutex guards, task groups,
   detached promises, and temporary borrows before suspension;
4. strict C compilation and external ABI layout/symbol probes; and
5. sanitizer logs with toolchain and target metadata.

These checks are separate from compiler self-hosting. A self-hosted compiler
must not be used to waive a missing native acceptance result, and a passing
fixed-point digest must not be used to waive semantic, runtime, or sanitizer
coverage.

## Boundary at preparation time

At the time this record was written, the goal had completed
compiler-independent preparation only:

- no self-hosting script or self-hosting check was run;
- no final Gate 11 result is claimed;
- the protected `compiler-tn/**`, `scripts/bootstrap-self-host.sh`, and old
  A/B/C/fixed-point artifacts remain outside the edit scope; and
- native Redis execution, sanitizer evidence, and independent compiler
  artifacts remain explicit follow-up requirements.
