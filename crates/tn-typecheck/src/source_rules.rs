use crate::CheckResult;
use std::collections::BTreeMap;
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_hir::{
    DeclarationKind, DefinitionData, FunctionType, PrimitiveType, Program, Type, Visibility,
};
use tn_syntax::{Token, TokenKind, lex};

#[allow(clippy::too_many_lines)]
pub fn check_source_rules(program: &Program) -> CheckResult {
    let mut diagnostics = Vec::new();
    let reserved = [
        "bool",
        "i8",
        "i16",
        "i32",
        "i64",
        "i128",
        "isize",
        "u8",
        "u16",
        "u32",
        "u64",
        "u128",
        "usize",
        "number",
        "f32",
        "f64",
        "char",
        "void",
        "never",
        "undefined",
        "string",
        "str",
        "Promise",
    ];
    let obsolete_public_types = [
        "Option", "Result", "Vec", "VecDeque", "HashMap", "HashSet", "BTreeMap", "BTreeSet",
    ];
    for module in &program.graph.modules {
        for declaration in &module.declarations {
            if declaration
                .name
                .as_deref()
                .is_some_and(|name| reserved.contains(&name))
            {
                diagnostics.push(diag(
                    "RESOLVE_PREDECLARED_NAME_REBOUND",
                    "predeclared core names cannot be rebound",
                    &declaration.span,
                    "choose a different declaration name",
                ));
            }
            if declaration.kind.namespace() == Some(tn_hir::Namespace::Type)
                && declaration
                    .name
                    .as_deref()
                    .is_some_and(|name| obsolete_public_types.contains(&name))
            {
                diagnostics.push(diag(
                    "TYPE_OBSOLETE_PUBLIC_TYPE",
                    "obsolete public type names are not part of canonical TypeNative",
                    &declaration.span,
                    "use the canonical optional or collection type name",
                ));
            }
        }
        for import in &module.imports {
            if let tn_hir::ImportClause::Named(names) = &import.clause {
                for name in names {
                    if obsolete_public_types.contains(&name.imported.as_str()) {
                        diagnostics.push(diag(
                            "TYPE_OBSOLETE_PUBLIC_TYPE",
                            format!(
                                "obsolete public type `{}` cannot be imported",
                                name.imported
                            ),
                            &name.span,
                            "import the canonical optional or collection type name",
                        ));
                    }
                }
            }
        }
        let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
        check_attributes(program, module, &lexed.tokens, &mut diagnostics);
        check_decorator_signatures(program, module, &mut diagnostics);
        check_constant_initializers(program, module.id, &lexed.tokens, &mut diagnostics);
    }
    for definition in &program.definitions {
        check_public_api(program, definition, &mut diagnostics);
        match &definition.data {
            DefinitionData::Struct {
                fields, methods, ..
            } => {
                for field in fields {
                    if field.visibility == Visibility::Protected {
                        diagnostics.push(diag(
                            "TYPE_PROTECTED_OUTSIDE_CLASS",
                            "protected visibility is valid only for class members",
                            &field.span,
                            "use public or private visibility",
                        ));
                    }
                }
                for method in methods {
                    check_function_body(
                        program,
                        definition.declaration,
                        &method.function,
                        &mut diagnostics,
                    );
                }
            }
            DefinitionData::Function(function) => {
                check_function_body(program, definition.declaration, function, &mut diagnostics);
            }
            DefinitionData::Enum { methods, .. }
            | DefinitionData::Class { methods, .. }
            | DefinitionData::Interface { methods, .. }
            | DefinitionData::Implementation { methods, .. } => {
                for method in methods {
                    check_function_body(
                        program,
                        definition.declaration,
                        &method.function,
                        &mut diagnostics,
                    );
                }
            }
            DefinitionData::Extern { functions } => {
                for function in functions {
                    if !function.function.generics.is_empty()
                        || function.function.is_async
                        || !function.function.effects.is_empty()
                        || !is_c_abi_type(program, &function.function.result)
                        || function
                            .function
                            .parameters
                            .iter()
                            .any(|parameter| !is_c_abi_type(program, &parameter.ty))
                    {
                        diagnostics.push(diag(
                            "TYPE_INVALID_C_ABI_SIGNATURE",
                            format!(
                                "foreign function `{}` uses a type or effect that is not C-compatible",
                                function.name
                            ),
                            &function.span,
                            "use fixed-width primitives, raw pointers, C-repr aggregates, or a C function pointer",
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    CheckResult { diagnostics }
}

#[allow(clippy::too_many_lines)]
fn check_public_api(
    program: &Program,
    definition: &tn_hir::Definition,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(declaration) = program.graph.declaration(definition.declaration) else {
        return;
    };
    // Every module shipped by the standard library or runtime is a trusted implementation
    // boundary.  Its raw operations are explicitly marked `unsafe` (or are C/OS FFI
    // declarations); user modules must make the same boundary explicit instead of exporting
    // pointers or static borrows from safe APIs.
    let is_bundled = program
        .graph
        .module(declaration.module)
        .is_some_and(|module| {
            module.path.starts_with(&program.graph.standard_library)
                || program
                    .graph
                    .runtime_root
                    .as_ref()
                    .is_some_and(|root| module.path.starts_with(root))
        });
    if is_bundled {
        return;
    }
    if declaration.exported
        && let DefinitionData::Function(function) = &definition.data
    {
        check_public_signature(
            diagnostics,
            &declaration.span,
            function.is_unsafe,
            &function
                .parameters
                .iter()
                .map(|parameter| parameter.ty.clone())
                .collect::<Vec<_>>(),
            &function.result,
        );
    }
    let methods = match &definition.data {
        DefinitionData::Struct {
            fields, methods, ..
        } => {
            if declaration.exported {
                for field in fields
                    .iter()
                    .filter(|field| field.visibility == Visibility::Public)
                {
                    if contains_raw_pointer(&field.ty) {
                        diagnostics.push(diag(
                            "TYPE_PUBLIC_RAW_POINTER",
                            "safe public fields cannot expose raw pointers",
                            &field.span,
                            "store the pointer behind a typed abstraction",
                        ));
                    }
                    if contains_static_borrow(&field.ty) {
                        diagnostics.push(diag(
                            "TYPE_PUBLIC_STATIC_BORROW",
                            "safe public fields cannot expose static borrows",
                            &field.span,
                            "store an owned value instead",
                        ));
                    }
                }
            }
            methods.as_slice()
        }
        DefinitionData::Class {
            fields, methods, ..
        } => {
            if declaration.exported {
                for field in fields
                    .iter()
                    .filter(|field| field.visibility == Visibility::Public)
                {
                    if contains_raw_pointer(&field.ty) {
                        diagnostics.push(diag(
                            "TYPE_PUBLIC_RAW_POINTER",
                            "safe public fields cannot expose raw pointers",
                            &field.span,
                            "store the pointer behind a typed abstraction",
                        ));
                    }
                    if contains_static_borrow(&field.ty) {
                        diagnostics.push(diag(
                            "TYPE_PUBLIC_STATIC_BORROW",
                            "safe public fields cannot expose static borrows",
                            &field.span,
                            "store an owned value instead",
                        ));
                    }
                }
            }
            methods.as_slice()
        }
        DefinitionData::Enum { methods, .. }
        | DefinitionData::Interface { methods }
        | DefinitionData::Implementation { methods, .. }
        | DefinitionData::Extern { functions: methods } => methods.as_slice(),
        _ => &[],
    };
    for method in methods
        .iter()
        .filter(|method| method.visibility == Visibility::Public)
    {
        if declaration.exported {
            check_public_signature(
                diagnostics,
                &method.span,
                method.function.is_unsafe,
                &method
                    .function
                    .parameters
                    .iter()
                    .map(|parameter| parameter.ty.clone())
                    .collect::<Vec<_>>(),
                &method.function.result,
            );
        }
    }
}

fn check_public_signature(
    diagnostics: &mut Vec<Diagnostic>,
    span: &SourceSpan,
    unsafe_api: bool,
    parameters: &[Type],
    result: &Type,
) {
    if !unsafe_api && (parameters.iter().any(contains_raw_pointer) || contains_raw_pointer(result))
    {
        diagnostics.push(diag(
            "TYPE_PUBLIC_RAW_POINTER",
            "safe public APIs cannot expose raw pointers",
            span,
            "mark the declaration unsafe or wrap the pointer in a typed value",
        ));
    }
    if !unsafe_api
        && (parameters.iter().any(contains_static_borrow) || contains_static_borrow(result))
    {
        diagnostics.push(diag(
            "TYPE_PUBLIC_STATIC_BORROW",
            "safe public APIs cannot expose static borrows",
            span,
            "return an owned value or mark the declaration unsafe",
        ));
    }
}

fn contains_raw_pointer(ty: &Type) -> bool {
    match ty {
        Type::RawPointer { .. } => true,
        Type::Reference { referent, .. }
        | Type::Optional(referent)
        | Type::Array(referent, _)
        | Type::Slice(referent) => contains_raw_pointer(referent),
        Type::Function(function) => {
            function.parameters.iter().any(contains_raw_pointer)
                || contains_raw_pointer(&function.result)
        }
        Type::Tuple(elements) | Type::Template(elements) => {
            elements.iter().any(contains_raw_pointer)
        }
        Type::Nominal(_, arguments) | Type::DynamicInterface(_, arguments) => {
            arguments.iter().any(contains_raw_pointer)
        }
        Type::Promise { result, error, .. } => {
            contains_raw_pointer(result) || contains_raw_pointer(error)
        }
        Type::ErrorUnion(_)
        | Type::Primitive(_)
        | Type::Str
        | Type::String
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::Error
        | Type::Unknown => false,
    }
}

fn contains_static_borrow(ty: &Type) -> bool {
    match ty {
        Type::Reference {
            lifetime, referent, ..
        } => lifetime == "static" || contains_static_borrow(referent),
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            contains_static_borrow(inner)
        }
        Type::Function(function) => {
            function.parameters.iter().any(contains_static_borrow)
                || contains_static_borrow(&function.result)
        }
        Type::Tuple(elements) | Type::Template(elements) => {
            elements.iter().any(contains_static_borrow)
        }
        Type::Nominal(_, arguments) | Type::DynamicInterface(_, arguments) => {
            arguments.iter().any(contains_static_borrow)
        }
        Type::Promise { result, error, .. } => {
            contains_static_borrow(result) || contains_static_borrow(error)
        }
        Type::RawPointer { pointee, .. } => contains_static_borrow(pointee),
        _ => false,
    }
}

pub fn is_c_abi_type(program: &Program, ty: &Type) -> bool {
    match ty {
        Type::Primitive(primitive) => matches!(
            primitive,
            PrimitiveType::Bool
                | PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::I64
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32
                | PrimitiveType::U64
                | PrimitiveType::F32
                | PrimitiveType::F64
                | PrimitiveType::Void
        ),
        Type::RawPointer { pointee, .. } => is_c_abi_type(program, pointee),
        Type::Function(function) => is_c_function_type(program, function),
        Type::Nominal(declaration, arguments) => {
            if !arguments.is_empty() {
                return false;
            }
            if is_declared_c_scalar(program, *declaration) {
                return true;
            }
            let Some(_item) = program.graph.declaration(*declaration) else {
                return false;
            };
            if !program.has_c_layout(*declaration) {
                return false;
            }
            let Some(definition) = program.definition(*declaration) else {
                return false;
            };
            match &definition.data {
                DefinitionData::Struct { fields, .. } => {
                    fields.iter().all(|field| is_c_abi_type(program, &field.ty))
                }
                DefinitionData::Enum { variants, .. } => {
                    variants.iter().all(|variant| variant.fields.is_empty())
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn is_c_function_type(program: &Program, function: &FunctionType) -> bool {
    function.generics.is_empty()
        && function.effects.is_empty()
        && !function.is_async
        && function.is_unsafe
        && function
            .parameters
            .iter()
            .all(|parameter| is_c_abi_type(program, parameter))
        && is_c_abi_type(program, &function.result)
}

fn is_declared_c_scalar(program: &Program, declaration: tn_hir::DeclarationId) -> bool {
    let Some(item) = program.graph.declaration(declaration) else {
        return false;
    };
    if !program.graph.is_bundled_module(item.module, "ffi.tn") {
        return false;
    }
    let Some(name) = item.name.as_deref() else {
        return false;
    };
    if !matches!(name, "c_char" | "c_int" | "c_uint" | "c_size" | "c_ssize") {
        return false;
    }
    let Some(DefinitionData::TypeAlias(Type::Primitive(primitive))) = program
        .definition(declaration)
        .map(|definition| &definition.data)
    else {
        return false;
    };
    matches!(
        (name, primitive),
        ("c_char", PrimitiveType::I8)
            | ("c_int", PrimitiveType::I32)
            | ("c_uint", PrimitiveType::U32)
            | ("c_size", PrimitiveType::Usize)
            | ("c_ssize", PrimitiveType::Isize)
    )
}

fn check_attributes(
    program: &Program,
    module: &tn_hir::Module,
    tokens: &[Token],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let source = module.source.as_str();
    let significant = tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let compiler_owned = [
        "Copy",
        "Clone",
        "Drop",
        "Conform",
        "Sealed",
        "Layout",
        "Export",
        "Intrinsic",
        "Inline",
        "Test",
        "Expand",
    ];
    for (index, token) in significant.iter().enumerate() {
        if token.kind != TokenKind::At {
            continue;
        }
        let Some(name) = significant.get(index + 1) else {
            continue;
        };
        let text = &source[name.range.clone()];
        if compiler_owned.contains(&text) {
            diagnostics.push(source_diag(
                "TYPE_UNKNOWN_ATTRIBUTE",
                format!("compiler-owned attribute `@{text}` is not part of canonical TypeNative"),
                name,
                source,
                &module.path.to_string_lossy(),
                "use structural semantics, an ordinary declaration, or the canonical keyword syntax",
            ));
        } else if name.kind != TokenKind::Export && !user_decorator_exists(program, module, text) {
            diagnostics.push(source_diag(
                "TYPE_UNKNOWN_ATTRIBUTE",
                format!("user-defined decorator `@{text}` is not declared"),
                name,
                source,
                &module.path.to_string_lossy(),
                "declare a decorator function and apply it by name",
            ));
        }
    }
}

fn user_decorator_exists(program: &Program, module: &tn_hir::Module, name: &str) -> bool {
    resolve_user_decorator(program, module, name).is_some()
}

fn resolve_user_decorator<'program>(
    program: &'program Program,
    module: &tn_hir::Module,
    name: &str,
) -> Option<&'program tn_hir::Function> {
    if module.declarations.iter().any(|declaration| {
        declaration.kind == DeclarationKind::Function && declaration.name.as_deref() == Some(name)
    }) {
        let declaration = module.declarations.iter().find(|declaration| {
            declaration.kind == DeclarationKind::Function
                && declaration.name.as_deref() == Some(name)
        })?;
        return program
            .definition(declaration.id)
            .and_then(|definition| match &definition.data {
                DefinitionData::Function(function) => Some(function),
                _ => None,
            });
    }
    module.imports.iter().find_map(|import| {
        let tn_hir::ImportClause::Named(names) = &import.clause else {
            return None;
        };
        let imported = names.iter().find(|item| item.local == name)?;
        program.graph.module(import.target).and_then(|target| {
            target
                .declarations
                .iter()
                .find(|declaration| {
                    declaration.kind == DeclarationKind::Function
                        && declaration.exported
                        && declaration.name.as_deref() == Some(imported.imported.as_str())
                })
                .and_then(|declaration| program.definition(declaration.id))
                .and_then(|definition| match &definition.data {
                    DefinitionData::Function(function) => Some(function),
                    _ => None,
                })
        })
    })
}

#[allow(clippy::too_many_lines)]
fn check_decorator_signatures(
    program: &Program,
    module: &tn_hir::Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for declaration in &module.declarations {
        // Decorators are deliberately limited to class/struct elements.  A declaration-level
        // attribute cannot safely manufacture a new nominal type or alter a top-level function's
        // ABI, so report it instead of silently carrying metadata that no backend consumes.
        for attribute in &declaration.attributes {
            diagnostics.push(diag(
                "TYPE_UNSUPPORTED_DECORATOR_TARGET",
                format!(
                    "decorator `@{}` may only be applied to a class or struct member",
                    attribute.name
                ),
                &attribute.span,
                "move the decorator to a supported method or constructor",
            ));
            let Some(function) = resolve_user_decorator(program, module, &attribute.name) else {
                continue;
            };
            let valid_arity = matches!(function.parameters.len(), 1 | 2);
            let accepts_target = function
                .parameters
                .first()
                .is_some_and(|parameter| parameter.ty == Type::Unknown);
            let returns_target = function.result == Type::Unknown;
            if valid_arity
                && accepts_target
                && returns_target
                && function.effects.is_empty()
                && !function.is_async
                && !function.is_unsafe
            {
                continue;
            }
            diagnostics.push(diag(
                "TYPE_INVALID_DECORATOR_SIGNATURE",
                format!(
                    "decorator `@{}` is not callable for this declaration",
                    attribute.name
                ),
                &attribute.span,
                "use a synchronous safe `(unknown) => unknown` decorator, with an optional context parameter",
            ));
        }
        let Some(definition) = program.definition(declaration.id) else {
            continue;
        };
        let methods = match &definition.data {
            DefinitionData::Struct { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Interface { methods }
            | DefinitionData::Class { methods, .. }
            | DefinitionData::Implementation { methods, .. }
            | DefinitionData::Extern { functions: methods } => methods.as_slice(),
            _ => &[],
        };
        for method in methods {
            if method.name == "drop" {
                diagnostics.push(diag(
                    "TYPE_OBSOLETE_DROP_METHOD",
                    "source-visible `drop()` methods are not part of canonical TypeNative",
                    &method.span,
                    "use automatic destruction or implement [Symbol.dispose] for an external resource",
                ));
            }
            for attribute in &method.attributes {
                let Some(decorator) = resolve_user_decorator(program, module, &attribute.name)
                else {
                    continue;
                };
                let target = Type::Function(FunctionType {
                    parameters: method
                        .function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.ty.clone())
                        .collect(),
                    result: Box::new(method.function.result.clone()),
                    effects: method.function.effects.clone(),
                    generics: method
                        .function
                        .generics
                        .iter()
                        .map(|parameter| tn_hir::GenericConstraint {
                            name: parameter.name.clone(),
                            namespace: parameter.namespace,
                            bounds: parameter.bounds.clone(),
                        })
                        .collect(),
                    is_async: method.function.is_async,
                    is_unsafe: method.function.is_unsafe,
                });
                let valid_context = decorator.parameters.get(1).is_none_or(|parameter| {
                    nominal_name(program, &parameter.ty) == Some("ClassMethodDecoratorContext")
                });
                let valid = matches!(decorator.parameters.len(), 1 | 2)
                    && decorator
                        .parameters
                        .first()
                        .is_some_and(|parameter| parameter.ty == target)
                    && decorator.result == target
                    && valid_context
                    && decorator.effects.is_empty()
                    && !decorator.is_async
                    && !decorator.is_unsafe
                    && method.function.effects.is_empty()
                    && !method.function.is_async
                    && !method.function.is_generator;
                if !valid {
                    diagnostics.push(diag(
                        "TYPE_INVALID_DECORATOR_SIGNATURE",
                        format!(
                            "decorator `@{}` cannot wrap method `{}`",
                            attribute.name, method.name
                        ),
                        &attribute.span,
                        "accept and return the exact method function type, with an optional ClassMethodDecoratorContext parameter",
                    ));
                }
            }
        }
    }
}

fn nominal_name<'a>(program: &'a Program, ty: &Type) -> Option<&'a str> {
    let Type::Nominal(declaration, _) = ty else {
        return None;
    };
    program
        .graph
        .declaration(*declaration)
        .and_then(|declaration| declaration.name.as_deref())
}

fn check_constant_initializers(
    program: &Program,
    module_id: tn_hir::ModuleId,
    tokens: &[Token],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(module) = program.graph.module(module_id) else {
        return;
    };
    for declaration in module.declarations.iter().filter(|declaration| {
        matches!(
            declaration.kind,
            DeclarationKind::Const | DeclarationKind::Static
        )
    }) {
        let significant = tokens.iter().filter(|token| {
            !token.kind.is_trivia()
                && token.range.start >= declaration.byte_start as usize
                && token.range.end <= declaration.byte_end as usize
        });
        let mut after_equal = false;
        for token in significant {
            if token.kind == TokenKind::Equal {
                after_equal = true;
                continue;
            }
            if after_equal
                && matches!(
                    token.kind,
                    TokenKind::New
                        | TokenKind::Await
                        | TokenKind::Throw
                        | TokenKind::Try
                        | TokenKind::Unsafe
                )
            {
                diagnostics.push(source_diag(
                    "TYPE_NON_CONSTANT_INITIALIZER",
                    "top-level initializer is not a constant expression",
                    token,
                    &module.source,
                    &module.path.to_string_lossy(),
                    "allocation, errors, suspension, and unsafe operations are not constant",
                ));
                break;
            }
        }
    }
}

fn check_function_body(
    program: &Program,
    owner: tn_hir::DeclarationId,
    function: &tn_hir::Function,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if function.body_start == 0 || function.body_end <= function.body_start {
        return;
    }
    let Some(declaration) = program.graph.declaration(owner) else {
        return;
    };
    let Some(module) = program.graph.module(declaration.module) else {
        return;
    };
    let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
    let tokens = lexed
        .tokens
        .iter()
        .filter(|token| {
            !token.kind.is_trivia()
                && token.range.start > function.body_start as usize
                && token.range.end < function.body_end as usize
        })
        .collect::<Vec<_>>();
    let mut variables = function
        .parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), parameter.ty.clone()))
        .collect::<BTreeMap<_, _>>();
    let parameters = function
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut moved = BTreeMap::<String, SourceSpan>::new();
    let mut loans = Vec::<Loan>::new();
    let last_uses = identifier_last_uses(&tokens, &module.source);
    let mut index = 0;
    while index < tokens.len() {
        loans.retain(|loan| {
            last_uses
                .get(&loan.binding)
                .is_some_and(|last| *last >= index)
        });
        let token = tokens[index];
        if matches!(token.kind, TokenKind::Const | TokenKind::Let) {
            index = check_local(
                &tokens,
                index,
                module,
                &mut variables,
                &mut loans,
                &last_uses,
                diagnostics,
            );
            continue;
        }
        if token.kind == TokenKind::Move
            && let Some(name) = tokens.get(index + 1)
            && name.kind == TokenKind::Identifier
        {
            check_move(name, module, &variables, &mut moved, diagnostics);
            index += 2;
            continue;
        } else if token.kind == TokenKind::Return
            && let Some(returned) = tokens.get(index + 1)
            && returned.kind == TokenKind::Identifier
        {
            check_returned_reference(returned, module, &parameters, &loans, diagnostics);
        } else if token.kind == TokenKind::Await {
            check_suspension_loans(token, module, &parameters, &loans, diagnostics);
        } else if token.kind == TokenKind::Identifier {
            let name = &module.source[token.range.clone()];
            if let Some(origin) = moved.get(name)
                && !is_declaration_name(&tokens, index)
            {
                let mut diagnostic = source_diag(
                    "OWNERSHIP_USE_AFTER_MOVE",
                    format!("cannot use `{name}` after it was moved"),
                    token,
                    &module.source,
                    &module.path.to_string_lossy(),
                    "this access requires moved ownership",
                );
                diagnostic.secondary.push(Label {
                    span: origin.clone(),
                    message: "the value was moved here".into(),
                });
                diagnostics.push(diagnostic);
            }
        }
        index += 1;
    }
}

struct Loan {
    binding: String,
    referent: String,
    mutable: bool,
    span: SourceSpan,
}

fn check_move(
    name: &Token,
    module: &tn_hir::Module,
    variables: &BTreeMap<String, Type>,
    moved: &mut BTreeMap<String, SourceSpan>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let name_text = &module.source[name.range.clone()];
    if variables
        .get(name_text)
        .is_some_and(|ty| matches!(ty, Type::Reference { .. }))
    {
        diagnostics.push(source_diag(
            "OWNERSHIP_MOVE_FROM_BORROW",
            format!("cannot move ownership from borrowed binding `{name_text}`"),
            name,
            &module.source,
            &module.path.to_string_lossy(),
            "copy through a Copy reference target or move the owning value",
        ));
    }
    moved.insert(name_text.to_owned(), token_span(module, name));
}

fn check_returned_reference(
    returned: &Token,
    module: &tn_hir::Module,
    parameters: &std::collections::BTreeSet<&str>,
    loans: &[Loan],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let returned_name = &module.source[returned.range.clone()];
    let Some(loan) = loans.iter().find(|loan| loan.binding == returned_name) else {
        return;
    };
    if parameters.contains(loan.referent.as_str()) {
        return;
    }
    let mut diagnostic = source_diag(
        "OWNERSHIP_RETURNED_LOCAL_REFERENCE",
        "cannot return a reference to local storage",
        returned,
        &module.source,
        &module.path.to_string_lossy(),
        "the returned reference would outlive its local referent",
    );
    diagnostic.secondary.push(Label {
        span: loan.span.clone(),
        message: "the local borrow originates here".into(),
    });
    diagnostics.push(diagnostic);
}

fn check_suspension_loans(
    token: &Token,
    module: &tn_hir::Module,
    parameters: &std::collections::BTreeSet<&str>,
    loans: &[Loan],
    diagnostics: &mut Vec<Diagnostic>,
) {
    for loan in loans {
        if parameters.contains(loan.referent.as_str()) {
            continue;
        }
        let mut diagnostic = source_diag(
            "OWNERSHIP_BORROW_ACROSS_SUSPEND",
            format!("borrow of `{}` is live across await", loan.referent),
            token,
            &module.source,
            &module.path.to_string_lossy(),
            "store the referent in stable pinned state or end the loan before await",
        );
        diagnostic.secondary.push(Label {
            span: loan.span.clone(),
            message: "this loan remains live at suspension".into(),
        });
        diagnostics.push(diagnostic);
    }
}

fn check_local(
    tokens: &[&Token],
    start: usize,
    module: &tn_hir::Module,
    variables: &mut BTreeMap<String, Type>,
    loans: &mut Vec<Loan>,
    last_uses: &BTreeMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) -> usize {
    let Some(name_token) = tokens.get(start + 1) else {
        return start + 1;
    };
    let name = module.source[name_token.range.clone()].to_owned();
    let end = tokens[start..]
        .iter()
        .position(|token| token.kind == TokenKind::Semicolon)
        .map_or(tokens.len(), |offset| start + offset);
    let equal = tokens[start..end]
        .iter()
        .position(|token| token.kind == TokenKind::Equal)
        .map(|offset| start + offset);
    if let Some(equal) = equal {
        if tokens
            .get(equal + 1)
            .is_some_and(|token| token.kind == TokenKind::Amp)
        {
            let mutable = tokens
                .get(equal + 2)
                .is_some_and(|token| token.kind == TokenKind::Mut);
            let referent_index = equal + 2 + usize::from(mutable);
            if let Some(referent) = tokens.get(referent_index) {
                let referent_name = module.source[referent.range.clone()].to_owned();
                for loan in loans.iter().filter(|loan| {
                    loan.referent == referent_name
                        && last_uses
                            .get(&loan.binding)
                            .is_some_and(|last| *last >= start)
                }) {
                    if mutable || loan.mutable {
                        let mut diagnostic = source_diag(
                            "OWNERSHIP_CONFLICTING_BORROW",
                            format!("conflicting borrow of `{referent_name}`"),
                            referent,
                            &module.source,
                            &module.path.to_string_lossy(),
                            "this loan overlaps an existing live loan",
                        );
                        diagnostic.secondary.push(Label {
                            span: loan.span.clone(),
                            message: "the existing loan starts here".into(),
                        });
                        diagnostics.push(diagnostic);
                    }
                }
                loans.push(Loan {
                    binding: name.clone(),
                    referent: referent_name.clone(),
                    mutable,
                    span: token_span(module, referent),
                });
                if let Some(ty) = variables.get(&referent_name).cloned() {
                    variables.insert(
                        name,
                        Type::Reference {
                            mutable,
                            lifetime: "scope".into(),
                            referent: Box::new(ty),
                        },
                    );
                }
                return end + usize::from(end < tokens.len());
            }
        }
        if equal + 2 == end
            && let Some(initializer) = tokens.get(equal + 1)
            && let Some(ty) = infer_atom(initializer, module, variables)
        {
            variables.insert(name, ty);
        }
    }
    end + usize::from(end < tokens.len())
}

fn infer_atom(
    token: &Token,
    module: &tn_hir::Module,
    variables: &BTreeMap<String, Type>,
) -> Option<Type> {
    Some(match token.kind {
        TokenKind::True | TokenKind::False => Type::Primitive(tn_hir::PrimitiveType::Bool),
        TokenKind::IntegerLiteral => Type::Primitive(tn_hir::PrimitiveType::Isize),
        TokenKind::FloatLiteral => Type::Primitive(tn_hir::PrimitiveType::F64),
        TokenKind::CharacterLiteral => Type::Primitive(tn_hir::PrimitiveType::Char),
        TokenKind::StringLiteral => Type::Reference {
            mutable: false,
            lifetime: "static".into(),
            referent: Box::new(Type::Str),
        },
        TokenKind::Identifier => variables.get(&module.source[token.range.clone()])?.clone(),
        _ => return None,
    })
}

fn identifier_last_uses(tokens: &[&Token], source: &str) -> BTreeMap<String, usize> {
    let mut uses = BTreeMap::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::Identifier {
            uses.insert(source[token.range.clone()].to_owned(), index);
        }
    }
    uses
}

fn is_declaration_name(tokens: &[&Token], index: usize) -> bool {
    index > 0 && matches!(tokens[index - 1].kind, TokenKind::Const | TokenKind::Let)
}

fn token_span(module: &tn_hir::Module, token: &Token) -> SourceSpan {
    SourceSpan::new(
        module.path.to_string_lossy(),
        token.range.clone(),
        &module.source,
    )
}

fn source_diag(
    id: &str,
    message: impl Into<String>,
    token: &Token,
    source: &str,
    file: &str,
    label: &str,
) -> Diagnostic {
    Diagnostic::error(
        ConditionId::new(id).expect("static condition is valid"),
        message,
        Label {
            span: SourceSpan::new(file, token.range.clone(), source),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    )
}

fn diag(id: &str, message: impl Into<String>, span: &SourceSpan, label: &str) -> Diagnostic {
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
