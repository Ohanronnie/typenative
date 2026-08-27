use crate::project::SupportMode;
use crate::{Emit, LinkConfig, Profile, Project, ProjectConfig, Sanitizer, Target};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tn_diagnostics::Diagnostic;
use tn_hir::{
    DeclarationId, DeclarationKind, DefinitionData, Namespace, PrimitiveType, Program, Type,
    Visibility,
};
use tn_mir::{Callable, GenericBody, Instance, MonomorphizedBody};

const BUILD_CACHE_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CachedFile {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BuildCache {
    version: u32,
    configuration: String,
    compiler_sha256: String,
    sources: Vec<CachedFile>,
    product: CachedFile,
    companions: Vec<CachedFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOutput {
    pub product: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("compilation failed")]
    Diagnostics(Vec<Diagnostic>),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
struct PhaseTimings {
    enabled: bool,
    entries: Vec<(&'static str, Duration)>,
}

impl PhaseTimings {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            entries: Vec::new(),
        }
    }

    fn record(&mut self, phase: &'static str, started: Instant) {
        if self.enabled {
            self.entries.push((phase, started.elapsed()));
        }
    }

    fn record_duration(&mut self, phase: &'static str, duration: Duration) {
        if self.enabled {
            self.entries.push((phase, duration));
        }
    }

    fn emit(&self) {
        if !self.enabled {
            return;
        }
        for (phase, duration) in &self.entries {
            eprintln!("tn-timing phase={phase} micros={}", duration.as_micros());
        }
    }
}

fn cache_path(product: &Path) -> PathBuf {
    let name = product
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "typenative-product".to_owned(), str::to_owned);
    product.with_file_name(format!(".{name}.tn-cache.json"))
}

fn support_mode_name(mode: SupportMode) -> &'static str {
    match mode {
        SupportMode::None => "none",
        SupportMode::Runtime => "runtime",
        SupportMode::Startup => "startup",
    }
}

fn configuration_fingerprint(project: &Project) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "root": &project.root,
        "entry": &project.entry,
        "configuration": &project.config,
        "supportMode": support_mode_name(project.config.support_mode),
    }))
    .ok()
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(encoded)
}

fn cached_file(path: PathBuf) -> std::io::Result<CachedFile> {
    let sha256 = file_sha256(&path)?;
    Ok(CachedFile { path, sha256 })
}

fn cached_file_is_current(file: &CachedFile) -> bool {
    file_sha256(&file.path).is_ok_and(|digest| digest == file.sha256)
}

fn compiler_sha256() -> std::io::Result<String> {
    file_sha256(&std::env::current_exe()?)
}

fn build_cache_is_current(project: &Project, product: &Path) -> bool {
    let Ok(bytes) = std::fs::read(cache_path(product)) else {
        return false;
    };
    let Ok(cache) = serde_json::from_slice::<BuildCache>(&bytes) else {
        return false;
    };
    cache.version == BUILD_CACHE_VERSION
        && configuration_fingerprint(project).as_deref() == Some(&cache.configuration)
        && compiler_sha256().is_ok_and(|digest| digest == cache.compiler_sha256.as_str())
        && cached_file_is_current(&cache.product)
        && cache.sources.iter().all(cached_file_is_current)
        && cache.companions.iter().all(cached_file_is_current)
}

fn companion_paths(project: &Project, product: &Path) -> Vec<PathBuf> {
    match project.config.emit {
        Emit::SharedLibrary => vec![product.with_extension("h")],
        Emit::NodeAddon => vec![product.with_extension("d.ts")],
        Emit::Executable | Emit::Object | Emit::LlvmIr | Emit::Bitcode | Emit::Assembly => {
            Vec::new()
        }
    }
}

fn write_build_cache(
    project: &Project,
    program: &Program,
    product: &Path,
) -> Result<(), BuildError> {
    let mut source_paths = program
        .graph
        .modules
        .iter()
        .map(|module| module.path.clone())
        .collect::<Vec<_>>();
    source_paths.sort();
    source_paths.dedup();
    let sources = source_paths
        .into_iter()
        .map(cached_file)
        .collect::<Result<Vec<_>, _>>()?;
    let companions = companion_paths(project, product)
        .into_iter()
        .map(cached_file)
        .collect::<Result<Vec<_>, _>>()?;
    let cache = BuildCache {
        version: BUILD_CACHE_VERSION,
        configuration: configuration_fingerprint(project)
            .ok_or_else(|| BuildError::Message("build configuration is not serializable".into()))?,
        compiler_sha256: compiler_sha256()?,
        sources,
        product: cached_file(product.to_path_buf())?,
        companions,
    };
    let path = cache_path(product);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temporary.as_file_mut(), &cache)
        .map_err(|error| BuildError::Message(format!("failed to encode build cache: {error}")))?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Builds the configured `TypeNative` project through verified LLVM emission.
///
/// # Errors
///
/// Returns semantic diagnostics, entry-point errors, backend failures, linker failures, or
/// filesystem errors.
#[allow(clippy::too_many_lines)]
pub fn build_project(project: &Project, output: Option<&Path>) -> Result<BuildOutput, BuildError> {
    build_project_with_timings(project, output, false)
}

/// Builds a project and optionally reports the measured compiler phases.
///
/// # Errors
///
/// Returns the same diagnostics, backend, linker, and filesystem errors as [`build_project`].
#[allow(clippy::too_many_lines)]
pub fn build_project_with_timings(
    project: &Project,
    output: Option<&Path>,
    timings_enabled: bool,
) -> Result<BuildOutput, BuildError> {
    let mut timings = PhaseTimings::new(timings_enabled);
    let product = output.map_or_else(|| default_output(project), Path::to_path_buf);
    if build_cache_is_current(project, &product) {
        timings.record_duration("cache-hit", Duration::ZERO);
        timings.emit();
        return Ok(BuildOutput { product });
    }
    let (program, checked_bodies, ownership, drop_semantics, module_duration, ownership_duration) =
        checked_program(project)?;
    timings.record_duration("module-check", module_duration);
    timings.record_duration("ownership", ownership_duration);
    validate_exports(&program, project.config.emit)?;
    let executable = match project.config.emit {
        Emit::Executable => Some(executable_entry(&program)?),
        Emit::SharedLibrary | Emit::NodeAddon => None,
        Emit::Object | Emit::LlvmIr | Emit::Bitcode | Emit::Assembly => {
            executable_entry(&program).ok()
        }
    };
    let started = Instant::now();
    let mir = tn_typecheck::lower_mir_with_ownership(&program, &checked_bodies.bodies, &ownership);
    let mut diagnostics = Vec::new();
    for body in &mir {
        diagnostics.extend(tn_typecheck::check_ownership(body, &ownership).diagnostics);
    }
    if !diagnostics.is_empty() {
        return Err(BuildError::Diagnostics(diagnostics));
    }
    let generic = mir
        .into_iter()
        .map(|body| GenericBody {
            type_parameters: type_parameters(&program, &body),
            body: tn_mir::lower_typed_errors(&tn_typecheck::elaborate_drops_with_semantics(
                &program,
                &body,
                &drop_semantics,
            )),
        })
        .collect::<Vec<_>>();
    timings.record("mir-drop", started);
    let drop_callables = drop_layouts(&program);
    let root = match project.config.support_mode {
        SupportMode::Startup => support_startup_entry(&program),
        SupportMode::None | SupportMode::Runtime => {
            executable.as_ref().map(|(entry, _)| Instance {
                callable: Callable::function(*entry),
                type_arguments: Vec::new(),
                effects: function_effects(&program, Callable::function(*entry)),
            })
        }
    };
    let entry_instance = root.clone();
    let mut roots = root.iter().cloned().collect::<Vec<_>>();
    roots.extend(decorator_roots(&program));
    roots.extend(drop_roots(&program, &drop_callables));
    match project.config.support_mode {
        SupportMode::Runtime => roots.extend(runtime_support_functions(&program).into_iter().map(
            |(declaration, function)| {
                instance_for_function(Callable::function(declaration.id), function)
            },
        )),
        SupportMode::None if project.config.emit != Emit::Executable => {
            roots.extend(
                entry_exported_functions(&program)
                    .into_iter()
                    .filter(|(_, function)| function.generics.is_empty())
                    .map(|(declaration, function)| {
                        instance_for_function(Callable::function(declaration.id), function)
                    }),
            );
        }
        SupportMode::Startup | SupportMode::None => {}
    }
    if project.config.support_mode == SupportMode::None && project.config.emit == Emit::NodeAddon {
        for (_, function) in entry_exported_functions(&program) {
            for parameter in &function.parameters {
                push_node_drop_roots(&program, &drop_callables, &parameter.ty, &mut roots);
            }
            push_node_drop_roots(&program, &drop_callables, &function.result, &mut roots);
            for effect in &function.effects {
                push_node_drop_roots(
                    &program,
                    &drop_callables,
                    &Type::Nominal(*effect, Vec::new()),
                    &mut roots,
                );
            }
        }
        for (declaration, definition) in entry_exported_classes(&program) {
            let DefinitionData::Class {
                constructor,
                methods,
                ..
            } = &definition.data
            else {
                continue;
            };
            if let Some(drop_callable) = drop_callables.get(&declaration.id).copied()
                && let Some(drop_method) = methods.iter().find(|method| {
                    Some(method.id) == drop_callable.member && method.function.generics.is_empty()
                })
            {
                roots.push(instance_for_function(drop_callable, &drop_method.function));
            }
            if let Some(constructor) = constructor {
                for parameter in &constructor.function.parameters {
                    push_node_drop_roots(&program, &drop_callables, &parameter.ty, &mut roots);
                }
                push_node_drop_roots(
                    &program,
                    &drop_callables,
                    &constructor.function.result,
                    &mut roots,
                );
                for effect in &constructor.function.effects {
                    push_node_drop_roots(
                        &program,
                        &drop_callables,
                        &Type::Nominal(*effect, Vec::new()),
                        &mut roots,
                    );
                }
            }
            for method in methods {
                for parameter in &method.function.parameters {
                    push_node_drop_roots(&program, &drop_callables, &parameter.ty, &mut roots);
                }
                push_node_drop_roots(
                    &program,
                    &drop_callables,
                    &method.function.result,
                    &mut roots,
                );
                for effect in &method.function.effects {
                    push_node_drop_roots(
                        &program,
                        &drop_callables,
                        &Type::Nominal(*effect, Vec::new()),
                        &mut roots,
                    );
                }
            }
            if let Some(constructor) = constructor
                && constructor.visibility == Visibility::Public
                && constructor.function.generics.is_empty()
            {
                roots.push(instance_for_function(
                    Callable {
                        declaration: declaration.id,
                        member: Some(constructor.id),
                    },
                    &constructor.function,
                ));
            }
            roots.extend(
                methods
                    .iter()
                    .filter(|method| {
                        method.visibility == Visibility::Public
                            && method.function.generics.is_empty()
                    })
                    .map(|method| {
                        instance_for_function(
                            Callable {
                                declaration: declaration.id,
                                member: Some(method.id),
                            },
                            &method.function,
                        )
                    }),
            );
        }
    }
    roots.sort();
    roots.dedup();
    let drop_implementations = drop_implementations(&program);
    let started = Instant::now();
    let units = tn_mir::monomorphize_with_drops(&generic, roots, &drop_implementations)
        .map_err(|error| BuildError::Message(error.to_string()))?;
    timings.record("monomorphization", started);
    let mut layouts = layouts(&program, &ownership);
    if let Some(entry) = entry_instance.as_ref() {
        layouts
            .exports
            .insert(entry.callable, tn_codegen_llvm::symbol_for_instance(entry));
    }
    if let Some((entry, mode)) = executable.as_ref() {
        let kind = match mode {
            EntryMode::FallibleVoid => Some(tn_codegen_llvm::AbiWrapperKind::FallibleVoid),
            EntryMode::FallibleInteger => Some(tn_codegen_llvm::AbiWrapperKind::FallibleValue),
            EntryMode::Void | EntryMode::Integer => None,
        };
        if let Some(kind) = kind {
            layouts
                .abi_wrappers
                .insert(Callable::function(*entry), kind);
        }
    }
    if project.config.emit == Emit::NodeAddon {
        for (declaration, function) in entry_exported_functions(&program) {
            if !function.is_async && !function.effects.is_empty() {
                layouts.abi_wrappers.insert(
                    Callable::function(declaration.id),
                    if function.result == Type::Primitive(PrimitiveType::Void) {
                        tn_codegen_llvm::AbiWrapperKind::FallibleVoid
                    } else if node_needs_indirect_abi(&program, &function.result)
                        || function
                            .parameters
                            .iter()
                            .any(|parameter| node_needs_indirect_abi(&program, &parameter.ty))
                    {
                        tn_codegen_llvm::AbiWrapperKind::FallibleIndirect
                    } else {
                        tn_codegen_llvm::AbiWrapperKind::FallibleValue
                    },
                );
            } else if !function.is_async
                && function.effects.is_empty()
                && (node_needs_indirect_abi(&program, &function.result)
                    || function
                        .parameters
                        .iter()
                        .any(|parameter| node_needs_indirect_abi(&program, &parameter.ty)))
            {
                layouts.abi_wrappers.insert(
                    Callable::function(declaration.id),
                    tn_codegen_llvm::AbiWrapperKind::Indirect,
                );
            }
        }
        for (declaration, definition) in entry_exported_classes(&program) {
            let DefinitionData::Class {
                constructor,
                methods,
                ..
            } = &definition.data
            else {
                continue;
            };
            let mut class_methods = methods.iter().collect::<Vec<_>>();
            if let Some(constructor) = constructor {
                class_methods.push(constructor);
            }
            for method in class_methods {
                if method.visibility != Visibility::Public {
                    continue;
                }
                let callable = Callable {
                    declaration: declaration.id,
                    member: Some(method.id),
                };
                if method.function.is_async {
                    continue;
                }
                layouts.exports.insert(
                    callable,
                    tn_codegen_llvm::symbol_for_instance(&Instance {
                        callable,
                        type_arguments: Vec::new(),
                        effects: method.function.effects.clone(),
                    }),
                );
                if !method.function.effects.is_empty() {
                    layouts.abi_wrappers.insert(
                        callable,
                        if method.function.result == Type::Primitive(PrimitiveType::Void) {
                            tn_codegen_llvm::AbiWrapperKind::FallibleVoid
                        } else if node_needs_indirect_abi(&program, &method.function.result)
                            || method
                                .function
                                .parameters
                                .iter()
                                .any(|parameter| node_needs_indirect_abi(&program, &parameter.ty))
                        {
                            tn_codegen_llvm::AbiWrapperKind::FallibleIndirect
                        } else {
                            tn_codegen_llvm::AbiWrapperKind::FallibleValue
                        },
                    );
                } else if node_needs_indirect_abi(&program, &method.function.result)
                    || method
                        .function
                        .parameters
                        .iter()
                        .any(|parameter| node_needs_indirect_abi(&program, &parameter.ty))
                {
                    layouts
                        .abi_wrappers
                        .insert(callable, tn_codegen_llvm::AbiWrapperKind::Indirect);
                }
            }
        }
        for unit in &units {
            if !layouts
                .drops
                .values()
                .any(|callable| *callable == unit.instance.callable)
            {
                continue;
            }
            let symbol = tn_codegen_llvm::symbol_for_instance(&unit.instance);
            if unit.instance.type_arguments.is_empty() {
                layouts.exports.insert(unit.instance.callable, symbol);
            } else {
                layouts
                    .export_instances
                    .insert(unit.instance.clone(), symbol);
            }
        }
    }
    if project.config.emit != Emit::NodeAddon {
        for (declaration, _) in entry_exported_functions(&program) {
            let callable = Callable::function(declaration.id);
            layouts.exports.insert(
                callable,
                program.export_name_for_declaration(declaration.id),
            );
        }
    }
    let target = project.config.target.triple();
    let profile = match project.config.profile {
        Profile::Debug => tn_codegen_llvm::CodegenProfile::Debug,
        Profile::Optimized => tn_codegen_llvm::CodegenProfile::Optimized,
    };
    if let Some(parent) = product.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let started = Instant::now();
    match project.config.emit {
        Emit::Executable => {
            let (_entry, mode) = executable
                .ok_or_else(|| BuildError::Message("executable entry is missing".into()))?;
            emit_executable(
                project,
                &units,
                &layouts,
                entry_instance.as_ref().ok_or_else(|| {
                    BuildError::Message("executable entry instance is missing".into())
                })?,
                mode,
                target,
                profile,
                &product,
            )?;
        }
        Emit::Object | Emit::LlvmIr | Emit::Bitcode | Emit::Assembly => {
            tn_codegen_llvm::emit_program_to_file_with_sanitizers(
                &project.entry.to_string_lossy(),
                &units,
                &layouts,
                target,
                profile,
                emission(project.config.emit),
                &codegen_sanitizers(project),
                &product,
            )
            .map_err(|error| BuildError::Message(error.to_string()))?;
        }
        Emit::SharedLibrary | Emit::NodeAddon => emit_shared_library(
            project,
            &program,
            &units,
            &layouts,
            target,
            profile,
            &product,
            project.config.emit,
        )?,
    }
    if project.config.emit == Emit::SharedLibrary {
        write_c_header(&program, &product.with_extension("h"))?;
    } else if project.config.emit == Emit::NodeAddon {
        write_node_declarations(&program, &product.with_extension("d.ts"))?;
    }
    write_build_cache(project, &program, &product)?;
    timings.record("llvm-link", started);
    timings.emit();
    Ok(BuildOutput { product })
}

fn exported_functions(program: &Program) -> Vec<(&tn_hir::Declaration, &tn_hir::Function)> {
    let mut functions = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let DefinitionData::Function(function) = &definition.data else {
                return None;
            };
            let declaration = program.graph.declaration(definition.declaration)?;
            declaration.exported.then_some((declaration, function))
        })
        .collect::<Vec<_>>();
    functions.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    functions
}

fn is_entry_declaration(program: &Program, declaration: DeclarationId) -> bool {
    program
        .graph
        .module(program.graph.entry)
        .is_some_and(|module| {
            module
                .declarations
                .iter()
                .any(|entry| entry.id == declaration)
        })
}

fn entry_exported_functions(program: &Program) -> Vec<(&tn_hir::Declaration, &tn_hir::Function)> {
    exported_functions(program)
        .into_iter()
        .filter(|(declaration, _)| is_entry_declaration(program, declaration.id))
        .collect()
}

fn runtime_support_functions(program: &Program) -> Vec<(&tn_hir::Declaration, &tn_hir::Function)> {
    exported_functions(program)
        .into_iter()
        .filter(|(declaration, function)| {
            function.generics.is_empty()
                && (program
                    .graph
                    .is_bundled_module(declaration.module, "runtime.tn")
                    || program
                        .graph
                        .is_bundled_module(declaration.module, "platform/linux-x86_64.tn")
                    || program
                        .graph
                        .is_bundled_module(declaration.module, "platform/darwin-arm64.tn"))
        })
        .collect()
}

fn support_startup_entry(program: &Program) -> Option<Instance> {
    let module = program.graph.module(program.graph.entry)?;
    let declaration = module
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("main"))?;
    let definition = program.definition(declaration.id)?;
    let DefinitionData::Function(function) = &definition.data else {
        return None;
    };
    Some(instance_for_function(
        Callable::function(declaration.id),
        function,
    ))
}

fn exported_classes(program: &Program) -> Vec<(&tn_hir::Declaration, &tn_hir::Definition)> {
    let mut classes = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let tn_hir::DefinitionData::Class { .. } = &definition.data else {
                return None;
            };
            let declaration = program.graph.declaration(definition.declaration)?;
            declaration.exported.then_some((declaration, definition))
        })
        .collect::<Vec<_>>();
    classes.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    classes
}

fn entry_exported_classes(program: &Program) -> Vec<(&tn_hir::Declaration, &tn_hir::Definition)> {
    exported_classes(program)
        .into_iter()
        .filter(|(declaration, _)| is_entry_declaration(program, declaration.id))
        .collect()
}

fn instance_for_function(callable: Callable, function: &tn_hir::Function) -> Instance {
    Instance {
        callable,
        type_arguments: Vec::new(),
        effects: function.effects.clone(),
    }
}

fn push_node_drop_roots(
    program: &Program,
    drops: &BTreeMap<DeclarationId, Callable>,
    ty: &Type,
    roots: &mut Vec<Instance>,
) {
    match ty {
        Type::Nominal(declaration, arguments) => {
            if let Some(callable) = drops.get(declaration) {
                roots.push(Instance {
                    callable: *callable,
                    type_arguments: arguments.clone(),
                    effects: function_effects(program, *callable),
                });
            }
            for argument in arguments {
                push_node_drop_roots(program, drops, argument, roots);
            }
        }
        Type::Promise { result, .. }
        | Type::Optional(result)
        | Type::Array(result, _)
        | Type::Slice(result)
        | Type::Reference {
            referent: result, ..
        } => push_node_drop_roots(program, drops, result, roots),
        Type::Tuple(elements) | Type::Template(elements) => {
            for element in elements {
                push_node_drop_roots(program, drops, element, roots);
            }
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::RawPointer { .. }
        | Type::Function(_)
        | Type::DynamicInterface(_, _)
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => {}
    }
}

fn function_effects(program: &Program, callable: Callable) -> Vec<DeclarationId> {
    let Some(definition) = program.definition(callable.declaration) else {
        return Vec::new();
    };
    let function = match (&definition.data, callable.member) {
        (DefinitionData::Function(function), None) => Some(function),
        (
            DefinitionData::Class {
                constructor,
                methods,
                ..
            },
            Some(member),
        ) => constructor
            .iter()
            .chain(methods)
            .find(|method| method.id == member)
            .map(|method| &method.function),
        (DefinitionData::Struct { methods, .. }, Some(member)) => methods
            .iter()
            .find(|method| method.id == member)
            .map(|method| &method.function),
        (
            DefinitionData::Implementation { methods, .. }
            | DefinitionData::Extern { functions: methods },
            Some(member),
        ) => methods
            .iter()
            .find(|method| method.id == member)
            .map(|method| &method.function),
        _ => None,
    };
    function.map_or_else(Vec::new, |function| function.effects.clone())
}

fn write_c_header(program: &Program, path: &Path) -> Result<(), BuildError> {
    let mut output = String::from(
        "#ifndef TYPENATIVE_GENERATED_H\n#define TYPENATIVE_GENERATED_H\n\n#include <stdint.h>\n#include <stdbool.h>\n#include <stddef.h>\n\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n",
    );
    write_c_layouts(program, &mut output)?;
    for (declaration, function) in entry_exported_functions(program) {
        let symbol = program.export_name_for_declaration(declaration.id);
        let parameters = function
            .parameters
            .iter()
            .map(|parameter| c_type(program, &parameter.ty))
            .collect::<Vec<_>>();
        let parameter_list = if parameters.is_empty() {
            "void".to_owned()
        } else {
            parameters.join(", ")
        };
        let _ = writeln!(
            output,
            "{} {}({});",
            c_type(program, &function.result),
            symbol,
            parameter_list
        );
    }
    output.push_str("\n#ifdef __cplusplus\n}\n#endif\n\n#endif\n");
    std::fs::write(path, output)?;
    Ok(())
}

fn write_c_layouts(program: &Program, output: &mut String) -> Result<(), BuildError> {
    let mut definitions = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let declaration = program.graph.declaration(definition.declaration)?;
            let repr_c = program.has_c_layout(declaration.id);
            if !repr_c {
                return None;
            }
            let name = declaration.name.clone()?;
            Some((name, definition))
        })
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, definition) in definitions {
        match &definition.data {
            DefinitionData::Struct { fields, .. } if definition.generics.is_empty() => {
                writeln!(output, "typedef struct {name} {{").map_err(write_header_error)?;
                for field in fields {
                    writeln!(output, "  {} {};", c_type(program, &field.ty), field.name)
                        .map_err(write_header_error)?;
                }
                writeln!(output, "}} {name};\n").map_err(write_header_error)?;
            }
            DefinitionData::Enum { variants, .. } if definition.generics.is_empty() => {
                let c_repr = declaration_has_repr_c(program, definition.declaration);
                if c_repr && variants.iter().all(|variant| variant.fields.is_empty()) {
                    writeln!(output, "typedef enum {name} {{").map_err(write_header_error)?;
                    for (index, variant) in variants.iter().enumerate() {
                        let discriminant = variant.discriminant.unwrap_or(index as i128);
                        writeln!(output, "  {name}_{} = {discriminant},", variant.name)
                            .map_err(write_header_error)?;
                    }
                    writeln!(output, "}} {name};\n").map_err(write_header_error)?;
                } else {
                    writeln!(
                        output,
                        "typedef struct {name} {{ int64_t discriminant; }} {name};\n"
                    )
                    .map_err(write_header_error)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn declaration_has_repr_c(program: &Program, declaration: DeclarationId) -> bool {
    program.has_c_layout(declaration)
}

fn c_type(program: &Program, ty: &Type) -> String {
    match ty {
        Type::Primitive(primitive) => match primitive {
            tn_hir::PrimitiveType::Bool => "bool".into(),
            tn_hir::PrimitiveType::I8 => "int8_t".into(),
            tn_hir::PrimitiveType::I16 => "int16_t".into(),
            tn_hir::PrimitiveType::I32 => "int32_t".into(),
            tn_hir::PrimitiveType::I64 => "int64_t".into(),
            tn_hir::PrimitiveType::I128 => "__int128".into(),
            tn_hir::PrimitiveType::Isize => "intptr_t".into(),
            tn_hir::PrimitiveType::U8 => "uint8_t".into(),
            tn_hir::PrimitiveType::U16 => "uint16_t".into(),
            tn_hir::PrimitiveType::U32 | tn_hir::PrimitiveType::Char => "uint32_t".into(),
            tn_hir::PrimitiveType::U64 => "uint64_t".into(),
            tn_hir::PrimitiveType::U128 => "unsigned __int128".into(),
            tn_hir::PrimitiveType::Usize => "size_t".into(),
            tn_hir::PrimitiveType::F32 => "float".into(),
            tn_hir::PrimitiveType::F64 => "double".into(),
            tn_hir::PrimitiveType::Void | tn_hir::PrimitiveType::Never => "void".into(),
        },
        Type::String | Type::Str => "const char *".into(),
        Type::Reference { referent, .. }
            if matches!(referent.as_ref(), Type::String | Type::Str) =>
        {
            "const char *".into()
        }
        Type::RawPointer { mutable, .. } => if *mutable { "void *" } else { "const void *" }.into(),
        Type::Nominal(declaration, _) => program
            .graph
            .declaration(*declaration)
            .and_then(|declaration| declaration.name.clone())
            .unwrap_or_else(|| "void *".into()),
        _ => "void *".into(),
    }
}

fn write_node_declarations(program: &Program, path: &Path) -> Result<(), BuildError> {
    let output = tn_node_api::generate_declarations(program)
        .map_err(|error| BuildError::Message(error.to_string()))?;
    std::fs::write(path, output)?;
    Ok(())
}

fn validate_exports(program: &Program, emit: Emit) -> Result<(), BuildError> {
    let Some(entry_module) = program.graph.module(program.graph.entry) else {
        return Err(BuildError::Message("entry module is missing".into()));
    };
    let entry_declarations = entry_module
        .declarations
        .iter()
        .map(|declaration| declaration.id)
        .collect::<std::collections::BTreeSet<_>>();
    for definition in &program.definitions {
        let Some(declaration) = program.graph.declaration(definition.declaration) else {
            continue;
        };
        if !declaration.exported || !entry_declarations.contains(&declaration.id) {
            continue;
        }
        let DefinitionData::Function(function) = &definition.data else {
            if emit == Emit::SharedLibrary {
                return Err(BuildError::Message(format!(
                    "exported declarations must be functions for shared-library emission: {}",
                    declaration.name.as_deref().unwrap_or("<anonymous>")
                )));
            }
            if emit == Emit::NodeAddon {
                let DefinitionData::Class {
                    constructor,
                    methods,
                    is_abstract,
                    ..
                } = &definition.data
                else {
                    continue;
                };
                if *is_abstract || !definition.generics.is_empty() {
                    return Err(BuildError::Message(format!(
                        "exported Node class `{}` must be concrete and non-generic",
                        declaration.name.as_deref().unwrap_or("<anonymous>")
                    )));
                }
                if let Some(constructor) = constructor {
                    validate_node_method(program, declaration, constructor, true)?;
                }
                for method in methods {
                    if method.visibility == Visibility::Public {
                        validate_node_method(program, declaration, method, false)?;
                    }
                }
            }
            continue;
        };
        if matches!(emit, Emit::SharedLibrary | Emit::NodeAddon)
            && (!definition.generics.is_empty() || !function.generics.is_empty())
        {
            return Err(BuildError::Message(format!(
                "exported function `{}` must be non-generic",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        if emit == Emit::SharedLibrary && (function.is_async || !function.effects.is_empty()) {
            return Err(BuildError::Message(format!(
                "exported C function `{}` must be synchronous and non-throwing",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        if emit == Emit::NodeAddon && function.is_unsafe {
            return Err(BuildError::Message(format!(
                "Node export `{}` must be safe",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        if emit == Emit::SharedLibrary
            && (!tn_typecheck::is_c_abi_type(program, &function.result)
                || function
                    .parameters
                    .iter()
                    .any(|parameter| !tn_typecheck::is_c_abi_type(program, &parameter.ty)))
        {
            return Err(BuildError::Message(format!(
                "exported C function `{}` uses a type without a C ABI representation",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        if emit == Emit::NodeAddon
            && (!(function.result == Type::Primitive(PrimitiveType::Void)
                || (!matches!(function.result, Type::Reference { .. })
                    && node_compatible(program, &function.result)))
                || function
                    .parameters
                    .iter()
                    .any(|parameter| !node_compatible(program, &parameter.ty)))
        {
            return Err(BuildError::Message(format!(
                "Node export `{}` uses a type without a Node-API mapping",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
    }
    Ok(())
}

fn validate_node_method(
    program: &Program,
    class: &tn_hir::Declaration,
    method: &tn_hir::Method,
    constructor: bool,
) -> Result<(), BuildError> {
    if method.visibility != Visibility::Public
        || method.is_abstract
        || !method.function.generics.is_empty()
        || method.function.is_unsafe
        || method.function.is_async
    {
        return Err(BuildError::Message(format!(
            "Node {} `{}` must be public, safe, non-generic, and synchronous",
            if constructor { "constructor" } else { "method" },
            class.name.as_deref().unwrap_or("<anonymous>")
        )));
    }
    if !node_compatible(program, &method.function.result)
        && method.function.result != Type::Primitive(PrimitiveType::Void)
    {
        return Err(BuildError::Message(format!(
            "Node class `{}` uses an unsupported method result type",
            class.name.as_deref().unwrap_or("<anonymous>")
        )));
    }
    if method
        .function
        .parameters
        .iter()
        .any(|parameter| !node_compatible(program, &parameter.ty))
    {
        return Err(BuildError::Message(format!(
            "Node class `{}` uses an unsupported method parameter type",
            class.name.as_deref().unwrap_or("<anonymous>")
        )));
    }
    if constructor
        && (node_needs_indirect_abi(program, &method.function.result)
            || method
                .function
                .parameters
                .iter()
                .any(|parameter| node_needs_indirect_abi(program, &parameter.ty)))
    {
        return Err(BuildError::Message(format!(
            "Node class `{}` constructor requires scalar boundary types",
            class.name.as_deref().unwrap_or("<anonymous>")
        )));
    }
    if constructor
        && !method.function.effects.is_empty()
        && (node_needs_indirect_abi(program, &method.function.result)
            || method
                .function
                .parameters
                .iter()
                .any(|parameter| node_needs_indirect_abi(program, &parameter.ty)))
    {
        return Err(BuildError::Message(format!(
            "Node class `{}` fallible method requires scalar boundary types",
            class.name.as_deref().unwrap_or("<anonymous>")
        )));
    }
    Ok(())
}

fn node_compatible(program: &Program, ty: &Type) -> bool {
    match ty {
        Type::Primitive(primitive) => {
            !matches!(primitive, PrimitiveType::Void | PrimitiveType::Never)
        }
        Type::String | Type::Str => true,
        Type::Reference { referent, .. } => {
            matches!(referent.as_ref(), Type::Str | Type::String)
                || nominal_is_node_array(program, referent)
        }
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            node_compatible(program, inner)
        }
        Type::Promise { result, .. } => node_compatible(program, result),
        Type::Nominal(declaration, arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            matches!(name, Some("Bytes"))
                || (name == Some("Array")
                    && arguments.len() == 1
                    && node_compatible(program, &arguments[0]))
        }
        _ => false,
    }
}

fn node_needs_indirect_abi(program: &Program, ty: &Type) -> bool {
    match ty {
        Type::Optional(_) | Type::Array(_, _) | Type::Slice(_) => true,
        Type::Reference { referent, .. } => nominal_is_node_array(program, referent),
        Type::Nominal(declaration, _arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            matches!(name, Some("Bytes"))
        }
        _ => false,
    }
}

fn nominal_is_node_array(program: &Program, ty: &Type) -> bool {
    let Type::Nominal(declaration, arguments) = ty else {
        return false;
    };
    arguments.len() == 1
        && program
            .graph
            .declaration(*declaration)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Array")
        && node_compatible(program, &arguments[0])
}

fn checked_program(
    project: &Project,
) -> Result<
    (
        Program,
        tn_typecheck::BodyCheckResult,
        tn_typecheck::OwnershipFacts,
        tn_mir::DropSemantics,
        Duration,
        Duration,
    ),
    BuildError,
> {
    let module_started = Instant::now();
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
    let module_duration = module_started.elapsed();
    let started = Instant::now();
    let ownership = tn_typecheck::derive_ownership_facts(&program);
    let ownership_duration = started.elapsed();
    let signatures = tn_typecheck::check_signatures_with_ownership(&program, &ownership);
    let source = tn_typecheck::check_source_rules(&program);
    let bodies = tn_typecheck::check_bodies_with_ownership(&program, &ownership);
    let statics = tn_typecheck::check_static_requirements(&program, &ownership);
    let mut diagnostics = signatures.diagnostics;
    diagnostics.extend(source.diagnostics);
    diagnostics.extend(bodies.diagnostics.iter().cloned());
    diagnostics.extend(statics.diagnostics);
    if diagnostics.is_empty() {
        let drop_semantics =
            tn_typecheck::derive_drop_semantics_with_ownership(&program, &ownership);
        Ok((
            program,
            bodies,
            ownership,
            drop_semantics,
            module_duration,
            ownership_duration,
        ))
    } else {
        Err(BuildError::Diagnostics(diagnostics))
    }
}

#[derive(Clone, Copy)]
enum EntryMode {
    Void,
    Integer,
    FallibleVoid,
    FallibleInteger,
}

fn executable_entry(program: &Program) -> Result<(tn_hir::DeclarationId, EntryMode), BuildError> {
    let entry_module = program
        .graph
        .module(program.graph.entry)
        .ok_or_else(|| BuildError::Message("entry module is missing".into()))?;
    let declarations = entry_module
        .declarations
        .iter()
        .filter(|declaration| declaration.name.as_deref() == Some("main"))
        .collect::<Vec<_>>();
    if declarations.len() != 1 {
        return Err(BuildError::Message(
            "an executable must define exactly one `main` function in its entry module".into(),
        ));
    }
    let declaration = declarations[0];
    let definition = program
        .definitions
        .iter()
        .find(|definition| definition.declaration == declaration.id)
        .ok_or_else(|| BuildError::Message("entry definition is missing".into()))?;
    let DefinitionData::Function(function) = &definition.data else {
        return Err(BuildError::Message("`main` must be a function".into()));
    };
    if !definition.generics.is_empty()
        || !function.generics.is_empty()
        || !function.parameters.is_empty()
        || function.is_async
        || function.is_unsafe
    {
        return Err(BuildError::Message(
            "`main` must be non-generic, safe, synchronous, and parameterless".into(),
        ));
    }
    let fallible = !function.effects.is_empty();
    let mode = match (&function.result, fallible) {
        (Type::Primitive(PrimitiveType::Void), false) => EntryMode::Void,
        (Type::Primitive(PrimitiveType::I32), false) => EntryMode::Integer,
        (Type::Primitive(PrimitiveType::Void), true) => EntryMode::FallibleVoid,
        (Type::Primitive(PrimitiveType::I32), true) => EntryMode::FallibleInteger,
        _ => {
            return Err(BuildError::Message(
                "`main` must return `void` or `i32`".into(),
            ));
        }
    };
    Ok((declaration.id, mode))
}

fn type_parameters(program: &Program, body: &tn_mir::Body) -> Vec<String> {
    let Some(definition) = program
        .definitions
        .iter()
        .find(|definition| definition.declaration == body.declaration)
    else {
        return Vec::new();
    };
    let mut parameters = definition
        .generics
        .iter()
        .filter(|parameter| parameter.namespace == Namespace::Type)
        .map(|parameter| parameter.name.clone())
        .collect::<Vec<_>>();
    let function = match (&definition.data, body.member) {
        (DefinitionData::Function(function), None) => Some(function),
        (
            DefinitionData::Class {
                constructor,
                methods,
                ..
            },
            Some(member),
        ) => constructor
            .iter()
            .chain(methods)
            .find(|method| method.id == member)
            .map(|method| &method.function),
        (DefinitionData::Struct { methods, .. }, Some(member)) => methods
            .iter()
            .find(|method| method.id == member)
            .map(|method| &method.function),
        (
            DefinitionData::Implementation { methods, .. }
            | DefinitionData::Extern { functions: methods },
            Some(member),
        ) => methods
            .iter()
            .find(|method| method.id == member)
            .map(|method| &method.function),
        _ => None,
    };
    if let Some(function) = function {
        parameters.extend(
            function
                .generics
                .iter()
                .filter(|parameter| parameter.namespace == Namespace::Type)
                .map(|parameter| parameter.name.clone()),
        );
    }
    parameters.sort();
    parameters.dedup();
    parameters
}

#[allow(clippy::too_many_lines)]
fn layouts(
    program: &Program,
    ownership: &tn_typecheck::OwnershipFacts,
) -> tn_codegen_llvm::Layouts {
    let globals = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let DefinitionData::Constant { ty, mutable_static } = &definition.data else {
                return None;
            };
            Some((
                definition.declaration,
                tn_codegen_llvm::GlobalLayout {
                    name: format!("tn_global_{}", definition.declaration.0),
                    ty: ty.clone(),
                    mutable_static: *mutable_static,
                },
            ))
        })
        .collect();
    let nominals = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let kind = match &definition.data {
                DefinitionData::Struct { fields, .. } => tn_codegen_llvm::NominalKind::Struct {
                    fields: fields.iter().map(|field| field.ty.clone()).collect(),
                },
                DefinitionData::Enum { variants, .. } => {
                    let c_repr = program.has_c_layout(definition.declaration);
                    tn_codegen_llvm::NominalKind::Enum {
                        variants: variants
                            .iter()
                            .map(|variant| {
                                variant
                                    .fields
                                    .iter()
                                    .map(|field| field.ty.clone())
                                    .collect()
                            })
                            .collect(),
                        c_repr,
                        discriminants: variants
                            .iter()
                            .enumerate()
                            .map(|(index, variant)| variant.discriminant.unwrap_or(index as i128))
                            .collect(),
                    }
                }
                DefinitionData::Class {
                    base,
                    fields,
                    constructor,
                    ..
                } => tn_codegen_llvm::NominalKind::Class {
                    base: *base,
                    fields: fields.iter().map(|field| field.ty.clone()).collect(),
                    vtable: class_vtable(program, definition.declaration),
                    constructor: constructor.as_ref().map(|constructor| {
                        tn_codegen_llvm::ConstructorLayout {
                            member: constructor.id,
                            function: tn_hir::FunctionType {
                                parameters: constructor
                                    .function
                                    .parameters
                                    .iter()
                                    .map(|parameter| parameter.ty.clone())
                                    .collect(),
                                result: Box::new(Type::Nominal(
                                    definition.declaration,
                                    definition
                                        .generics
                                        .iter()
                                        .filter(|parameter| parameter.namespace == Namespace::Type)
                                        .map(|parameter| Type::Generic(parameter.name.clone()))
                                        .collect(),
                                )),
                                effects: constructor.function.effects.clone(),
                                generics: Vec::new(),
                                is_async: constructor.function.is_async,
                                is_unsafe: constructor.function.is_unsafe,
                            },
                        }
                    }),
                },
                _ => return None,
            };
            Some((
                definition.declaration,
                tn_codegen_llvm::NominalLayout {
                    type_parameters: definition
                        .generics
                        .iter()
                        .filter(|parameter| parameter.namespace == Namespace::Type)
                        .map(|parameter| parameter.name.clone())
                        .collect(),
                    kind,
                },
            ))
        })
        .collect();
    let aliases = program
        .definitions
        .iter()
        .filter_map(|definition| match &definition.data {
            DefinitionData::TypeAlias(ty) => Some((
                definition.declaration,
                tn_codegen_llvm::AliasLayout {
                    parameters: definition
                        .generics
                        .iter()
                        .map(|parameter| parameter.name.clone())
                        .collect(),
                    body: ty.clone(),
                },
            )),
            _ => None,
        })
        .collect();
    tn_codegen_llvm::Layouts {
        globals,
        aliases,
        nominals,
        witnesses: witness_layouts(program),
        interfaces: program
            .definitions
            .iter()
            .filter_map(|definition| match &definition.data {
                DefinitionData::Interface { methods, .. } => Some((
                    definition.declaration,
                    u32::try_from(methods.len()).unwrap_or(u32::MAX),
                )),
                _ => None,
            })
            .collect(),
        interface_names: program
            .definitions
            .iter()
            .filter(|definition| matches!(definition.data, DefinitionData::Interface { .. }))
            .map(|definition| {
                (
                    definition.declaration,
                    program
                        .graph
                        .declaration(definition.declaration)
                        .and_then(|declaration| declaration.name.clone())
                        .unwrap_or_default(),
                )
            })
            .collect(),
        externs: program
            .definitions
            .iter()
            .filter_map(|definition| {
                let DefinitionData::Extern { functions } = &definition.data else {
                    return None;
                };
                Some(functions.iter().map(|method| {
                    (
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                        tn_codegen_llvm::ExternLayout {
                            name: method.name.clone(),
                            function: tn_hir::FunctionType {
                                parameters: method
                                    .function
                                    .parameters
                                    .iter()
                                    .map(|parameter| parameter.ty.clone())
                                    .collect(),
                                result: Box::new(method.function.result.clone()),
                                effects: method.function.effects.clone(),
                                generics: Vec::new(),
                                is_async: method.function.is_async,
                                is_unsafe: method.function.is_unsafe,
                            },
                        },
                    )
                }))
            })
            .flatten()
            .collect(),
        export_instances: std::collections::BTreeMap::new(),
        exports: program
            .definitions
            .iter()
            .filter_map(|definition| {
                let tn_hir::DefinitionData::Function(function) = &definition.data else {
                    return None;
                };
                let declaration = program.graph.declaration(definition.declaration)?;
                if !declaration.exported {
                    return None;
                }
                let name = program.export_name_for_declaration(declaration.id);
                if !definition.generics.is_empty() || !function.generics.is_empty() {
                    return None;
                }
                Some((Callable::function(definition.declaration), name))
            })
            .collect(),
        drops: drop_layouts(program),
        copies: ownership.copy.clone(),
        inlines: inline_callables(program),
        async_functions: async_function_layouts(program),
        abi_wrappers: std::collections::BTreeMap::new(),
        decorators: decorator_layouts(program),
    }
}

fn inline_callables(program: &Program) -> std::collections::BTreeSet<Callable> {
    let _ = program;
    std::collections::BTreeSet::new()
}

fn decorator_layouts(
    program: &Program,
) -> std::collections::BTreeMap<Callable, Vec<tn_codegen_llvm::DecoratorLayout>> {
    let mut layouts: std::collections::BTreeMap<Callable, Vec<tn_codegen_llvm::DecoratorLayout>> =
        std::collections::BTreeMap::new();
    for definition in &program.definitions {
        let module = program
            .graph
            .declaration(definition.declaration)
            .and_then(|declaration| program.graph.module(declaration.module));
        let Some(module) = module else {
            continue;
        };
        let mut add = |callable: Callable,
                       attributes: &[tn_hir::Attribute],
                       signature: &tn_hir::Function,
                       name: String,
                       is_static: bool,
                       is_private: bool| {
            for attribute in attributes {
                let Some((decorator_declaration, decorator)) =
                    resolve_decorator(program, module, &attribute.name)
                else {
                    continue;
                };
                layouts
                    .entry(callable)
                    .or_default()
                    .push(tn_codegen_llvm::DecoratorLayout {
                        decorator: Callable::function(decorator_declaration),
                        signature: function_type(decorator),
                        name: name.clone(),
                        is_static,
                        is_private,
                    });
            }
            let _ = signature;
        };

        let declaration_attributes: &[tn_hir::Attribute] = program
            .graph
            .declaration(definition.declaration)
            .map_or(&[], |declaration| declaration.attributes.as_slice());
        if let DefinitionData::Function(function) = &definition.data {
            add(
                Callable::function(definition.declaration),
                declaration_attributes,
                function,
                program
                    .graph
                    .declaration(definition.declaration)
                    .and_then(|declaration| declaration.name.clone())
                    .unwrap_or_else(|| format!("function_{}", definition.declaration.0)),
                false,
                false,
            );
        }

        let methods = match &definition.data {
            DefinitionData::Struct { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Interface { methods }
            | DefinitionData::Implementation { methods, .. }
            | DefinitionData::Class { methods, .. } => methods.as_slice(),
            _ => &[],
        };
        for method in methods {
            if method.attributes.is_empty() {
                continue;
            }
            add(
                Callable {
                    declaration: definition.declaration,
                    member: Some(method.id),
                },
                &method.attributes,
                &method.function,
                method.name.clone(),
                method.receiver == tn_hir::ReceiverMode::Static,
                method.visibility == Visibility::Private,
            );
        }
        if let DefinitionData::Class {
            constructor: Some(constructor),
            ..
        } = &definition.data
            && !constructor.attributes.is_empty()
        {
            add(
                Callable {
                    declaration: definition.declaration,
                    member: Some(constructor.id),
                },
                &constructor.attributes,
                &constructor.function,
                "constructor".to_owned(),
                constructor.receiver == tn_hir::ReceiverMode::Static,
                constructor.visibility == Visibility::Private,
            );
        }
    }
    layouts
}

fn decorator_roots(program: &Program) -> Vec<Instance> {
    let layouts = decorator_layouts(program);
    let mut roots = layouts
        .values()
        .flat_map(|decorators| decorators.iter())
        .filter_map(|decorator| {
            // The canonical unknown-identity decorator is intentionally a source-level marker;
            // it has no first-class LLVM representation.  Typed method decorators do have a
            // concrete callable contract and therefore need their bodies rooted for codegen.
            if decorator
                .signature
                .parameters
                .first()
                .is_none_or(|ty| matches!(ty, Type::Unknown))
            {
                return None;
            }
            let definition = program.definition(decorator.decorator.declaration)?;
            let DefinitionData::Function(function) = &definition.data else {
                return None;
            };
            if !function.generics.is_empty() {
                return None;
            }
            Some(instance_for_function(decorator.decorator, function))
        })
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

fn resolve_decorator<'a>(
    program: &'a Program,
    module: &'a tn_hir::Module,
    name: &str,
) -> Option<(DeclarationId, &'a tn_hir::Function)> {
    if let Some(declaration) = module.declarations.iter().find(|declaration| {
        declaration.kind == DeclarationKind::Function && declaration.name.as_deref() == Some(name)
    }) {
        return program
            .definition(declaration.id)
            .and_then(|definition| match &definition.data {
                DefinitionData::Function(function) => Some((declaration.id, function)),
                _ => None,
            });
    }
    module.imports.iter().find_map(|import| {
        let tn_hir::ImportClause::Named(names) = &import.clause else {
            return None;
        };
        let imported = names.iter().find(|item| item.local == name)?;
        let target = program.graph.module(import.target)?;
        let declaration = target.declarations.iter().find(|declaration| {
            declaration.kind == DeclarationKind::Function
                && declaration.exported
                && declaration.name.as_deref() == Some(imported.imported.as_str())
        })?;
        let definition = program.definition(declaration.id)?;
        match &definition.data {
            DefinitionData::Function(function) => Some((declaration.id, function)),
            _ => None,
        }
    })
}

fn async_function_layouts(
    program: &Program,
) -> std::collections::BTreeMap<Callable, tn_hir::FunctionType> {
    let mut functions = std::collections::BTreeMap::new();
    for definition in &program.definitions {
        match &definition.data {
            DefinitionData::Function(function) if function.is_async && !function.is_generator => {
                functions.insert(
                    Callable::function(definition.declaration),
                    function_type(function),
                );
            }
            DefinitionData::Class {
                constructor,
                methods,
                ..
            } => {
                if let Some(method) = constructor
                    .as_ref()
                    .filter(|method| method.function.is_async && !method.function.is_generator)
                {
                    functions.insert(
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                        function_type(&method.function),
                    );
                }
                for method in methods
                    .iter()
                    .filter(|method| method.function.is_async && !method.function.is_generator)
                {
                    functions.insert(
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                        function_type(&method.function),
                    );
                }
            }
            DefinitionData::Implementation { methods, .. } => {
                for method in methods
                    .iter()
                    .filter(|method| method.function.is_async && !method.function.is_generator)
                {
                    functions.insert(
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                        function_type(&method.function),
                    );
                }
            }
            DefinitionData::Struct { methods, .. } | DefinitionData::Enum { methods, .. } => {
                for method in methods
                    .iter()
                    .filter(|method| method.function.is_async && !method.function.is_generator)
                {
                    functions.insert(
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                        function_type(&method.function),
                    );
                }
            }
            _ => {}
        }
    }
    functions
}

fn function_type(function: &tn_hir::Function) -> tn_hir::FunctionType {
    tn_hir::FunctionType {
        parameters: function
            .parameters
            .iter()
            .map(|parameter| parameter.ty.clone())
            .collect(),
        result: Box::new(function.result.clone()),
        effects: function.effects.clone(),
        generics: function
            .generics
            .iter()
            .map(|parameter| tn_hir::GenericConstraint {
                name: parameter.name.clone(),
                namespace: parameter.namespace,
                bounds: parameter
                    .bounds
                    .iter()
                    .map(|bound| match bound {
                        tn_hir::GenericBound::Interface(id, args) => {
                            tn_hir::GenericBound::Interface(*id, args.clone())
                        }
                        tn_hir::GenericBound::Static => tn_hir::GenericBound::Static,
                        tn_hir::GenericBound::Outlives(name) => {
                            tn_hir::GenericBound::Outlives(name.clone())
                        }
                    })
                    .collect(),
            })
            .collect(),
        is_async: function.is_async && !function.is_generator,
        is_unsafe: function.is_unsafe,
    }
}

fn drop_layouts(program: &Program) -> std::collections::BTreeMap<DeclarationId, Callable> {
    let mut drops = std::collections::BTreeMap::new();
    for definition in &program.definitions {
        match &definition.data {
            DefinitionData::Implementation {
                interface: Some(Type::Nominal(interface, _)),
                target: Type::Nominal(target, _),
                methods,
                ..
            } => {
                let is_disposable = program
                    .graph
                    .declaration(*interface)
                    .and_then(|declaration| declaration.name.as_deref())
                    == Some("Disposable");
                if is_disposable
                    && let Some(method) = methods
                        .iter()
                        .find(|method| method.name == "[Symbol.dispose]")
                {
                    drops.insert(
                        *target,
                        Callable {
                            declaration: definition.declaration,
                            member: Some(method.id),
                        },
                    );
                }
            }
            DefinitionData::Struct { methods, .. } | DefinitionData::Class { methods, .. }
                if has_dispose_method(program, definition.declaration)
                    && let Some(method) = methods
                        .iter()
                        .find(|method| method.name == "[Symbol.dispose]") =>
            {
                drops.entry(definition.declaration).or_insert(Callable {
                    declaration: definition.declaration,
                    member: Some(method.id),
                });
            }
            _ => {}
        }
    }
    drops
}

fn drop_roots(
    program: &Program,
    drops: &std::collections::BTreeMap<DeclarationId, Callable>,
) -> Vec<Instance> {
    drops
        .iter()
        .filter_map(|(declaration, callable)| {
            let definition = program.definition(*declaration)?;
            if !definition.generics.is_empty() {
                return None;
            }
            let (DefinitionData::Struct { methods, .. } | DefinitionData::Class { methods, .. }) =
                &definition.data
            else {
                return None;
            };
            let method = methods
                .iter()
                .find(|method| Some(method.id) == callable.member)?;
            method
                .function
                .generics
                .is_empty()
                .then(|| instance_for_function(*callable, &method.function))
        })
        .collect()
}

fn drop_implementations(program: &Program) -> Vec<tn_mir::DropImplementation> {
    let mut implementations = Vec::new();
    for definition in &program.definitions {
        let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, _)),
            target,
            methods,
            ..
        } = &definition.data
        else {
            continue;
        };
        let is_disposable = program
            .graph
            .declaration(*interface)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Disposable");
        if !is_disposable {
            continue;
        }
        let Some(method) = methods
            .iter()
            .find(|method| method.name == "[Symbol.dispose]")
        else {
            continue;
        };
        implementations.push(tn_mir::DropImplementation {
            target: target.clone(),
            callable: Callable {
                declaration: definition.declaration,
                member: Some(method.id),
            },
        });
    }
    for definition in &program.definitions {
        let (methods, target) = match &definition.data {
            DefinitionData::Struct { methods, .. } | DefinitionData::Class { methods, .. }
                if has_dispose_method(program, definition.declaration) =>
            {
                (
                    methods,
                    Type::Nominal(
                        definition.declaration,
                        definition
                            .generics
                            .iter()
                            .filter(|parameter| parameter.namespace == Namespace::Type)
                            .map(|parameter| Type::Generic(parameter.name.clone()))
                            .collect(),
                    ),
                )
            }
            _ => continue,
        };
        let Some(method) = methods
            .iter()
            .find(|method| method.name == "[Symbol.dispose]")
        else {
            continue;
        };
        implementations.push(tn_mir::DropImplementation {
            target,
            callable: Callable {
                declaration: definition.declaration,
                member: Some(method.id),
            },
        });
    }
    implementations.sort_by_key(|implementation| implementation.callable);
    implementations
}

fn has_dispose_method(program: &Program, declaration: DeclarationId) -> bool {
    program.definition(declaration).is_some_and(|definition| {
        matches!(
            &definition.data,
            DefinitionData::Struct { methods, .. } | DefinitionData::Class { methods, .. }
                if methods.iter().any(|method| method.name == "[Symbol.dispose]")
        )
    })
}

fn class_vtable(
    program: &Program,
    declaration: DeclarationId,
) -> Vec<tn_codegen_llvm::VtableEntry> {
    let Some(DefinitionData::Class { base, methods, .. }) = program
        .definition(declaration)
        .map(|definition| &definition.data)
    else {
        return Vec::new();
    };
    let mut entries = base.map_or_else(Vec::new, |base| class_vtable(program, base));
    for method in methods {
        if let Some(entry) = entries.iter_mut().find(|entry| entry.name == method.name) {
            entry.owner = declaration;
            entry.member = method.id;
        } else {
            entries.push(tn_codegen_llvm::VtableEntry {
                name: method.name.clone(),
                owner: declaration,
                member: method.id,
            });
        }
    }
    entries
}

#[allow(clippy::too_many_lines)]
fn witness_layouts(
    program: &Program,
) -> std::collections::BTreeMap<(DeclarationId, DeclarationId), Vec<tn_codegen_llvm::VtableEntry>> {
    let mut witnesses = std::collections::BTreeMap::new();
    for definition in &program.definitions {
        match &definition.data {
            DefinitionData::Implementation {
                interface: Some(Type::Nominal(interface, _)),
                target: Type::Nominal(target, _),
                methods,
                ..
            } => {
                let Some(DefinitionData::Interface {
                    methods: interface_methods,
                    ..
                }) = program
                    .definition(*interface)
                    .map(|definition| &definition.data)
                else {
                    continue;
                };
                let entries = interface_methods
                    .iter()
                    .filter_map(|interface_method| {
                        methods
                            .iter()
                            .find(|method| method.name == interface_method.name)
                            .map(|method| tn_codegen_llvm::VtableEntry {
                                name: interface_method.name.clone(),
                                owner: definition.declaration,
                                member: method.id,
                            })
                    })
                    .collect::<Vec<_>>();
                witnesses.insert((*interface, *target), entries);
            }
            DefinitionData::Struct { methods, .. } => {
                let interface_ids = program.implemented_interfaces(definition.declaration);
                for interface in interface_ids {
                    let Some(DefinitionData::Interface {
                        methods: interface_methods,
                        ..
                    }) = program
                        .definition(interface)
                        .map(|definition| &definition.data)
                    else {
                        continue;
                    };
                    let entries = interface_methods
                        .iter()
                        .filter_map(|interface_method| {
                            methods
                                .iter()
                                .find(|method| method.name == interface_method.name)
                                .map(|method| tn_codegen_llvm::VtableEntry {
                                    name: interface_method.name.clone(),
                                    owner: definition.declaration,
                                    member: method.id,
                                })
                        })
                        .collect::<Vec<_>>();
                    witnesses.insert((interface, definition.declaration), entries);
                }
            }
            DefinitionData::Class { .. } => {
                let interface_ids = program.implemented_interfaces(definition.declaration);
                for interface in interface_ids {
                    let Some(DefinitionData::Interface {
                        methods: interface_methods,
                        ..
                    }) = program
                        .definition(interface)
                        .map(|definition| &definition.data)
                    else {
                        continue;
                    };
                    let class_methods = class_vtable(program, definition.declaration);
                    let entries = interface_methods
                        .iter()
                        .filter_map(|interface_method| {
                            class_methods
                                .iter()
                                .find(|method| method.name == interface_method.name)
                                .cloned()
                        })
                        .collect::<Vec<_>>();
                    witnesses.insert((interface, definition.declaration), entries);
                }
            }
            _ => {}
        }
    }
    witnesses
}

#[allow(clippy::too_many_arguments)]
fn emit_executable(
    project: &Project,
    units: &[MonomorphizedBody],
    layouts: &tn_codegen_llvm::Layouts,
    entry: &Instance,
    mode: EntryMode,
    target: &str,
    profile: tn_codegen_llvm::CodegenProfile,
    output: &Path,
) -> Result<(), BuildError> {
    let temporary = tempfile::tempdir()?;
    let object = temporary.path().join("program.o");
    tn_codegen_llvm::emit_program_to_file_with_sanitizers(
        &project.entry.to_string_lossy(),
        units,
        layouts,
        target,
        profile,
        tn_codegen_llvm::Emission::Object,
        &codegen_sanitizers(project),
        &object,
    )
    .map_err(|error| BuildError::Message(error.to_string()))?;
    let runtime_support = temporary.path().join("runtime.o");
    compile_support_object(
        project,
        runtime_source(project.config.target),
        &runtime_support,
    )?;
    let startup_source = temporary.path().join("startup.tn");
    std::fs::write(
        &startup_source,
        startup_source_text(&tn_codegen_llvm::symbol_for_instance(entry), mode),
    )?;
    let startup_object = temporary.path().join("startup.o");
    compile_support_object(project, startup_source, &startup_object)?;
    let mut linker = native_linker();
    linker
        .arg(&object)
        .arg(runtime_support)
        .arg(startup_object)
        .arg("-pthread")
        .arg("-o")
        .arg(output)
        .arg(format!(
            "-DTN_ENTRY={}",
            tn_codegen_llvm::symbol_for_instance(entry)
        ));
    if !project.config.target.is_macos() {
        linker.arg("-ldl");
    }
    linker.arg(match mode {
        EntryMode::Void => "-DTN_ENTRY_VOID",
        EntryMode::Integer => "-DTN_ENTRY_I32",
        EntryMode::FallibleVoid => "-DTN_ENTRY_FALLIBLE_VOID",
        EntryMode::FallibleInteger => "-DTN_ENTRY_FALLIBLE_I32",
    });
    for search in &project.config.link.search_paths {
        linker.arg("-L").arg(search);
    }
    for library in &project.config.link.libraries {
        linker.arg(format!("-l{library}"));
    }
    linker.args(&project.config.link.arguments);
    append_sanitizer_link_arguments(&mut linker, &project.config.sanitizers);
    let result = linker.output()?;
    if !result.status.success() {
        return Err(BuildError::Message(format!(
            "native linker failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    if profile == tn_codegen_llvm::CodegenProfile::Optimized && project.config.target.is_macos() {
        let result = Command::new("strip").arg("-x").arg(output).output()?;
        if !result.status.success() {
            return Err(BuildError::Message(format!(
                "optimized symbol stripping failed:\n{}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
    }
    if profile == tn_codegen_llvm::CodegenProfile::Debug && project.config.target.is_macos() {
        let debug_bundle = output.with_extension("dSYM");
        let result = Command::new("dsymutil")
            .arg(output)
            .arg("-o")
            .arg(&debug_bundle)
            .output()?;
        if !result.status.success() {
            return Err(BuildError::Message(format!(
                "debug symbol generation failed:\n{}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_shared_library(
    project: &Project,
    program: &Program,
    units: &[MonomorphizedBody],
    layouts: &tn_codegen_llvm::Layouts,
    target: &str,
    profile: tn_codegen_llvm::CodegenProfile,
    output: &Path,
    emit: Emit,
) -> Result<(), BuildError> {
    let temporary = tempfile::tempdir()?;
    let object = temporary.path().join("program.o");
    tn_codegen_llvm::emit_program_to_file_with_sanitizers(
        &project.entry.to_string_lossy(),
        units,
        layouts,
        target,
        profile,
        tn_codegen_llvm::Emission::Object,
        &codegen_sanitizers(project),
        &object,
    )
    .map_err(|error| BuildError::Message(error.to_string()))?;

    let runtime = temporary.path().join("runtime.o");
    compile_support_object(project, runtime_source(project.config.target), &runtime)?;
    let mut linker = native_linker();
    linker.arg(&object).arg("-pthread");
    let wrapper_object = if emit == Emit::NodeAddon {
        let plan = tn_node_api::build_bridge_plan(program)
            .map_err(|error| BuildError::Message(error.to_string()))?;
        let bridge = temporary.path().join("node_bridge.o");
        tn_codegen_llvm::emit_node_bridge_to_file_with_sanitizers(
            &project.entry.to_string_lossy(),
            &plan,
            layouts,
            target,
            profile,
            &codegen_sanitizers(project),
            &bridge,
        )
        .map_err(|error| BuildError::Message(error.to_string()))?;
        Some(bridge)
    } else {
        None
    };
    if let Some(wrapper_object) = &wrapper_object {
        linker.arg(wrapper_object);
    }
    linker.arg(runtime);
    if !project.config.target.is_macos() {
        linker.arg("-ldl");
    }
    match emit {
        Emit::SharedLibrary => {
            if project.config.target.is_macos() {
                linker.arg("-dynamiclib");
            } else {
                linker.arg("-shared");
            }
        }
        Emit::NodeAddon => {
            if project.config.target.is_macos() {
                linker
                    .arg("-bundle")
                    .arg("-undefined")
                    .arg("dynamic_lookup");
            } else {
                linker.arg("-shared");
            }
        }
        _ => unreachable!("shared-library emission was selected by the caller"),
    }
    linker.arg("-o").arg(output);
    for search in &project.config.link.search_paths {
        linker.arg("-L").arg(search);
    }
    for library in &project.config.link.libraries {
        linker.arg(format!("-l{library}"));
    }
    linker.args(&project.config.link.arguments);
    append_sanitizer_link_arguments(&mut linker, &project.config.sanitizers);
    let result = linker.output()?;
    if !result.status.success() {
        return Err(BuildError::Message(format!(
            "native linker failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(())
}

fn runtime_source(target: Target) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime/platform")
        .join(target.runtime_module())
}

fn compile_support_object(
    project: &Project,
    source: PathBuf,
    output: &Path,
) -> Result<(), BuildError> {
    let root = source.parent().unwrap_or(Path::new(".")).to_path_buf();
    let support_mode = if source == runtime_source(project.config.target) {
        SupportMode::Runtime
    } else {
        SupportMode::Startup
    };
    let support = Project {
        root,
        entry: source.clone(),
        config: ProjectConfig {
            entry: source,
            out_dir: PathBuf::from("build"),
            target: project.config.target,
            profile: project.config.profile,
            emit: Emit::Object,
            sanitizers: project.config.sanitizers.clone(),
            link: LinkConfig::default(),
            support_mode,
        },
        config_path: None,
    };
    build_project_with_timings(&support, Some(output), false).map(|_| ())
}

fn codegen_sanitizers(project: &Project) -> Vec<tn_codegen_llvm::Sanitizer> {
    project
        .config
        .sanitizers
        .iter()
        .copied()
        .map(Sanitizer::codegen)
        .collect()
}

fn append_sanitizer_link_arguments(linker: &mut Command, sanitizers: &[Sanitizer]) {
    for sanitizer in sanitizers {
        linker.arg(sanitizer.link_argument());
    }
}

fn native_linker() -> Command {
    Command::new(llvm_clang_path())
}

fn llvm_clang_path() -> PathBuf {
    let prefixes = [
        std::env::var_os("LLVM_SYS_221_PREFIX").map(PathBuf::from),
        Some(PathBuf::from("/opt/homebrew/opt/llvm")),
        Some(PathBuf::from("/usr/local/opt/llvm")),
    ];
    prefixes
        .into_iter()
        .flatten()
        .map(|prefix| prefix.join("bin/clang"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("clang"))
}

fn startup_source_text(entry: &str, mode: EntryMode) -> String {
    let preamble = format!(
        "declare extern \"C\" {{\n  function tn_process_set_args(argc: i32, argv: * mut u8): void;\n  function {entry}"
    );
    match mode {
        EntryMode::Void => format!(
            "{preamble}(): void;\n}}\nexport unsafe function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    {entry}();\n  }}\n  return 0i32;\n}}\n"
        ),
        EntryMode::Integer => format!(
            "{preamble}(): i32;\n}}\nexport unsafe function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    return {entry}();\n  }}\n}}\n"
        ),
        EntryMode::FallibleVoid | EntryMode::FallibleInteger => format!(
            "{preamble}(): EntryResult;\n  function tn_runtime_free(pointer: * mut u8): void;\n}}\nextern struct EntryResult {{\n  public failed: u64;\n  public payload: u64;\n}}\nexport unsafe function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    let result = {entry}();\n    if (result.failed !== 0u64) {{\n      const errorField = & mut result.payload;\n      tn_runtime_free(*(errorField as * mut * mut u8));\n      return 1i32;\n    }}\n    {}\n  }}\n}}\n",
            if matches!(mode, EntryMode::FallibleInteger) {
                "const valueField = & mut result.payload;\n    return *(valueField as * mut i32);"
            } else {
                "return 0i32;"
            }
        ),
    }
}

fn write_header_error(_: std::fmt::Error) -> BuildError {
    BuildError::Message("failed to render native header".into())
}

fn emission(emit: Emit) -> tn_codegen_llvm::Emission {
    match emit {
        Emit::Object => tn_codegen_llvm::Emission::Object,
        Emit::LlvmIr => tn_codegen_llvm::Emission::LlvmIr,
        Emit::Bitcode => tn_codegen_llvm::Emission::Bitcode,
        Emit::Assembly => tn_codegen_llvm::Emission::Assembly,
        _ => unreachable!("emission filtered by caller"),
    }
}

fn default_output(project: &Project) -> PathBuf {
    let name = project
        .entry
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("program");
    let extension = match project.config.emit {
        Emit::Executable => "",
        Emit::Object => ".o",
        Emit::LlvmIr => ".ll",
        Emit::Bitcode => ".bc",
        Emit::Assembly => ".s",
        Emit::SharedLibrary => {
            if project.config.target.is_macos() {
                ".dylib"
            } else {
                ".so"
            }
        }
        Emit::NodeAddon => ".node",
    };
    project
        .root
        .join(&project.config.out_dir)
        .join(format!("{name}{extension}"))
}
