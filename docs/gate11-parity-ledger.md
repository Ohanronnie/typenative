# Gate 11 parity ledger

Status: complete. Direct LLVM self-hosting is verified.

The previous completion claim was incorrect. Commit
`4b3137e19bd62b567e48f04ed84629681e5abe7a` is preserved as a deterministic
TypeNative-to-C bootstrap checkpoint, not as evidence of the required direct
LLVM pipeline.

## Delivered pipeline

- The useful TypeNative lexer, parser, formatter, HIR, semantic checks, MIR,
  ownership/drop work, tooling, runtime, and bootstrap harness are preserved.
- `compiler-tn/direct_ir.tn` constructs typed LLVM modules through the LLVM C
  API for the self-hosted compiler, runtime, standard library, product, async,
  error, aggregate, pointer, collection, debug-info, and foreign-call paths.
- `compiler-tn/direct_codegen.tn` verifies and optimizes those modules, emits
  LLVM target-machine objects, and links only already-emitted objects into
  native products. No generated C is a TypeNative compilation stage.
- `compiler-tn/generic_codegen.tn` and the whole-program C renderer are gone.

The normal pipeline is:

```text
TypeNative source
  -> lossless CST/AST
  -> typed HIR
  -> validated MIR
  -> direct LLVM C API module construction in TypeNative
  -> LLVM verification and optimization
  -> LLVM target-machine object emission
  -> system linker
  -> native product
```

The deterministic regression scan rejects generated C, C-source compiler
subprocesses, the removed renderer, and source-pattern backend remnants.

## Direct self-hosting evidence

The final guarded command was:

```sh
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
TYPENATIVE_BOOTSTRAP_DIR=/tmp/gate11-final \
sh scripts/bootstrap-self-host.sh
```

The manifest was
`/tmp/gate11-final/run-1787383007-17394/bootstrap-manifest.txt` and reports:

```text
compiler-b-sha256=ed69a2e3d52a2afae2b41d64be08eda1da17ecccce47dc3e864e2fa017d55b44
compiler-c-sha256=ed69a2e3d52a2afae2b41d64be08eda1da17ecccce47dc3e864e2fa017d55b44
compiler-d-sha256=ed69a2e3d52a2afae2b41d64be08eda1da17ecccce47dc3e864e2fa017d55b44
compiler-b-repeat-sha256=ed69a2e3d52a2afae2b41d64be08eda1da17ecccce47dc3e864e2fa017d55b44
fixed-point=compiler-b=compiler-c=compiler-d=compiler-b-repeat
```

Stage A is the Rust-seed host that produces the first direct-LLVM compiler.
Stages B, C, D, and the repeat stage compile the TypeNative compiler source
through self-hosted direct LLVM object emission and the platform linker. The
runtime objects for B, C, and D are independently rebuilt from the runtime
source and have identical digests. The command trace reports
`max_guarded_seconds=13` under the 175-second alarm guard.

## Historical C-bootstrap checkpoint

The previous bootstrap run at
`/Users/ronnie/Projects/typenative/build/bootstrap/run-1786741378-10144`
reported:

```text
fixed-point=compiler-b=compiler-c=compiler-d=compiler-b-repeat
source-fixed-point=3360a803b0a26bf4df57969ed34a60ab8451268b4c801dfb65351eb40057e1b0
compiler-b-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-c-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-d-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
compiler-b-repeat-sha256=1761f57cbe5edb7d89f7914ae1ca5ea6e4649d284e705064d6a7f944afd49d29
```

These hashes remain useful reproducibility evidence for the old backend and are
not used as evidence for the direct pipeline.

## Performance policy

The compiler performance guard remains part of this gate: every individual
TypeNative compiler check or build is alarm-guarded below 175 seconds. If one
approaches that limit, feature work stops, the stage is profiled and optimized,
and the command repeats below the limit before continuing.

## Completion evidence

- [x] Whole-program C renderer and all `.tn.c`/C-source subprocess paths are removed.
- [x] TypeNative constructs general typed LLVM modules through the LLVM C API.
- [x] LLVM verification, optimization, target-machine object emission, and linking are direct.
- [x] No normal TypeNative build creates a `.tn.c` file or invokes Clang on generated C.
- [x] A deterministic regression scan rejects forbidden C-backend paths.
- [x] B builds C through direct LLVM object emission without Rust codegen delegation.
- [x] C builds D through direct LLVM object emission without Rust codegen delegation.
- [x] D and a repeat build reach a direct-LLVM fixed point.
- [x] Product behavior and prior acceptance suites pass under the direct pipeline.
- [x] The protected benchmark remains unchanged by this work and unstaged.

The preserved worktree hash of `benchmarks/json-parser/results.json` is
`634c57f2f3b53be3bd51912b3321026a80f0099043b50f6dc0b53587d485634d`.
