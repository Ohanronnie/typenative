//! Node-API export validation, wrapper MIR, and declaration generation.

use std::fmt::Write;
use tn_hir::{
    AttributeKind, DeclarationId, DefinitionData, FunctionType, Method, PrimitiveType, Program,
    ReceiverMode, Type, Visibility,
};
use tn_mir::Callable;

pub const NODE_API_MODULE_SUFFIX: &str = ".node";
pub const TYPESCRIPT_DECLARATION_SUFFIX: &str = ".d.ts";

/// The complete, resolved boundary description consumed by the LLVM Node-API backend.
///
/// This is deliberately a semantic plan rather than a source-rendering intermediate.  Native
/// callables remain identified by their `TypeNative` `Callable`; the backend assigns the stable
/// LLVM symbol and applies the ABI represented by the resolved `FunctionType`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgePlan {
    pub functions: Vec<BridgeFunction>,
    pub classes: Vec<BridgeClass>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeFunction {
    pub export_name: String,
    pub callable: Callable,
    pub signature: FunctionType,
    pub parameters: Vec<NodeType>,
    pub result: NodeType,
    pub errors: Vec<BridgeError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeError {
    pub declaration: DeclarationId,
    pub native: Type,
    pub name: String,
    pub fields: Vec<BridgeErrorField>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeErrorField {
    pub name: String,
    pub index: u32,
    pub ty: NodeType,
    pub message: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeClass {
    pub export_name: String,
    pub declaration: DeclarationId,
    pub constructor: Option<BridgeMethod>,
    pub methods: Vec<BridgeMethod>,
    pub drop: Option<Callable>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeMethod {
    pub name: String,
    pub callable: Callable,
    pub signature: FunctionType,
    pub parameters: Vec<NodeType>,
    pub result: NodeType,
    pub receiver: ReceiverMode,
    pub constructor: bool,
    pub errors: Vec<BridgeError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeType {
    pub native: Type,
    pub kind: NodeTypeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeTypeKind {
    Void,
    Scalar(PrimitiveType),
    String,
    Bytes,
    Promise {
        result: Box<NodeType>,
        errors: Vec<BridgeError>,
    },
    Optional(Box<NodeType>),
    Array {
        element: Box<NodeType>,
        fixed_length: Option<usize>,
        borrowed: bool,
    },
    Class(DeclarationId),
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NodeApiError {
    #[error("Node-API does not support TypeNative type `{0:?}`")]
    UnsupportedType(Type),
    #[error("exported Node function `{0}` is not a function definition")]
    NotAFunction(String),
    #[error("exported Node function `{0}` must be safe and non-generic")]
    InvalidFunction(String),
    #[error("exported Node class `{0}` is not constructible or has an invalid public member")]
    InvalidClass(String),
}

/// Resolves every exported Node function and class into one typed bridge plan.
///
/// The driver performs target-specific ABI validation before emission. This function owns the
/// semantic inventory so the LLVM backend never has to rediscover exports by scanning source or
/// rendering an intermediate C wrapper.
///
/// # Errors
///
/// Returns an error when an exported function or class has an unsupported signature or violates
/// the Node-API boundary rules.
pub fn build_bridge_plan(program: &Program) -> Result<BridgePlan, NodeApiError> {
    let mut functions = Vec::new();
    let mut classes = Vec::new();
    for definition in &program.definitions {
        let Some(declaration) = program.graph.declaration(definition.declaration) else {
            continue;
        };
        if !declaration
            .attributes
            .iter()
            .any(|attribute| attribute.kind == AttributeKind::Export)
        {
            continue;
        }
        match &definition.data {
            DefinitionData::Function(function) => {
                let name = exported_name(declaration);
                if !definition.generics.is_empty()
                    || !function.generics.is_empty()
                    || function.is_unsafe
                {
                    return Err(NodeApiError::InvalidFunction(name));
                }
                functions.push(BridgeFunction {
                    export_name: name,
                    callable: tn_mir::Callable::function(declaration.id),
                    signature: function_type(function),
                    parameters: function
                        .parameters
                        .iter()
                        .map(|parameter| node_type(program, &parameter.ty))
                        .collect::<Result<Vec<_>, _>>()?,
                    result: node_type(program, &function.result)?,
                    errors: bridge_errors(program, &function.effects)?,
                });
            }
            DefinitionData::Class {
                constructor,
                methods,
                is_abstract,
                ..
            } => {
                let name = exported_name(declaration);
                if *is_abstract || !definition.generics.is_empty() {
                    return Err(NodeApiError::InvalidClass(name));
                }
                let constructor = constructor
                    .as_ref()
                    .map(|method| bridge_method(program, declaration.id, method, true))
                    .transpose()?;
                let drop = methods
                    .iter()
                    .find(|method| method.name == "drop")
                    .filter(|method| method.function.effects.is_empty())
                    .map(|method| Callable {
                        declaration: declaration.id,
                        member: Some(method.id),
                    });
                let methods = methods
                    .iter()
                    .filter(|method| method.visibility == Visibility::Public)
                    .map(|method| bridge_method(program, declaration.id, method, false))
                    .collect::<Result<Vec<_>, _>>()?;
                classes.push(BridgeClass {
                    export_name: name,
                    declaration: declaration.id,
                    constructor,
                    methods,
                    drop,
                });
            }
            _ => return Err(NodeApiError::NotAFunction(exported_name(declaration))),
        }
    }
    functions.sort_by(|left, right| left.export_name.cmp(&right.export_name));
    classes.sort_by(|left, right| left.export_name.cmp(&right.export_name));
    Ok(BridgePlan { functions, classes })
}

fn bridge_method(
    program: &Program,
    declaration: DeclarationId,
    method: &Method,
    constructor: bool,
) -> Result<BridgeMethod, NodeApiError> {
    if method.visibility != Visibility::Public
        || method.is_abstract
        || !method.function.generics.is_empty()
        || method.function.is_unsafe
    {
        return Err(NodeApiError::InvalidClass(method.name.clone()));
    }
    Ok(BridgeMethod {
        name: method.name.clone(),
        callable: Callable {
            declaration,
            member: Some(method.id),
        },
        signature: function_type(&method.function),
        parameters: method
            .function
            .parameters
            .iter()
            .map(|parameter| node_type(program, &parameter.ty))
            .collect::<Result<Vec<_>, _>>()?,
        result: node_type(program, &method.function.result)?,
        receiver: method.receiver,
        constructor,
        errors: bridge_errors(program, &method.function.effects)?,
    })
}

fn node_type(program: &Program, ty: &Type) -> Result<NodeType, NodeApiError> {
    let kind = match ty {
        Type::Primitive(primitive) => match primitive {
            PrimitiveType::Void => NodeTypeKind::Void,
            primitive => NodeTypeKind::Scalar(primitive.clone()),
        },
        Type::String | Type::Str => NodeTypeKind::String,
        Type::Promise { result, effects } => NodeTypeKind::Promise {
            result: Box::new(node_type(program, result)?),
            errors: bridge_errors(program, effects)?,
        },
        Type::Optional(inner) => NodeTypeKind::Optional(Box::new(node_type(program, inner)?)),
        Type::Array(inner, length) => NodeTypeKind::Array {
            element: Box::new(node_type(program, inner)?),
            fixed_length: Some(
                usize::try_from(*length).map_err(|_| NodeApiError::UnsupportedType(ty.clone()))?,
            ),
            borrowed: false,
        },
        Type::Slice(inner) => NodeTypeKind::Array {
            element: Box::new(node_type(program, inner)?),
            fixed_length: None,
            borrowed: true,
        },
        Type::Reference { referent, .. } => {
            let mut referenced = node_type(program, referent)?;
            if let NodeTypeKind::Array { borrowed, .. } = &mut referenced.kind {
                *borrowed = true;
            }
            referenced.kind
        }
        Type::Nominal(declaration, arguments) => {
            let name = program
                .graph
                .declaration(*declaration)
                .and_then(|declaration| declaration.name.as_deref());
            if name == Some("Bytes") && arguments.is_empty() {
                NodeTypeKind::Bytes
            } else if name == Some("Array") && arguments.len() == 1 {
                NodeTypeKind::Array {
                    element: Box::new(node_type(program, &arguments[0])?),
                    fixed_length: None,
                    borrowed: false,
                }
            } else {
                NodeTypeKind::Class(*declaration)
            }
        }
        unsupported => return Err(NodeApiError::UnsupportedType(unsupported.clone())),
    };
    Ok(NodeType {
        native: ty.clone(),
        kind,
    })
}

fn bridge_errors(
    program: &Program,
    effects: &[DeclarationId],
) -> Result<Vec<BridgeError>, NodeApiError> {
    effects
        .iter()
        .map(|declaration| bridge_error(program, *declaration))
        .collect()
}

fn bridge_error(
    program: &Program,
    declaration: DeclarationId,
) -> Result<BridgeError, NodeApiError> {
    let name = program
        .graph
        .declaration(declaration)
        .and_then(|declaration| declaration.name.clone())
        .unwrap_or_else(|| format!("TypeNativeError{}", declaration.0));
    let native = Type::Nominal(declaration, Vec::new());
    let mut fields = Vec::new();
    if let Some(definition) = program.definition(declaration) {
        match &definition.data {
            DefinitionData::Struct { fields: stored, .. } => {
                for (index, field) in stored
                    .iter()
                    .filter(|field| field.visibility == Visibility::Public)
                    .enumerate()
                {
                    fields.push(BridgeErrorField {
                        name: field.name.clone(),
                        index: u32::try_from(index)
                            .map_err(|_| NodeApiError::UnsupportedType(native.clone()))?,
                        ty: node_type(program, &field.ty)?,
                        message: field.name == "message",
                    });
                }
            }
            DefinitionData::Class {
                base,
                fields: stored,
                ..
            } => {
                let base_fields = base.map_or(0, |base| class_field_count(program, base));
                for (index, field) in stored
                    .iter()
                    .filter(|field| field.visibility == Visibility::Public)
                    .enumerate()
                {
                    let index = base_fields
                        .checked_add(index)
                        .and_then(|index| index.checked_add(1))
                        .and_then(|index| u32::try_from(index).ok())
                        .ok_or_else(|| NodeApiError::UnsupportedType(native.clone()))?;
                    fields.push(BridgeErrorField {
                        name: field.name.clone(),
                        index,
                        ty: node_type(program, &field.ty)?,
                        message: field.name == "message",
                    });
                }
            }
            DefinitionData::Enum { .. }
            | DefinitionData::Function(_)
            | DefinitionData::Constant { .. }
            | DefinitionData::TypeAlias(_)
            | DefinitionData::Interface { .. }
            | DefinitionData::Implementation { .. }
            | DefinitionData::Extern { .. } => {}
        }
    }
    Ok(BridgeError {
        declaration,
        native,
        name,
        fields,
    })
}

fn class_field_count(program: &Program, declaration: DeclarationId) -> usize {
    let Some(DefinitionData::Class { base, fields, .. }) = program
        .definition(declaration)
        .map(|definition| &definition.data)
    else {
        return 0;
    };
    base.map_or(0, |base| class_field_count(program, base)) + fields.len()
}

fn function_type(function: &tn_hir::Function) -> FunctionType {
    FunctionType {
        parameters: function
            .parameters
            .iter()
            .map(|parameter| parameter.ty.clone())
            .collect(),
        result: Box::new(function.result.clone()),
        effects: function.effects.clone(),
        generics: Vec::new(),
        is_async: function.is_async && !function.is_generator,
        is_unsafe: function.is_unsafe,
    }
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
                .any(|attribute| attribute.kind == AttributeKind::Export)
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
                .any(|attribute| attribute.kind == AttributeKind::Export)
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
        .find(|attribute| attribute.kind == AttributeKind::Export)
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
