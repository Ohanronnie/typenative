#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _ = tn_syntax::lex("fuzz.tn", bytes);
});

