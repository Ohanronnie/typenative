#![no_main]

#[path = "../support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    support::validate_hir_and_mir(bytes);
});
