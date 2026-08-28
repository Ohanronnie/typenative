//! Revision-oriented compiler driver and configuration boundary.

mod build;
mod docs;
mod lint;
mod lsp;
mod project;
mod test;

use std::path::Path;
use std::time::Instant;
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_hir::{DeclarationKind, DefinitionData, Function, Type};

pub use build::{BuildError, BuildOutput, build_project, build_project_with_timings};
pub use docs::generate_docs;
pub use lint::lint_project;
pub use lsp::run_lsp;
pub use project::{
    Emit, JsxConfig, LinkConfig, Profile, Project, ProjectConfig, Sanitizer, Target,
    UnsupportedHost, load_project,
};
pub use test::{TestRun, run_tests};

#[derive(Clone, Debug)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckOutput {
    pub fn is_success(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

pub fn check_source(path: &Path, bytes: &[u8]) -> CheckOutput {
    let file = path.to_string_lossy();
    let parsed = tn_syntax::parse(&file, bytes);
    CheckOutput {
        diagnostics: parsed.diagnostics().to_vec(),
    }
}

pub fn check_project(project: &Project) -> CheckOutput {
    check_project_with_timings(project, false)
}

pub fn check_project_with_timings(project: &Project, timings_enabled: bool) -> CheckOutput {
    let started = Instant::now();
    let standard_library = standard_library_path();
    let graph = match tn_hir::load_module_graph_with_jsx_runtime(
        &project.root,
        &project.entry,
        &standard_library,
        project.config.jsx.as_ref().map(|jsx| jsx.runtime.clone()),
    ) {
        Ok(graph) => graph,
        Err(error) => {
            if !error.diagnostics().is_empty() {
                return CheckOutput {
                    diagnostics: error.diagnostics().to_vec(),
                };
            }
            return CheckOutput {
                diagnostics: vec![driver_diagnostic(
                    &project.entry,
                    format!("failed to load module graph: {error}"),
                )],
            };
        }
    };
    let program = match tn_hir::lower_program(graph) {
        Ok(program) => program,
        Err(diagnostics) => return CheckOutput { diagnostics },
    };
    let jsx_diagnostics = validate_jsx_runtime(&program);
    if !jsx_diagnostics.is_empty() {
        return CheckOutput {
            diagnostics: jsx_diagnostics,
        };
    }
    if timings_enabled {
        eprintln!(
            "tn-timing phase=module-check micros={}",
            started.elapsed().as_micros()
        );
    }
    let started = Instant::now();
    let ownership_facts = tn_typecheck::derive_ownership_facts(&program);
    let checked = tn_typecheck::check_signatures_with_ownership(&program, &ownership_facts);
    let source_rules = tn_typecheck::check_source_rules(&program);
    let bodies = tn_typecheck::check_bodies_with_ownership(&program, &ownership_facts);
    let mir_ready = checked.diagnostics.is_empty() && bodies.diagnostics.is_empty();
    let static_requirements = tn_typecheck::check_static_requirements(&program, &ownership_facts);
    if timings_enabled {
        eprintln!(
            "tn-timing phase=ownership micros={}",
            started.elapsed().as_micros()
        );
    }
    let mut diagnostics = checked.diagnostics;
    diagnostics.extend(source_rules.diagnostics);
    diagnostics.extend(bodies.diagnostics);
    diagnostics.extend(static_requirements.diagnostics);
    if mir_ready {
        let started = Instant::now();
        for body in
            tn_typecheck::lower_mir_with_ownership(&program, &bodies.bodies, &ownership_facts)
        {
            diagnostics.extend(tn_typecheck::check_ownership(&body, &ownership_facts).diagnostics);
        }
        if timings_enabled {
            eprintln!(
                "tn-timing phase=mir-drop micros={}",
                started.elapsed().as_micros()
            );
        }
    }
    diagnostics.sort_by(|left, right| {
        left.primary
            .span
            .file
            .cmp(&right.primary.span.file)
            .then(
                left.primary
                    .span
                    .byte_start
                    .cmp(&right.primary.span.byte_start),
            )
            .then(left.condition.as_str().cmp(right.condition.as_str()))
    });
    diagnostics.dedup_by(|left, right| {
        left.condition == right.condition && left.primary.span == right.primary.span
    });
    CheckOutput { diagnostics }
}

fn standard_library_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("TYPENATIVE_STDLIB") {
        return path.into();
    }
    let installed = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .map(|directory| directory.join("../lib/typenative/std"));
    if let Some(path) = installed.filter(|path| path.is_dir()) {
        return path;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std")
}

fn driver_diagnostic(path: &Path, message: String) -> Diagnostic {
    let file = path.to_string_lossy();
    Diagnostic::error(
        ConditionId::new("DRIVER_MODULE_IO_ERROR").expect("static condition is valid"),
        message,
        Label {
            span: SourceSpan::new(&*file, 0..0, ""),
            message: "the compiler could not read a required module".into(),
        },
        "driver/module/io/error",
    )
}

pub(crate) fn validate_jsx_runtime(program: &tn_hir::Program) -> Vec<Diagnostic> {
    let Some(module) = program.graph.modules.iter().find(|module| {
        module
            .path
            .extension()
            .is_some_and(|extension| extension == "tnx")
    }) else {
        return Vec::new();
    };
    let Some(runtime) = program.graph.jsx_runtime.as_deref() else {
        return vec![jsx_runtime_required_diagnostic(module)];
    };
    if runtime.trim().is_empty() {
        return vec![jsx_runtime_required_diagnostic(module)];
    }
    let Some(runtime_module) = program.graph.jsx_runtime_module else {
        return vec![jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_MODULE_MISSING",
            "the configured JSX runtime module was not loaded",
            &module_start_span(module),
            "configure `jsx.runtime` with a resolvable TypeNative module",
        )];
    };
    let Some(runtime_module) = program.graph.module(runtime_module) else {
        return vec![jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_MODULE_MISSING",
            "the configured JSX runtime module is absent from the module graph",
            &module_start_span(module),
            "configure `jsx.runtime` with a resolvable TypeNative module",
        )];
    };
    let mut diagnostics = Vec::new();
    for (operation, arity) in [("jsx", 3_usize), ("jsxs", 3), ("fragment", 1)] {
        let Some(declaration) = runtime_module
            .declarations
            .iter()
            .find(|declaration| declaration.name.as_deref() == Some(operation))
        else {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_MISSING_EXPORT",
                format!("configured JSX runtime does not export `{operation}`"),
                &module_start_span(module),
                "export jsx, jsxs, and fragment from the configured runtime module",
            ));
            continue;
        };
        if !declaration.exported {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_PRIVATE_EXPORT",
                format!("JSX runtime export `{operation}` is private"),
                &declaration.span,
                "mark the runtime operation `export` so the compiler can resolve it",
            ));
            continue;
        }
        if declaration.kind == DeclarationKind::ExternFunction {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_FOREIGN_DECLARATION",
                format!("JSX runtime `{operation}` must not be a foreign declaration"),
                &declaration.span,
                "implement the JSX runtime operation in TypeNative source",
            ));
            continue;
        }
        if declaration.kind != DeclarationKind::Function {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_NOT_FUNCTION",
                format!("JSX runtime export `{operation}` is not a function"),
                &declaration.span,
                "export a TypeNative function for this runtime operation",
            ));
            continue;
        }
        let Some(DefinitionData::Function(function)) = program
            .definition(declaration.id)
            .map(|definition| &definition.data)
        else {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_MISSING_DECLARATION",
                format!("JSX runtime `{operation}` has no semantic definition"),
                &declaration.span,
                "provide the complete function declaration in the runtime module",
            ));
            continue;
        };
        validate_jsx_runtime_function(
            program,
            operation,
            arity,
            &declaration.span,
            function,
            &mut diagnostics,
        );
    }
    diagnostics
}

fn jsx_runtime_required_diagnostic(module: &tn_hir::Module) -> Diagnostic {
    jsx_runtime_diagnostic(
        "DRIVER_JSX_RUNTIME_REQUIRED",
        "a `.tnx` project must configure a JSX runtime",
        &module_start_span(module),
        "add `jsx.runtime` to typenative.json",
    )
}

fn module_start_span(module: &tn_hir::Module) -> SourceSpan {
    SourceSpan::new(module.path.to_string_lossy(), 0..0, &module.source)
}

fn validate_jsx_runtime_function(
    program: &tn_hir::Program,
    operation: &str,
    arity: usize,
    span: &SourceSpan,
    function: &Function,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if function.parameters.len() != arity {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_WRONG_ARITY",
            format!(
                "JSX runtime `{operation}` declares {} parameter(s), expected {arity}",
                function.parameters.len()
            ),
            span,
            "declare the runtime operation with the required callable arity",
        ));
    }
    if function.is_async {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_ASYNC",
            format!("JSX runtime `{operation}` cannot be asynchronous"),
            span,
            "return an Element synchronously from the runtime operation",
        ));
    }
    if function.is_unsafe {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_UNSAFE",
            format!("JSX runtime `{operation}` cannot be unsafe"),
            span,
            "declare a safe TypeNative runtime operation",
        ));
    }
    if !function.effects.is_empty() {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_EFFECTS",
            format!("JSX runtime `{operation}` declares throwing effects"),
            span,
            "make JSX runtime operations infallible; effect handling belongs in ordinary application code",
        ));
    }
    if function.body_start == 0 || function.body_end <= function.body_start {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_NO_BODY",
            format!("JSX runtime `{operation}` has no TypeNative body"),
            span,
            "implement the runtime operation in TypeNative source",
        ));
    }
    if operation != "fragment"
        && function
            .parameters
            .first()
            .is_some_and(|parameter| !matches!(parameter.ty, Type::Function(_) | Type::Generic(_)))
    {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_COMPONENT_PARAMETER",
            format!("JSX runtime `{operation}` must accept a component callable"),
            span,
            "make the first parameter a component function or an inferred generic component value",
        ));
    }
    if operation == "fragment"
        && function.parameters.first().is_some_and(|parameter| {
            !matches!(
                parameter.ty,
                Type::Array(_, _) | Type::Slice(_) | Type::Generic(_)
            )
        })
    {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_CHILDREN_PARAMETER",
            "JSX runtime `fragment` must accept an array, slice, or inferred children value",
            span,
            "declare the fragment children parameter as a child collection",
        ));
    }
    if !type_named_element(program, &function.result) {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_RESULT_MISMATCH",
            format!(
                "JSX runtime `{operation}` must return the runtime `Element` type, found {:?}",
                function.result
            ),
            span,
            "return the exported Element type from every JSX runtime operation",
        ));
    }
    for generic in &function.generics {
        if !function
            .parameters
            .iter()
            .any(|parameter| type_contains_generic(&parameter.ty, &generic.name))
            && !type_contains_generic(&function.result, &generic.name)
        {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_GENERIC_INFERENCE",
                format!(
                    "JSX runtime `{operation}` generic `{}` cannot be inferred",
                    generic.name
                ),
                span,
                "use every runtime generic in a parameter or result type",
            ));
        }
    }
}

fn type_named_element(program: &tn_hir::Program, ty: &Type) -> bool {
    let Some(runtime_module) = program.graph.jsx_runtime_module else {
        return false;
    };
    matches!(
        ty,
        Type::Nominal(declaration, _)
            if program
                .graph
                .declaration(*declaration)
                .is_some_and(|declaration| {
                    declaration.module == runtime_module
                        && declaration.name.as_deref() == Some("Element")
                })
    ) || matches!(ty, Type::Generic(_))
}

fn type_contains_generic(ty: &Type, name: &str) -> bool {
    match ty {
        Type::Generic(candidate) | Type::Lifetime(candidate) => candidate == name,
        Type::Promise { result, error, .. } => {
            type_contains_generic(result, name) || type_contains_generic(error, name)
        }
        Type::Nominal(_, arguments)
        | Type::DynamicInterface(_, arguments)
        | Type::Tuple(arguments)
        | Type::Template(arguments) => arguments
            .iter()
            .any(|argument| type_contains_generic(argument, name)),
        Type::Optional(inner)
        | Type::Array(inner, _)
        | Type::Slice(inner)
        | Type::Reference {
            referent: inner, ..
        }
        | Type::RawPointer { pointee: inner, .. } => type_contains_generic(inner, name),
        Type::Function(function) => {
            function
                .parameters
                .iter()
                .any(|parameter| type_contains_generic(parameter, name))
                || type_contains_generic(&function.result, name)
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => false,
    }
}

fn jsx_runtime_diagnostic(
    id: &str,
    message: impl Into<String>,
    span: &SourceSpan,
    label: &str,
) -> Diagnostic {
    Diagnostic::error(
        ConditionId::new(id).expect("static condition is valid"),
        message,
        Label {
            span: span.clone(),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    )
}

#[derive(Clone, Debug)]
pub struct FormatOutput {
    pub formatted: String,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn format_source(path: &Path, bytes: &[u8]) -> FormatOutput {
    let file = path.to_string_lossy();
    let formatted = tn_syntax::format(&file, bytes);
    FormatOutput {
        formatted: formatted.output,
        diagnostics: formatted.diagnostics,
    }
}
