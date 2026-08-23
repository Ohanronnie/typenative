#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    match String::from_utf8(bytes.to_vec()) {
        Ok(value) => {
            assert_eq!(value.len(), value.as_bytes().len());
            let roundtrip = String::from_utf8(value.as_bytes().to_vec()).expect("UTF-8 roundtrip");
            assert_eq!(roundtrip, value);
            let _ = value.to_uppercase();
            let _ = value.chars().count();
            for (offset, character) in value.char_indices() {
                assert!(offset < value.len());
                assert!(character.len_utf8() <= value.len() - offset);
            }
        }
        Err(error) => {
            assert!(error.utf8_error().valid_up_to() <= bytes.len());
        }
    }
    let _ = tn_syntax::lex("fuzz-utf8.tn", bytes);
});
