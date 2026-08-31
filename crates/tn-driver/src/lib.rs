//! Revision-oriented compiler driver and configuration boundary.

mod build;
mod docs;
mod lint;
mod lsp;
mod project;
mod test;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    let standard_library = standard_library_path();
    let fingerprint = check_cache_fingerprint(project, &standard_library).ok();
    if let Some(fingerprint) = fingerprint.as_deref()
        && let Some(diagnostics) = read_check_cache(project, fingerprint)
    {
        if timings_enabled {
            eprintln!("tn-timing phase=cache-hit micros=0");
        }
        return CheckOutput { diagnostics };
    }
    let output = check_project_uncached(project, timings_enabled, &standard_library);
    if let Some(fingerprint) = fingerprint.as_deref() {
        write_check_cache(project, fingerprint, &output.diagnostics);
    }
    output
}

fn check_project_uncached(
    project: &Project,
    timings_enabled: bool,
    standard_library: &Path,
) -> CheckOutput {
    let started = Instant::now();
    let graph = match tn_hir::load_module_graph_with_jsx_runtime(
        &project.root,
        &project.entry,
        standard_library,
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

const CHECK_CACHE_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CheckCache {
    version: u32,
    fingerprint: String,
    diagnostics: Vec<Diagnostic>,
}

fn check_cache_path(project: &Project) -> std::path::PathBuf {
    let directory = if project.config.out_dir.is_absolute() {
        project.config.out_dir.clone()
    } else {
        project.root.join(&project.config.out_dir)
    };
    directory.join(".tn-check-cache.json")
}

fn read_check_cache(project: &Project, fingerprint: &str) -> Option<Vec<Diagnostic>> {
    let bytes = std::fs::read(check_cache_path(project)).ok()?;
    let cache = serde_json::from_slice::<CheckCache>(&bytes).ok()?;
    (cache.version == CHECK_CACHE_VERSION && cache.fingerprint == fingerprint)
        .then_some(cache.diagnostics)
}

fn write_check_cache(project: &Project, fingerprint: &str, diagnostics: &[Diagnostic]) {
    let path = check_cache_path(project);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let cache = CheckCache {
        version: CHECK_CACHE_VERSION,
        fingerprint: fingerprint.to_owned(),
        diagnostics: diagnostics.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    let temporary = parent.join(".tn-check-cache.json.tmp");
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

fn check_cache_fingerprint(project: &Project, standard_library: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(CHECK_CACHE_VERSION.to_le_bytes());
    hasher.update(project.root.to_string_lossy().as_bytes());
    hasher.update(project.entry.to_string_lossy().as_bytes());
    hasher.update(serde_json::to_vec(&project.config).map_err(std::io::Error::other)?);
    hash_file_metadata(&mut hasher, &std::env::current_exe()?)?;
    hash_source_tree(&mut hasher, check_source_root(&project.root))?;
    hash_source_tree(&mut hasher, standard_library)?;
    let digest = hasher.finalize();
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut fingerprint, "{byte:02x}").expect("writing a String cannot fail");
    }
    Ok(fingerprint)
}

fn hash_source_tree(hasher: &mut Sha256, root: &Path) -> std::io::Result<()> {
    let mut paths = Vec::new();
    collect_source_paths(root, &mut paths)?;
    paths.sort();
    for path in paths {
        hash_file_if_present(hasher, &path)?;
    }
    Ok(())
}

fn check_source_root(root: &Path) -> &Path {
    let original = root;
    let mut current = root;
    loop {
        if current.join(".git").exists() {
            return current;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    original
}

fn collect_source_paths(root: &Path, paths: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some(".git" | "build" | "target" | ".cache" | "node_modules")
            )
        {
            collect_source_paths(&path, paths)?;
        } else if file_type.is_file()
            && !matches!(
                entry.file_name().to_str(),
                Some(".tn-check-cache.json" | ".tn-check-cache.json.tmp")
            )
            && path
                .extension()
                .is_some_and(|extension| matches!(extension.to_str(), Some("tn" | "tnx" | "json")))
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn hash_file_if_present(hasher: &mut Sha256, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    Ok(())
}

fn hash_file_metadata(hasher: &mut Sha256, path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified()
        && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        hasher.update(duration.as_secs().to_le_bytes());
        hasher.update(duration.subsec_nanos().to_le_bytes());
    }
    Ok(())
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
    for (operation, arity) in [
        ("createElement", 3_usize),
        ("createElements", 3),
        ("createFragment", 1),
    ] {
        let Some(declaration) = runtime_module
            .declarations
            .iter()
            .find(|declaration| declaration.name.as_deref() == Some(operation))
        else {
            diagnostics.push(jsx_runtime_diagnostic(
                "DRIVER_JSX_RUNTIME_MISSING_EXPORT",
                format!("configured JSX runtime does not export `{operation}`"),
                &module_start_span(module),
                "export createElement, createElements, and createFragment from the configured runtime module",
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
    if operation != "createFragment"
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
    if operation == "createFragment"
        && function.parameters.first().is_some_and(|parameter| {
            !matches!(
                parameter.ty,
                Type::Array(_, _) | Type::Slice(_) | Type::Generic(_)
            ) && !type_named_array(program, &parameter.ty)
        })
    {
        diagnostics.push(jsx_runtime_diagnostic(
            "DRIVER_JSX_RUNTIME_CHILDREN_PARAMETER",
            "JSX runtime `createFragment` must accept an array, slice, or inferred children value",
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

fn type_named_array(program: &tn_hir::Program, ty: &Type) -> bool {
    matches!(
        ty,
        Type::Nominal(declaration, _)
            if program
                .graph
                .declaration(*declaration)
                .is_some_and(|declaration| declaration.name.as_deref() == Some("Array"))
    )
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
        Type::Union(alternatives) => alternatives
            .iter()
            .any(|alternative| type_contains_generic(alternative, name)),
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
