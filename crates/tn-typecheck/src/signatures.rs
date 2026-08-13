use crate::ownership::declared_conformances;
use crate::{CheckResult, derive_ownership_facts};
use std::collections::{BTreeMap, BTreeSet};
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_hir::{
    DeclarationId, Definition, DefinitionData, Function, GenericBound, GenericParameter, Method,
    Namespace, PrimitiveType, Program, Type, Visibility,
};
use tn_syntax::{TokenKind, lex};

pub fn check_signatures(program: &Program) -> CheckResult {
    let facts = derive_ownership_facts(program);
    check_signatures_with_ownership(program, &facts)
}

pub fn check_signatures_with_ownership(
    program: &Program,
    ownership_facts: &crate::OwnershipFacts,
) -> CheckResult {
    let mut diagnostics = Vec::new();
    let definitions = program
        .definitions
        .iter()
        .map(|definition| (definition.declaration, definition))
        .collect::<BTreeMap<_, _>>();
    for definition in &program.definitions {
        check_definition(program, definition, &definitions, &mut diagnostics);
        check_generic_parameters(&definition.generics, &definitions, &mut diagnostics);
        check_definition_types(program, definition, &definitions, &mut diagnostics);
    }
    check_copy_implementations(program, ownership_facts, &mut diagnostics);
    check_unsafe_marker_implementations(program, &mut diagnostics);
    CheckResult { diagnostics }
}

fn check_unsafe_marker_implementations(program: &Program, diagnostics: &mut Vec<Diagnostic>) {
    for definition in &program.definitions {
        if let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, _)),
            is_unsafe,
            ..
        } = &definition.data
        {
            let marker = program
                .graph
                .declaration(*interface)
                .and_then(|declaration| declaration.name.as_deref());
            if matches!(marker, Some("Send" | "Sync")) && !is_unsafe {
                diagnostics.push(diag(
                    "TYPE_UNSAFE_MARKER_IMPLEMENTATION_REQUIRES_UNSAFE",
                    "Send and Sync implementations require an explicit unsafe implementation",
                    &declaration_span(program, definition),
                    "write `unsafe impl` only after validating the type's thread-safety invariants",
                ));
            }
        }

        for interface in declared_conformances(program, definition.declaration) {
            let Some(marker) = program
                .graph
                .declaration(interface)
                .and_then(|declaration| declaration.name.as_deref())
            else {
                continue;
            };
            if !matches!(marker, "Send" | "Sync")
                || has_unsafe_marker_conformance(program, definition.declaration, marker)
            {
                continue;
            }
            diagnostics.push(diag(
                "TYPE_UNSAFE_MARKER_IMPLEMENTATION_REQUIRES_UNSAFE",
                "Send and Sync conformances require an explicit unsafe marker",
                &declaration_span(program, definition),
                "write `@Conform(Send, unsafe)` or `@Conform(Sync, unsafe)` only after validating thread-safety invariants",
            ));
        }
    }
}

fn has_unsafe_marker_conformance(program: &Program, target: DeclarationId, marker: &str) -> bool {
    program
        .graph
        .declaration(target)
        .is_some_and(|declaration| {
            declaration.attributes.iter().any(|attribute| {
                attribute.name == "Conform"
                    && attribute
                        .arguments
                        .iter()
                        .any(|argument| argument == marker)
                    && attribute
                        .arguments
                        .iter()
                        .any(|argument| argument == "unsafe")
            })
        })
}

fn check_copy_implementations(
    program: &Program,
    facts: &crate::OwnershipFacts,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for target in &facts.copy {
        let Some(definition) = program.definition(*target) else {
            continue;
        };
        let fields = match &definition.data {
            DefinitionData::Struct { fields, .. } => fields.iter().map(|field| &field.ty).collect(),
            DefinitionData::Enum { variants } => variants
                .iter()
                .flat_map(|variant| variant.fields.iter().map(|field| &field.ty))
                .collect(),
            _ => Vec::new(),
        };
        let valid_kind = matches!(
            definition.data,
            DefinitionData::Struct { .. } | DefinitionData::Enum { .. }
        );
        if !valid_kind || facts.drop.contains(target) || !fields.iter().all(|ty| facts.is_copy(ty))
        {
            diagnostics.push(diag(
                "TYPE_INVALID_COPY_IMPLEMENTATION",
                "Copy requires a struct or enum whose fields are all Copy and which has no Drop implementation",
                &declaration_span(program, definition),
                "remove Copy or make every stored field Copy without a destructor",
            ));
        }
    }
}

fn check_definition_types(
    program: &Program,
    definition: &Definition,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let declaration_span = declaration_span(program, definition);
    if !matches!(definition.data, DefinitionData::Function(_)) {
        check_generic_bound_types(&definition.generics, definitions, diagnostics);
    }
    if matches!(
        definition.data,
        DefinitionData::Struct { .. }
            | DefinitionData::Enum { .. }
            | DefinitionData::Interface { .. }
            | DefinitionData::Class { .. }
            | DefinitionData::TypeAlias(_)
    ) {
        for parameter in &definition.generics {
            if parameter.namespace == Namespace::Value {
                diagnostics.push(diag(
                    "TYPE_NOMINAL_EFFECT_PARAMETER_EXCLUDED",
                    "nominal types cannot declare error-effect parameters",
                    &parameter.span,
                    "declare error-effect parameters only on functions or methods",
                ));
            }
        }
    }

    match &definition.data {
        DefinitionData::Constant { ty, .. } | DefinitionData::TypeAlias(ty) => {
            check_type(ty, &declaration_span, definitions, diagnostics);
        }
        DefinitionData::Function(function) => {
            check_function_types(function, &declaration_span, definitions, diagnostics);
        }
        DefinitionData::Struct { fields, methods } => {
            for field in fields {
                check_type(&field.ty, &field.span, definitions, diagnostics);
            }
            for method in methods {
                check_function_types(&method.function, &method.span, definitions, diagnostics);
            }
        }
        DefinitionData::Enum { variants } => {
            for variant in variants {
                for field in &variant.fields {
                    check_type(&field.ty, &field.span, definitions, diagnostics);
                }
            }
        }
        DefinitionData::Interface { methods } => {
            for method in methods {
                check_function_types(&method.function, &method.span, definitions, diagnostics);
            }
        }
        DefinitionData::Class {
            interfaces,
            fields,
            constructor,
            methods,
            ..
        } => {
            for interface in interfaces {
                check_interface_type(interface, &declaration_span, definitions, diagnostics);
            }
            for field in fields {
                check_type(&field.ty, &field.span, definitions, diagnostics);
            }
            if let Some(constructor) = constructor {
                check_function_types(
                    &constructor.function,
                    &constructor.span,
                    definitions,
                    diagnostics,
                );
            }
            for method in methods {
                check_function_types(&method.function, &method.span, definitions, diagnostics);
            }
        }
        DefinitionData::Implementation {
            interface,
            target,
            methods,
            ..
        } => {
            if let Some(interface) = interface {
                check_interface_type(interface, &declaration_span, definitions, diagnostics);
            }
            check_type(target, &declaration_span, definitions, diagnostics);
            for method in methods {
                check_function_types(&method.function, &method.span, definitions, diagnostics);
            }
        }
        DefinitionData::Extern { functions } => {
            for function in functions {
                check_function_types(&function.function, &function.span, definitions, diagnostics);
            }
        }
    }
}

fn check_generic_bound_types(
    parameters: &[GenericParameter],
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for parameter in parameters {
        for bound in &parameter.bounds {
            if let GenericBound::Interface(interface, arguments) = bound {
                check_generic_arguments(
                    *interface,
                    arguments,
                    &parameter.span,
                    definitions,
                    diagnostics,
                );
            }
        }
    }
}

fn check_function_types(
    function: &Function,
    result_span: &SourceSpan,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for parameter in &function.parameters {
        check_type(&parameter.ty, &parameter.span, definitions, diagnostics);
    }
    check_type(&function.result, result_span, definitions, diagnostics);
    check_generic_bound_types(&function.generics, definitions, diagnostics);
}

fn check_interface_type(
    ty: &Type,
    span: &SourceSpan,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    check_type(ty, span, definitions, diagnostics);
    let Some(id) = nominal_id(ty) else {
        diagnostics.push(diag(
            "TYPE_CONFORMANCE_TARGET_NOT_INTERFACE",
            "conformance declarations must name an interface",
            span,
            "replace this type with an interface",
        ));
        return;
    };
    if !definitions
        .get(&id)
        .is_some_and(|definition| matches!(definition.data, DefinitionData::Interface { .. }))
    {
        diagnostics.push(diag(
            "TYPE_CONFORMANCE_TARGET_NOT_INTERFACE",
            "conformance declarations must name an interface",
            span,
            "replace this type with an interface",
        ));
    }
}

fn check_type(
    ty: &Type,
    span: &SourceSpan,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match ty {
        Type::Nominal(id, arguments) => {
            check_generic_arguments(*id, arguments, span, definitions, diagnostics);
        }
        Type::DynamicInterface(id, arguments) => {
            check_generic_arguments(*id, arguments, span, definitions, diagnostics);
            if !definitions.get(id).is_some_and(|definition| {
                matches!(definition.data, DefinitionData::Interface { .. })
            }) {
                diagnostics.push(diag(
                    "TYPE_DYNAMIC_TARGET_NOT_INTERFACE",
                    "dynamic interface types must name an interface",
                    span,
                    "replace the dynamic target with an interface",
                ));
            }
        }
        Type::Optional(inner)
        | Type::Array(inner, _)
        | Type::Slice(inner)
        | Type::Promise { result: inner, .. }
        | Type::Reference {
            referent: inner, ..
        }
        | Type::RawPointer { pointee: inner, .. } => {
            check_type(inner, span, definitions, diagnostics);
        }
        Type::Tuple(elements) | Type::Template(elements) => {
            for element in elements {
                check_type(element, span, definitions, diagnostics);
            }
        }
        Type::Function(function) => {
            for parameter in &function.parameters {
                check_type(parameter, span, definitions, diagnostics);
            }
            check_type(&function.result, span, definitions, diagnostics);
        }
        Type::ErrorUnion(effects) => {
            for effect in effects {
                if !definitions.contains_key(effect) {
                    diagnostics.push(diag(
                        "TYPE_UNKNOWN_ERROR_EFFECT",
                        "internal error union contains an unknown effect",
                        span,
                        "use a declared throwable error type",
                    ));
                }
            }
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::Error
        | Type::Unknown => {}
    }
}

fn check_generic_arguments(
    declaration: DeclarationId,
    arguments: &[Type],
    span: &SourceSpan,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(definition) = definitions.get(&declaration) else {
        return;
    };
    let parameters = definition
        .generics
        .iter()
        .filter(|parameter| parameter.namespace != Namespace::Value)
        .collect::<Vec<_>>();
    if arguments.len() != parameters.len() {
        diagnostics.push(diag(
            "TYPE_GENERIC_ARGUMENT_ARITY",
            format!(
                "generic type expects {} argument(s), but {} were supplied",
                parameters.len(),
                arguments.len()
            ),
            span,
            "supply one argument for every declared type and lifetime parameter",
        ));
    }
    for (parameter, argument) in parameters.iter().zip(arguments) {
        let correct_namespace = match parameter.namespace {
            Namespace::Lifetime => matches!(argument, Type::Lifetime(_)),
            Namespace::Type => !matches!(argument, Type::Lifetime(_)),
            Namespace::Value | Namespace::Method => false,
        };
        if !correct_namespace {
            diagnostics.push(diag(
                "TYPE_GENERIC_ARGUMENT_NAMESPACE",
                format!(
                    "argument for `{}` is in the wrong generic namespace",
                    parameter.name
                ),
                span,
                "use a lifetime for a lifetime parameter and a type for a type parameter",
            ));
        }
        check_type(argument, span, definitions, diagnostics);
    }
}

#[allow(clippy::too_many_lines)]
fn check_definition(
    program: &Program,
    definition: &Definition,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &definition.data {
        DefinitionData::Constant { ty, .. } => {
            if ty == &Type::Error
                && program
                    .graph
                    .declaration(definition.declaration)
                    .is_some_and(|declaration| declaration.exported)
            {
                declaration_diagnostic(
                    program,
                    definition.declaration,
                    diagnostics,
                    "TYPE_PUBLIC_ANNOTATION_REQUIRED",
                    "exported constants require an explicit type annotation",
                    "write the stable public type after `:`",
                );
            }
        }
        DefinitionData::Function(function) => {
            check_function(program, definition.declaration, function, diagnostics);
            let exported = program
                .graph
                .declaration(definition.declaration)
                .is_some_and(|declaration| declaration.exported);
            check_public_reference_lifetimes(
                function,
                exported,
                false,
                &declaration_span(program, definition),
                diagnostics,
            );
            check_generic_parameters(&function.generics, definitions, diagnostics);
        }
        DefinitionData::Struct { fields, methods } => {
            for field in fields {
                reject_void(&field.ty, &field.span, diagnostics);
            }
            reject_duplicate_methods(methods, diagnostics);
            for method in methods {
                check_function(
                    program,
                    definition.declaration,
                    &method.function,
                    diagnostics,
                );
                check_generic_parameters(&method.function.generics, definitions, diagnostics);
            }
            check_declared_interfaces(
                program,
                definition.declaration,
                methods,
                definitions,
                diagnostics,
            );
        }
        DefinitionData::Enum { variants } => {
            check_enum_discriminants(variants, diagnostics);
            for variant in variants {
                for field in &variant.fields {
                    reject_void(&field.ty, &field.span, diagnostics);
                }
            }
            check_declared_interfaces(
                program,
                definition.declaration,
                &[],
                definitions,
                diagnostics,
            );
        }
        DefinitionData::Interface { methods } => {
            reject_duplicate_methods(methods, diagnostics);
            let exported = program
                .graph
                .declaration(definition.declaration)
                .is_some_and(|declaration| declaration.exported);
            for method in methods {
                check_function(
                    program,
                    definition.declaration,
                    &method.function,
                    diagnostics,
                );
                check_public_reference_lifetimes(
                    &method.function,
                    exported,
                    true,
                    &method.span,
                    diagnostics,
                );
            }
        }
        DefinitionData::Class {
            base,
            interfaces: _,
            fields,
            constructor,
            methods,
            is_abstract,
            is_final,
        } => {
            for field in fields {
                reject_void(&field.ty, &field.span, diagnostics);
            }
            reject_duplicate_methods(methods, diagnostics);
            check_class(
                program,
                definition,
                ClassSignature {
                    base: *base,
                    constructor: constructor.as_ref(),
                    methods,
                    is_abstract: *is_abstract,
                    is_final: *is_final,
                },
                definitions,
                diagnostics,
            );
        }
        DefinitionData::Implementation {
            interface, methods, ..
        } => {
            reject_duplicate_methods(methods, diagnostics);
            if let Some(interface) = interface
                && let Some(interface) = nominal_id(interface)
            {
                check_implementation(
                    program,
                    definition.declaration,
                    interface,
                    methods,
                    definitions,
                    diagnostics,
                );
            }
        }
        DefinitionData::Extern { functions } => {
            check_foreign_functions(functions, diagnostics);
        }
        DefinitionData::TypeAlias(ty) => {
            reject_void(ty, &declaration_span(program, definition), diagnostics);
        }
    }
}

fn check_enum_discriminants(variants: &[tn_hir::EnumVariant], diagnostics: &mut Vec<Diagnostic>) {
    let has_payload = variants.iter().any(|variant| !variant.fields.is_empty());
    let has_explicit = variants
        .iter()
        .any(|variant| variant.discriminant.is_some());
    if has_payload && has_explicit {
        let variant = variants
            .iter()
            .find(|variant| variant.discriminant.is_some())
            .expect("explicit discriminant exists");
        diagnostics.push(diag(
            "TYPE_ENUM_PAYLOAD_DISCRIMINANT_MIX",
            "payload variants and explicit integer discriminants cannot be mixed",
            &variant.span,
            "remove the explicit discriminants or all variant payloads",
        ));
    }

    let mut occupied = BTreeMap::new();
    for (index, variant) in variants.iter().enumerate() {
        let discriminant = variant
            .discriminant
            .unwrap_or_else(|| i128::try_from(index).expect("enum variant limit"));
        if let Some(previous) = occupied.insert(discriminant, variant.name.as_str()) {
            diagnostics.push(diag(
                "TYPE_DUPLICATE_ENUM_DISCRIMINANT",
                format!(
                    "enum variants `{previous}` and `{}` share discriminant {discriminant}",
                    variant.name
                ),
                &variant.span,
                "assign every variant a distinct integer discriminant",
            ));
        }
    }
}

fn check_function(
    program: &Program,
    declaration: DeclarationId,
    function: &Function,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for parameter in &function.parameters {
        reject_void(&parameter.ty, &parameter.span, diagnostics);
    }
    if function.is_async
        && !function.is_generator
        && !matches!(function.result, Type::Promise { .. })
    {
        declaration_diagnostic(
            program,
            declaration,
            diagnostics,
            "TYPE_ASYNC_RESULT_MUST_BE_PROMISE",
            "an async declaration must return Promise<T, E>",
            "write the declared result as `Promise<T, E>`",
        );
    }
}

#[derive(Clone, Copy)]
struct ClassSignature<'a> {
    base: Option<DeclarationId>,
    constructor: Option<&'a Method>,
    methods: &'a [Method],
    is_abstract: bool,
    is_final: bool,
}

fn check_foreign_functions(functions: &[Method], diagnostics: &mut Vec<Diagnostic>) {
    for function in functions {
        if function.function.is_async || !function.function.effects.is_empty() {
            diagnostics.push(diag(
                "TYPE_INVALID_FOREIGN_SIGNATURE",
                "foreign functions cannot be async or throwing",
                &function.span,
                "use a synchronous non-throwing C signature",
            ));
        }
    }
}

fn check_class(
    program: &Program,
    definition: &Definition,
    class: ClassSignature<'_>,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    check_class_own_rules(program, definition, &class, definitions, diagnostics);
    if let Some(base) = class.base {
        let Some(base_definition) = definitions.get(&base) else {
            return;
        };
        let DefinitionData::Class {
            methods: base_methods,
            is_final,
            ..
        } = &base_definition.data
        else {
            declaration_diagnostic(
                program,
                definition.declaration,
                diagnostics,
                "TYPE_BASE_MUST_BE_CLASS",
                "a class can extend only another class",
                "name a class after `extends`",
            );
            return;
        };
        if *is_final {
            declaration_diagnostic(
                program,
                definition.declaration,
                diagnostics,
                "TYPE_EXTENDS_FINAL_CLASS",
                "cannot extend a final class",
                "remove the inheritance edge",
            );
        }
        for method in class.methods {
            let base_method = find_method(base_methods, &method.name);
            match (method.is_override, base_method) {
                (true, None) => diagnostics.push(diag(
                    "TYPE_OVERRIDE_NOT_FOUND",
                    format!("method `{}` does not override a base method", method.name),
                    &method.span,
                    "remove `override` or correct the method name",
                )),
                (false, Some(_)) => diagnostics.push(diag(
                    "TYPE_OVERRIDE_MODIFIER_REQUIRED",
                    format!("method `{}` must be marked override", method.name),
                    &method.span,
                    "add the `override` modifier",
                )),
                (true, Some(base_method)) => {
                    check_override(method, base_method, definitions, diagnostics);
                }
                (false, None) => {}
            }
        }
    }
    check_declared_interfaces(
        program,
        definition.declaration,
        class.methods,
        definitions,
        diagnostics,
    );
}

fn check_declared_interfaces(
    program: &Program,
    implementation: DeclarationId,
    methods: &[Method],
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for interface in declared_conformances(program, implementation) {
        check_implementation(
            program,
            implementation,
            interface,
            methods,
            definitions,
            diagnostics,
        );
    }
}

fn check_class_own_rules(
    program: &Program,
    definition: &Definition,
    class: &ClassSignature<'_>,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    check_class_construction(program, definition, class, definitions, diagnostics);
    if class.is_abstract && class.is_final {
        declaration_diagnostic(
            program,
            definition.declaration,
            diagnostics,
            "TYPE_ABSTRACT_FINAL_CLASS",
            "a class cannot be both abstract and final",
            "choose abstract extensibility or final construction",
        );
    }
    if let Some(constructor) = class.constructor {
        check_function(
            program,
            definition.declaration,
            &constructor.function,
            diagnostics,
        );
    }
    for method in class.methods {
        check_generic_parameters(&method.function.generics, definitions, diagnostics);
        let exported_owner = program
            .graph
            .declaration(definition.declaration)
            .is_some_and(|declaration| declaration.exported);
        check_public_reference_lifetimes(
            &method.function,
            exported_owner && method.visibility != Visibility::Private,
            method.receiver != tn_hir::ReceiverMode::Static,
            &method.span,
            diagnostics,
        );
        if method.is_abstract && !class.is_abstract {
            diagnostics.push(diag(
                "TYPE_ABSTRACT_METHOD_IN_CONCRETE_CLASS",
                format!(
                    "abstract method `{}` requires an abstract class",
                    method.name
                ),
                &method.span,
                "mark the class abstract or provide a method body",
            ));
        }
        if method.is_abstract && method.function.body_start != 0 {
            diagnostics.push(diag(
                "TYPE_ABSTRACT_METHOD_HAS_BODY",
                format!("abstract method `{}` cannot have a body", method.name),
                &method.span,
                "replace the body with a semicolon",
            ));
        }
        if !method.is_abstract && method.function.body_start == 0 {
            diagnostics.push(diag(
                "TYPE_CONCRETE_METHOD_MISSING_BODY",
                format!("concrete method `{}` requires a body", method.name),
                &method.span,
                "add a body or mark the method abstract",
            ));
        }
        if !method.function.generics.is_empty() && !class.is_final && !method.is_final {
            diagnostics.push(diag(
                "TYPE_GENERIC_VIRTUAL_METHOD_EXCLUDED",
                format!("virtual method `{}` cannot be generic", method.name),
                &method.span,
                "mark the method final or make the containing class final",
            ));
        }
    }
}

fn check_class_construction(
    program: &Program,
    definition: &Definition,
    class: &ClassSignature<'_>,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let DefinitionData::Class { fields, .. } = &definition.data else {
        return;
    };
    let required = fields
        .iter()
        .filter(|field| !field.optional && !field.has_initializer)
        .collect::<Vec<_>>();
    let base_constructor = class.base.and_then(|base| {
        definitions
            .get(&base)
            .and_then(|definition| match &definition.data {
                DefinitionData::Class { constructor, .. } => constructor.as_ref(),
                _ => None,
            })
    });
    let base_is_synthesizable = class.base.is_none_or(|_| {
        base_constructor.is_none_or(|constructor| {
            constructor.function.parameters.is_empty()
                && constructor.function.effects.is_empty()
                && constructor.visibility != Visibility::Private
        })
    });
    let Some(constructor) = class.constructor else {
        if !required.is_empty() || !base_is_synthesizable {
            declaration_diagnostic(
                program,
                definition.declaration,
                diagnostics,
                "TYPE_CONSTRUCTOR_REQUIRED",
                "this class cannot synthesize a constructor",
                "declare a constructor that initializes every field and base class",
            );
        }
        return;
    };
    let Some(declaration) = program.graph.declaration(definition.declaration) else {
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
                && token.range.start > constructor.function.body_start as usize
                && token.range.end < constructor.function.body_end as usize
        })
        .collect::<Vec<_>>();
    let super_calls = tokens
        .iter()
        .filter(|token| token.kind == TokenKind::Super)
        .count();
    if class.base.is_some()
        && (super_calls != 1
            || !matches!(
                tokens.first().map(|token| token.kind),
                Some(TokenKind::Super | TokenKind::Try)
            ))
    {
        diagnostics.push(diag(
            "TYPE_INVALID_SUPER_CONSTRUCTOR_CALL",
            "a derived constructor must call super exactly once as its first statement",
            &constructor.span,
            "begin with `super(...)` or `try super(...)`",
        ));
    } else if class.base.is_none() && super_calls != 0 {
        diagnostics.push(diag(
            "TYPE_SUPER_WITHOUT_BASE",
            "a class without a base cannot call super",
            &constructor.span,
            "remove the super call",
        ));
    }
    let required_names = required
        .iter()
        .map(|field| field.name.clone())
        .collect::<BTreeSet<_>>();
    let initialized = analyze_constructor_sequence(
        &tokens,
        BTreeSet::new(),
        &required_names,
        module,
        diagnostics,
    );
    for field in required {
        if initialized
            .as_ref()
            .is_some_and(|initialized| !initialized.contains(&field.name))
        {
            diagnostics.push(diag(
                "TYPE_UNINITIALIZED_CLASS_FIELD",
                format!("constructor does not initialize field `{}`", field.name),
                &field.span,
                "assign this field on every constructor path",
            ));
        }
    }
}

#[allow(clippy::too_many_lines)]
fn analyze_constructor_sequence(
    tokens: &[&tn_syntax::Token],
    mut initialized: BTreeSet<String>,
    required: &BTreeSet<String>,
    module: &tn_hir::Module,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BTreeSet<String>> {
    let mut index = 0_usize;
    while index < tokens.len() {
        match tokens[index].kind {
            TokenKind::LeftBrace => {
                let Some(end) = matching_constructor_token(
                    tokens,
                    index,
                    TokenKind::LeftBrace,
                    TokenKind::RightBrace,
                ) else {
                    return Some(initialized);
                };
                initialized = analyze_constructor_sequence(
                    &tokens[index + 1..end],
                    initialized,
                    required,
                    module,
                    diagnostics,
                )?;
                index = end + 1;
            }
            TokenKind::If => {
                let Some(condition_start) = tokens[index..]
                    .iter()
                    .position(|token| token.kind == TokenKind::LeftParen)
                    .map(|offset| index + offset)
                else {
                    index += 1;
                    continue;
                };
                let Some(condition_end) = matching_constructor_token(
                    tokens,
                    condition_start,
                    TokenKind::LeftParen,
                    TokenKind::RightParen,
                ) else {
                    return Some(initialized);
                };
                let then_start = condition_end + 1;
                let then_end = constructor_statement_end(tokens, then_start);
                let then_state = analyze_constructor_sequence(
                    &tokens[then_start..then_end],
                    initialized.clone(),
                    required,
                    module,
                    diagnostics,
                );
                let (else_state, next) = if tokens
                    .get(then_end)
                    .is_some_and(|token| token.kind == TokenKind::Else)
                {
                    let else_start = then_end + 1;
                    let else_end = constructor_statement_end(tokens, else_start);
                    (
                        analyze_constructor_sequence(
                            &tokens[else_start..else_end],
                            initialized.clone(),
                            required,
                            module,
                            diagnostics,
                        ),
                        else_end,
                    )
                } else {
                    (Some(initialized.clone()), then_end)
                };
                initialized = match (then_state, else_state) {
                    (Some(then_state), Some(else_state)) => {
                        then_state.intersection(&else_state).cloned().collect()
                    }
                    (Some(state), None) | (None, Some(state)) => state,
                    (None, None) => return None,
                };
                index = next;
            }
            TokenKind::While | TokenKind::For => {
                let Some(condition_start) = tokens[index..]
                    .iter()
                    .position(|token| token.kind == TokenKind::LeftParen)
                    .map(|offset| index + offset)
                else {
                    index += 1;
                    continue;
                };
                let Some(condition_end) = matching_constructor_token(
                    tokens,
                    condition_start,
                    TokenKind::LeftParen,
                    TokenKind::RightParen,
                ) else {
                    return Some(initialized);
                };
                let body_start = condition_end + 1;
                let body_end = constructor_statement_end(tokens, body_start);
                analyze_constructor_sequence(
                    &tokens[body_start..body_end],
                    initialized.clone(),
                    required,
                    module,
                    diagnostics,
                );
                index = body_end;
            }
            TokenKind::Return => {
                report_constructor_missing_fields(
                    &initialized,
                    required,
                    tokens[index],
                    module,
                    diagnostics,
                );
                return None;
            }
            TokenKind::This
                if tokens
                    .get(index + 3)
                    .is_some_and(|token| token.kind == TokenKind::Equal)
                    && tokens
                        .get(index + 1)
                        .is_some_and(|token| token.kind == TokenKind::Dot)
                    && tokens
                        .get(index + 2)
                        .is_some_and(|token| token.kind == TokenKind::Identifier) =>
            {
                let name = &module.source[tokens[index + 2].range.clone()];
                if required.contains(name) {
                    initialized.insert(name.to_owned());
                }
                index += 4;
            }
            TokenKind::This if !required.is_subset(&initialized) => {
                diagnostics.push(diag(
                    "TYPE_SELF_USE_BEFORE_INITIALIZATION",
                    "this cannot escape before every required field is initialized",
                    &SourceSpan::new(
                        module.path.to_string_lossy(),
                        tokens[index].range.clone(),
                        &module.source,
                    ),
                    "initialize every required field before borrowing this or calling a method",
                ));
                index += 1;
            }
            _ => index += 1,
        }
    }
    Some(initialized)
}

fn report_constructor_missing_fields(
    initialized: &BTreeSet<String>,
    required: &BTreeSet<String>,
    token: &tn_syntax::Token,
    module: &tn_hir::Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if required.is_subset(initialized) {
        return;
    }
    diagnostics.push(diag(
        "TYPE_UNINITIALIZED_CLASS_FIELD",
        "constructor can return before every required field is initialized",
        &SourceSpan::new(
            module.path.to_string_lossy(),
            token.range.clone(),
            &module.source,
        ),
        "initialize every required field on this return path",
    ));
}

fn constructor_statement_end(tokens: &[&tn_syntax::Token], start: usize) -> usize {
    if tokens
        .get(start)
        .is_some_and(|token| token.kind == TokenKind::LeftBrace)
    {
        return matching_constructor_token(
            tokens,
            start,
            TokenKind::LeftBrace,
            TokenKind::RightBrace,
        )
        .map_or(tokens.len(), |end| end + 1);
    }
    let mut depth = 0_u32;
    for (offset, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBrace | TokenKind::LeftBracket => depth += 1,
            TokenKind::RightParen | TokenKind::RightBrace | TokenKind::RightBracket => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::Semicolon if depth == 0 => return offset + 1,
            _ => {}
        }
    }
    tokens.len()
}

fn matching_constructor_token(
    tokens: &[&tn_syntax::Token],
    start: usize,
    open: TokenKind,
    close: TokenKind,
) -> Option<usize> {
    if tokens.get(start)?.kind != open {
        return None;
    }
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if token.kind == open {
            depth += 1;
        } else if token.kind == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn check_implementation(
    program: &Program,
    implementation: DeclarationId,
    interface: DeclarationId,
    methods: &[Method],
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(interface_definition) = definitions.get(&interface) else {
        return;
    };
    let DefinitionData::Interface {
        methods: requirements,
    } = &interface_definition.data
    else {
        declaration_diagnostic(
            program,
            implementation,
            diagnostics,
            "TYPE_IMPLEMENTED_TYPE_NOT_INTERFACE",
            "an implementation target must be an interface",
            "name an interface before `for` or after `implements`",
        );
        return;
    };
    let mut generic_arguments = BTreeMap::new();
    for requirement in requirements {
        let Some(method) = find_method(methods, &requirement.name) else {
            declaration_diagnostic(
                program,
                implementation,
                diagnostics,
                "TYPE_MISSING_INTERFACE_METHOD",
                format!("missing interface method `{}`", requirement.name),
                "implement every required interface operation",
            );
            continue;
        };
        if !substitutable_interface_signature(
            &method.function,
            &requirement.function,
            definitions,
            &mut generic_arguments,
        ) || method.receiver != requirement.receiver
        {
            diagnostics.push(diag(
                "TYPE_INTERFACE_METHOD_MISMATCH",
                format!(
                    "method `{}` does not match its interface requirement",
                    method.name
                ),
                &method.span,
                "parameter, receiver, result, and effect types must match",
            ));
        }
    }
}

fn substitutable_interface_signature(
    implementation: &Function,
    requirement: &Function,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    generic_arguments: &mut BTreeMap<String, Type>,
) -> bool {
    implementation.parameters.len() == requirement.parameters.len()
        && implementation
            .parameters
            .iter()
            .zip(&requirement.parameters)
            .all(|(actual, expected)| {
                interface_type_matches(&actual.ty, &expected.ty, definitions, generic_arguments)
            })
        && interface_type_matches(
            &implementation.result,
            &requirement.result,
            definitions,
            generic_arguments,
        )
        && effect_subset(&implementation.effects, &requirement.effects)
        && implementation.is_async == requirement.is_async
        && (!implementation.is_unsafe || requirement.is_unsafe)
}

fn interface_type_matches(
    actual: &Type,
    expected: &Type,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    generic_arguments: &mut BTreeMap<String, Type>,
) -> bool {
    if let Type::Generic(name) = expected {
        return generic_arguments
            .entry(name.clone())
            .or_insert_with(|| actual.clone())
            == actual;
    }
    match (actual, expected) {
        (Type::Nominal(actual_id, actual_args), Type::Nominal(expected_id, expected_args))
        | (
            Type::DynamicInterface(actual_id, actual_args),
            Type::DynamicInterface(expected_id, expected_args),
        ) if actual_id == expected_id && actual_args.len() == expected_args.len() => actual_args
            .iter()
            .zip(expected_args)
            .all(|(actual, expected)| {
                interface_type_matches(actual, expected, definitions, generic_arguments)
            }),
        (Type::Optional(actual), Type::Optional(expected))
        | (Type::Slice(actual), Type::Slice(expected)) => {
            interface_type_matches(actual, expected, definitions, generic_arguments)
        }
        (
            Type::Promise {
                result: actual,
                effects: actual_effects,
            },
            Type::Promise {
                result: expected,
                effects: expected_effects,
            },
        ) => {
            actual_effects == expected_effects
                && interface_type_matches(actual, expected, definitions, generic_arguments)
        }
        (Type::Array(actual, actual_length), Type::Array(expected, expected_length))
            if actual_length == expected_length =>
        {
            interface_type_matches(actual, expected, definitions, generic_arguments)
        }
        (
            Type::Reference {
                mutable: actual_mutable,
                referent: actual,
                ..
            },
            Type::Reference {
                mutable: expected_mutable,
                referent: expected,
                ..
            },
        ) if actual_mutable == expected_mutable => {
            interface_type_matches(actual, expected, definitions, generic_arguments)
        }
        _ => is_subtype(actual, expected, definitions),
    }
}

fn check_override(
    method: &Method,
    base: &Method,
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if base.is_final {
        diagnostics.push(diag(
            "TYPE_OVERRIDE_FINAL_METHOD",
            format!("cannot override final method `{}`", method.name),
            &method.span,
            "remove this override",
        ));
    }
    if method.receiver != base.receiver
        || method
            .function
            .parameters
            .iter()
            .map(|parameter| &parameter.ty)
            .ne(base
                .function
                .parameters
                .iter()
                .map(|parameter| &parameter.ty))
        || !is_subtype(&method.function.result, &base.function.result, definitions)
        || method.function.is_async != base.function.is_async
        || (method.function.is_unsafe && !base.function.is_unsafe)
        || !effect_subset(&method.function.effects, &base.function.effects)
    {
        diagnostics.push(diag(
            "TYPE_INVALID_OVERRIDE_SIGNATURE",
            format!("override `{}` is not substitutable for its base method", method.name),
            &method.span,
            "keep receiver and parameter types equal, the result compatible, and remove rather than add errors",
        ));
    }
    if visibility_rank(method.visibility) < visibility_rank(base.visibility) {
        diagnostics.push(diag(
            "TYPE_OVERRIDE_REDUCES_VISIBILITY",
            format!("override `{}` reduces base visibility", method.name),
            &method.span,
            "use at least the base method's visibility",
        ));
    }
}

fn effect_subset(left: &[DeclarationId], right: &[DeclarationId]) -> bool {
    left.iter().all(|effect| right.contains(effect))
}

fn is_subtype(
    left: &Type,
    right: &Type,
    definitions: &BTreeMap<DeclarationId, &Definition>,
) -> bool {
    if left == right {
        return true;
    }
    let (Type::Nominal(current, left_arguments), Type::Nominal(target, right_arguments)) =
        (left, right)
    else {
        return false;
    };
    if !left_arguments.is_empty() || !right_arguments.is_empty() {
        return false;
    }
    let mut current = *current;
    let mut visited = BTreeSet::new();
    while visited.insert(current) {
        let Some(Definition {
            data: DefinitionData::Class {
                base: Some(base), ..
            },
            ..
        }) = definitions.get(&current).copied()
        else {
            return false;
        };
        if base == target {
            return true;
        }
        current = *base;
    }
    false
}

fn check_generic_parameters(
    parameters: &[GenericParameter],
    definitions: &BTreeMap<DeclarationId, &Definition>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let namespaces = parameters
        .iter()
        .map(|parameter| (parameter.name.as_str(), parameter.namespace))
        .collect::<BTreeMap<_, _>>();
    for parameter in parameters {
        for bound in &parameter.bounds {
            match bound {
                GenericBound::Interface(interface, _)
                    if !definitions.get(interface).is_some_and(|definition| {
                        matches!(definition.data, DefinitionData::Interface { .. })
                    }) =>
                {
                    diagnostics.push(diag(
                        "TYPE_GENERIC_BOUND_NOT_INTERFACE",
                        "generic type constraints must name interfaces",
                        &parameter.span,
                        "replace this bound with an interface",
                    ));
                }
                GenericBound::Outlives(lifetime)
                    if parameter.namespace != Namespace::Lifetime
                        || namespaces.get(lifetime.as_str()) != Some(&Namespace::Lifetime) =>
                {
                    diagnostics.push(diag(
                        "TYPE_INVALID_OUTLIVES_BOUND",
                        "outlives constraints relate declared lifetime parameters",
                        &parameter.span,
                        "name an in-scope lifetime parameter",
                    ));
                }
                GenericBound::Interface(_, _) if parameter.namespace != Namespace::Type => {
                    diagnostics.push(diag(
                        "TYPE_INVALID_GENERIC_BOUND_NAMESPACE",
                        "interface constraints apply only to type parameters",
                        &parameter.span,
                        "use an outlives or static bound for a lifetime parameter",
                    ));
                }
                GenericBound::Interface(_, _)
                | GenericBound::Static
                | GenericBound::Outlives(_) => {}
            }
        }
    }
}

fn check_public_reference_lifetimes(
    function: &Function,
    public_api: bool,
    has_receiver: bool,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !public_api {
        return;
    }
    let input_lifetimes = function
        .parameters
        .iter()
        .filter_map(|parameter| match &parameter.ty {
            Type::Reference { lifetime, .. } => Some(lifetime.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let result = match &function.result {
        Type::Promise { result, .. } => result.as_ref(),
        result => result,
    };
    let Type::Reference { lifetime, .. } = result else {
        return;
    };
    if lifetime == "scope" {
        if has_receiver || input_lifetimes.len() == 1 {
            return;
        }
        let (condition, message, label) = if input_lifetimes.is_empty() {
            (
                "TYPE_RETURN_REFERENCE_WITHOUT_INPUT",
                "a public borrowed result has no input lifetime",
                "return owned data or relate the result to an explicit borrowed input",
            )
        } else {
            (
                "TYPE_AMBIGUOUS_ELIDED_OUTPUT_LIFETIME",
                "a public borrowed result is ambiguous between multiple inputs",
                "declare a named lifetime and use it on the related input and output",
            )
        };
        diagnostics.push(diag(condition, message, span, label));
        return;
    }
    if lifetime != "static" && !input_lifetimes.contains(&lifetime.as_str()) {
        diagnostics.push(diag(
            "TYPE_UNRELATED_OUTPUT_LIFETIME",
            format!("borrowed result lifetime `{lifetime}` is not provided by an input"),
            span,
            "use the same named lifetime on at least one borrowed input",
        ));
    }
}

fn reject_duplicate_methods(methods: &[Method], diagnostics: &mut Vec<Diagnostic>) {
    let mut names = BTreeSet::new();
    for method in methods {
        if !names.insert(&method.name) {
            diagnostics.push(diag(
                "TYPE_METHOD_OVERLOAD_EXCLUDED",
                format!("method overload set `{}` is not supported", method.name),
                &method.span,
                "give each method a unique name",
            ));
        }
    }
}

fn reject_void(ty: &Type, span: &SourceSpan, diagnostics: &mut Vec<Diagnostic>) {
    if contains_void(ty) {
        diagnostics.push(diag(
            "TYPE_VOID_VALUE_EXCLUDED",
            "void is valid only as a function or method return type",
            span,
            "use a concrete value type here",
        ));
    }
}

fn contains_void(ty: &Type) -> bool {
    match ty {
        Type::Primitive(PrimitiveType::Void) => true,
        Type::Optional(inner)
        | Type::Array(inner, _)
        | Type::Slice(inner)
        | Type::Promise { result: inner, .. }
        | Type::Reference {
            referent: inner, ..
        }
        | Type::RawPointer { pointee: inner, .. } => contains_void(inner),
        Type::Tuple(elements)
        | Type::Nominal(_, elements)
        | Type::DynamicInterface(_, elements) => elements.iter().any(contains_void),
        Type::Function(function) => function.parameters.iter().any(contains_void),
        _ => false,
    }
}

fn find_method<'methods>(methods: &'methods [Method], name: &str) -> Option<&'methods Method> {
    methods.iter().find(|method| method.name == name)
}

fn nominal_id(ty: &Type) -> Option<DeclarationId> {
    match ty {
        Type::Nominal(id, _) | Type::DynamicInterface(id, _) => Some(*id),
        _ => None,
    }
}

const fn visibility_rank(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Private => 0,
        Visibility::Protected => 1,
        Visibility::Public => 2,
    }
}

fn declaration_span(program: &Program, definition: &Definition) -> SourceSpan {
    program
        .graph
        .declaration(definition.declaration)
        .map_or_else(
            || SourceSpan::new("<compiler>", 0..0, ""),
            |item| item.span.clone(),
        )
}

fn declaration_diagnostic(
    program: &Program,
    declaration: DeclarationId,
    diagnostics: &mut Vec<Diagnostic>,
    id: &str,
    message: impl Into<String>,
    label: &str,
) {
    let span = program.graph.declaration(declaration).map_or_else(
        || SourceSpan::new("<compiler>", 0..0, ""),
        |item| item.span.clone(),
    );
    diagnostics.push(diag(id, message, &span, label));
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
