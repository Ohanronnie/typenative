# Time and filesystem runtime contract

TypeNative keeps calendar time separate from elapsed time. The public
standard-library surface is defined in `std/time.tn`:

| Type | Meaning | Construction and observation |
| --- | --- | --- |
| `Date` | Unix wall-clock time | `new Date()`, `Date.now()`, `Date.fromEpochMilliseconds(i64)`, `getTime()`, UTC accessors, and deterministic `toISOString()` |
| `Instant` | Monotonic elapsed-time point | `Instant.now()`, `elapsed()`, and `durationSince(&Instant)` |
| `Duration` | Elapsed nanoseconds | `fromNanoseconds`, `fromMicroseconds`, `fromMilliseconds`, `fromSeconds`, matching accessors, and saturating arithmetic |

An `Instant` is not a calendar timestamp and has no serialization or
conversion operation to `Date`. A `Duration` is not a Unix timestamp. Scaled
duration constructors and `saturatingAdd` clamp at the maximum `u64` value;
`saturatingSub` clamps at zero.

`std/fs.tn` exposes structured `FileMetadata` with `size`, `kind`, and
`modifiedNanoseconds`. The raw operating-system `stat` record is private to
the selected runtime platform module, and metadata failures return a
`FileError` containing the negative native error code.

The runtime selects one target module from the project target configuration:

- `runtime/platform/darwin-arm64.tn` uses Darwin ARM64 `CLOCK_REALTIME = 0`,
  `CLOCK_MONOTONIC = 6`, the Darwin `timespec` layout, and the Darwin ARM64
  `struct stat` layout.
- `runtime/platform/linux-x86_64.tn` uses Linux AMD64 `CLOCK_REALTIME = 0`,
  `CLOCK_MONOTONIC = 1`, the Linux `timespec` layout, and the Linux AMD64
  `struct stat` layout.

Clock and metadata foreign calls are infallible runtime boundaries: an OS
failure aborts rather than becoming a plausible zero timestamp or size.
`stat` failures are reported by `std/fs.metadata` as structured errors before
any platform record is read.
