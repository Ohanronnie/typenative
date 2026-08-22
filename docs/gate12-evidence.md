# Gate 12 evidence

Recorded on 2026-08-22 for the macOS ARM64 host. This is the reproducibility
manifest for the direct-LLVM compiler, Forge, the Redis comparison, and the
final verification commands. Historical preparation records remain linked from
the architecture and implementation ledgers; they are not current status.

## Guard and exact entrypoints

Every TypeNative invocation in this record uses the 175-second alarm wrapper:

```sh
scripts/tn-guarded.sh /Users/ronnie/.cargo.target/release/tn <arguments>
```

The full product runner is:

```sh
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
CARGO_TARGET_DIR=/Users/ronnie/.cargo.target \
scripts/verify-all.sh
```

The Forge product runner is:

```sh
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
FORGE_BUILD_DIR=/tmp/typenative-forge-host-final2.UXBOCt \
validation/forge/run.sh
```

The self-hosted Forge run used compiler D from the final bootstrap directory:

```sh
TN_BIN=/tmp/typenative-bootstrap-gate12-final3.JoOWEa/run-1787395777-15374/compiler-d \
FORGE_BUILD_DIR=/tmp/typenative-forge-selfhost-final6.bcUQvS \
validation/forge/run.sh
```

The Redis comparison uses five fresh samples and writes
`benchmarks/redis-comparison/results.json`:

```sh
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
benchmarks/redis-comparison/run.sh
```

## Host and toolchain

```text
platform=macOS 27.0 (BuildVersion 26A5416b)
kernel=Darwin 27.0.0 arm64
rustc=1.96.0 (ac68faa20 2026-05-25)
cargo=1.96.0 (30a34c682 2026-05-25)
clang=Apple clang version 21.0.0 (clang-2100.1.1.101)
llvm-config=22.1.8
node=v24.14.0
compiler-driver-sha256=0438852fab9f00886976c6c61510cfa5a7bce0d505dbf7016bfeee6507e144e4
```

The supported target-dependent exclusions are macOS ARM64 and Linux AMD64
runtime modules, plus the host-specific dynamic-library path used by Forge
(`/usr/lib/libSystem.B.dylib` on macOS and `/lib/x86_64-linux-gnu/libc.so.6`
on Linux). The coverage manifest classifies compiler contracts and validation
owned test APIs as justified rather than pretending they are application
behavior.

## Bootstrap fixed point

The final guarded command was:

```sh
TYPENATIVE_BOOTSTRAP_DIR=/tmp/typenative-bootstrap-gate12-final3.JoOWEa \
CARGO_TARGET_DIR=/Users/ronnie/.cargo.target \
TN_BIN=/Users/ronnie/.cargo.target/release/tn \
sh scripts/bootstrap-self-host.sh
```

Manifest:
`/tmp/typenative-bootstrap-gate12-final3.JoOWEa/run-1787395777-15374/bootstrap-manifest.txt`.

```text
source-fixed-point=27893b67564b83a71fcac29bfe3e87ca9fa7f7428a0faef76b1dca6022678b33
compiler-a-sha256=988eb9b647922d0eb33385675a64e9d8a38fc2cc8c6d26717161d44473a093b2
compiler-b-sha256=d8f8b500f9e437a38b1bd67152581d597e6b87ddfb49c1115858b429e235b0c9
compiler-c-sha256=d8f8b500f9e437a38b1bd67152581d597e6b87ddfb49c1115858b429e235b0c9
compiler-d-sha256=d8f8b500f9e437a38b1bd67152581d597e6b87ddfb49c1115858b429e235b0c9
compiler-b-repeat-sha256=d8f8b500f9e437a38b1bd67152581d597e6b87ddfb49c1115858b429e235b0c9
fixed-point=compiler-b=compiler-c=compiler-d=compiler-b-repeat
runtime-source-sha256=d58da12b295d84b0335d53db6e6b654ebe498c80f4f7d1ee0e92cdb6136e2e55
runtime-object-a-sha256=0ec7bf5156fb770821d89bc74e584a4ca97534bc4f21d9c8827664a6184ccd07
runtime-object-b-sha256=3d1cd35202e92a879340101be2961f111d77a2ee61b1c6830dd5b74cf06047ac
runtime-object-c-sha256=3d1cd35202e92a879340101be2961f111d77a2ee61b1c6830dd5b74cf06047ac
runtime-object-d-sha256=3d1cd35202e92a879340101be2961f111d77a2ee61b1c6830dd5b74cf06047ac
```

The command trace contained 389 guarded invocations. Expected negative
diagnostic probes account for their nonzero statuses; the maximum elapsed
invocation was 12 seconds and the aggregate trace time was 82 seconds. The
source discovery input is `LC_ALL=C` sorted paths, and source/artifact hashing
is deterministic.

## Forge coverage and artifacts

`node validation/forge/coverage.mjs --check` reports `exports=277` across 19
standard-library modules: 225 behavioral entries and 52 justified contract or
validation entries. The coverage manifest SHA-256 is
`9bf8f0c7c5b85b92883b83b1f0e6d9052593b4a41d0dacaa9384f10a7e8774c4`.

Both Rust A and self-hosted D report `forge-conformance=pass`. The product set
contains debug and optimized executables, object, LLVM IR, bitcode, assembly,
shared-library, and Node-API outputs; the runner also executes FFI, Node, and
TCP behavior. Selected final artifact hashes are:

| product | Rust A | self-hosted D |
| --- | --- | --- |
| executable (debug) | `65a2640b1288192239fd1d2dcc0da2030e5f9412cf8a431826d340487abda8a0` | `b7931163c9f6e79fe80a140cb7e27d87cb9c7630bfd53e0450c390266763b304` |
| executable (optimized) | `d44a18ba2f559d1f8f94ff415f52641e3f90bf7b1687709d6e1dab8a9cdf5d4e` | `ddc36fc6b231fc1eec8ccb017968b5fb287562a8129a81bdff1edabdf5475e93` |
| object | `e565730431faa5aa371f2b58e232d61b97e89aebe1932235ca26d3b4c27a7f6a` | `2a09550598c9c8214135c39eedb065594ed44dcdaded74cc3d79978c9a75d6a2` |
| LLVM IR | `cdf424b04194a466e4c3d5886af227bcbce750908fdf54dacf6121dc1f07e961` | `e4577a013578c93e0a28b55cfb1f8943851372dd70bbcc7c0d351133fd9d2234` |
| bitcode | `69514e3c11c66e9dbe216527e2c5db52a2a273cf31edeb5ebb7879688ddcdd62` | `dd2ef2d4b0b2766988b64c47e57ca4e22a57a439a3731208e4e37402674a54c6` |
| assembly | `23a3f0a17e4631124d45e6429a75bd8a7e33628f21c67c4b2b4bdb9a2761f0a5` | `cf1164fb3dd5ce76d448daf65e685134bd6e640f752d54f62a583b15a80c3749` |
| shared library | `32a5be084e0983b1cd54a23489d1094ef9b5f0d276541ccbbf6c45b66231697c` | `41b644e7bdbb709e4c5cfbf25217b7677d46610c9d58d61cfbf8043dceb2a7f6` |
| Node addon | `0c1ad9036b54cf7353f82e12689de0521f5d6df690ef7c5bfbe85c8a908c978f` | `748ffaa3f183c5caf55a2897a4f1147cfb984b7176de70cabd58bc5520255ddc` |

## Redis comparison

The committed benchmark result uses five samples, seed `305419896`
(`0x12345678`), 100,000 pipelined PINGs, 10,000 non-pipelined PINGs, 10,000
randomized SETs, 10,000 randomized GETs, eight concurrent clients, and 12,000
byte large values. The result file records fresh ports, compilation time,
artifact size, startup latency, RSS, RSS growth, and all per-sample values.

| implementation | startup median (ms) | pipelined PING median (ops/s) | non-pipelined PING median (ops/s) | SET median (ops/s) | GET median (ops/s) | RSS growth median (KiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| TypeNative Node addon | 82.017 | 638,607 | 29,767 | 28,566 | 28,445 | 368 |
| TypeNative native executable | 27.497 | 579,508 | 29,915 | 28,622 | 28,442 | 368 |
| Handwritten Node.js | 83.407 | 684,326 | 24,958 | 22,968 | 22,958 | 7,216 |

Compilation medians are recorded as 1.68 seconds for the addon and 1.66
seconds for the native executable. Artifact sizes are 138,584 bytes and
116,224 bytes respectively. The result hash at this evidence revision is
`2272b63f865e5d7e6146cd1d79909653f0c27551f34fd6cc398ea1c57c3589ed`.

## Determinism, diagnostics, and hierarchy semantics

The deterministic semantic test was invoked for all nine discovery/hash seed
labels from `{0, 1, 0x5eed}` × `{0, 1, 0x5eed}`. Each invocation exercises both
source orders repeatedly; the compiler intentionally makes discovery and
artifact hashing independent of those host seed labels. All nine invocations
passed with equivalent valid output. The bootstrap trace independently
verifies sorted source discovery and fixed-point artifact hashing.

Forge’s sealed fixtures cover same-module permission, cross-module class
subclass rejection, cross-module interface-conformance rejection, and final
class rejection. Rust A and self-hosted D emit the corresponding structured
conditions; the focused HIR, signature, ownership, and LLVM regression tests
cover the root fixes.

## Verification record

The final `scripts/verify-all.sh` run passed in 392.45 seconds with these
categories green: design/link checks, direct LLVM scan, toolchain and native
source scans, Rust and TypeNative formatting, foreign syntax, all standard
library checks, workspace tests, Clippy with `-D warnings`, rustdoc with
`-Dwarnings`, runtime object generation, CLI, runtime, time, filesystem,
debug-info, C ABI, Node-API, Redis protocol/memory checks, and the sanitizer
suite. The sanitizer suite passed ordinary TypeNative and Redis Address/Undefined
checks plus ordinary TypeNative and Redis Thread checks. The lexer and parser
fuzz targets each passed 10,000 runs with `-max_len=4096 -timeout=5`.

The clean local clone at `/tmp/typenative-clean-gate12.XHwsjC` checked out
`84d459e30fc351f3fa9a42a43aa5199d3b866460`, built the release CLI in 28.87
seconds, and ran the full Forge runner to `forge-conformance=pass`. The
benchmark refresh is based on that pushed source commit. The final delivery
SHA is the last commit reported with the remote equality proof. The protected
`benchmarks/json-parser/results.json` is excluded from every staging command
and remains the only unstaged worktree modification.
