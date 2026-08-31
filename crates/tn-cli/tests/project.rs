use std::path::Path;
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
fn executable_language_spec_examples_are_type_checked_and_run() {
    let documentation = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/language-spec.md"),
    )
    .expect("language specification");
    let mut examples = Vec::new();
    let mut current = None;
    for line in documentation.lines() {
        if line.trim() == "```tn-executable" {
            assert!(current.is_none(), "nested executable documentation fence");
            current = Some(String::new());
        } else if line.trim() == "```" && current.is_some() {
            examples.push(current.take().expect("example body"));
        } else if let Some(example) = current.as_mut() {
            example.push_str(line);
            example.push('\n');
        }
    }
    assert!(
        !examples.is_empty(),
        "the specification must contain an executable example"
    );
    for (index, example) in examples.into_iter().enumerate() {
        let directory = tempfile::tempdir().expect("documentation example directory");
        let source = directory.path().join(format!("example-{index}.tn"));
        std::fs::write(&source, example).expect("documentation example source");
        let run = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args(["run", source.to_str().expect("UTF-8 documentation path")])
            .output()
            .expect("documentation example runs");
        assert_eq!(
            run.status.code(),
            Some(42),
            "documentation example {index} failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

#[test]
fn lint_runs_canonical_sources_and_reports_hygiene_warnings() {
    let canonical = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["lint", "../../validation/generators/main.tn", "--json"])
        .output()
        .expect("tn lint canonical source runs");
    assert!(
        canonical.status.success(),
        "{}",
        String::from_utf8_lossy(&canonical.stderr)
    );
    assert!(canonical.stdout.is_empty());

    let directory = tempfile::tempdir().expect("temporary lint project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        "import { Range } from \"std/core\";\nfunction main(): void { }  \n",
    )
    .expect("lint source fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "lint",
            source.to_str().expect("UTF-8 source path"),
            "--json",
        ])
        .output()
        .expect("tn lint fixture runs");
    assert!(output.status.success());
    let conditions = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON lint diagnostic"))
        .map(|record| {
            record["condition"]
                .as_str()
                .expect("condition string")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(conditions.contains(&"LINT_TRAILING_WHITESPACE".into()));
    assert!(conditions.contains(&"LINT_UNUSED_IMPORT".into()));
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

fn jsx_runtime_diagnostic_conditions(runtime_source: &str) -> Vec<String> {
    jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        Some(runtime_source),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    )
}

fn jsx_runtime_diagnostic_conditions_for(
    entry_source: &str,
    runtime_source: Option<&str>,
    project_configuration: &str,
) -> Vec<String> {
    let directory = tempfile::tempdir().expect("temporary JSX diagnostic project");
    std::fs::write(directory.path().join("main.tnx"), entry_source)
        .expect("JSX diagnostic entry source");
    if let Some(runtime_source) = runtime_source {
        std::fs::write(directory.path().join("tnx-runtime.tn"), runtime_source)
            .expect("JSX diagnostic runtime source");
    }
    std::fs::write(
        directory.path().join("typenative.json"),
        project_configuration,
    )
    .expect("JSX diagnostic project configuration");
    let output = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "check",
            directory.path().to_str().expect("UTF-8 JSX project path"),
            "--json",
        ])
        .output()
        .expect("tn check JSX diagnostic project");
    assert_eq!(
        output.status.code(),
        Some(1),
        "configuration={project_configuration}; entry={entry_source}; stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON diagnostic"))
        .filter_map(|record| record["condition"].as_str().map(str::to_owned))
        .collect()
}

fn jsx_entry_source() -> &'static str {
    r#"import { Element } from "./tnx-runtime";
struct Props {
  public enabled: bool;
}
function Text(props: Props): Element { return new Element(); }
function main(): Element { return <Text enabled />; }
"#
}

#[test]
fn jsx_runtime_contract_failures_have_specific_diagnostics() {
    let private_and_missing = jsx_runtime_diagnostic_conditions(
        "export struct Element {}\nfunction createElement<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\n",
    );
    assert!(private_and_missing.contains(&"DRIVER_JSX_RUNTIME_PRIVATE_EXPORT".into()));
    assert_eq!(
        private_and_missing
            .iter()
            .filter(|condition| condition.as_str() == "DRIVER_JSX_RUNTIME_MISSING_EXPORT")
            .count(),
        2
    );

    let wrong_shape = jsx_runtime_diagnostic_conditions(
        "export struct Element {}\nexport struct createElement {}\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C, extra: C): Element { return new Element(); }\n",
    );
    assert!(wrong_shape.contains(&"DRIVER_JSX_RUNTIME_NOT_FUNCTION".into()));
    assert!(wrong_shape.contains(&"DRIVER_JSX_RUNTIME_WRONG_ARITY".into()));
}

#[test]
fn jsx_runtime_configuration_and_type_contract_fail_before_llvm() {
    let missing_configuration = jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        None,
        r#"{"entry":"main.tnx"}"#,
    );
    assert!(missing_configuration.contains(&"DRIVER_JSX_RUNTIME_REQUIRED".into()));

    let missing_module = jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        None,
        r#"{"entry":"main.tnx","jsx":{"runtime":"./missing"}}"#,
    );
    assert!(missing_module.contains(&"RESOLVE_JSX_RUNTIME_MODULE".into()));

    let missing_exports = jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        Some("export struct Element {}\n"),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    for operation in ["createElement", "createElements", "createFragment"] {
        assert!(
            missing_exports.contains(&"DRIVER_JSX_RUNTIME_MISSING_EXPORT".into()),
            "missing {operation}: {missing_exports:?}"
        );
    }
    assert_eq!(
        missing_exports
            .iter()
            .filter(|condition| condition.as_str() == "DRIVER_JSX_RUNTIME_MISSING_EXPORT")
            .count(),
        3
    );

    let wrong_property = jsx_runtime_diagnostic_conditions_for(
        jsx_entry_source(),
        Some(
            "export struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: i32, key: K): E { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
        ),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    assert!(
        wrong_property.contains(&"TYPE_JSX_RUNTIME_PROPERTIES_PARAMETER".into()),
        "{wrong_property:?}"
    );

    let wrong_key = jsx_runtime_diagnostic_conditions_for(
        jsx_entry_source(),
        Some(
            "export struct Element {}\nexport function createElement<P, E>(component: (P) => E, properties: P, key: bool): E { return component(properties); }\nexport function createElements<P, E>(component: (P) => E, properties: P, key: bool): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
        ),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    assert!(
        wrong_key.contains(&"TYPE_JSX_RUNTIME_KEY_PARAMETER".into()),
        "{wrong_key:?}"
    );

    let wrong_result = jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        Some(
            "export struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): i32 { return 0; }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): i32 { return 0; }\nexport function createFragment<C>(children: C): i32 { return 0; }\n",
        ),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    assert!(
        wrong_result.contains(&"DRIVER_JSX_RUNTIME_RESULT_MISMATCH".into()),
        "{wrong_result:?}"
    );

    let non_generic = jsx_runtime_diagnostic_conditions_for(
        jsx_entry_source(),
        Some(
            "export struct Element {}\nexport function createElement(component: (i32) => Element, properties: i32, key: string): Element { return component(properties); }\nexport function createElements(component: (i32) => Element, properties: i32, key: string): Element { return component(properties); }\nexport function createFragment(children: [Element; 1usize]): Element { return new Element(); }\n",
        ),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    assert!(
        non_generic.contains(&"TYPE_JSX_RUNTIME_COMPONENT_PARAMETER".into()),
        "{non_generic:?}"
    );
    assert!(
        non_generic.contains(&"TYPE_JSX_RUNTIME_PROPERTIES_PARAMETER".into()),
        "{non_generic:?}"
    );

    let effects = jsx_runtime_diagnostic_conditions_for(
        "function main(): i32 { return 0; }\n",
        Some(
            "struct Failure {}\nexport struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): E throws Failure { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
        ),
        r#"{"entry":"main.tnx","jsx":{"runtime":"./tnx-runtime"}}"#,
    );
    assert!(
        effects.contains(&"DRIVER_JSX_RUNTIME_EFFECTS".into()),
        "{effects:?}"
    );
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn tnx_jsx_runtime_reaches_native_execution_in_debug_and_optimized_profiles() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../validation/jsx/runtime/typenative.json");
    for profile in ["debug", "optimized"] {
        let directory = tempfile::tempdir().expect("temporary JSX build directory");
        let product = directory.path().join(format!("jsx-{profile}"));
        let build = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args([
                "build",
                fixture.to_str().expect("UTF-8 JSX fixture path"),
                "--profile",
                profile,
                "--out",
                product.to_str().expect("UTF-8 JSX output path"),
                "--timings",
            ])
            .output()
            .expect("tn build JSX fixture");
        assert!(
            build.status.success(),
            "{profile}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = Command::new(&product)
            .output()
            .expect("run native JSX fixture");
        assert_eq!(
            run.status.code(),
            Some(0),
            "{profile}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn tnx_jsx_llvm_contains_ordinary_calls_without_hashed_runtime_externals() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../validation/jsx/runtime/typenative.json");
    let directory = tempfile::tempdir().expect("temporary JSX LLVM directory");
    let product = directory.path().join("jsx.ll");
    let build = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            fixture.to_str().expect("UTF-8 JSX fixture path"),
            "--emit",
            "llvm-ir",
            "--out",
            product.to_str().expect("UTF-8 JSX LLVM path"),
        ])
        .output()
        .expect("tn build JSX LLVM fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let ir = std::fs::read_to_string(product).expect("JSX LLVM IR");
    assert!(!ir.contains("tn_jsx_runtime"));
    assert!(ir.contains("call { i32 } @tn_"), "{ir}");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn tnx_jsx_property_shape_collision_is_verified_in_llvm_assembly_and_symbols() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../validation/jsx/collision/typenative.json");
    let directory = tempfile::tempdir().expect("temporary JSX collision directory");
    let mut executables = Vec::new();
    for profile in ["debug", "optimized"] {
        let product = directory.path().join(format!("collision-{profile}"));
        let build = Command::new(env!("CARGO_BIN_EXE_tn"))
            .args([
                "build",
                fixture.to_str().expect("UTF-8 JSX collision fixture path"),
                "--profile",
                profile,
                "--out",
                product.to_str().expect("UTF-8 JSX collision output path"),
                "--timings",
            ])
            .output()
            .expect("build JSX collision fixture");
        assert!(
            build.status.success(),
            "{profile}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let timing_output = String::from_utf8_lossy(&build.stderr);
        assert!(timing_output.contains("phase=llvm-link"), "{timing_output}");
        let run = Command::new(&product)
            .output()
            .expect("run JSX collision fixture");
        assert_eq!(
            run.status.code(),
            Some(0),
            "{profile}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        executables.push(product);
    }

    let llvm = directory.path().join("collision.ll");
    let llvm_build = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            fixture.to_str().expect("UTF-8 JSX collision fixture path"),
            "--emit",
            "llvm-ir",
            "--out",
            llvm.to_str().expect("UTF-8 JSX collision LLVM path"),
        ])
        .output()
        .expect("emit JSX collision LLVM");
    assert!(
        llvm_build.status.success(),
        "{}",
        String::from_utf8_lossy(&llvm_build.stderr)
    );
    let ir = std::fs::read_to_string(&llvm).expect("read JSX collision LLVM");
    assert!(!ir.contains("tn_jsx_runtime_"), "{ir}");
    let runtime_calls = ir
        .lines()
        .filter(|line| line.contains("call { i32 } @tn_") && line.contains("zeroinitializer"))
        .collect::<Vec<_>>();
    assert_eq!(runtime_calls.len(), 2, "{runtime_calls:?}");
    assert!(
        runtime_calls
            .iter()
            .any(|line| line.contains("{ { ptr, i64 } }"))
    );
    assert!(
        runtime_calls
            .iter()
            .any(|line| line.contains("{ i1, { i32 } }"))
    );

    let assembly = directory.path().join("collision.s");
    let assembly_build = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args([
            "build",
            fixture.to_str().expect("UTF-8 JSX collision fixture path"),
            "--emit",
            "assembly",
            "--out",
            assembly
                .to_str()
                .expect("UTF-8 JSX collision assembly path"),
        ])
        .output()
        .expect("emit JSX collision assembly");
    assert!(
        assembly_build.status.success(),
        "{}",
        String::from_utf8_lossy(&assembly_build.stderr)
    );
    let assembly_source = std::fs::read_to_string(&assembly).expect("read JSX collision assembly");
    assert!(!assembly_source.contains("tn_jsx_runtime_"));

    for executable in executables {
        let symbols = Command::new("nm")
            .args(["-gU", executable.to_str().expect("UTF-8 executable path")])
            .output()
            .expect("inspect JSX collision symbols");
        assert!(symbols.status.success());
        assert!(!String::from_utf8_lossy(&symbols.stdout).contains("tn_jsx_runtime_"));
    }
}

#[test]
fn native_using_invokes_the_symbol_disposal_protocol_at_scope_exit() {
    let directory = tempfile::tempdir().expect("temporary disposal project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        r#"import { Disposable } from "std/core";
import { exit } from "std/process";
class Resource implements Disposable {
  public [Symbol.dispose](): void { exit(77); }
}

function main(): i32 {
  {
    using resource = new Resource();
    resource;
  }
  return 1;
}
"#,
    )
    .expect("disposal source fixture");

    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run executes disposal fixture");
    assert_eq!(
        run.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn native_task_group_returns_an_independent_awaitable_task_and_closes_asynchronously() {
    let directory = tempfile::tempdir().expect("temporary structured task project");
    let source = directory.path().join("main.tn");
    std::fs::write(
        &source,
        r#"import { runI32, TaskGroup } from "std/async";
async function child(): Promise<i32, never> { return 23; }
async function parent(): Promise<i32, never> {
  await using tasks = new TaskGroup();
  const task = tasks.spawn(child());
  return await task;
}
function main(): i32 { return runI32(parent()); }
"#,
    )
    .expect("structured task source fixture");

    let run = Command::new(env!("CARGO_BIN_EXE_tn"))
        .args(["run", source.to_str().expect("UTF-8 source path")])
        .output()
        .expect("tn run executes structured task fixture");
    assert_eq!(
        run.status.code(),
        Some(23),
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
fn unchanged_native_builds_reuse_content_addressed_products() {
    let directory = tempfile::tempdir().expect("temporary incremental build project");
    let source = directory.path().join("main.tn");
    let product = directory.path().join("main.ll");
    std::fs::write(&source, "function main(): i32 { return 7; }\n")
        .expect("incremental source fixture");
    let build = || {
        Command::new(env!("CARGO_BIN_EXE_tn"))
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
            .expect("incremental tn build runs")
    };

    let first = build();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!String::from_utf8_lossy(&first.stderr).contains("phase=cache-hit"));
    let first_product = std::fs::read(&product).expect("first incremental product");

    let second = build();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("phase=cache-hit"));
    assert_eq!(
        std::fs::read(&product).expect("cached incremental product"),
        first_product
    );

    std::fs::write(&source, "function main(): i32 { return 8; }\n")
        .expect("change incremental source fixture");
    let third = build();
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert!(!String::from_utf8_lossy(&third.stderr).contains("phase=cache-hit"));
    assert_ne!(
        std::fs::read(&product).expect("rebuilt incremental product"),
        first_product
    );
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
        "function passes(): void {}\ntest(\"passes\", () => passes());\nexport function answer(): i32 { return 42i32; }\n",
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
