use crate::{BuildError, Project};
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tn_hir::{DefinitionData, Function, Module, Program, Type};
use tn_syntax::{TokenKind, lex};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestRun {
    pub total: usize,
    pub passed: usize,
    pub lines: Vec<String>,
}

impl TestRun {
    pub const fn is_success(&self) -> bool {
        self.total == self.passed
    }
}

#[derive(Clone, Debug)]
struct TestCase {
    module: PathBuf,
    name: String,
    function: Function,
    effects: Vec<String>,
}

/// Runs every top-level function marked with `@Test`.
///
/// # Errors
///
/// Returns diagnostics for invalid projects and driver errors when a test cannot be compiled or
/// launched.
pub fn run_tests(project: &Project, filter: Option<&str>) -> Result<TestRun, BuildError> {
    let graph = tn_hir::load_module_graph(
        &project.root,
        &project.entry,
        &super::standard_library_path(),
    )
    .map_err(|error| {
        if error.diagnostics().is_empty() {
            BuildError::Message(error.to_string())
        } else {
            BuildError::Diagnostics(error.diagnostics().to_vec())
        }
    })?;
    let program = tn_hir::lower_program(graph).map_err(BuildError::Diagnostics)?;
    let tests = discover_tests(&program)
        .into_iter()
        .filter(|test| filter.is_none_or(|filter| test.name.contains(filter)))
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut passed = 0;
    for test in &tests {
        let result = run_one(project, &program, test)?;
        if result {
            passed += 1;
            lines.push(format!("test {} ... ok", test.name));
        } else {
            lines.push(format!("test {} ... FAILED", test.name));
        }
    }
    Ok(TestRun {
        total: tests.len(),
        passed,
        lines,
    })
}

fn discover_tests(program: &Program) -> Vec<TestCase> {
    let mut tests = Vec::new();
    for module in &program.graph.modules {
        for name in attribute_test_names(module) {
            let Some(declaration) = module
                .declarations
                .iter()
                .find(|declaration| declaration.name.as_deref() == Some(&name))
            else {
                continue;
            };
            let Some(definition) = program.definition(declaration.id) else {
                continue;
            };
            let DefinitionData::Function(function) = &definition.data else {
                continue;
            };
            let effects = function
                .effects
                .iter()
                .filter_map(|effect| program.graph.declaration(*effect))
                .filter_map(|declaration| declaration.name.clone())
                .collect();
            tests.push(TestCase {
                module: module.path.clone(),
                name,
                function: function.clone(),
                effects,
            });
        }
    }
    tests.sort_by(|left, right| {
        left.module
            .cmp(&right.module)
            .then(left.name.cmp(&right.name))
    });
    tests
}

fn attribute_test_names(module: &Module) -> BTreeSet<String> {
    let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let mut names = BTreeSet::new();
    let mut depth = 0_u32;
    let mut index = 0;
    while index < significant.len() {
        match significant[index].kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            TokenKind::At if depth == 0 => {
                let mut declaration_index = index + 2;
                while significant
                    .get(declaration_index)
                    .is_some_and(|token| matches!(token.kind, TokenKind::Async | TokenKind::Unsafe))
                {
                    declaration_index += 1;
                }
                if significant
                    .get(index + 1)
                    .is_some_and(|token| token.kind == TokenKind::Identifier)
                    && significant
                        .get(index + 1)
                        .is_some_and(|token| &module.source[token.range.clone()] == "Test")
                    && significant
                        .get(declaration_index)
                        .is_some_and(|token| token.kind == TokenKind::Function)
                    && let Some(name) = significant.get(declaration_index + 1)
                    && name.kind == TokenKind::Identifier
                {
                    names.insert(module.source[name.range.clone()].to_owned());
                }
            }
            _ => {}
        }
        index += 1;
    }
    names
}

fn run_one(project: &Project, program: &Program, test: &TestCase) -> Result<bool, BuildError> {
    let async_test = test.function.is_async || matches!(test.function.result, Type::Promise { .. });
    let temporary = tempfile::tempdir()?;
    let source_root = project.root.canonicalize()?;
    let standard_library = super::standard_library_path();
    for module in &program.graph.modules {
        if module.path.starts_with(&standard_library) {
            continue;
        }
        let relative = module.path.strip_prefix(&source_root).map_or_else(
            |_| {
                Path::new(
                    module
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("module.tn"),
                )
                .to_path_buf()
            },
            Path::to_path_buf,
        );
        let destination = temporary.path().join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut source = std::fs::read_to_string(&module.path)?;
        if module.path == test.module {
            rename_existing_main(&mut source);
            if !source.ends_with('\n') {
                source.push('\n');
            }
            let effects = if async_test || test.effects.is_empty() {
                String::new()
            } else {
                format!(" throws {}", test.effects.join(" | "))
            };
            let call = if async_test {
                "  const promise = ".to_owned()
                    + &test.name
                    + "();\n  unsafe { tn_runtime_promise_wait(promise as *mut u8); tn_runtime_async_destroy(promise as *mut u8); }\n"
            } else if test.effects.is_empty() {
                format!("  {}();\n", test.name)
            } else {
                format!("  try {}();\n", test.name)
            };
            if async_test {
                source.insert_str(
                    0,
                    "extern \"C\" { function tn_runtime_promise_wait(promise: *mut u8): void; function tn_runtime_async_destroy(promise: *mut u8): i32; }\n",
                );
            }
            let _ = write!(source, "function main(): void{effects} {{\n{call}}}\n");
        }
        std::fs::write(destination, source)?;
    }
    let relative_test = test.module.strip_prefix(&source_root).map_or_else(
        |_| {
            Path::new(
                test.module
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("test.tn"),
            )
            .to_path_buf()
        },
        Path::to_path_buf,
    );
    let entry = temporary.path().join(relative_test);
    let mut harness = project.clone();
    harness.root = temporary.path().to_path_buf();
    harness.entry.clone_from(&entry);
    harness.config.entry = entry;
    harness.config.emit = crate::Emit::Executable;
    let output = super::build_project(&harness, None)?;
    Ok(Command::new(output.product).status()?.success())
}

fn rename_existing_main(source: &mut String) {
    let lexed = lex("<test-harness>", source.as_bytes());
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    for pair in significant.windows(2) {
        if pair[0].kind == TokenKind::Function
            && pair[1].kind == TokenKind::Identifier
            && &source[pair[1].range.clone()] == "main"
        {
            source.replace_range(pair[1].range.clone(), "__tn_original_main");
            break;
        }
    }
}
