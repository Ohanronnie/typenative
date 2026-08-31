use crate::{BuildError, Project};
use std::fmt::Write;
use tn_hir::{Declaration, DefinitionData, Function, Method, Program, Type, Visibility};

/// Produces deterministic Markdown API documentation for the public declarations in a project.
///
/// The documentation is generated from resolved HIR, so names and types are rendered only after
/// module loading and semantic validation have succeeded.
///
/// # Errors
///
/// Returns diagnostics when the project cannot be loaded or semantically lowered, and a driver
/// error if a resolved declaration is unexpectedly missing.
pub fn generate_docs(project: &Project) -> Result<String, BuildError> {
    let graph = tn_hir::load_module_graph_with_jsx_runtime(
        &project.root,
        &project.entry,
        &super::standard_library_path(),
        project.config.jsx.as_ref().map(|jsx| jsx.runtime.clone()),
    )
    .map_err(|error| {
        if error.diagnostics().is_empty() {
            BuildError::Message(error.to_string())
        } else {
            BuildError::Diagnostics(error.diagnostics().to_vec())
        }
    })?;
    let program = tn_hir::lower_program(graph).map_err(BuildError::Diagnostics)?;
    let jsx_diagnostics = crate::validate_jsx_runtime(&program);
    if !jsx_diagnostics.is_empty() {
        return Err(BuildError::Diagnostics(jsx_diagnostics));
    }
    render_program(&program)
}

fn render_program(program: &Program) -> Result<String, BuildError> {
    let mut modules = program.graph.modules.iter().collect::<Vec<_>>();
    modules.sort_by(|left, right| left.path.cmp(&right.path));
    let mut output = String::from("# TypeNative API\n\n");
    let mut rendered_module = false;
    for module in modules {
        let mut declarations = module
            .declarations
            .iter()
            .filter(|declaration| declaration.exported && declaration.name.is_some())
            .collect::<Vec<_>>();
        declarations
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.kind.cmp(&right.kind)));
        if declarations.is_empty() {
            continue;
        }
        rendered_module = true;
        writeln!(output, "## {}", module_path(program, module.id)).map_err(write_error)?;
        output.push('\n');
        for declaration in declarations {
            render_declaration(program, declaration, &mut output)?;
            output.push('\n');
        }
    }
    if !rendered_module {
        output.push_str("No public declarations.\n");
    }
    Ok(output)
}

#[allow(clippy::too_many_lines)]
fn render_declaration(
    program: &Program,
    declaration: &Declaration,
    output: &mut String,
) -> Result<(), BuildError> {
    let Some(name) = declaration.name.as_deref() else {
        return Ok(());
    };
    let display_name = declaration_display_name(program, declaration);
    let Some(definition) = program.definition(declaration.id) else {
        return Err(BuildError::Message(format!(
            "documentation definition is missing for `{name}`"
        )));
    };
    match &definition.data {
        DefinitionData::Constant { ty, mutable_static } => {
            writeln!(
                output,
                "### `{name}`\n\n```typenative\n{} {}: {};\n```",
                if *mutable_static { "static" } else { "const" },
                name,
                type_display(program, ty),
            )
            .map_err(write_error)?;
        }
        DefinitionData::TypeAlias(ty) => {
            writeln!(
                output,
                "### `{display_name}`\n\n```typenative\nexport type {display_name} = {};\n```",
                type_display(program, ty),
            )
            .map_err(write_error)?;
        }
        DefinitionData::Function(function) => {
            render_named_function(program, name, function, output)?;
        }
        DefinitionData::Struct { fields, .. } => {
            writeln!(
                output,
                "### `{display_name}`\n\n```typenative\nexport struct {display_name} {{"
            )
            .map_err(write_error)?;
            for field in fields {
                if field.visibility != Visibility::Private {
                    writeln!(
                        output,
                        "  {}{}: {},",
                        if field.readonly { "readonly " } else { "" },
                        field.name,
                        type_display(program, &field.ty),
                    )
                    .map_err(write_error)?;
                }
            }
            output.push_str("}\n```");
        }
        DefinitionData::Enum { variants, .. } => {
            writeln!(
                output,
                "### `{display_name}`\n\n```typenative\nexport enum {display_name} {{"
            )
            .map_err(write_error)?;
            for variant in variants {
                write!(output, "  {}", variant.name).map_err(write_error)?;
                if !variant.fields.is_empty() {
                    output.push('(');
                    for (index, field) in variant.fields.iter().enumerate() {
                        if index > 0 {
                            output.push_str(", ");
                        }
                        write!(output, "{}", type_display(program, &field.ty))
                            .map_err(write_error)?;
                    }
                    output.push(')');
                }
                output.push_str(",\n");
            }
            output.push_str("}\n```");
        }
        DefinitionData::Interface { methods, .. } => {
            render_interface(program, &display_name, methods, output)?;
        }
        DefinitionData::Class {
            base,
            fields,
            constructor,
            methods,
            is_abstract,
            ..
        } => {
            writeln!(
                output,
                "### `{display_name}`\n\n```typenative\nexport class {display_name}"
            )
            .map_err(write_error)?;
            if let Some(base) = base {
                write!(output, " extends {}", declaration_name(program, *base))
                    .map_err(write_error)?;
            }
            if *is_abstract {
                output.push_str(" abstract");
            }
            output.push_str(" {\n");
            for field in fields {
                if field.visibility != Visibility::Private {
                    writeln!(
                        output,
                        "  {}{}: {},",
                        if field.readonly { "readonly " } else { "" },
                        field.name,
                        type_display(program, &field.ty),
                    )
                    .map_err(write_error)?;
                }
            }
            if let Some(constructor) = constructor {
                render_method(program, constructor, output, "  ")?;
            }
            for method in methods {
                if method.visibility != Visibility::Private {
                    render_method(program, method, output, "  ")?;
                }
            }
            output.push_str("}\n```");
        }
        DefinitionData::Implementation {
            interface,
            target,
            methods,
            ..
        } => {
            writeln!(
                output,
                "### `impl {}`\n\n```typenative\nexport impl{} for {} {{",
                type_display(program, target),
                interface
                    .as_ref()
                    .map(|ty| format!(" {}", type_display(program, ty)))
                    .unwrap_or_default(),
                type_display(program, target),
            )
            .map_err(write_error)?;
            for method in methods {
                if method.visibility != Visibility::Private {
                    render_method(program, method, output, "  ")?;
                }
            }
            output.push_str("}\n```");
        }
        DefinitionData::Extern { functions } => {
            writeln!(
                output,
                "### `{display_name}`\n\n```typenative\ndeclare extern \"C\" {{"
            )
            .map_err(write_error)?;
            for function in functions {
                render_method(program, function, output, "  ")?;
            }
            output.push_str("}\n```");
        }
    }
    Ok(())
}

fn declaration_display_name(program: &Program, declaration: &Declaration) -> String {
    let Some(name) = declaration.name.as_deref() else {
        return String::new();
    };
    let Some(definition) = program.definition(declaration.id) else {
        return name.to_owned();
    };
    if definition.generics.is_empty() {
        return name.to_owned();
    }
    let parameters = definition
        .generics
        .iter()
        .map(|generic| generic.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}<{parameters}>")
}

fn render_named_function(
    program: &Program,
    name: &str,
    function: &Function,
    output: &mut String,
) -> Result<(), BuildError> {
    writeln!(output, "### `{name}`\n\n```typenative").map_err(write_error)?;
    writeln!(
        output,
        "{}\n```",
        function_signature(program, Some(name), function)
    )
    .map_err(write_error)
}

fn render_interface(
    program: &Program,
    name: &str,
    methods: &[Method],
    output: &mut String,
) -> Result<(), BuildError> {
    writeln!(
        output,
        "### `{name}`\n\n```typenative\nexport interface {name} {{"
    )
    .map_err(write_error)?;
    for method in methods {
        if method.visibility != Visibility::Private {
            render_method(program, method, output, "  ")?;
        }
    }
    output.push_str("}\n```");
    Ok(())
}

fn render_method(
    program: &Program,
    method: &Method,
    output: &mut String,
    indent: &str,
) -> Result<(), BuildError> {
    let signature = function_signature(program, Some(&method.name), &method.function);
    writeln!(output, "{indent}{signature}").map_err(write_error)
}

fn function_signature(program: &Program, name: Option<&str>, function: &Function) -> String {
    let mut output = String::new();
    if function.is_async {
        output.push_str("async ");
    }
    if function.is_unsafe {
        output.push_str("unsafe ");
    }
    output.push_str("function ");
    if let Some(name) = name {
        output.push_str(name);
    }
    if !function.generics.is_empty() {
        output.push('<');
        for (index, generic) in function.generics.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            output.push_str(&generic.name);
        }
        output.push('>');
    }
    output.push('(');
    for (index, parameter) in function.parameters.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        write!(
            output,
            "{}: {}",
            parameter.name,
            type_display(program, &parameter.ty)
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("): ");
    output.push_str(&type_display(program, &function.result));
    if !function.effects.is_empty() {
        output.push_str(" throws ");
        for (index, effect) in function.effects.iter().enumerate() {
            if index > 0 {
                output.push_str(" | ");
            }
            output.push_str(&declaration_name(program, *effect));
        }
    }
    output.push(';');
    output
}

fn type_display(program: &Program, ty: &Type) -> String {
    match ty {
        Type::Primitive(primitive) => format!("{primitive:?}").to_lowercase(),
        Type::String => "string".into(),
        Type::Str => "str".into(),
        Type::Promise { result, error, .. } => format!(
            "Promise<{}, {}>",
            type_display(program, result),
            type_display(program, error)
        ),
        Type::Nominal(declaration, arguments) => render_nominal(program, *declaration, arguments),
        Type::Optional(inner) => format!("{}?", type_display(program, inner)),
        Type::Union(alternatives) => alternatives
            .iter()
            .map(|alternative| type_display(program, alternative))
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Array(inner, length) => format!("[{}; {length}]", type_display(program, inner)),
        Type::Slice(inner) => format!("[{}]", type_display(program, inner)),
        Type::Tuple(elements) => format!(
            "({})",
            elements
                .iter()
                .map(|element| type_display(program, element))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Reference {
            mutable, referent, ..
        } => format!(
            "&{}{}",
            if *mutable { "mut " } else { "" },
            type_display(program, referent)
        ),
        Type::RawPointer { mutable, pointee } => format!(
            "*{} {}",
            if *mutable { "mut" } else { "const" },
            type_display(program, pointee)
        ),
        Type::Function(function) => function_signature(
            program,
            None,
            &Function {
                parameters: function
                    .parameters
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| tn_hir::Parameter {
                        name: format!("arg{index}"),
                        ty: ty.clone(),
                        pattern: tn_hir::BindingPattern::identifier(
                            format!("arg{index}"),
                            false,
                            tn_diagnostics::SourceSpan::new("<doc>", 0..0, ""),
                        ),
                        default: None,
                        span: tn_diagnostics::SourceSpan::new("<doc>", 0..0, ""),
                    })
                    .collect(),
                result: (*function.result).clone(),
                effects: function.effects.clone(),
                generics: function
                    .generics
                    .iter()
                    .map(|generic| tn_hir::GenericParameter {
                        name: generic.name.clone(),
                        namespace: generic.namespace,
                        bounds: generic.bounds.clone(),
                        span: tn_diagnostics::SourceSpan::new("<doc>", 0..0, ""),
                    })
                    .collect(),
                is_async: function.is_async,
                is_generator: false,
                is_unsafe: function.is_unsafe,
                body_start: 0,
                body_end: 0,
            },
        ),
        Type::Template(elements) => format!(
            "template<{}>",
            elements
                .iter()
                .map(|element| type_display(program, element))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::DynamicInterface(declaration, arguments) => {
            format!("dyn {}", render_nominal(program, *declaration, arguments))
        }
        Type::Generic(name) | Type::Lifetime(name) => name.clone(),
        Type::ErrorUnion(effects) => effects
            .iter()
            .map(|effect| declaration_name(program, *effect))
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Error => "<error>".into(),
        Type::Unknown => "unknown".into(),
    }
}

fn render_nominal(
    program: &Program,
    declaration: tn_hir::DeclarationId,
    arguments: &[Type],
) -> String {
    let mut output = declaration_name(program, declaration);
    if !arguments.is_empty() {
        output.push('<');
        output.push_str(
            &arguments
                .iter()
                .map(|argument| type_display(program, argument))
                .collect::<Vec<_>>()
                .join(", "),
        );
        output.push('>');
    }
    output
}

fn declaration_name(program: &Program, declaration: tn_hir::DeclarationId) -> String {
    program
        .graph
        .declaration(declaration)
        .and_then(|declaration| declaration.name.clone())
        .unwrap_or_else(|| format!("declaration-{}", declaration.0))
}

fn module_path(program: &Program, module: tn_hir::ModuleId) -> String {
    program.graph.module(module).map_or_else(
        || format!("module-{}", module.0),
        |module| module.path.display().to_string(),
    )
}

fn write_error(_: std::fmt::Error) -> BuildError {
    BuildError::Message("failed to render documentation".into())
}
