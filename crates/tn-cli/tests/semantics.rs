use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_files(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files = std::fs::read_dir(root)
        .expect("semantic fixture directory")
        .map(|entry| entry.expect("semantic fixture entry").path())
        .filter(|path| path.extension().is_some_and(|value| value == extension))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn check(path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "check",
            path.to_str().expect("UTF-8 fixture path"),
            "--json",
        ])
        .output()
        .expect("tn check semantic fixture")
}

fn condition_records(output: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(output)
        .lines()
        .map(|line| serde_json::from_str(line).expect("structured diagnostic record"))
        .collect()
}

#[test]
fn semantic_compile_pass_corpus_is_deterministic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/semantics/pass");
    let fixtures = fixture_files(&root, "tn");
    assert!(!fixtures.is_empty());
    for fixture in fixtures {
        let first = check(&fixture);
        let second = check(&fixture);
        assert!(
            first.status.success(),
            "{}\n{}\n{}",
            fixture.display(),
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(first.stdout, second.stdout, "{}", fixture.display());
        assert_eq!(first.stderr, second.stderr, "{}", fixture.display());
    }
}

#[test]
fn semantic_compile_fail_corpus_has_expected_causal_diagnostics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/semantics/fail");
    let expectations = fixture_files(&root, "conditions");
    assert!(!expectations.is_empty());
    for expected_path in expectations {
        let fixture = expected_path.with_extension("tn");
        let output = check(&fixture);
        assert_eq!(output.status.code(), Some(1), "{}", fixture.display());
        let records = condition_records(&output.stdout);
        let conditions = records
            .iter()
            .filter_map(|record| record["condition"].as_str())
            .collect::<Vec<_>>();
        let expected = std::fs::read_to_string(&expected_path)
            .unwrap_or_else(|error| panic!("{}: {error}", expected_path.display()));
        for condition in expected.lines().filter(|line| !line.is_empty()) {
            assert!(
                conditions.contains(&condition),
                "{} missing {condition}: {conditions:?}",
                fixture.display()
            );
        }
        assert!(
            !conditions.contains(&"MIR_INVALID_BEFORE_BORROW_CHECK"),
            "{} exposed invalid compiler MIR: {conditions:?}",
            fixture.display()
        );
        for record in records {
            assert!(record["condition"].is_string());
            assert!(record["primary"]["span"]["file"].is_string());
            assert!(record["primary"]["message"].is_string());
            assert!(record["documentation_key"].is_string());
        }
    }
}

#[test]
fn semantic_type_inventory_has_a_causal_failure_for_every_declared_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/semantics");
    let passing = check(&root.join("pass/types.tn"));
    assert!(
        passing.status.success(),
        "{}",
        String::from_utf8_lossy(&passing.stdout)
    );

    let fixture = root.join("fail/type-inventory.tn");
    let source = std::fs::read_to_string(&fixture).expect("type inventory source");
    let output = check(&fixture);
    assert_eq!(output.status.code(), Some(1));
    let mismatch_lines = condition_records(&output.stdout)
        .into_iter()
        .filter(|record| record["condition"] == "TYPE_MISMATCH")
        .filter_map(|record| record["primary"]["span"]["line"].as_u64())
        .collect::<std::collections::BTreeSet<_>>();
    let declared_lines = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("const "))
        .map(|(line, _)| u64::try_from(line + 1).expect("fixture line limit"))
        .collect::<Vec<_>>();
    assert_eq!(declared_lines.len(), 35, "inventory changed without review");
    for line in declared_lines {
        assert!(
            mismatch_lines.contains(&line),
            "type inventory declaration on line {line} lacks a causal TYPE_MISMATCH"
        );
    }
}

#[test]
fn semantic_pipeline_ignores_randomized_discovery_and_hash_seeds() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/semantics/pass");
    let sources = ["cycle-a.tn", "cycle-b.tn"].map(|name| {
        (
            name,
            std::fs::read(fixture_root.join(name)).expect("cycle fixture"),
        )
    });
    let mut baseline = None;
    for reverse in [false, true] {
        for _ in 0..4 {
            let directory = tempfile::tempdir().expect("randomized semantic fixture directory");
            let order = if reverse { [1, 0] } else { [0, 1] };
            for index in order {
                std::fs::write(directory.path().join(sources[index].0), &sources[index].1)
                    .expect("write randomized semantic fixture");
            }
            let output = check(&directory.path().join("cycle-a.tn"));
            assert!(output.status.success());
            let observed = (output.stdout, output.stderr);
            if let Some(baseline) = &baseline {
                assert_eq!(baseline, &observed);
            } else {
                baseline = Some(observed);
            }
        }
    }
}
