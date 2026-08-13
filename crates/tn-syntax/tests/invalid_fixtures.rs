use serde_json::Value;
use std::path::Path;

#[test]
fn invalid_fixture_corpus_has_localized_diagnostics_and_lossless_recovery() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/syntax/invalid");
    let mut files = std::fs::read_dir(&root)
        .expect("invalid fixture directory exists")
        .map(|entry| entry.expect("fixture entry is readable").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tn"))
        .collect::<Vec<_>>();
    files.sort();
    assert!(
        files.len() >= 10,
        "invalid fixture inventory unexpectedly shrank"
    );
    for path in files {
        let bytes = std::fs::read(&path).expect("fixture is readable");
        let expected_path = path.with_extension("expected.json");
        let expected: Value = serde_json::from_slice(
            &std::fs::read(&expected_path).expect("expected condition file is readable"),
        )
        .expect("expected condition file is valid JSON");
        let expected_condition = expected["condition"]
            .as_str()
            .expect("condition is a string");
        let expected_message = expected["message"].as_str().expect("message is a string");
        let expected_start =
            u32::try_from(expected["start"].as_u64().expect("start is an integer"))
                .expect("start fits u32");
        let expected_end = u32::try_from(expected["end"].as_u64().expect("end is an integer"))
            .expect("end fits u32");
        let parsed = tn_syntax::parse(&path.to_string_lossy(), &bytes);
        assert!(
            !parsed.is_success(),
            "{} unexpectedly parsed",
            path.display()
        );
        assert_eq!(parsed.syntax().to_string().as_bytes(), bytes);
        let diagnostic = &parsed.diagnostics()[0];
        assert_eq!(diagnostic.condition.as_str(), expected_condition);
        assert_eq!(diagnostic.message, expected_message);
        assert_eq!(diagnostic.primary.span.file, path.to_string_lossy());
        assert_eq!(diagnostic.primary.span.byte_start, expected_start);
        assert_eq!(diagnostic.primary.span.byte_end, expected_end);
    }
}

#[test]
fn invalid_utf8_is_rejected_before_any_token_or_tree_text() {
    let parsed = tn_syntax::parse("invalid-utf8.tn", &[b'f', 0x80, b'o']);
    assert_eq!(parsed.syntax().to_string(), "");
    assert_eq!(
        parsed.diagnostics()[0].condition.as_str(),
        "SYNTAX_INVALID_UTF8"
    );
}
