# Self-host freeze

The Rust compiler under `crates/tn-*` is the sole active compiler. The
self-hosted sources under `compiler-tn/**` and `scripts/bootstrap-self-host.sh`
are frozen at checkpoint `20d75142562839528b021ff9e7c6784dbc20483a`.

The exact file set and SHA-256 content manifest are in
[`selfhost-freeze.json`](selfhost-freeze.json). Run
`scripts/verify-selfhost-freeze.sh` to verify it. Active verification invokes
that read-only check and does not invoke bootstrap, self-hosted builds, fixed
point comparisons, or self-host differential tests.

Historical self-host documents remain historical evidence. They do not provide
current active-language acceptance evidence and are not edited as part of
active remediation.
