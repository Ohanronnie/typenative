#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let parsed = tn_syntax::parse("fuzz.tnx", bytes);
    if let Ok(source) = std::str::from_utf8(bytes) {
        assert_eq!(parsed.syntax().to_string(), source);
    }
});
