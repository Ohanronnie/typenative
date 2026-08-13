use crate::{Emit, LinkConfig, Profile, Project, ProjectConfig};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tn_diagnostics::Diagnostic;
use tn_hir::{
    DeclarationId, DefinitionData, ImportClause, Namespace, PrimitiveType, Program, Type,
    Visibility,
};
use tn_mir::{Callable, GenericBody, Instance, MonomorphizedBody};

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
    let root = executable.as_ref().map(|(entry, _)| Instance {
        callable: Callable::function(*entry),
        type_arguments: Vec::new(),
        effects: function_effects(&program, Callable::function(*entry)),
    });
    let entry_instance = root.clone();
    let mut roots = root.iter().cloned().collect::<Vec<_>>();
    if project.config.emit != Emit::Executable {
        roots.extend(
            exported_functions(&program)
                .into_iter()
                .filter(|(_, function)| function.generics.is_empty())
                .map(|(declaration, function)| {
                    instance_for_function(Callable::function(declaration.id), function)
                }),
        );
        if project.config.emit == Emit::NodeAddon {
            for (declaration, definition) in exported_classes(&program) {
                let DefinitionData::Class {
                    constructor,
                    methods,
                    ..
                } = &definition.data
                else {
                    continue;
                };
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
        for (declaration, function) in exported_functions(&program) {
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
        for (declaration, definition) in exported_classes(&program) {
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
        for callable in layouts.drops.values().copied().collect::<Vec<_>>() {
            layouts.exports.insert(
                callable,
                tn_codegen_llvm::symbol_for_instance(&Instance {
                    callable,
                    type_arguments: Vec::new(),
                    effects: function_effects(&program, callable),
                }),
            );
        }
    }
    if project.config.emit != Emit::NodeAddon {
        for (declaration, _) in exported_functions(&program) {
            let callable = Callable::function(declaration.id);
            layouts.exports.insert(callable, exported_name(declaration));
        }
    }
    let target = project.config.target.triple();
    let profile = match project.config.profile {
        Profile::Debug => tn_codegen_llvm::CodegenProfile::Debug,
        Profile::Optimized => tn_codegen_llvm::CodegenProfile::Optimized,
    };
    let product = output.map_or_else(|| default_output(project), Path::to_path_buf);
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
            tn_codegen_llvm::emit_program_to_file(
                &project.entry.to_string_lossy(),
                &units,
                &layouts,
                target,
                profile,
                emission(project.config.emit),
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
            declaration
                .attributes
                .iter()
                .any(|attribute| attribute.name == "Export")
                .then_some((declaration, function))
        })
        .collect::<Vec<_>>();
    functions.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    functions
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
            declaration
                .attributes
                .iter()
                .any(|attribute| attribute.name == "Export")
                .then_some((declaration, definition))
        })
        .collect::<Vec<_>>();
    classes.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    classes
}

fn exported_name(declaration: &tn_hir::Declaration) -> String {
    declaration
        .attributes
        .iter()
        .find(|attribute| attribute.name == "Export")
        .and_then(|attribute| attribute.arguments.first())
        .cloned()
        .or_else(|| declaration.name.clone())
        .unwrap_or_else(|| "exported".into())
}

fn instance_for_function(callable: Callable, function: &tn_hir::Function) -> Instance {
    Instance {
        callable,
        type_arguments: Vec::new(),
        effects: function.effects.clone(),
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
    for (declaration, function) in exported_functions(program) {
        let symbol = declaration
            .attributes
            .iter()
            .find(|attribute| attribute.name == "Export")
            .and_then(|attribute| attribute.arguments.first())
            .cloned()
            .or_else(|| declaration.name.clone())
            .unwrap_or_else(|| "tn_export".into());
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
            let repr_c = declaration.attributes.iter().any(|attribute| {
                attribute.name == "Layout"
                    && attribute
                        .arguments
                        .first()
                        .is_some_and(|argument| argument == "C")
            });
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
                writeln!(output, "typedef struct {name} {{").map_err(write_error)?;
                for field in fields {
                    writeln!(output, "  {} {};", c_type(program, &field.ty), field.name)
                        .map_err(write_error)?;
                }
                writeln!(output, "}} {name};\n").map_err(write_error)?;
            }
            DefinitionData::Enum { variants } if definition.generics.is_empty() => {
                let c_repr = declaration_has_repr_c(program, definition.declaration);
                if c_repr && variants.iter().all(|variant| variant.fields.is_empty()) {
                    writeln!(output, "typedef enum {name} {{").map_err(write_error)?;
                    for (index, variant) in variants.iter().enumerate() {
                        let discriminant = variant.discriminant.unwrap_or(index as i128);
                        writeln!(output, "  {name}_{} = {discriminant},", variant.name)
                            .map_err(write_error)?;
                    }
                    writeln!(output, "}} {name};\n").map_err(write_error)?;
                } else {
                    writeln!(
                        output,
                        "typedef struct {name} {{ int64_t discriminant; }} {name};\n"
                    )
                    .map_err(write_error)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn declaration_has_repr_c(program: &Program, declaration: DeclarationId) -> bool {
    program
        .graph
        .declaration(declaration)
        .is_some_and(|declaration| {
            declaration.attributes.iter().any(|attribute| {
                attribute.name == "Layout"
                    && attribute
                        .arguments
                        .first()
                        .is_some_and(|argument| argument == "C")
            })
        })
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

fn node_type_key(program: &Program, ty: &Type) -> String {
    match ty {
        Type::Primitive(primitive) => format!("p_{primitive:?}").to_lowercase(),
        Type::String => "string".into(),
        Type::Str => "str".into(),
        Type::Reference { referent, .. } => format!("ref_{}", node_type_key(program, referent)),
        Type::Optional(inner) => format!("optional_{}", node_type_key(program, inner)),
        Type::Array(inner, length) => format!("array_{}_{}", node_type_key(program, inner), length),
        Type::Slice(inner) => format!("slice_{}", node_type_key(program, inner)),
        Type::Nominal(declaration, arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref())
                .unwrap_or("nominal")
                .to_lowercase();
            if arguments.is_empty() {
                name
            } else {
                format!(
                    "{}_{}",
                    name,
                    arguments
                        .iter()
                        .map(|argument| node_type_key(program, argument))
                        .collect::<Vec<_>>()
                        .join("_")
                )
            }
        }
        Type::Promise { result, .. } => format!("promise_{}", node_type_key(program, result)),
        _ => "unsupported".into(),
    }
}

fn node_c_type(program: &Program, ty: &Type) -> String {
    match ty {
        Type::Optional(_) | Type::Array(_, _) | Type::Slice(_) => {
            format!("tn_node_{}", node_type_key(program, ty))
        }
        Type::Nominal(declaration, arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            match (name, arguments.len()) {
                (Some("Bytes"), 0) => "tn_node_bytes".into(),
                (Some("Array"), 1) => "void *".into(),
                _ => c_type(program, ty),
            }
        }
        _ => c_type(program, ty),
    }
}

fn collect_node_compound_types(program: &Program, ty: &Type, types: &mut BTreeSet<String>) {
    let name = match ty {
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            collect_node_compound_types(program, inner, types);
            Some(format!("tn_node_{}", node_type_key(program, ty)))
        }
        Type::Reference { referent, .. } if nominal_is_node_array(program, referent) => {
            collect_node_compound_types(program, referent, types);
            Some("tn_node_array".into())
        }
        Type::Nominal(declaration, arguments) => {
            for argument in arguments {
                collect_node_compound_types(program, argument, types);
            }
            let declaration_name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            match declaration_name {
                Some("Bytes") if arguments.is_empty() => Some("tn_node_bytes".into()),
                Some("Array") if arguments.len() == 1 => Some("tn_node_array".into()),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(name) = name {
        types.insert(name);
    }
}

fn write_node_type_definitions(
    program: &Program,
    output: &mut String,
    values: &[Type],
) -> Result<(), BuildError> {
    let mut names = BTreeSet::new();
    for value in values {
        collect_node_compound_types(program, value, &mut names);
    }
    for name in &names {
        if name == "tn_node_bytes" {
            output.push_str(
                "typedef struct { const uint8_t *pointer; size_t length; } tn_node_bytes;\n",
            );
        } else if name == "tn_node_vec" {
            output.push_str(
                "typedef struct { void *pointer; size_t length; size_t capacity; size_t elementSize; } tn_node_vec;\n",
            );
        } else if name == "tn_node_array" {
            output.push_str(
                "typedef struct { void *descriptor; void *pointer; void *initialized; size_t length; size_t capacity; size_t elementSize; } tn_node_array;\n",
            );
        } else if let Some(key) = name.strip_prefix("tn_node_optional_") {
            let inner = type_from_node_key(program, key).unwrap_or_else(|| "void *".into());
            writeln!(
                output,
                "typedef struct {{ bool present; {inner} value; }} {name};"
            )
            .map_err(write_error)?;
        } else if let Some(key) = name.strip_prefix("tn_node_slice_") {
            let inner = type_from_node_key(program, key).unwrap_or_else(|| "uint8_t".into());
            writeln!(
                output,
                "typedef struct {{ {inner} *pointer; size_t length; }} {name};"
            )
            .map_err(write_error)?;
        } else if let Some(key) = name.strip_prefix("tn_node_array_")
            && let Some((element, length)) = key.rsplit_once('_')
        {
            let inner = type_from_node_key(program, element).unwrap_or_else(|| "uint8_t".into());
            writeln!(
                output,
                "typedef struct {{ {inner} value[{length}]; }} {name};"
            )
            .map_err(write_error)?;
        }
    }
    if !names.is_empty() {
        output.push('\n');
    }
    Ok(())
}

fn type_from_node_key(_program: &Program, key: &str) -> Option<String> {
    let primitive = match key {
        "p_bool" => "bool",
        "p_i8" => "int8_t",
        "p_i16" => "int16_t",
        "p_i32" => "int32_t",
        "p_i64" => "int64_t",
        "p_i128" => "__int128",
        "p_isize" => "intptr_t",
        "p_u8" => "uint8_t",
        "p_u16" => "uint16_t",
        "p_u32" | "p_char" => "uint32_t",
        "p_u64" => "uint64_t",
        "p_u128" => "unsigned __int128",
        "p_usize" => "size_t",
        "p_f32" => "float",
        "p_f64" => "double",
        "string" | "str" => "const char *",
        _ => return None,
    };
    Some(primitive.into())
}

fn write_node_declarations(program: &Program, path: &Path) -> Result<(), BuildError> {
    let output = tn_node_api::generate_declarations(program)
        .map_err(|error| BuildError::Message(error.to_string()))?;
    std::fs::write(path, output)?;
    Ok(())
}

fn validate_exports(program: &Program, emit: Emit) -> Result<(), BuildError> {
    for definition in &program.definitions {
        let Some(declaration) = program.graph.declaration(definition.declaration) else {
            continue;
        };
        let Some(attribute) = declaration
            .attributes
            .iter()
            .find(|attribute| attribute.name == "Export")
        else {
            continue;
        };
        let DefinitionData::Function(function) = &definition.data else {
            if emit == Emit::SharedLibrary {
                return Err(BuildError::Message(format!(
                    "@Export is valid for functions in shared-library emission: {}",
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
        if !attribute.arguments.is_empty() && attribute.arguments.len() != 1 {
            return Err(BuildError::Message(format!(
                "@Export accepts at most one symbol argument on `{}`",
                declaration.name.as_deref().unwrap_or("<anonymous>")
            )));
        }
        if !definition.generics.is_empty() || !function.generics.is_empty() {
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
            && (!c_compatible(program, &function.result)
                || function
                    .parameters
                    .iter()
                    .any(|parameter| !c_compatible(program, &parameter.ty)))
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

fn c_compatible(program: &Program, ty: &Type) -> bool {
    match ty {
        Type::Primitive(primitive) => matches!(
            primitive,
            tn_hir::PrimitiveType::Bool
                | tn_hir::PrimitiveType::I8
                | tn_hir::PrimitiveType::I16
                | tn_hir::PrimitiveType::I32
                | tn_hir::PrimitiveType::I64
                | tn_hir::PrimitiveType::I128
                | tn_hir::PrimitiveType::Isize
                | tn_hir::PrimitiveType::U8
                | tn_hir::PrimitiveType::U16
                | tn_hir::PrimitiveType::U32
                | tn_hir::PrimitiveType::U64
                | tn_hir::PrimitiveType::U128
                | tn_hir::PrimitiveType::Usize
                | tn_hir::PrimitiveType::F32
                | tn_hir::PrimitiveType::F64
                | tn_hir::PrimitiveType::Char
                | tn_hir::PrimitiveType::Void
        ),
        Type::RawPointer { .. } => true,
        Type::Nominal(declaration, arguments) => {
            if !arguments.is_empty() {
                return false;
            }
            let Some(definition) = program.definition(*declaration) else {
                return false;
            };
            let Some(declaration) = program.graph.declaration(*declaration) else {
                return false;
            };
            if !declaration.attributes.iter().any(|attribute| {
                attribute.name == "Layout"
                    && attribute.arguments.first().is_some_and(|arg| arg == "C")
            }) {
                return false;
            }
            match &definition.data {
                DefinitionData::Struct { fields, .. } => {
                    fields.iter().all(|field| c_compatible(program, &field.ty))
                }
                DefinitionData::Enum { variants } => {
                    variants.iter().all(|variant| variant.fields.is_empty())
                }
                _ => false,
            }
        }
        _ => false,
    }
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
                DefinitionData::Enum { variants } => {
                    let c_repr = program
                        .graph
                        .declaration(definition.declaration)
                        .is_some_and(|declaration| {
                            declaration.attributes.iter().any(|attribute| {
                                attribute.name == "Layout"
                                    && attribute
                                        .arguments
                                        .first()
                                        .is_some_and(|argument| argument == "C")
                            })
                        });
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
    tn_codegen_llvm::Layouts {
        globals,
        nominals,
        witnesses: witness_layouts(program),
        interfaces: program
            .definitions
            .iter()
            .filter_map(|definition| match &definition.data {
                DefinitionData::Interface { methods } => Some((
                    definition.declaration,
                    u32::try_from(methods.len()).unwrap_or(u32::MAX),
                )),
                _ => None,
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
        exports: program
            .definitions
            .iter()
            .filter_map(|definition| {
                let tn_hir::DefinitionData::Function(function) = &definition.data else {
                    return None;
                };
                let declaration = program.graph.declaration(definition.declaration)?;
                let attribute = declaration
                    .attributes
                    .iter()
                    .find(|attribute| attribute.name == "Export")?;
                let name = attribute
                    .arguments
                    .first()
                    .cloned()
                    .or_else(|| declaration.name.clone())?;
                if !definition.generics.is_empty() || !function.generics.is_empty() {
                    return None;
                }
                Some((Callable::function(definition.declaration), name))
            })
            .collect(),
        drops: drop_layouts(program),
        copies: ownership.copy.clone(),
        async_functions: async_function_layouts(program),
        abi_wrappers: std::collections::BTreeMap::new(),
    }
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
                let is_drop = program
                    .graph
                    .declaration(*interface)
                    .and_then(|declaration| declaration.name.as_deref())
                    == Some("Drop");
                if is_drop && let Some(method) = methods.iter().find(|method| method.name == "drop")
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
                if has_drop_attribute(program, definition.declaration)
                    && let Some(method) = methods.iter().find(|method| method.name == "drop") =>
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
        let is_drop = program
            .graph
            .declaration(*interface)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Drop");
        if !is_drop {
            continue;
        }
        let Some(method) = methods.iter().find(|method| method.name == "drop") else {
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
                if has_drop_attribute(program, definition.declaration) =>
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
        let Some(method) = methods.iter().find(|method| method.name == "drop") else {
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

fn has_drop_attribute(program: &Program, declaration: DeclarationId) -> bool {
    program
        .graph
        .declaration(declaration)
        .is_some_and(|declaration| {
            declaration
                .attributes
                .iter()
                .any(|attribute| attribute.name == "Drop" && attribute.arguments.is_empty())
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
            DefinitionData::Class { interfaces, .. } => {
                let mut interface_ids = interfaces
                    .iter()
                    .filter_map(|interface| match interface {
                        Type::Nominal(interface, _) | Type::DynamicInterface(interface, _) => {
                            Some(*interface)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if let Some(declaration) = program.graph.declaration(definition.declaration) {
                    let module = declaration.module;
                    for attribute in declaration
                        .attributes
                        .iter()
                        .filter(|attribute| attribute.name == "Conform")
                    {
                        for name in &attribute.arguments {
                            if let Some(interface) = resolve_interface_name(program, module, name) {
                                interface_ids.push(interface);
                            }
                        }
                    }
                }
                interface_ids.sort_unstable();
                interface_ids.dedup();
                for interface in interface_ids {
                    let Some(DefinitionData::Interface {
                        methods: interface_methods,
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

fn resolve_interface_name(
    program: &Program,
    module_id: tn_hir::ModuleId,
    name: &str,
) -> Option<DeclarationId> {
    let module = program.graph.module(module_id)?;
    module
        .declarations
        .iter()
        .find(|declaration| {
            declaration.kind == tn_hir::DeclarationKind::Interface
                && declaration.name.as_deref() == Some(name)
        })
        .map(|declaration| declaration.id)
        .or_else(|| {
            module.imports.iter().find_map(|import| {
                let ImportClause::Named(names) = &import.clause else {
                    return None;
                };
                let imported = names.iter().find(|item| item.local == name)?;
                program
                    .graph
                    .module(import.target)?
                    .declarations
                    .iter()
                    .find(|declaration| {
                        declaration.kind == tn_hir::DeclarationKind::Interface
                            && declaration.name.as_deref() == Some(imported.imported.as_str())
                    })
                    .map(|declaration| declaration.id)
            })
        })
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
    tn_codegen_llvm::emit_program_to_file(
        &project.entry.to_string_lossy(),
        units,
        layouts,
        target,
        profile,
        tn_codegen_llvm::Emission::Object,
        &object,
    )
    .map_err(|error| BuildError::Message(error.to_string()))?;
    let runtime_support = temporary.path().join("runtime.o");
    compile_support_object(project, runtime_source(), &runtime_support)?;
    let startup_source = temporary.path().join("startup.tn");
    std::fs::write(
        &startup_source,
        startup_source_text(&tn_codegen_llvm::symbol_for_instance(entry), mode),
    )?;
    let startup_object = temporary.path().join("startup.o");
    compile_support_object(project, startup_source, &startup_object)?;
    let mut linker = Command::new("clang");
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
    if !cfg!(target_os = "macos") {
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
    let result = linker.output()?;
    if !result.status.success() {
        return Err(BuildError::Message(format!(
            "native linker failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    if profile == tn_codegen_llvm::CodegenProfile::Optimized && cfg!(target_os = "macos") {
        let result = Command::new("strip").arg("-x").arg(output).output()?;
        if !result.status.success() {
            return Err(BuildError::Message(format!(
                "optimized symbol stripping failed:\n{}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
    }
    if profile == tn_codegen_llvm::CodegenProfile::Debug && cfg!(target_os = "macos") {
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
    tn_codegen_llvm::emit_program_to_file(
        &project.entry.to_string_lossy(),
        units,
        layouts,
        target,
        profile,
        tn_codegen_llvm::Emission::Object,
        &object,
    )
    .map_err(|error| BuildError::Message(error.to_string()))?;

    let runtime = temporary.path().join("runtime.o");
    compile_support_object(project, runtime_source(), &runtime)?;
    let mut linker = Command::new("clang");
    linker.arg(&object).arg("-pthread");
    let wrapper_object = if emit == Emit::NodeAddon {
        let source = temporary.path().join("node_addon.c");
        let wrapper = node_wrapper_source(program)?;
        std::fs::write(&source, wrapper)?;
        if let Some(dump) = std::env::var_os("TN_NODE_WRAPPER_DUMP") {
            std::fs::write(dump, std::fs::read(&source)?)?;
        }
        let object = temporary.path().join("node_addon.o");
        let include = node_include_directory()?;
        let result = Command::new("clang")
            .arg("-fPIC")
            .arg("-I")
            .arg(include)
            .arg("-c")
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .output()?;
        if !result.status.success() {
            return Err(BuildError::Message(format!(
                "Node-API wrapper compilation failed:\n{}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
        Some(object)
    } else {
        None
    };
    if let Some(wrapper_object) = &wrapper_object {
        linker.arg(wrapper_object);
    }
    linker.arg(runtime);
    if !cfg!(target_os = "macos") {
        linker.arg("-ldl");
    }
    match emit {
        Emit::SharedLibrary => {
            if cfg!(target_os = "macos") {
                linker.arg("-dynamiclib");
            } else {
                linker.arg("-shared");
            }
        }
        Emit::NodeAddon => {
            if cfg!(target_os = "macos") {
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
    let result = linker.output()?;
    if !result.status.success() {
        return Err(BuildError::Message(format!(
            "native linker failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(())
}

fn runtime_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/runtime.tn")
}

fn compile_support_object(
    project: &Project,
    source: PathBuf,
    output: &Path,
) -> Result<(), BuildError> {
    let root = source.parent().unwrap_or(Path::new(".")).to_path_buf();
    let support = Project {
        root,
        entry: source.clone(),
        config: ProjectConfig {
            entry: source,
            out_dir: PathBuf::from("build"),
            target: project.config.target,
            profile: project.config.profile,
            emit: Emit::Object,
            link: LinkConfig::default(),
        },
        config_path: None,
    };
    build_project_with_timings(&support, Some(output), false).map(|_| ())
}

fn startup_source_text(entry: &str, mode: EntryMode) -> String {
    let preamble = format!(
        "extern \"C\" {{\n  function tn_process_set_args(argc: i32, argv: * mut u8): void;\n  function {entry}"
    );
    match mode {
        EntryMode::Void => format!(
            "{preamble}(): void;\n}}\n@Export(\"main\")\nexport function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    {entry}();\n  }}\n  return 0i32;\n}}\n"
        ),
        EntryMode::Integer => format!(
            "{preamble}(): i32;\n}}\n@Export(\"main\")\nexport function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    return {entry}();\n  }}\n}}\n"
        ),
        EntryMode::FallibleVoid | EntryMode::FallibleInteger => format!(
            "{preamble}(): EntryResult;\n  function tn_runtime_free(pointer: * mut u8): void;\n}}\n@Layout(\"C\")\nstruct EntryResult {{\n  public failed: u64;\n  public payload: u64;\n}}\n@Export(\"main\")\nexport function main(argc: i32, argv: * mut u8): i32 {{\n  unsafe {{\n    tn_process_set_args(argc, argv);\n    let result = {entry}();\n    if (result.failed !== 0u64) {{\n      const errorField = & mut result.payload;\n      tn_runtime_free(*(errorField as * mut * mut u8));\n      return 1i32;\n    }}\n    {}\n  }}\n}}\n",
            if matches!(mode, EntryMode::FallibleInteger) {
                "const valueField = & mut result.payload;\n    return *(valueField as * mut i32);"
            } else {
                "return 0i32;"
            }
        ),
    }
}

fn node_include_directory() -> Result<PathBuf, BuildError> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("NODE_INCLUDE_DIR") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/opt/node@24/include/node"),
        PathBuf::from("/opt/homebrew/opt/node/include/node"),
        PathBuf::from("/usr/local/include/node"),
        PathBuf::from("/usr/include/node"),
    ]);
    candidates
        .into_iter()
        .find(|candidate| candidate.join("node_api.h").is_file())
        .ok_or_else(|| BuildError::Message("Node-API headers were not found".into()))
}

#[allow(clippy::too_many_lines)]
fn node_wrapper_source(program: &Program) -> Result<String, BuildError> {
    let functions = exported_functions(program);
    let classes = exported_classes(program);
    let mut output = String::from(
        "#include <node_api.h>\n#include <stdint.h>\n#include <stdbool.h>\n#include <stdlib.h>\n#include <stddef.h>\n#include <string.h>\n\nextern void *tn_runtime_alloc(size_t size);\nextern void tn_runtime_free(void *pointer);\nextern void tn_runtime_promise_wait(void *promise);\nextern void *tn_runtime_async_result(void *promise);\nextern void *tn_runtime_async_raw_result(void *promise);\nextern int tn_runtime_async_destroy(void *promise);\n\n",
    );
    output
        .push_str("typedef struct { uint64_t failed; uint64_t payload; } tn_node_abi_result;\n\n");
    let mut exposed_types = Vec::new();
    for (_, function) in &functions {
        exposed_types.extend(
            function
                .parameters
                .iter()
                .map(|parameter| parameter.ty.clone()),
        );
        exposed_types.push(if function.is_async {
            match &function.result {
                Type::Promise { result, .. } => result.as_ref().clone(),
                result => result.clone(),
            }
        } else {
            function.result.clone()
        });
    }
    for (_, definition) in &classes {
        if let DefinitionData::Class {
            constructor,
            methods,
            ..
        } = &definition.data
        {
            if let Some(constructor) = constructor {
                exposed_types.extend(
                    constructor
                        .function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.ty.clone()),
                );
            }
            for method in methods {
                exposed_types.extend(
                    method
                        .function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.ty.clone()),
                );
                exposed_types.push(method.function.result.clone());
            }
        }
    }
    write_node_type_definitions(program, &mut output, &exposed_types)?;
    for (index, (declaration, function)) in functions.iter().enumerate() {
        let export = declaration
            .attributes
            .iter()
            .find(|attribute| attribute.name == "Export")
            .and_then(|attribute| attribute.arguments.first())
            .cloned()
            .or_else(|| declaration.name.clone())
            .ok_or_else(|| BuildError::Message("Node export has no name".into()))?;
        let symbol = c_symbol(&export)?;
        let wrapper = format!("tn_node_wrap_{index}");
        let inner_result = if function.is_async {
            match &function.result {
                Type::Promise { result, .. } => result.as_ref(),
                result => result,
            }
        } else {
            &function.result
        };
        let return_type = if function.is_async {
            if let Type::Promise { effects, .. } = &function.result
                && !effects.is_empty()
            {
                let completion = node_completion_name(index);
                write_node_completion_definition(&mut output, program, &completion, inner_result)?;
            }
            "void *".to_owned()
        } else if function.effects.is_empty() {
            if node_needs_indirect_abi(program, &function.result) {
                "void *".into()
            } else {
                node_c_type(program, &function.result)
            }
        } else {
            "tn_node_abi_result".to_owned()
        };
        let parameters = function
            .parameters
            .iter()
            .map(|parameter| {
                if !function.is_async && node_needs_indirect_abi(program, &parameter.ty) {
                    "void *".into()
                } else {
                    node_c_type(program, &parameter.ty)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        if function.parameters.is_empty() {
            writeln!(output, "extern {return_type} {symbol}(void);").map_err(write_error)?;
        } else {
            writeln!(output, "extern {return_type} {symbol}({parameters});")
                .map_err(write_error)?;
        }
        write_node_wrapper(
            program,
            &mut output,
            index,
            &export,
            &symbol,
            &wrapper,
            function,
        )?;
    }
    for (class_index, (declaration, definition)) in classes.iter().enumerate() {
        write_node_class_support(program, &mut output, class_index, declaration, definition)?;
    }
    output.push_str("\nNAPI_MODULE_INIT() {\n");
    if functions.is_empty() {
        output.push_str("  napi_status status = napi_ok;\n");
    } else {
        output.push_str("  napi_property_descriptor properties[] = {\n");
        for (index, (declaration, _)) in functions.iter().enumerate() {
            let export = exported_name(declaration);
            let property = c_string(&export);
            writeln!(
                output,
                "  {{ {property}, 0, tn_node_wrap_{index}, 0, 0, 0, napi_default, 0 }},"
            )
            .map_err(write_error)?;
        }
        output.push_str(
            "  };\n  napi_status status = napi_define_properties(env, exports, sizeof(properties) / sizeof(properties[0]), properties);\n  if (status != napi_ok) return NULL;\n",
        );
    }
    for (class_index, (declaration, _)) in classes.iter().enumerate() {
        let name = c_string(&exported_name(declaration));
        writeln!(
            output,
            "  napi_value tn_node_class_value_{class_index};\n  {{\n    napi_property_descriptor *class_properties = tn_node_class_properties_{class_index};\n    size_t class_property_count = tn_node_class_property_count_{class_index};\n    status = napi_define_class(env, {name}, NAPI_AUTO_LENGTH, tn_node_class_ctor_{class_index}, NULL, class_property_count, class_properties, &tn_node_class_value_{class_index});\n    if (status != napi_ok) return NULL;\n    status = napi_set_named_property(env, exports, {name}, tn_node_class_value_{class_index});\n    if (status != napi_ok) return NULL;\n  }}"
        )
        .map_err(write_error)?;
    }
    output.push_str("  return exports;\n}\n");
    Ok(output)
}

#[allow(clippy::too_many_lines)]
fn write_node_class_support(
    program: &Program,
    output: &mut String,
    class_index: usize,
    declaration: &tn_hir::Declaration,
    definition: &tn_hir::Definition,
) -> Result<(), BuildError> {
    let DefinitionData::Class {
        constructor,
        methods,
        ..
    } = &definition.data
    else {
        return Ok(());
    };
    let constructor_signature = constructor.as_ref().map_or_else(
        || tn_hir::FunctionType {
            parameters: Vec::new(),
            result: Box::new(Type::Nominal(declaration.id, Vec::new())),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        },
        |constructor| tn_hir::FunctionType {
            parameters: constructor
                .function
                .parameters
                .iter()
                .map(|parameter| parameter.ty.clone())
                .collect(),
            result: Box::new(Type::Nominal(declaration.id, Vec::new())),
            effects: constructor.function.effects.clone(),
            generics: Vec::new(),
            is_async: constructor.function.is_async,
            is_unsafe: constructor.function.is_unsafe,
        },
    );
    let constructor_symbol = tn_codegen_llvm::symbol_for_constructor(
        declaration.id,
        constructor.as_ref().map(|constructor| constructor.id),
        &constructor_signature,
    );
    let constructor_completion = format!("tn_node_class_completion_{class_index}");
    if !constructor_signature.effects.is_empty() {
        output.push_str("typedef struct ");
        output.push_str(&constructor_completion);
        output.push_str(" { uint8_t failed; void *value; void *error; } ");
        output.push_str(&constructor_completion);
        output.push_str(";\n");
    }
    let constructor_return = if constructor_signature.effects.is_empty() {
        "void *".to_owned()
    } else {
        constructor_completion.clone()
    };
    let constructor_parameters = constructor_signature
        .parameters
        .iter()
        .map(|parameter| node_c_type(program, parameter))
        .collect::<Vec<_>>()
        .join(", ");
    if constructor_parameters.is_empty() {
        writeln!(
            output,
            "extern {constructor_return} {constructor_symbol}(void);"
        )
        .map_err(write_error)?;
    } else {
        writeln!(
            output,
            "extern {constructor_return} {constructor_symbol}({constructor_parameters});"
        )
        .map_err(write_error)?;
    }
    let drop_symbol = class_drop_symbol(program, declaration.id);
    if let Some(drop_symbol) = &drop_symbol {
        writeln!(output, "extern void {drop_symbol}(void *self);").map_err(write_error)?;
    }
    writeln!(
        output,
        "static void tn_node_class_finalize_{class_index}(napi_env env, void *data, void *hint) {{ (void)env; (void)hint; if (!data) return; {} tn_runtime_free(data); }}",
        drop_symbol.map_or_else(String::new, |symbol| format!("{symbol}(data);"))
    )
    .map_err(write_error)?;
    writeln!(
        output,
        "static napi_value tn_node_class_ctor_{class_index}(napi_env env, napi_callback_info info) {{"
    )
    .map_err(write_error)?;
    let constructor_count = constructor_signature.parameters.len();
    writeln!(output, "  size_t argc = {constructor_count};").map_err(write_error)?;
    if constructor_count > 0 {
        writeln!(output, "  napi_value argv[{constructor_count}];").map_err(write_error)?;
    }
    output.push_str(
        "  napi_value this_arg;\n  napi_status status = napi_get_cb_info(env, info, &argc, ",
    );
    if constructor_count > 0 {
        output.push_str("argv");
    } else {
        output.push_str("NULL");
    }
    output.push_str(", &this_arg, NULL);\n");
    writeln!(
        output,
        "  if (status != napi_ok || argc != {constructor_count}) {{ napi_throw_type_error(env, NULL, \"invalid constructor arguments\"); return NULL; }}"
    )
    .map_err(write_error)?;
    for (index, parameter) in constructor_signature.parameters.iter().enumerate() {
        let name = format!("constructor_arg{index}");
        writeln!(output, "  {} {name};", node_c_type(program, parameter)).map_err(write_error)?;
        write_node_argument_conversion(program, output, parameter, index, &name)?;
    }
    let constructor_arguments = (0..constructor_count)
        .map(|index| format!("constructor_arg{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    if constructor_signature.effects.is_empty() {
        writeln!(
            output,
            "  void *native = {constructor_symbol}({constructor_arguments});\n  if (!native) {{ napi_throw_error(env, NULL, \"native constructor allocation failed\"); return NULL; }}\n  status = napi_wrap(env, this_arg, native, tn_node_class_finalize_{class_index}, NULL, NULL);\n  if (status != napi_ok) {{ tn_runtime_free(native); return NULL; }}\n  return this_arg;\n}}"
        )
        .map_err(write_error)?;
    } else {
        writeln!(
            output,
            "  {constructor_completion} native = {constructor_symbol}({constructor_arguments});\n  if (native.failed) {{ napi_throw_error(env, NULL, \"TypeNative constructor failed\"); tn_runtime_free(native.error); return NULL; }}\n  if (!native.value) {{ napi_throw_error(env, NULL, \"native constructor allocation failed\"); return NULL; }}\n  status = napi_wrap(env, this_arg, native.value, tn_node_class_finalize_{class_index}, NULL, NULL);\n  if (status != napi_ok) {{ tn_runtime_free(native.value); return NULL; }}\n  return this_arg;\n}}"
        )
        .map_err(write_error)?;
    }

    let public_methods = methods
        .iter()
        .filter(|method| method.visibility == Visibility::Public)
        .collect::<Vec<_>>();
    for (method_index, method) in public_methods.iter().enumerate() {
        write_node_class_method_wrapper(
            program,
            output,
            class_index,
            method_index,
            declaration,
            method,
        )?;
    }
    if public_methods.is_empty() {
        writeln!(
            output,
            "static napi_property_descriptor *tn_node_class_properties_{class_index} = NULL; static size_t tn_node_class_property_count_{class_index} = 0;"
        )
        .map_err(write_error)?;
    } else {
        writeln!(
            output,
            "static napi_property_descriptor tn_node_class_properties_{class_index}[] = {{"
        )
        .map_err(write_error)?;
        for (method_index, method) in public_methods.iter().enumerate() {
            let flags = if method.receiver == tn_hir::ReceiverMode::Static {
                "napi_static"
            } else {
                "napi_default"
            };
            writeln!(
                output,
                "  {{ {}, 0, tn_node_class_method_{class_index}_{method_index}, 0, 0, 0, {flags}, 0 }},",
                c_string(&method.name)
            )
            .map_err(write_error)?;
        }
        output.push_str("};\n");
        writeln!(
            output,
            "static size_t tn_node_class_property_count_{class_index} = {};",
            public_methods.len()
        )
        .map_err(write_error)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn write_node_class_method_wrapper(
    program: &Program,
    output: &mut String,
    class_index: usize,
    method_index: usize,
    class: &tn_hir::Declaration,
    method: &tn_hir::Method,
) -> Result<(), BuildError> {
    let callable = Callable {
        declaration: class.id,
        member: Some(method.id),
    };
    let instance = Instance {
        callable,
        type_arguments: Vec::new(),
        effects: method.function.effects.clone(),
    };
    let symbol = tn_codegen_llvm::symbol_for_instance(&instance);
    let static_method = method.receiver == tn_hir::ReceiverMode::Static;
    let indirect_result = node_needs_indirect_abi(program, &method.function.result);
    let return_type = if !method.function.effects.is_empty() {
        "tn_node_abi_result".to_owned()
    } else if method.function.result == Type::Primitive(PrimitiveType::Void) {
        "void".to_owned()
    } else if indirect_result {
        "void *".to_owned()
    } else {
        node_c_type(program, &method.function.result)
    };
    let mut parameters = Vec::new();
    if !static_method {
        parameters.push("void *self".to_owned());
    }
    parameters.extend(
        method
            .function
            .parameters
            .iter()
            .enumerate()
            .map(|(index, parameter)| {
                if node_needs_indirect_abi(program, &parameter.ty) {
                    format!("void *arg{index}")
                } else {
                    format!("{} arg{index}", node_c_type(program, &parameter.ty))
                }
            }),
    );
    let parameter_list = if parameters.is_empty() {
        "void".into()
    } else {
        parameters.join(", ")
    };
    writeln!(output, "extern {return_type} {symbol}({parameter_list});").map_err(write_error)?;
    writeln!(
        output,
        "static napi_value tn_node_class_method_{class_index}_{method_index}(napi_env env, napi_callback_info info) {{"
    )
    .map_err(write_error)?;
    let count = method.function.parameters.len();
    writeln!(output, "  size_t argc = {count};").map_err(write_error)?;
    if count > 0 {
        writeln!(output, "  napi_value argv[{count}];").map_err(write_error)?;
    }
    output.push_str("  napi_value this_arg; napi_value result; napi_status status = napi_get_cb_info(env, info, &argc, ");
    if count > 0 {
        output.push_str("argv");
    } else {
        output.push_str("NULL");
    }
    output.push_str(", &this_arg, NULL);\n");
    writeln!(
        output,
        "  if (status != napi_ok || argc != {count}) {{ napi_throw_type_error(env, NULL, \"invalid method arguments\"); return NULL; }}"
    )
    .map_err(write_error)?;
    if !static_method {
        output.push_str("  void *self = NULL; status = napi_unwrap(env, this_arg, &self); if (status != napi_ok || !self) { napi_throw_type_error(env, NULL, \"invalid TypeNative receiver\"); return NULL; }\n");
    }
    for (index, parameter) in method.function.parameters.iter().enumerate() {
        let name = format!("arg{index}");
        let declaration = format!("{} {name};", node_c_type(program, &parameter.ty));
        writeln!(output, "  {declaration}").map_err(write_error)?;
        write_node_argument_conversion(program, output, &parameter.ty, index, &name)?;
    }
    let mut arguments = Vec::new();
    if !static_method {
        arguments.push("self".to_owned());
    }
    arguments.extend(
        method
            .function
            .parameters
            .iter()
            .enumerate()
            .map(|(index, parameter)| {
                if node_needs_indirect_abi(program, &parameter.ty) {
                    format!("(void *)&arg{index}")
                } else {
                    format!("arg{index}")
                }
            }),
    );
    let arguments = arguments.join(", ");
    if !method.function.effects.is_empty() {
        writeln!(
            output,
            "  tn_node_abi_result native_result = {symbol}({arguments});\n  if (native_result.failed) {{ napi_throw_error(env, NULL, \"TypeNative recoverable error\"); tn_runtime_free((void *)(uintptr_t)native_result.payload); return NULL; }}"
        )
        .map_err(write_error)?;
        if method.function.result == Type::Primitive(PrimitiveType::Void) {
            output.push_str("  status = napi_get_undefined(env, &result);\n");
        } else if node_needs_indirect_abi(program, &method.function.result) {
            let result_type = node_c_type(program, &method.function.result);
            writeln!(
                output,
                "  void *native_result_pointer = (void *)(uintptr_t)native_result.payload; if (!native_result_pointer) {{ napi_throw_error(env, NULL, \"native result allocation failed\"); return NULL; }} {result_type} native_value = *(({result_type} *)native_result_pointer);"
            )
            .map_err(write_error)?;
            write_node_result_conversion(program, output, &method.function.result, "native_value")?;
            output.push_str("  tn_runtime_free(native_result_pointer);\n");
        } else {
            let payload = node_abi_payload_expression(&method.function.result);
            write_node_result_conversion(program, output, &method.function.result, &payload)?;
        }
    } else if method.function.result == Type::Primitive(PrimitiveType::Void) {
        writeln!(output, "  {symbol}({arguments});").map_err(write_error)?;
        output.push_str("  status = napi_get_undefined(env, &result);\n");
    } else if indirect_result {
        let result_type = node_c_type(program, &method.function.result);
        writeln!(
            output,
            "  void *native_result_pointer = {symbol}({arguments}); if (!native_result_pointer) {{ napi_throw_error(env, NULL, \"native result allocation failed\"); return NULL; }} {result_type} native_result = *(({result_type} *)native_result_pointer);"
        )
        .map_err(write_error)?;
        write_node_result_conversion(program, output, &method.function.result, "native_result")?;
        output.push_str("  tn_runtime_free(native_result_pointer);\n");
    } else {
        let result_type = node_c_type(program, &method.function.result);
        writeln!(
            output,
            "  {result_type} native_result = {symbol}({arguments});"
        )
        .map_err(write_error)?;
        write_node_result_conversion(program, output, &method.function.result, "native_result")?;
    }
    write_node_parameter_cleanup(program, output, &method.function)?;
    output.push_str("  if (status != napi_ok) return NULL;\n  return result;\n}\n");
    Ok(())
}

fn class_drop_symbol(program: &Program, target: DeclarationId) -> Option<String> {
    for definition in &program.definitions {
        let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, _)),
            target: Type::Nominal(implemented, _),
            methods,
            ..
        } = &definition.data
        else {
            continue;
        };
        if *implemented != target {
            continue;
        }
        let Some(name) = program
            .graph
            .declaration(*interface)
            .and_then(|declaration| declaration.name.as_deref())
        else {
            continue;
        };
        let Some(method) = methods.iter().find(|method| method.name == "drop") else {
            continue;
        };
        if name != "Drop" || !method.function.effects.is_empty() {
            continue;
        }
        return Some(tn_codegen_llvm::symbol_for_instance(&Instance {
            callable: Callable {
                declaration: definition.declaration,
                member: Some(method.id),
            },
            type_arguments: Vec::new(),
            effects: Vec::new(),
        }));
    }
    None
}

fn node_completion_name(index: usize) -> String {
    format!("tn_node_completion_{index}")
}

fn write_node_completion_definition(
    output: &mut String,
    program: &Program,
    name: &str,
    result: &Type,
) -> Result<(), BuildError> {
    output.push_str("typedef struct ");
    output.push_str(name);
    output.push_str(" { bool failed; ");
    if *result != Type::Primitive(PrimitiveType::Void) {
        write!(output, "{} value; ", node_c_type(program, result)).map_err(write_error)?;
    }
    output.push_str("void *error; } ");
    output.push_str(name);
    output.push_str(";\n");
    Ok(())
}

fn node_abi_payload_expression(ty: &Type) -> String {
    match ty {
        Type::String | Type::Str | Type::Reference { .. } => {
            "(const char *)(uintptr_t)native_result.payload".into()
        }
        Type::Primitive(PrimitiveType::Bool) => "(bool)native_result.payload".into(),
        Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            "(double)native_result.payload".into()
        }
        Type::Primitive(_) => "native_result.payload".into(),
        _ => "(void *)(uintptr_t)native_result.payload".into(),
    }
}

fn write_node_parameter_cleanup(
    program: &Program,
    output: &mut String,
    function: &tn_hir::Function,
) -> Result<(), BuildError> {
    for (index, parameter) in function.parameters.iter().enumerate() {
        if is_node_string(&parameter.ty) {
            writeln!(output, "  free(arg{index});").map_err(write_error)?;
        }
        if let Type::Reference { referent, .. } = &parameter.ty
            && nominal_is_node_array(program, referent)
        {
            writeln!(
                output,
                "  if (arg{index}) {{ tn_node_array *array_arg{index} = (tn_node_array *)arg{index}; tn_runtime_free(array_arg{index}->pointer); tn_runtime_free(array_arg{index}->initialized); tn_runtime_free(array_arg{index}); }}"
            )
            .map_err(write_error)?;
        }
    }
    Ok(())
}

fn write_node_async_wrapper(
    program: &Program,
    output: &mut String,
    index: usize,
    symbol: &str,
    arguments: &str,
    function: &tn_hir::Function,
) -> Result<(), BuildError> {
    writeln!(output, "  void *native_promise = {symbol}({arguments});").map_err(write_error)?;
    output.push_str("  if (!native_promise) { napi_throw_error(env, NULL, \"native promise allocation failed\"); return NULL; }\n");
    let context = format!("tn_node_async_context_{index}");
    let execute = format!("tn_node_async_execute_{index}");
    let complete = format!("tn_node_async_complete_{index}");
    output.push_str("  napi_deferred deferred;\n  napi_value promise;\n  status = napi_create_promise(env, &deferred, &promise);\n  if (status != napi_ok) { tn_runtime_async_destroy(native_promise); return NULL; }\n");
    writeln!(
        output,
        "  {context} *context = ({context} *)malloc(sizeof(*context)); if (!context) {{ tn_runtime_async_destroy(native_promise); napi_throw_error(env, NULL, \"async context allocation failed\"); return NULL; }} context->env = env; context->deferred = deferred; context->native_promise = native_promise;"
    )
    .map_err(write_error)?;
    output.push_str("  napi_value resource_name; status = napi_create_string_utf8(env, \"TypeNative async\", NAPI_AUTO_LENGTH, &resource_name); if (status != napi_ok) { tn_runtime_async_destroy(native_promise); free(context); return NULL; }\n");
    writeln!(
        output,
        "  status = napi_create_async_work(env, NULL, resource_name, {execute}, {complete}, context, &context->work); if (status != napi_ok) {{ tn_runtime_async_destroy(native_promise); free(context); return NULL; }} status = napi_queue_async_work(env, context->work); if (status != napi_ok) {{ napi_delete_async_work(env, context->work); tn_runtime_async_destroy(native_promise); free(context); return NULL; }}"
    )
    .map_err(write_error)?;
    write_node_parameter_cleanup(program, output, function)?;
    output.push_str("  return promise;\n");
    Ok(())
}

fn write_node_wrapper(
    program: &Program,
    output: &mut String,
    index: usize,
    export: &str,
    symbol: &str,
    wrapper: &str,
    function: &tn_hir::Function,
) -> Result<(), BuildError> {
    if function.is_async {
        write_node_async_support(program, output, index, function)?;
    }
    writeln!(
        output,
        "static napi_value {wrapper}(napi_env env, napi_callback_info info) {{"
    )
    .map_err(write_error)?;
    let count = function.parameters.len();
    writeln!(output, "  size_t argc = {count};").map_err(write_error)?;
    if count > 0 {
        writeln!(output, "  napi_value argv[{count}];").map_err(write_error)?;
    }
    output.push_str("  napi_value result;\n");
    output.push_str("  napi_status status = napi_get_cb_info(env, info, &argc, ");
    if count > 0 {
        output.push_str("argv");
    } else {
        output.push_str("NULL");
    }
    output.push_str(", NULL, NULL);\n  if (status != napi_ok || argc != ");
    output.push_str(&count.to_string());
    output.push_str(") { napi_throw_type_error(env, NULL, ");
    output.push_str(&c_string(&format!("{export} expects {count} arguments")));
    output.push_str("); return NULL; }\n");
    for (index, parameter) in function.parameters.iter().enumerate() {
        let c_name = format!("arg{index}");
        let c_decl = node_c_type(program, &parameter.ty);
        writeln!(output, "  {c_decl} {c_name};").map_err(write_error)?;
        write_node_argument_conversion(program, output, &parameter.ty, index, &c_name)?;
    }
    let arguments = function
        .parameters
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            if !function.is_async && node_needs_indirect_abi(program, &parameter.ty) {
                format!("(void *)&arg{index}")
            } else {
                format!("arg{index}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if function.is_async {
        write_node_async_wrapper(program, output, index, symbol, &arguments, function)?;
        output.push_str("}\n\n");
        return Ok(());
    } else if !function.effects.is_empty() {
        writeln!(
            output,
            "  tn_node_abi_result native_result = {symbol}({arguments});"
        )
        .map_err(write_error)?;
        output.push_str("  if (native_result.failed) {\n    napi_throw_error(env, NULL, \"TypeNative recoverable error\");\n    tn_runtime_free((void *)(uintptr_t)native_result.payload);\n    return NULL;\n  }\n");
        if function.result == Type::Primitive(PrimitiveType::Void) {
            output.push_str("  napi_get_undefined(env, &result);\n");
        } else if node_needs_indirect_abi(program, &function.result) {
            let result_type = node_c_type(program, &function.result);
            writeln!(
                output,
                "  void *native_result_pointer = (void *)(uintptr_t)native_result.payload; if (!native_result_pointer) {{ napi_throw_error(env, NULL, \"native result allocation failed\"); return NULL; }} {result_type} native_value = *(({result_type} *)native_result_pointer);"
            )
            .map_err(write_error)?;
            write_node_result_conversion(program, output, &function.result, "native_value")?;
            output.push_str("  tn_runtime_free(native_result_pointer);\n");
        } else {
            let payload = node_abi_payload_expression(&function.result);
            write_node_result_conversion(program, output, &function.result, &payload)?;
        }
    } else if function.result == Type::Primitive(PrimitiveType::Void) {
        writeln!(output, "  {symbol}({arguments});").map_err(write_error)?;
        output.push_str("  napi_get_undefined(env, &result);\n");
    } else {
        let indirect_result = node_needs_indirect_abi(program, &function.result);
        let result_type = node_c_type(program, &function.result);
        if indirect_result {
            writeln!(
                output,
                "  void *native_result_pointer = {symbol}({arguments});\n  if (!native_result_pointer) {{ napi_throw_error(env, NULL, \"native result allocation failed\"); return NULL; }}\n  {result_type} native_result = *(({result_type} *)native_result_pointer);"
            )
            .map_err(write_error)?;
        } else {
            writeln!(
                output,
                "  {result_type} native_result = {symbol}({arguments});"
            )
            .map_err(write_error)?;
        }
        write_node_result_conversion(program, output, &function.result, "native_result")?;
        if indirect_result {
            output.push_str("  tn_runtime_free(native_result_pointer);\n");
        }
    }
    write_node_parameter_cleanup(program, output, function)?;
    output.push_str("  return result;\n}\n\n");
    Ok(())
}

fn write_node_async_support(
    program: &Program,
    output: &mut String,
    index: usize,
    function: &tn_hir::Function,
) -> Result<(), BuildError> {
    let inner = match &function.result {
        Type::Promise { result, .. } => result.as_ref(),
        result => result,
    };
    let context = format!("tn_node_async_context_{index}");
    let execute = format!("tn_node_async_execute_{index}");
    let complete = format!("tn_node_async_complete_{index}");
    writeln!(
        output,
        "typedef struct {context} {{ napi_env env; napi_deferred deferred; napi_async_work work; void *native_promise; }} {context};"
    )
    .map_err(write_error)?;
    writeln!(
        output,
        "static void {execute}(napi_env env, void *data) {{ (void)env; {context} *context = ({context} *)data; tn_runtime_promise_wait(context->native_promise); }}"
    )
    .map_err(write_error)?;
    writeln!(
        output,
        "static void {complete}(napi_env env, napi_status status, void *data) {{"
    )
    .map_err(write_error)?;
    writeln!(
        output,
        "  {context} *context = ({context} *)data; napi_value result;"
    )
    .map_err(write_error)?;
    output.push_str(
            "  if (status != napi_ok) { napi_value message; napi_value error; if (napi_create_string_utf8(env, \"TypeNative async work failed\", NAPI_AUTO_LENGTH, &message) == napi_ok && napi_create_error(env, NULL, message, &error) == napi_ok) napi_reject_deferred(env, context->deferred, error); tn_runtime_async_destroy(context->native_promise); napi_delete_async_work(env, context->work); tn_runtime_free(context); return; }\n",
        );
    let has_effects =
        matches!(&function.result, Type::Promise { effects, .. } if !effects.is_empty());
    if has_effects {
        let completion = node_completion_name(index);
        writeln!(
            output,
            "  {completion} *native = ({completion} *)tn_runtime_async_raw_result(context->native_promise);"
        )
        .map_err(write_error)?;
        output.push_str(
            "  if (native->failed) { napi_value message; napi_value error; if (napi_create_string_utf8(env, \"TypeNative recoverable error\", NAPI_AUTO_LENGTH, &message) == napi_ok && napi_create_error(env, NULL, message, &error) == napi_ok) napi_reject_deferred(env, context->deferred, error); tn_runtime_free(native->error); tn_runtime_async_destroy(context->native_promise); napi_delete_async_work(env, context->work); tn_runtime_free(context); return; }\n",
        );
        if *inner == Type::Primitive(PrimitiveType::Void) {
            output.push_str("  status = napi_get_undefined(env, &result);\n");
        } else {
            let start = output.len();
            write_node_result_conversion(program, output, inner, "native->value")?;
            let generated = output
                .split_off(start)
                .replace("return NULL;", &format!("goto {context}_cleanup;"));
            output.push_str(&generated);
        }
        output.push_str(
            "  if (status == napi_ok) status = napi_resolve_deferred(env, context->deferred, result);\n  tn_runtime_async_destroy(context->native_promise); context->native_promise = NULL;\n",
        );
    } else {
        if *inner == Type::Primitive(PrimitiveType::Void) {
            output.push_str("  status = napi_get_undefined(env, &result);\n");
        } else {
            let native_type = node_c_type(program, inner);
            writeln!(
                output,
                "  {native_type} native = *(({native_type} *)tn_runtime_async_result(context->native_promise));"
            )
            .map_err(write_error)?;
            let start = output.len();
            write_node_result_conversion(program, output, inner, "native")?;
            let generated = output
                .split_off(start)
                .replace("return NULL;", &format!("goto {context}_cleanup;"));
            output.push_str(&generated);
        }
        output.push_str(
            "  if (status == napi_ok) status = napi_resolve_deferred(env, context->deferred, result);\n  tn_runtime_async_destroy(context->native_promise); context->native_promise = NULL;\n",
        );
    }
    writeln!(
        output,
        "  napi_delete_async_work(env, context->work); tn_runtime_async_destroy(context->native_promise); context->native_promise = NULL; tn_runtime_free(context); return;\n  {context}_cleanup: tn_runtime_async_destroy(context->native_promise); napi_delete_async_work(env, context->work); tn_runtime_free(context);\n}}\n\n"
    )
    .map_err(write_error)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn write_node_argument_conversion(
    program: &Program,
    output: &mut String,
    ty: &Type,
    index: usize,
    name: &str,
) -> Result<(), BuildError> {
    let argv = format!("argv[{index}]");
    if let Type::Nominal(declaration, arguments) = ty
        && arguments.is_empty()
        && program
            .graph
            .declaration(*declaration)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Bytes")
    {
        writeln!(
            output,
            "  napi_typedarray_type {name}_type; size_t {name}_length = 0; void *{name}_data = NULL; napi_value {name}_buffer; size_t {name}_offset = 0;\n  status = napi_get_typedarray_info(env, {argv}, &{name}_type, &{name}_length, &{name}_data, &{name}_buffer, &{name}_offset);\n  if (status != napi_ok || {name}_type != napi_uint8_array) {{ napi_throw_type_error(env, NULL, \"expected a Uint8Array\"); return NULL; }}\n  {name}.pointer = (const uint8_t *){name}_data;\n  {name}.length = {name}_length;"
        )
        .map_err(write_error)?;
        return Ok(());
    }
    let array_arguments = match ty {
        Type::Nominal(declaration, arguments) if arguments.len() == 1 => Some(arguments),
        Type::Reference { referent, .. } => match referent.as_ref() {
            Type::Nominal(declaration, arguments) if arguments.len() == 1 => Some(arguments),
            _ => None,
        },
        _ => None,
    };
    if let Some(arguments) = array_arguments {
        let nominal_type = match ty {
            Type::Reference { referent, .. } => referent.as_ref(),
            _ => ty,
        };
        if !nominal_is_node_array(program, nominal_type) {
            return Err(BuildError::Message(
                "Node array conversion received a non-Array nominal".into(),
            ));
        }
        let inner = &arguments[0];
        let inner_c = node_c_type(program, inner);
        if !matches!(inner, Type::Primitive(_)) {
            return Err(BuildError::Message(
                "Node Array arguments currently require a primitive element type".into(),
            ));
        }
        writeln!(
            output,
            "  tn_node_array *{name}_object = (tn_node_array *)tn_runtime_alloc(sizeof(tn_node_array)); if (!{name}_object) return NULL; {name}_object->descriptor = NULL; {name}_object->elementSize = sizeof({inner_c});"
        )
        .map_err(write_error)?;
        writeln!(
            output,
            "  bool {name}_is_array = false;\n  status = napi_is_array(env, {argv}, &{name}_is_array);\n  if (status != napi_ok || !{name}_is_array) {{ napi_throw_type_error(env, NULL, \"expected an Array\"); return NULL; }}\n  uint32_t {name}_array_length = 0;\n  status = napi_get_array_length(env, {argv}, &{name}_array_length);\n  if (status != napi_ok) return NULL;\n  {name}_object->length = {name}_array_length;\n  {name}_object->capacity = {name}_object->length;\n  {name}_object->pointer = tn_runtime_alloc({name}_object->length * sizeof({inner_c}));\n  {name}_object->initialized = {name}_object->length == 0 ? NULL : tn_runtime_alloc({name}_object->length);\n  if (({name}_object->length != 0) && (!{name}_object->pointer || !{name}_object->initialized)) return NULL;\n  if ({name}_object->initialized) memset({name}_object->initialized, 0, {name}_object->length);\n  for (size_t index = 0; index < {name}_object->length; ++index) {{ napi_value element; status = napi_get_element(env, {argv}, index, &element); if (status != napi_ok) return NULL;"
        )
        .map_err(write_error)?;
        writeln!(output, "  {name} = {name}_object;").map_err(write_error)?;
        write_node_scalar_argument_conversion(
            output,
            inner,
            "element",
            &format!("(({inner_c} *){name}_object->pointer)[index]"),
        )?;
        output.push_str("  }\n");
        return Ok(());
    }
    if let Type::Array(inner, length) = ty {
        if !matches!(inner.as_ref(), Type::Primitive(_)) {
            return Err(BuildError::Message(
                "Node fixed-array arguments currently require a primitive element type".into(),
            ));
        }
        writeln!(
            output,
            "  bool {name}_is_array = false;\n  status = napi_is_array(env, {argv}, &{name}_is_array);\n  if (status != napi_ok || !{name}_is_array) {{ napi_throw_type_error(env, NULL, \"expected an Array\"); return NULL; }}\n  uint32_t {name}_length = 0;\n  status = napi_get_array_length(env, {argv}, &{name}_length);\n  if (status != napi_ok || {name}_length != {length}) {{ napi_throw_range_error(env, NULL, \"array length mismatch\"); return NULL; }}\n  for (size_t index = 0; index < {length}; ++index) {{ napi_value element; status = napi_get_element(env, {argv}, index, &element); if (status != napi_ok) return NULL;"
        )
        .map_err(write_error)?;
        write_node_scalar_argument_conversion(
            output,
            inner,
            "element",
            &format!("{name}.value[index]"),
        )?;
        output.push_str("  }\n");
        return Ok(());
    }
    if let Type::Optional(inner) = ty {
        writeln!(
            output,
            "  napi_valuetype {name}_value_type;\n  status = napi_typeof(env, {argv}, &{name}_value_type);\n  if (status != napi_ok) return NULL;\n  {name}.present = {name}_value_type != napi_undefined;"
        )
        .map_err(write_error)?;
        let inner_type = node_c_type(program, inner);
        writeln!(output, "  {name}.value = ({inner_type}){{0}};").map_err(write_error)?;
        if matches!(
            inner.as_ref(),
            Type::Primitive(_) | Type::String | Type::Str
        ) {
            write_node_scalar_argument_conversion(output, inner, &argv, &format!("{name}.value"))?;
        } else {
            return Err(BuildError::Message(
                "Node optional argument requires a scalar or string payload".into(),
            ));
        }
        return Ok(());
    }
    if is_node_string(ty) {
        writeln!(
            output,
            "  size_t {name}_length = 0;\n  status = napi_get_value_string_utf8(env, argv[{index}], NULL, 0, &{name}_length);\n  if (status != napi_ok) return NULL;\n  {name} = malloc({name}_length + 1);\n  if (!{name}) return NULL;\n  status = napi_get_value_string_utf8(env, argv[{index}], (char *){name}, {name}_length + 1, &{name}_length);\n  if (status != napi_ok) {{ free({name}); return NULL; }}"
        )
        .map_err(write_error)?;
        return Ok(());
    }
    if matches!(
        ty,
        Type::Primitive(PrimitiveType::I128 | PrimitiveType::U128)
    ) {
        let signed = matches!(ty, Type::Primitive(PrimitiveType::I128));
        writeln!(
            output,
            "  uint64_t {name}_words[2] = {{ 0, 0 }};\n  size_t {name}_word_count = 2;\n  int {name}_sign = 0;\n  status = napi_get_value_bigint_words(env, argv[{index}], &{name}_sign, &{name}_word_count, {name}_words);\n  if (status != napi_ok || {name}_word_count > 2) {{ napi_throw_type_error(env, NULL, \"expected a 128-bit bigint\"); return NULL; }}\n  {name} = ({}){name}_words[0] | (({}){name}_words[1] << 64);{}",
            if signed { "__int128" } else { "unsigned __int128" },
            if signed { "__int128" } else { "unsigned __int128" },
            if signed {
                format!("\n  if ({name}_sign) {name} = -{name};")
            } else {
                String::new()
            }
        )
        .map_err(write_error)?;
        return Ok(());
    }
    let (getter, cast) = match ty {
        Type::Primitive(PrimitiveType::Bool) => ("napi_get_value_bool", "bool"),
        Type::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Char,
        ) => ("napi_get_value_int32", "int32_t"),
        Type::Primitive(PrimitiveType::I64 | PrimitiveType::Isize) => {
            ("napi_get_value_bigint_int64", "int64_t")
        }
        Type::Primitive(PrimitiveType::U64 | PrimitiveType::Usize) => {
            ("napi_get_value_bigint_uint64", "uint64_t")
        }
        Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            ("napi_get_value_double", "double")
        }
        _ => {
            return Err(BuildError::Message(format!(
                "Node export argument {index} has no generated conversion for {ty:?}"
            )));
        }
    };
    writeln!(output, "  status = {getter}(env, {argv}, ({cast} *)&{name});\n  if (status != napi_ok) return NULL;")
        .map_err(write_error)
}

fn write_node_scalar_argument_conversion(
    output: &mut String,
    ty: &Type,
    value: &str,
    name: &str,
) -> Result<(), BuildError> {
    let temporary = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if is_node_string(ty) {
        writeln!(
            output,
            "  size_t {temporary}_length = 0;\n  status = napi_get_value_string_utf8(env, {value}, NULL, 0, &{temporary}_length);\n  if (status != napi_ok) return NULL;\n  {name} = malloc({temporary}_length + 1);\n  if (!{name}) return NULL;\n  status = napi_get_value_string_utf8(env, {value}, (char *){name}, {temporary}_length + 1, &{temporary}_length);\n  if (status != napi_ok) {{ free({name}); return NULL; }}"
        )
        .map_err(write_error)?;
        return Ok(());
    }
    if matches!(
        ty,
        Type::Primitive(PrimitiveType::I128 | PrimitiveType::U128)
    ) {
        return Err(BuildError::Message(
            "128-bit optional Node arguments are not supported by the generated wrapper".into(),
        ));
    }
    let (getter, cast) = match ty {
        Type::Primitive(PrimitiveType::Bool) => ("napi_get_value_bool", "bool"),
        Type::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Char,
        ) => ("napi_get_value_int32", "int32_t"),
        Type::Primitive(PrimitiveType::I64 | PrimitiveType::Isize) => {
            ("napi_get_value_bigint_int64", "int64_t")
        }
        Type::Primitive(PrimitiveType::U64 | PrimitiveType::Usize) => {
            ("napi_get_value_bigint_uint64", "uint64_t")
        }
        Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            ("napi_get_value_double", "double")
        }
        _ => {
            return Err(BuildError::Message(
                "Node export argument has no generated scalar conversion".into(),
            ));
        }
    };
    writeln!(
        output,
        "  status = {getter}(env, {value}, ({cast} *)&{name});\n  if (status != napi_ok) return NULL;"
    )
    .map_err(write_error)
}

#[allow(clippy::too_many_lines)]
fn write_node_result_conversion(
    program: &Program,
    output: &mut String,
    ty: &Type,
    expression: &str,
) -> Result<(), BuildError> {
    if let Type::Nominal(declaration, arguments) = ty
        && arguments.is_empty()
        && program
            .graph
            .declaration(*declaration)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Bytes")
    {
        output.push_str("  napi_value native_arraybuffer; void *native_bytes;\n");
        writeln!(
            output,
            "  status = napi_create_arraybuffer(env, {expression}.length, &native_bytes, &native_arraybuffer);\n  if (status != napi_ok) return NULL;\n  memcpy(native_bytes, {expression}.pointer, {expression}.length);\n  status = napi_create_typedarray(env, napi_uint8_array, {expression}.length, native_arraybuffer, 0, &result);\n  if (status != napi_ok) return NULL;"
        )
        .map_err(write_error)?;
        return Ok(());
    }
    if let Type::Optional(inner) = ty {
        output.push_str("  if (!");
        output.push_str(expression);
        output.push_str(".present) { status = napi_get_undefined(env, &result); } else {");
        write_node_result_conversion(program, output, inner, &format!("({expression}).value"))?;
        output.push_str("  }");
        return Ok(());
    }
    if let Type::Nominal(declaration, arguments) = ty
        && arguments.len() == 1
        && program
            .graph
            .declaration(*declaration)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Array")
    {
        let inner = &arguments[0];
        if !matches!(inner, Type::Primitive(_)) {
            return Err(BuildError::Message(
                "Node Array results currently require a primitive element type".into(),
            ));
        }
        let array = format!("((tn_node_array *)({expression}))");
        output.push_str(
            "  status = napi_create_array(env, &result);\n  if (status != napi_ok) return NULL;\n",
        );
        writeln!(
            output,
            "  for (size_t index = 0; index < {array}->length; ++index) {{ napi_value element_value;"
        )
        .map_err(write_error)?;
        let inner_c = node_c_type(program, inner);
        write_node_scalar_result_conversion(
            output,
            inner,
            &format!("(({inner_c} *){array}->pointer)[index]"),
            "element_value",
        )?;
        output.push_str("  status = napi_set_element(env, result, index, element_value); if (status != napi_ok) return NULL; }\n");
        return Ok(());
    }
    if let Type::Array(inner, length) = ty {
        if !matches!(inner.as_ref(), Type::Primitive(_)) {
            return Err(BuildError::Message(
                "Node fixed-array results currently require a primitive element type".into(),
            ));
        }
        output.push_str(
            "  status = napi_create_array(env, &result);\n  if (status != napi_ok) return NULL;\n",
        );
        writeln!(
            output,
            "  for (size_t index = 0; index < {length}; ++index) {{ napi_value element_value;"
        )
        .map_err(write_error)?;
        write_node_scalar_result_conversion(
            output,
            inner,
            &format!("({expression}).value[index]"),
            "element_value",
        )?;
        output.push_str("  status = napi_set_element(env, result, index, element_value); if (status != napi_ok) return NULL; }\n");
        return Ok(());
    }
    let (creator, cast) = match ty {
        Type::Primitive(PrimitiveType::Bool) => ("napi_create_bool", expression.to_owned()),
        Type::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Char,
        ) => ("napi_create_int32", format!("(int32_t){expression}")),
        Type::Primitive(PrimitiveType::I64 | PrimitiveType::Isize) => {
            ("napi_create_bigint_int64", format!("(int64_t){expression}"))
        }
        Type::Primitive(PrimitiveType::U64 | PrimitiveType::Usize) => (
            "napi_create_bigint_uint64",
            format!("(uint64_t){expression}"),
        ),
        Type::Primitive(PrimitiveType::I128 | PrimitiveType::U128) => {
            let signed = matches!(ty, Type::Primitive(PrimitiveType::I128));
            output.push_str("  uint64_t native_words[2];\n");
            writeln!(
                output,
                "  unsigned __int128 native_bits = (unsigned __int128)({expression});\n  native_words[0] = (uint64_t)native_bits;\n  native_words[1] = (uint64_t)(native_bits >> 64);\n  status = napi_create_bigint_words(env, {}, 2, native_words, &result);\n  if (status != napi_ok) return NULL;",
                if signed {
                    format!("(({expression}) < 0)")
                } else {
                    "0".into()
                }
            )
            .map_err(write_error)?;
            return Ok(());
        }
        Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            ("napi_create_double", format!("(double){expression}"))
        }
        Type::String | Type::Str | Type::Reference { .. } => (
            "napi_create_string_utf8",
            format!("(const char *){expression}"),
        ),
        _ => {
            return Err(BuildError::Message(
                "Node export result has no generated conversion".into(),
            ));
        }
    };
    if creator == "napi_create_string_utf8" {
        output.push_str("  status = napi_create_string_utf8(env, ");
        output.push_str(&cast);
        output.push_str(", NAPI_AUTO_LENGTH, &result);\n  if (status != napi_ok) return NULL;\n");
    } else {
        writeln!(
            output,
            "  status = {creator}(env, {cast}, &result);\n  if (status != napi_ok) return NULL;"
        )
        .map_err(write_error)?;
    }
    Ok(())
}

fn write_node_scalar_result_conversion(
    output: &mut String,
    ty: &Type,
    expression: &str,
    target: &str,
) -> Result<(), BuildError> {
    match ty {
        Type::Primitive(PrimitiveType::Bool) => {
            writeln!(
                output,
                "  status = napi_create_bool(env, (bool)({expression}), &{target}); if (status != napi_ok) return NULL;"
            )
            .map_err(write_error)?;
        }
        Type::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Char,
        ) => {
            writeln!(
                output,
                "  status = napi_create_int32(env, (int32_t)({expression}), &{target}); if (status != napi_ok) return NULL;"
            )
            .map_err(write_error)?;
        }
        Type::Primitive(PrimitiveType::I64 | PrimitiveType::Isize) => {
            writeln!(
                output,
                "  status = napi_create_bigint_int64(env, (int64_t)({expression}), &{target}); if (status != napi_ok) return NULL;"
            )
            .map_err(write_error)?;
        }
        Type::Primitive(PrimitiveType::U64 | PrimitiveType::Usize) => {
            writeln!(
                output,
                "  status = napi_create_bigint_uint64(env, (uint64_t)({expression}), &{target}); if (status != napi_ok) return NULL;"
            )
            .map_err(write_error)?;
        }
        Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64) => {
            writeln!(
                output,
                "  status = napi_create_double(env, (double)({expression}), &{target}); if (status != napi_ok) return NULL;"
            )
            .map_err(write_error)?;
        }
        _ => {
            return Err(BuildError::Message(
                "Node collection conversion requires a scalar element".into(),
            ));
        }
    }
    Ok(())
}

fn is_node_string(ty: &Type) -> bool {
    match ty {
        Type::String | Type::Str => true,
        Type::Reference { referent, .. } => matches!(referent.as_ref(), Type::String | Type::Str),
        _ => false,
    }
}

fn c_symbol(value: &str) -> Result<String, BuildError> {
    if value.is_empty()
        || !value.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric()
                    && (index > 0 || character.is_ascii_alphabetic())
        })
    {
        return Err(BuildError::Message(format!(
            "invalid exported symbol `{value}`"
        )));
    }
    Ok(value.to_owned())
}

fn c_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn write_error(_: std::fmt::Error) -> BuildError {
    BuildError::Message("failed to render native wrapper".into())
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
            if cfg!(target_os = "macos") {
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
