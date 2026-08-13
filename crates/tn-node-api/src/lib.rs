//! Node-API export validation, wrapper MIR, and declaration generation.

use std::fmt::Write;
use tn_hir::{DefinitionData, Method, PrimitiveType, Program, Type, Visibility};

pub const NODE_API_MODULE_SUFFIX: &str = ".node";
pub const TYPESCRIPT_DECLARATION_SUFFIX: &str = ".d.ts";

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NodeApiError {
    #[error("Node-API does not support TypeNative type `{0:?}`")]
    UnsupportedType(Type),
    #[error("exported Node function `{0}` is not a function definition")]
    NotAFunction(String),
    #[error("exported Node function `{0}` must be safe, non-generic, and synchronous")]
    InvalidFunction(String),
    #[error("exported Node class `{0}` is not constructible or has an invalid public member")]
    InvalidClass(String),
}

/// Maps a resolved `TypeNative` type to its TypeScript Node-API boundary type.
///
/// # Errors
///
/// Returns [`NodeApiError::UnsupportedType`] when the type has no documented Node-API mapping.
pub fn typescript_type(program: &Program, ty: &Type, result: bool) -> Result<String, NodeApiError> {
    match ty {
        Type::Primitive(primitive) => Ok(match primitive {
            PrimitiveType::Bool => "boolean",
            PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::I128
            | PrimitiveType::U128
            | PrimitiveType::Isize
            | PrimitiveType::Usize => "bigint",
            PrimitiveType::Void | PrimitiveType::Never if result => "void",
            _ => "number",
        }
        .into()),
        Type::String | Type::Str => Ok("string".into()),
        Type::Reference { referent, .. }
            if matches!(referent.as_ref(), Type::String | Type::Str) =>
        {
            Ok("string".into())
        }
        Type::Reference { referent, .. } => typescript_type(program, referent, result),
        Type::Optional(inner) => Ok(format!(
            "{} | undefined",
            typescript_type(program, inner, result)?
        )),
        Type::Array(inner, _) | Type::Slice(inner) => Ok(format!(
            "Array<{}>",
            typescript_type(program, inner, false)?
        )),
        Type::Promise { result: inner, .. } => Ok(format!(
            "Promise<{}>",
            typescript_type(program, inner, true)?
        )),
        Type::Nominal(declaration, arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            if name == Some("Bytes") {
                Ok("Uint8Array".into())
            } else if name == Some("Array") && arguments.len() == 1 {
                Ok(format!(
                    "Array<{}>",
                    typescript_type(program, &arguments[0], false)?
                ))
            } else {
                Ok(name.unwrap_or("unknown").into())
            }
        }
        unsupported => Err(NodeApiError::UnsupportedType(unsupported.clone())),
    }
}

/// Generates TypeScript declarations for every resolved `@Export` function.
///
/// # Errors
///
/// Returns a structured error when an exported declaration is not a valid Node-API function or
/// contains a type without a documented mapping.
pub fn generate_declarations(program: &Program) -> Result<String, NodeApiError> {
    let mut functions = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let declaration = program.graph.declaration(definition.declaration)?;
            declaration
                .attributes
                .iter()
                .any(|attribute| attribute.name == "Export")
                .then_some((declaration, definition))
        })
        .collect::<Vec<_>>();
    functions.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    let mut output = String::from("// Generated from the resolved TypeNative export model.\n\n");
    for (declaration, definition) in functions {
        let name = exported_name(declaration);
        let DefinitionData::Function(function) = &definition.data else {
            if matches!(definition.data, DefinitionData::Class { .. }) {
                continue;
            }
            return Err(NodeApiError::NotAFunction(name));
        };
        if !definition.generics.is_empty() || !function.generics.is_empty() || function.is_unsafe {
            return Err(NodeApiError::InvalidFunction(name));
        }
        let parameters = function
            .parameters
            .iter()
            .enumerate()
            .map(|(index, parameter)| {
                Ok(format!(
                    "arg{index}: {}",
                    typescript_type(program, &parameter.ty, false)?
                ))
            })
            .collect::<Result<Vec<_>, NodeApiError>>()?
            .join(", ");
        writeln!(
            output,
            "export function {name}({parameters}): {};",
            typescript_type(program, &function.result, true)?
        )
        .expect("writing to String cannot fail");
    }

    let mut classes = program
        .definitions
        .iter()
        .filter_map(|definition| {
            let declaration = program.graph.declaration(definition.declaration)?;
            let DefinitionData::Class { .. } = &definition.data else {
                return None;
            };
            declaration
                .attributes
                .iter()
                .any(|attribute| attribute.name == "Export")
                .then_some((declaration, definition))
        })
        .collect::<Vec<_>>();
    classes.sort_by(|left, right| left.0.name.cmp(&right.0.name));
    for (declaration, definition) in classes {
        let name = exported_name(declaration);
        let DefinitionData::Class {
            constructor,
            methods,
            is_abstract,
            ..
        } = &definition.data
        else {
            unreachable!("class list contains only class definitions");
        };
        if *is_abstract || !definition.generics.is_empty() {
            return Err(NodeApiError::InvalidClass(name));
        }
        let Some(constructor) = constructor else {
            writeln!(output, "export class {name} {{\n  constructor();")
                .expect("writing to String cannot fail");
            write_class_methods(program, &mut output, methods)?;
            output.push_str("}\n\n");
            continue;
        };
        validate_method(name.as_str(), constructor)?;
        let parameters = method_parameters(program, constructor)?;
        writeln!(
            output,
            "export class {name} {{\n  constructor({parameters});"
        )
        .expect("writing to String cannot fail");
        write_class_methods(program, &mut output, methods)?;
        output.push_str("}\n\n");
    }
    Ok(output)
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

fn validate_method(class_name: &str, method: &Method) -> Result<(), NodeApiError> {
    if method.visibility != Visibility::Public
        || method.is_abstract
        || !method.function.generics.is_empty()
        || method.function.is_unsafe
    {
        return Err(NodeApiError::InvalidClass(class_name.into()));
    }
    Ok(())
}

fn method_parameters(program: &Program, method: &Method) -> Result<String, NodeApiError> {
    method
        .function
        .parameters
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            Ok(format!(
                "arg{index}: {}",
                typescript_type(program, &parameter.ty, false)?
            ))
        })
        .collect::<Result<Vec<_>, NodeApiError>>()
        .map(|parameters| parameters.join(", "))
}

fn write_class_methods(
    program: &Program,
    output: &mut String,
    methods: &[Method],
) -> Result<(), NodeApiError> {
    for method in methods
        .iter()
        .filter(|method| method.visibility == Visibility::Public)
    {
        validate_method("class", method)?;
        let parameters = method_parameters(program, method)?;
        let prefix = if method.receiver == tn_hir::ReceiverMode::Static {
            "static "
        } else {
            ""
        };
        writeln!(
            output,
            "  {prefix}{}({parameters}): {};",
            method.name,
            typescript_type(program, &method.function.result, true)?
        )
        .expect("writing to String cannot fail");
    }
    Ok(())
}
