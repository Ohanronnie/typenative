# Forge conformance application

Forge is a multi-module TypeNative build-service exercise. `main.tn` is the executable and Node-API entrypoint; `native.tn` is the C-ABI-safe shared-library entrypoint. Both link the same local service modules through `probe.tn`. The modules cover:

- protocol framing with malformed-input handling and byte cursors;
- model types, interfaces, generic functions, inheritance, final classes, decorators, macros, generators, enums, and pattern selection;
- ownership decorators, raw-pointer safety, C ABI calls, optional narrowing, typed async errors, cancellation, and synchronous/asynchronous iterator consumption;
- an in-memory artifact index plus a filesystem-backed marker store;
- atomic counters, mutex guards, channels, task groups, and awaited work;
- UDP metrics emission and IPv4 resolution;
- dynamic FFI symbol discovery through `FORGE_PLUGIN_PATH`;
- process and thread identity/yield APIs;
- an async TCP endpoint accepting RESP commands `PING`, `BUILD`, and `STATUS`.
- Node-API functions for strings, bytes, arrays, fixed arrays, optionals, promises, rejected errors, and an exported class.

Run the complete linked matrix with:

```sh
TN_BIN=/Users/ronnie/.cargo.target/release/tn validation/forge/run.sh
```

The script checks every Forge source and sealed-hierarchy fixture, expands the standard-library coverage manifest, emits debug and optimized executables plus object/LLVM/bitcode/assembly/shared-library/Node artifacts, runs both native products, calls the addon exports, and drives the TCP endpoint. Build outputs stay under the ignored `validation/forge/build/` directory.

`FORGE_PLUGIN_PATH` is selected by the runner for the host platform. Set `FORGE_BUILD_DIR` to keep artifacts elsewhere. Every compiler invocation in the runner goes through `scripts/tn-guarded.sh`; a guarded invocation is terminated before the three-minute ceiling and is expected to remain below the 175-second compiler budget.
