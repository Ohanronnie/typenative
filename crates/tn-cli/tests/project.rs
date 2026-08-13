use std::process::Command;

#[test]
fn check_resolves_strict_project_configuration_and_direct_sources() {
    let directory = tempfile::tempdir().expect("temporary project directory");
    let source_directory = directory.path().join("src");
    std::fs::create_dir(&source_directory).expect("source directory");
    let source = source_directory.join("main.tn");
    std::fs::write(&source, "function main(): void {}\n").expect("source fixture");
    std::fs::write(
        directory.path().join("typenative.json"),
        r#"{"entry":"src/main.tn"}"#,
    )
    .expect("project configuration");

    let project = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["check", directory.path().to_str().expect("UTF-8 path")])
        .output()
        .expect("tn check project runs");
    assert!(
        project.status.success(),
        "{}",
        String::from_utf8_lossy(&project.stderr)
    );

    let direct = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["check", source.to_str().expect("UTF-8 path")])
        .output()
        .expect("tn check source runs");
    assert!(direct.status.success());

    std::fs::write(
        directory.path().join("typenative.json"),
        r#"{"entry":"src/main.tn","dependencies":{}}"#,
    )
    .expect("invalid project configuration");
    let invalid = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["check", directory.path().to_str().expect("UTF-8 path")])
        .output()
        .expect("tn check invalid project runs");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown field `dependencies`"));
}

#[test]
fn check_runs_resolution_signature_effect_and_ownership_rules_without_llvm() {
    let directory = tempfile::tempdir().expect("temporary semantic project");
    let source = directory.path().join("semantic.tn");
    std::fs::write(
        &source,
        r"struct Failure {}
class Thing {}
@unknown
function fail(): void throws Failure {}
const invalid = new Thing();
function caller(value: string): void {
  fail();
  await pending;
  move value;
  value;
}
",
    )
    .expect("semantic fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "check",
            source.to_str().expect("UTF-8 source path"),
            "--json",
        ])
        .output()
        .expect("tn check semantic fixture");
    assert_eq!(output.status.code(), Some(1));
    let conditions = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("JSON diagnostic")["condition"]
                .as_str()
                .expect("condition string")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(conditions.contains(&"TYPE_UNKNOWN_ATTRIBUTE".into()));
    assert!(conditions.contains(&"TYPE_NON_CONSTANT_INITIALIZER".into()));
    assert!(conditions.contains(&"TYPE_MISSING_TRY".into()));
    assert!(conditions.contains(&"TYPE_AWAIT_OUTSIDE_ASYNC".into()));
    assert!(conditions.contains(&"OWNERSHIP_USE_AFTER_MOVE".into()));
}

#[test]
fn build_emits_verified_native_products_and_run_preserves_exit_status() {
    let directory = tempfile::tempdir().expect("temporary native project");
    let source = directory.path().join("main.tn");
    std::fs::write(&source, "function main(): i32 { return 7; }\n").expect("native source fixture");
    for (emit, extension) in [
        ("llvm-ir", "ll"),
        ("bitcode", "bc"),
        ("assembly", "s"),
        ("object", "o"),
    ] {
        let product = directory.path().join(format!("main.{extension}"));
        let output = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args([
                "build",
                source.to_str().expect("UTF-8 source path"),
                "--emit",
                emit,
                "--out",
                product.to_str().expect("UTF-8 product path"),
            ])
            .output()
            .expect("tn build runs");
        assert!(
            output.status.success(),
            "{emit}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(std::fs::metadata(product).expect("native product").len() > 0);
    }

    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run executes native program");
    assert_eq!(
        run.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn build_timings_report_each_compiler_phase() {
    let directory = tempfile::tempdir().expect("temporary timing project");
    let source = directory.path().join("main.tn");
    let product = directory.path().join("main.ll");
    std::fs::write(&source, "function main(): i32 { return 7; }\n").expect("timing source fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            source.to_str().expect("UTF-8 source path"),
            "--emit",
            "llvm-ir",
            "--out",
            product.to_str().expect("UTF-8 product path"),
            "--timings",
        ])
        .output()
        .expect("tn build with timings runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for phase in [
        "module-check",
        "ownership",
        "mir-drop",
        "monomorphization",
        "llvm-link",
    ] {
        assert!(
            stderr.contains(&format!("tn-timing phase={phase} micros=")),
            "missing timing phase {phase}: {stderr}"
        );
    }
}

#[test]
fn check_timings_report_reused_analysis_phases() {
    let directory = tempfile::tempdir().expect("temporary check timing project");
    let source = directory.path().join("main.tn");
    std::fs::write(&source, "function main(): i32 { return 7; }\n")
        .expect("check timing source fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "check",
            source.to_str().expect("UTF-8 source path"),
            "--timings",
        ])
        .output()
        .expect("tn check with timings runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for phase in ["module-check", "ownership", "mir-drop"] {
        assert!(
            stderr.contains(&format!("tn-timing phase={phase} micros=")),
            "missing timing phase {phase}: {stderr}"
        );
    }
}

#[test]
fn native_reachability_prunes_unreferenced_non_exports() {
    let directory = tempfile::tempdir().expect("temporary reachability project");
    let source = directory.path().join("main.tn");
    let product = directory.path().join("main.ll");
    std::fs::write(
        &source,
        "function dead(): i32 { return 99; }\nfunction main(): i32 { return 7; }\n",
    )
    .expect("reachability source fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            source.to_str().expect("UTF-8 source path"),
            "--emit",
            "llvm-ir",
            "--out",
            product.to_str().expect("UTF-8 product path"),
        ])
        .output()
        .expect("reachability build runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ir = std::fs::read_to_string(product).expect("LLVM IR product");
    assert_eq!(ir.matches("define i32 @").count(), 1, "{ir}");
}

#[test]
fn debug_and_optimized_array_bounds_execution_are_equivalent() {
    let directory = tempfile::tempdir().expect("temporary bounds project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "function main(): i32 {\n  let values: [i32; 3] = [10, 20, 30];\n  return values[1];\n}\n",
    )
    .expect("array source fixture");
    for profile in ["debug", "optimized"] {
        let run = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args([
                "run",
                source.to_str().expect("UTF-8 source path"),
                "--profile",
                profile,
            ])
            .output()
            .expect("tn run array program");
        assert_eq!(
            run.status.code(),
            Some(20),
            "{profile}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
    std::fs::write(
        &source,
        "function main(): i32 {\n  let values: [i32; 3] = [10, 20, 30];\n  return values[3];\n}\n",
    )
    .expect("out-of-range source fixture");
    for profile in ["debug", "optimized"] {
        let run = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args([
                "run",
                source.to_str().expect("UTF-8 source path"),
                "--profile",
                profile,
            ])
            .output()
            .expect("tn run out-of-range program");
        assert!(!run.status.success(), "{profile} must abort");
        assert!(
            String::from_utf8_lossy(&run.stderr).contains("TypeNative panic"),
            "{profile}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

#[test]
fn native_struct_fields_follow_declaration_layout() {
    let directory = tempfile::tempdir().expect("temporary struct project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "struct Point { public x: i32; public y: i32; }\nfunction main(): i32 {\n  const point: Point = { y: 2, x: 40 };\n  return point.x + point.y;\n}\n",
    )
    .expect("struct source fixture");
    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run struct program");
    assert_eq!(
        run.status.code(),
        Some(42),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn native_enum_switches_preserve_payload_layout() {
    let directory = tempfile::tempdir().expect("temporary enum project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "enum Value { Number(i32), Empty, }\nfunction main(): i32 {\n  const value: Value = Value.Number(42);\n  return switch (value) {\n    case Value.Number(number): number,\n    case Value.Empty: 0,\n  };\n}\n",
    )
    .expect("enum source fixture");
    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run enum program");
    assert_eq!(
        run.status.code(),
        Some(42),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn native_reachability_emits_inferred_generic_instances() {
    let directory = tempfile::tempdir().expect("temporary generic project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "function identity<T>(value: T): T { return value; }\nfunction main(): i32 { return identity(42); }\n",
    )
    .expect("generic source fixture");
    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run generic program");
    assert_eq!(
        run.status.code(),
        Some(42),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn doc_and_test_commands_use_resolved_programs() {
    let directory = tempfile::tempdir().expect("temporary tooling project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "@Test\nfunction passes(): void {}\n@Export(\"answer\") function answer(): i32 { return 42i32; }\n",
    )
    .expect("tooling source fixture");
    let documentation = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["doc", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn doc runs");
    assert!(documentation.status.success());
    assert!(String::from_utf8_lossy(&documentation.stdout).contains("answer"));
    let tests = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["test", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn test runs");
    assert!(
        tests.status.success(),
        "{}",
        String::from_utf8_lossy(&tests.stderr)
    );
    assert!(String::from_utf8_lossy(&tests.stdout).contains("1 passed; 1 total"));
}
