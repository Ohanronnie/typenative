# Gate 11 parity ledger

Status: complete.

This ledger records the compiler/tooling implementation and the evidence for the
independent self-hosting gate. The canonical starting point was
`8b93db96d8fb3861e2edc3da2636f99c66b6e7fc` on `main`.

## Scope delivered

- `compiler-tn/` contains the self-hosted lexer, parser, formatter, HIR, type
  checking, ownership checks, semantic analysis, MIR, drop lowering, LLVM
  lowering, diagnostics, tooling, driver, collections, and generic codegen.
- The self-hosted compiler handles executable, library, Node addon, directory
  project, async, error propagation, filesystem, class/method, generic,
  collection, foreign-function, C ABI, and debug-information paths exercised by
  the product matrix.
- The runtime and standard library contain the platform declarations,
  byte-addressing intrinsics, typed collection allocation, map/string helpers,
  and the supporting async, filesystem, and error paths.
- The host LLVM backend recognizes the runtime byte-addressing intrinsics needed
  to compile the committed runtime source. This is backend plumbing, not a
  semantic fallback for the self-hosted compiler.
- `scripts/bootstrap-self-host.sh` records source discovery, compiler hashes,
  runtime-object hashing, every guarded command, and fixed-point artifact
  evidence. `scripts/measure-compiler-check.sh` is the focused performance
  regression check.

## Independence evidence

The guarded bootstrap command was:

```sh
TYPENATIVE_RUNTIME_ROOT=/Users/ronnie/Projects/typenative \
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
scripts/bootstrap-self-host.sh
```

The latest successful run is recorded at:

`/Users/ronnie/Projects/typenative/build/bootstrap/run-1786741378-10144`

Its evidence manifest reports:

```text
fixed-point=compiler-b=compiler-c=compiler-d=compiler-b-repeat
source-fixed-point=3360a803b0a26bf4df57969ed34a60ab8451268b4c801dfb65351eb40057e1b0
compiler-b-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-c-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-d-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-b-repeat-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
```

Stage A is the committed host driver that produces the first self-hosted
compiler. Stages B, C, D, and the repeat stage compile the committed compiler
source, runtime object, and product inputs through the self-hosted driver and
the platform linker. No stage delegates semantic analysis or lowering to the
Rust implementation.

The source manifest is discovered in sorted order, and the artifact digest
uses allocated executable sections so linker metadata does not make identical
machine code appear different. The full command record is in
`command-trace.tsv` beside the manifest.

## Performance evidence

An early self-hosted collection build exposed an unbounded backward source scan;
it was stopped before the three-minute limit. The scan was replaced with a
bounded forward pass, and the regression probe now reports:

```text
tn-timing phase=module-check micros=98315
tn-timing phase=ownership micros=1397359
tn-timing phase=mir-drop micros=823348
compiler-check-regression: seconds=2 timeout=175 status=0 driver=/Users/ronnie/.cargo.target/release/tn
```

The bootstrap command trace has `max-guarded-seconds=95`. The two slowest
stages were the self-hosted compiler builds from B to C and C to D, each at 95
seconds, below the 180-second limit. Any future guarded TypeNative compiler
check or build exceeding that limit must be profiled and corrected before
feature work continues.

## Product and repository evidence

The final `scripts/verify-all.sh` matrix completed with:

```text
verification-matrix=pass
```

It covers native source and formatting checks, all standard-library checks,
Rust tests, lint and documentation checks, CLI behavior, hosted standard
library and async execution, runtime/time/filesystem behavior, debug info, C
ABI layout and calls, Node behavior, Redis benchmarks, sanitizers, and lexer/
parser fuzz checks. Every TypeNative compiler invocation is alarm-guarded.

The protected benchmark file remains byte-identical to the starting state:

```text
634c57f2f3b53be3bd51912b3321026a80f0099043b50f6dc0b53587d485634d  benchmarks/json-parser/results.json
```

Generated fuzz corpus files are verification artifacts and are kept out of the
commit. The benchmark result is also kept out of the commit.

## Completion checklist

- [x] Self-hosted compiler frontend and semantic pipeline are implemented.
- [x] MIR, drop lowering, LLVM lowering, runtime, and tooling paths are covered.
- [x] Product behavior is checked beyond compiler-only fixtures.
- [x] No bounded integer-program evaluator or source-pattern evaluator remains.
- [x] A→B→C→D and repeat-stage artifact hashes reach a fixed point.
- [x] Every guarded compiler command stays below 180 seconds.
- [x] The performance regression probe is committed and passes below the limit.
- [x] The full verification matrix passes.
- [x] The protected benchmark remains unchanged and unstaged.
- [x] The evidence manifest and command trace are reproducible from the script.
