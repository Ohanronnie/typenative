#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let formatted = tn_syntax::format("fuzz.tnx", bytes);
    if formatted.diagnostics.is_empty() {
        let reparsed = tn_syntax::format("fuzz.tnx", formatted.output.as_bytes());
        assert!(
            reparsed.diagnostics.is_empty(),
            "JSX formatter produced invalid source"
        );
        assert_eq!(
            reparsed.output, formatted.output,
            "JSX formatter is not idempotent"
        );
    }
});
