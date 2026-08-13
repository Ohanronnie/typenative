use std::path::Path;

#[test]
fn from_is_a_contextual_method_name() {
    let source = br"struct TextFactory {
  public static from(value: & str): string { return value; }
}
";
    let parsed = tn_syntax::parse("contextual-method.tn", source);
    assert!(parsed.is_success(), "{:#?}", parsed.diagnostics());
}

#[test]
fn valid_fixture_corpus_parses_losslessly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/syntax/valid");
    let mut files = std::fs::read_dir(&root)
        .expect("valid fixture directory exists")
        .map(|entry| entry.expect("fixture entry is readable").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tn"))
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty(), "valid fixture corpus must not be empty");
    for path in files {
        let bytes = std::fs::read(&path).expect("fixture is readable");
        let parsed = tn_syntax::parse(&path.to_string_lossy(), &bytes);
        assert!(
            parsed.is_success(),
            "{} produced diagnostics: {:#?}",
            path.display(),
            parsed.diagnostics()
        );
        assert_eq!(parsed.syntax().to_string().as_bytes(), bytes);

        let first = tn_syntax::format(&path.to_string_lossy(), &bytes);
        assert!(first.is_success(), "{} failed to format", path.display());
        let second = tn_syntax::format(&path.to_string_lossy(), first.output.as_bytes());
        assert_eq!(
            first.output,
            second.output,
            "{} changed on the second formatting pass",
            path.display()
        );
        let reparsed = tn_syntax::parse(&path.to_string_lossy(), first.output.as_bytes());
        assert!(
            reparsed.is_success(),
            "formatted {} no longer parses: {:#?}",
            path.display(),
            reparsed.diagnostics()
        );
    }
}
