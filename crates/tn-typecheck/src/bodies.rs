use crate::ownership::declared_conformances;
use crate::{
    Capture, CaptureKind, OwnershipFacts, check_capture_requirements, derive_ownership_facts,
};
use std::collections::{BTreeMap, BTreeSet};
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_hir::{
    BindingPattern, BindingPatternKind, BodyHir, BodyOwner, DeclarationId, DeclarationKind,
    Definition, DefinitionData, Function, HirBindingPattern, HirBindingPatternId, HirCaptureMode,
    HirClosure, HirClosureCapture, HirClosureId, HirExpression, HirExpressionId, HirExpressionKind,
    HirJsxChild, HirJsxElement, HirJsxId, HirJsxProperty, HirJsxValue, HirLocal, HirLocalId,
    HirPattern, HirPatternBinding, HirPatternProjection, HirStatement, HirStatementId,
    HirStatementKind, HirTemplate, HirTemplateId, HirTemplatePart, HirTemplateStorage,
    ImportClause, IterationWitness, MemberId, Method, Module, ModuleId, PrimitiveType, Program,
    ReceiverMode, ResolvedValue, Type, Visibility,
};
use tn_syntax::{Token, TokenKind, lex, template_interpolation_ranges};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CallableIdentity {
    Function(DeclarationId),
    Method(MemberId),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MonomorphizationInstance {
    pub callable: CallableIdentity,
    pub arguments: Vec<Type>,
}

#[derive(Clone, Debug, Default)]
pub struct BodyCheckResult {
    pub diagnostics: Vec<Diagnostic>,
    pub monomorphizations: BTreeSet<MonomorphizationInstance>,
    pub closures: Vec<ClosureAnalysis>,
    pub bodies: Vec<BodyHir>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClosureAnalysis {
    pub owner: DeclarationId,
    pub captures: Vec<Capture>,
    pub moved: bool,
    pub span: SourceSpan,
}

#[allow(clippy::too_many_lines)]
pub fn check_bodies(program: &Program) -> BodyCheckResult {
    let ownership_facts = derive_ownership_facts(program);
    check_bodies_with_ownership(program, &ownership_facts)
}

#[allow(clippy::too_many_lines)]
pub fn check_bodies_with_ownership(
    program: &Program,
    ownership_facts: &OwnershipFacts,
) -> BodyCheckResult {
    let mut diagnostics = Vec::new();
    let mut monomorphizations = BTreeSet::new();
    let mut closures = Vec::new();
    let mut hir_bodies = Vec::new();
    let callable = callable_index(program);
    for definition in &program.definitions {
        match &definition.data {
            DefinitionData::Function(function) => check_one(
                program,
                definition.declaration,
                None,
                function,
                None,
                &callable,
                ownership_facts,
                &mut diagnostics,
                &mut monomorphizations,
                &mut closures,
                &mut hir_bodies,
            ),
            DefinitionData::Class {
                constructor,
                methods,
                ..
            } => {
                if let Some(constructor) = constructor {
                    check_one(
                        program,
                        definition.declaration,
                        Some(constructor.id),
                        &constructor.function,
                        Some(Type::Nominal(
                            definition.declaration,
                            definition
                                .generics
                                .iter()
                                .map(generic_identity_type)
                                .collect(),
                        )),
                        &callable,
                        ownership_facts,
                        &mut diagnostics,
                        &mut monomorphizations,
                        &mut closures,
                        &mut hir_bodies,
                    );
                }
                for method in methods {
                    check_one(
                        program,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        (method.receiver != ReceiverMode::Static).then_some(Type::Nominal(
                            definition.declaration,
                            definition
                                .generics
                                .iter()
                                .map(generic_identity_type)
                                .collect(),
                        )),
                        &callable,
                        ownership_facts,
                        &mut diagnostics,
                        &mut monomorphizations,
                        &mut closures,
                        &mut hir_bodies,
                    );
                }
            }
            DefinitionData::Struct { methods, .. } => {
                let self_type = program
                    .intrinsic_type_for_declaration(definition.declaration)
                    .unwrap_or_else(|| {
                        Type::Nominal(
                            definition.declaration,
                            definition
                                .generics
                                .iter()
                                .map(generic_identity_type)
                                .collect(),
                        )
                    });
                for method in methods {
                    check_one(
                        program,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        (method.receiver != ReceiverMode::Static).then_some(self_type.clone()),
                        &callable,
                        ownership_facts,
                        &mut diagnostics,
                        &mut monomorphizations,
                        &mut closures,
                        &mut hir_bodies,
                    );
                }
            }
            DefinitionData::Enum { methods, .. } => {
                let self_type = Type::Nominal(
                    definition.declaration,
                    definition
                        .generics
                        .iter()
                        .map(generic_identity_type)
                        .collect(),
                );
                for method in methods {
                    check_one(
                        program,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        (method.receiver != ReceiverMode::Static).then_some(self_type.clone()),
                        &callable,
                        ownership_facts,
                        &mut diagnostics,
                        &mut monomorphizations,
                        &mut closures,
                        &mut hir_bodies,
                    );
                }
            }
            DefinitionData::Implementation {
                target, methods, ..
            } => {
                for method in methods {
                    check_one(
                        program,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        (method.receiver != ReceiverMode::Static).then_some(target.clone()),
                        &callable,
                        ownership_facts,
                        &mut diagnostics,
                        &mut monomorphizations,
                        &mut closures,
                        &mut hir_bodies,
                    );
                }
            }
            _ => {}
        }
    }
    BodyCheckResult {
        diagnostics,
        monomorphizations,
        closures,
        bodies: hir_bodies,
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn check_one(
    program: &Program,
    owner: DeclarationId,
    member: Option<MemberId>,
    function: &Function,
    self_type: Option<Type>,
    callable: &BTreeMap<(ModuleId, String), (DeclarationId, Function)>,
    ownership_facts: &OwnershipFacts,
    diagnostics: &mut Vec<Diagnostic>,
    monomorphizations: &mut BTreeSet<MonomorphizationInstance>,
    closures: &mut Vec<ClosureAnalysis>,
    hir_bodies: &mut Vec<BodyHir>,
) {
    let normalized_function = normalize_function_aliases(program, function);
    let function = &normalized_function;
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
    let all_tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia() && token.range.end <= function.body_end as usize)
        .cloned()
        .collect::<Vec<_>>();
    let body_start = all_tokens
        .iter()
        .position(|token| token.range.start > function.body_start as usize)
        .unwrap_or(all_tokens.len());
    let body_limit = all_tokens
        .iter()
        .position(|token| token.range.end >= function.body_end as usize)
        .unwrap_or(all_tokens.len());
    let body_tokens = all_tokens[body_start..body_limit].to_vec();
    let result = if function.is_async {
        match &function.result {
            Type::Promise { result, .. } => result.as_ref(),
            result => result,
        }
    } else {
        &function.result
    };
    if !function.is_generator
        && *result != Type::Primitive(PrimitiveType::Void)
        && !guaranteed_sequence(&body_tokens)
    {
        diagnostics.push(Diagnostic::error(
            ConditionId::new("TYPE_MISSING_RETURN").expect("static condition is valid"),
            format!("not every control-flow path returns {result:?}"),
            Label {
                span: declaration.span.clone(),
                message: "add a return or throw on every reachable path".into(),
            },
            "type/control-flow/missing-return",
        ));
    }
    let mut initial_scope = BTreeMap::new();
    let mut initial_hir_scope = BTreeMap::new();
    let mut hir_locals = Vec::new();
    let mut parameter_roots = Vec::new();
    let mut hir_binding_patterns = Vec::new();
    let mut destructured_parameters = Vec::new();
    for parameter in &function.parameters {
        let id = HirLocalId(u32::try_from(hir_locals.len()).expect("HIR local limit"));
        parameter_roots.push(id);
        if parameter.pattern.is_simple_identifier() {
            let (name, mutable) = binding_identifier(&parameter.pattern)
                .unwrap_or_else(|| (parameter.name.clone(), false));
            initial_scope.insert(name.clone(), parameter.ty.clone());
            initial_hir_scope.insert(name.clone(), id);
            hir_locals.push(HirLocal {
                id,
                name,
                ty: parameter.ty.clone(),
                mutable,
                origin: parameter.span.clone(),
            });
        } else {
            hir_locals.push(HirLocal {
                id,
                name: parameter.name.clone(),
                ty: parameter.ty.clone(),
                mutable: false,
                origin: parameter.span.clone(),
            });
            destructured_parameters.push((id, parameter));
        }
    }
    if let Some(self_type) = self_type {
        let id = HirLocalId(u32::try_from(hir_locals.len()).expect("HIR local limit"));
        initial_scope.insert("this".into(), self_type.clone());
        initial_hir_scope.insert("this".into(), id);
        hir_locals.push(HirLocal {
            id,
            name: "this".into(),
            ty: self_type,
            mutable: true,
            origin: declaration.span.clone(),
        });
    }
    for (root, parameter) in destructured_parameters {
        let mut bindings = Vec::new();
        collect_binding_pattern_bindings(
            program,
            &parameter.pattern,
            &parameter.ty,
            &mut Vec::new(),
            &mut initial_scope,
            &mut initial_hir_scope,
            &mut hir_locals,
            &mut bindings,
            diagnostics,
        );
        let id = HirBindingPatternId(
            u32::try_from(hir_binding_patterns.len()).expect("HIR binding pattern limit"),
        );
        hir_binding_patterns.push(HirBindingPattern {
            id,
            root,
            bindings,
            origin: parameter.pattern.span.clone(),
        });
    }
    let mut checker = BodyChecker {
        program,
        module,
        owner,
        function,
        callable,
        ownership_facts,
        tokens: all_tokens,
        body_limit,
        index: body_start,
        scopes: vec![initial_scope],
        hir_scopes: vec![initial_hir_scope],
        diagnostics,
        monomorphizations,
        unsafe_depth: 0,
        loop_depth: 0,
        try_prefix_depth: 0,
        try_block_effects: Vec::new(),
        caught_effects: Vec::new(),
        capture_contexts: Vec::new(),
        closure_bindings: BTreeMap::new(),
        closures,
        hir_bodies,
        hir_owner: member.map_or(BodyOwner::Declaration(owner), |member| BodyOwner::Member {
            declaration: owner,
            member,
        }),
        hir_locals,
        parameter_roots,
        hir_binding_patterns,
        hir_jsx_elements: Vec::new(),
        hir_expressions: Vec::new(),
        hir_patterns: Vec::new(),
        hir_statements: Vec::new(),
        hir_closures: Vec::new(),
        hir_templates: Vec::new(),
        iteration_witnesses: BTreeMap::new(),
    };
    checker.check_parameter_defaults(function);
    while checker.kind().is_some() {
        let before = checker.index;
        checker.statement();
        if checker.index == before {
            checker.bump();
        }
    }
    checker.finish_hir();
}

fn binding_identifier(pattern: &BindingPattern) -> Option<(String, bool)> {
    match &pattern.kind {
        BindingPatternKind::Identifier { name, mutable } => Some((name.clone(), *mutable)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_binding_pattern_bindings(
    program: &Program,
    pattern: &BindingPattern,
    pattern_type: &Type,
    projection: &mut Vec<HirPatternProjection>,
    scope: &mut BTreeMap<String, Type>,
    hir_scope: &mut BTreeMap<String, HirLocalId>,
    hir_locals: &mut Vec<HirLocal>,
    bindings: &mut Vec<HirPatternBinding>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let pattern_type = binding_default_type(pattern_type, pattern.default.is_some());
    match &pattern.kind {
        BindingPatternKind::Identifier { name, mutable } => {
            if name == "_" {
                return;
            }
            let ty = pattern_type.clone();
            if scope.contains_key(name) {
                binding_diagnostic(
                    diagnostics,
                    "RESOLVE_DUPLICATE_LOCAL",
                    format!("local `{name}` is already declared in this scope"),
                    &pattern.span,
                    "choose a distinct binding name",
                );
            }
            let id = HirLocalId(u32::try_from(hir_locals.len()).expect("HIR local limit"));
            scope.insert(name.clone(), ty.clone());
            hir_scope.insert(name.clone(), id);
            hir_locals.push(HirLocal {
                id,
                name: name.clone(),
                ty: ty.clone(),
                mutable: *mutable,
                origin: pattern.span.clone(),
            });
            bindings.push(HirPatternBinding {
                local: id,
                ty,
                projection: projection.clone(),
                default: pattern.default.clone(),
            });
        }
        BindingPatternKind::Array { elements, rest } => {
            for (index, element) in elements.iter().enumerate() {
                let Some(element) = element else {
                    continue;
                };
                let element_type = binding_projection_type(
                    program,
                    &pattern_type,
                    &[HirPatternProjection::Index(
                        u32::try_from(index).unwrap_or(u32::MAX),
                    )],
                );
                projection.push(HirPatternProjection::Index(
                    u32::try_from(index).unwrap_or(u32::MAX),
                ));
                collect_binding_pattern_bindings(
                    program,
                    element,
                    &element_type,
                    projection,
                    scope,
                    hir_scope,
                    hir_locals,
                    bindings,
                    diagnostics,
                );
                projection.pop();
            }
            if let Some(rest) = rest {
                let start = u32::try_from(elements.len()).unwrap_or(u32::MAX);
                let rest_type = binding_projection_type(
                    program,
                    &pattern_type,
                    &[HirPatternProjection::Rest { start }],
                );
                projection.push(HirPatternProjection::Rest { start });
                collect_binding_pattern_bindings(
                    program,
                    rest,
                    &rest_type,
                    projection,
                    scope,
                    hir_scope,
                    hir_locals,
                    bindings,
                    diagnostics,
                );
                projection.pop();
            }
        }
        BindingPatternKind::Object { properties, rest } => {
            let parent_type = pattern_type.clone();
            for property in properties {
                let Some((field_index, field_type)) =
                    binding_field(program, &parent_type, &property.key)
                else {
                    binding_diagnostic(
                        diagnostics,
                        "TYPE_UNKNOWN_PROPERTY",
                        format!(
                            "property `{}` is not present in the binding type",
                            property.key
                        ),
                        &property.span,
                        "bind a declared property or change the source type",
                    );
                    continue;
                };
                let field_type =
                    binding_default_type(&field_type, property.pattern.default.is_some());
                projection.push(HirPatternProjection::Field(field_index));
                collect_binding_pattern_bindings(
                    program,
                    &property.pattern,
                    &field_type,
                    projection,
                    scope,
                    hir_scope,
                    hir_locals,
                    bindings,
                    diagnostics,
                );
                projection.pop();
            }
            if let Some(rest) = rest {
                let rest_type = binding_projection_type(
                    program,
                    &pattern_type,
                    &[HirPatternProjection::Rest { start: 0 }],
                );
                projection.push(HirPatternProjection::Rest { start: 0 });
                collect_binding_pattern_bindings(
                    program,
                    rest,
                    &rest_type,
                    projection,
                    scope,
                    hir_scope,
                    hir_locals,
                    bindings,
                    diagnostics,
                );
                projection.pop();
            }
        }
    }
}

fn binding_default_type(ty: &Type, has_default: bool) -> Type {
    if has_default {
        if let Type::Optional(inner) = ty {
            return inner.as_ref().clone();
        }
    }
    ty.clone()
}

fn binding_projection_type(
    program: &Program,
    root_type: &Type,
    projection: &[HirPatternProjection],
) -> Type {
    let mut current = root_type.clone();
    for projection in projection {
        current = match projection {
            HirPatternProjection::Field(index) => {
                binding_field_by_index(program, &current, *index).map_or(Type::Error, |(_, ty)| ty)
            }
            HirPatternProjection::Index(index) => match &current {
                Type::Array(element, _) | Type::Slice(element) => element.as_ref().clone(),
                Type::Tuple(elements) => elements
                    .get(*index as usize)
                    .cloned()
                    .unwrap_or(Type::Error),
                Type::Reference { referent, .. } => match referent.as_ref() {
                    Type::Array(element, _) | Type::Slice(element) => element.as_ref().clone(),
                    Type::Tuple(elements) => elements
                        .get(*index as usize)
                        .cloned()
                        .unwrap_or(Type::Error),
                    _ => Type::Error,
                },
                _ => Type::Error,
            },
            HirPatternProjection::Rest { .. } => match &current {
                Type::Array(element, _) | Type::Slice(element) => Type::Slice(element.clone()),
                _ => Type::Unknown,
            },
            HirPatternProjection::OptionalPayload => match &current {
                Type::Optional(inner) => inner.as_ref().clone(),
                _ => Type::Error,
            },
            HirPatternProjection::Variant(_) => current,
        };
    }
    current
}

fn binding_field(program: &Program, ty: &Type, name: &str) -> Option<(u32, Type)> {
    let Type::Nominal(declaration, _) = ty else {
        return None;
    };
    let fields = match &program.definition(*declaration)?.data {
        DefinitionData::Struct { fields, .. } | DefinitionData::Class { fields, .. } => fields,
        _ => return None,
    };
    fields
        .iter()
        .position(|field| field.name == name)
        .and_then(|index| Some((u32::try_from(index).ok()?, fields.get(index)?.ty.clone())))
}

fn binding_field_by_index(program: &Program, ty: &Type, index: u32) -> Option<(String, Type)> {
    let Type::Nominal(declaration, _) = ty else {
        return None;
    };
    let fields = match &program.definition(*declaration)?.data {
        DefinitionData::Struct { fields, .. } | DefinitionData::Class { fields, .. } => fields,
        _ => return None,
    };
    fields
        .get(index as usize)
        .map(|field| (field.name.clone(), field.ty.clone()))
}

fn binding_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    id: &str,
    message: String,
    span: &SourceSpan,
    label: &str,
) {
    diagnostics.push(Diagnostic::error(
        ConditionId::new(id).expect("static condition identifier is valid"),
        message,
        Label {
            span: span.clone(),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    ));
}

struct BodyChecker<'a> {
    program: &'a Program,
    module: &'a Module,
    owner: DeclarationId,
    function: &'a Function,
    callable: &'a BTreeMap<(ModuleId, String), (DeclarationId, Function)>,
    ownership_facts: &'a OwnershipFacts,
    tokens: Vec<Token>,
    body_limit: usize,
    index: usize,
    scopes: Vec<BTreeMap<String, Type>>,
    hir_scopes: Vec<BTreeMap<String, HirLocalId>>,
    diagnostics: &'a mut Vec<Diagnostic>,
    monomorphizations: &'a mut BTreeSet<MonomorphizationInstance>,
    unsafe_depth: u32,
    loop_depth: u32,
    try_prefix_depth: u32,
    try_block_effects: Vec<BTreeSet<DeclarationId>>,
    caught_effects: Vec<Vec<DeclarationId>>,
    capture_contexts: Vec<CaptureContext>,
    closure_bindings: BTreeMap<String, Vec<Capture>>,
    closures: &'a mut Vec<ClosureAnalysis>,
    hir_bodies: &'a mut Vec<BodyHir>,
    hir_owner: BodyOwner,
    hir_locals: Vec<HirLocal>,
    parameter_roots: Vec<HirLocalId>,
    hir_binding_patterns: Vec<HirBindingPattern>,
    hir_jsx_elements: Vec<HirJsxElement>,
    hir_expressions: Vec<HirExpression>,
    hir_patterns: Vec<HirPattern>,
    hir_statements: Vec<HirStatement>,
    hir_closures: Vec<HirClosure>,
    hir_templates: Vec<HirTemplate>,
    iteration_witnesses: BTreeMap<HirLocalId, IterationWitness>,
}

struct CaptureContext {
    scope_base: usize,
    captures: BTreeMap<String, Capture>,
}

#[derive(Clone)]
struct BodyBindingPattern {
    start: usize,
    kind: BodyBindingPatternKind,
    span: SourceSpan,
    default: Option<SourceSpan>,
}

#[derive(Clone)]
enum BodyBindingPatternKind {
    Identifier {
        name: String,
        mutable: bool,
    },
    Array {
        elements: Vec<Option<BodyBindingPattern>>,
        rest: Option<Box<BodyBindingPattern>>,
    },
    Object {
        properties: Vec<BodyBindingProperty>,
        rest: Option<Box<BodyBindingPattern>>,
    },
}

#[derive(Clone)]
struct BodyBindingProperty {
    key: String,
    pattern: BodyBindingPattern,
    span: SourceSpan,
}

impl BodyBindingPattern {
    fn is_simple_identifier(&self) -> bool {
        matches!(self.kind, BodyBindingPatternKind::Identifier { .. })
    }
}

fn body_binding_identifier(pattern: &BodyBindingPattern) -> Option<(String, bool)> {
    match &pattern.kind {
        BodyBindingPatternKind::Identifier { name, mutable } => Some((name.clone(), *mutable)),
        _ => None,
    }
}

fn make_body_binding_mutable(pattern: &mut BodyBindingPattern) {
    match &mut pattern.kind {
        BodyBindingPatternKind::Identifier { mutable, .. } => *mutable = true,
        BodyBindingPatternKind::Array { elements, rest } => {
            for element in elements.iter_mut().flatten() {
                make_body_binding_mutable(element);
            }
            if let Some(rest) = rest {
                make_body_binding_mutable(rest);
            }
        }
        BodyBindingPatternKind::Object { properties, rest } => {
            for property in properties {
                make_body_binding_mutable(&mut property.pattern);
            }
            if let Some(rest) = rest {
                make_body_binding_mutable(rest);
            }
        }
    }
}

#[derive(Clone)]
struct ExpressionType {
    ty: Type,
    optional_chain_value: Option<Type>,
    place: Option<String>,
    effects: Vec<DeclarationId>,
    callable: Option<CallableIdentity>,
    call_name: Option<String>,
    captures: Vec<Capture>,
    resolution: Option<ResolvedValue>,
    type_qualifier: bool,
    jsx: Option<HirJsxId>,
}

impl BodyChecker<'_> {
    fn kind(&self) -> Option<TokenKind> {
        (self.index < self.body_limit)
            .then(|| self.tokens.get(self.index).map(|token| token.kind))
            .flatten()
    }

    fn check_parameter_defaults(&mut self, function: &Function) {
        let parameter_defaults = function
            .parameters
            .iter()
            .filter_map(|parameter| {
                parameter
                    .default
                    .as_ref()
                    .map(|default| (default.clone(), parameter.ty.clone()))
            })
            .collect::<Vec<_>>();
        let pattern_defaults = self
            .hir_binding_patterns
            .iter()
            .flat_map(|pattern| {
                pattern.bindings.iter().filter_map(|binding| {
                    binding
                        .default
                        .as_ref()
                        .map(|default| (default.clone(), binding.ty.clone()))
                })
            })
            .collect::<Vec<_>>();
        for (default, expected) in parameter_defaults.into_iter().chain(pattern_defaults) {
            self.check_binding_default(&default, &expected);
        }
    }

    fn nth(&self, offset: usize) -> Option<TokenKind> {
        self.tokens.get(self.index + offset).map(|token| token.kind)
    }

    fn token(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn text(&self) -> Option<&str> {
        self.token()
            .map(|token| &self.module.source[token.range.clone()])
    }

    fn bump(&mut self) -> Option<&Token> {
        let index = self.index;
        self.index += usize::from(index < self.tokens.len());
        self.tokens.get(index)
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.kind() == Some(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn statement(&mut self) {
        let start = self.index;
        let Some(start_kind) = self.kind() else {
            return;
        };
        let locals_before = self.hir_locals.len();
        match start_kind {
            TokenKind::LeftBrace => self.block(),
            TokenKind::Const | TokenKind::Let => self.local_declaration(),
            TokenKind::Return => self.return_statement(),
            TokenKind::Yield => self.yield_statement(),
            TokenKind::Throw => self.throw_statement(),
            TokenKind::If => self.if_statement(),
            TokenKind::While => self.while_statement(),
            TokenKind::For => self.for_statement(),
            TokenKind::Await if self.nth(1) == Some(TokenKind::Using) => {
                self.bump();
                self.using_statement(true);
            }
            TokenKind::Using => self.using_statement(false),
            TokenKind::Try if self.nth(1) == Some(TokenKind::LeftBrace) => {
                self.try_statement();
            }
            TokenKind::Unsafe if self.nth(1) == Some(TokenKind::LeftBrace) => {
                self.bump();
                self.unsafe_depth += 1;
                self.block();
                self.unsafe_depth = self.unsafe_depth.saturating_sub(1);
            }
            TokenKind::Break | TokenKind::Continue => {
                let token = self.bump().cloned();
                if self.loop_depth == 0
                    && let Some(token) = token
                {
                    self.error(
                        "TYPE_LOOP_CONTROL_OUTSIDE_LOOP",
                        "break and continue are valid only inside a loop",
                        &token,
                        "move this statement into a loop body",
                    );
                }
                self.eat(TokenKind::Semicolon);
            }
            _ => {
                self.expression(0, None);
                self.eat(TokenKind::Semicolon);
            }
        }
        self.record_hir_statement(start, start_kind, locals_before);
    }

    fn block(&mut self) {
        if !self.eat(TokenKind::LeftBrace) {
            return;
        }
        self.scopes.push(BTreeMap::new());
        self.hir_scopes.push(BTreeMap::new());
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightBrace) {
            let before = self.index;
            self.statement();
            if self.index == before {
                self.bump();
            }
        }
        self.eat(TokenKind::RightBrace);
        self.scopes.pop();
        self.hir_scopes.pop();
    }

    fn body_binding_pattern(&mut self) -> Option<BodyBindingPattern> {
        let start = self.index;
        let mutable = self.eat(TokenKind::Mut);
        let kind = match self.kind()? {
            TokenKind::Identifier => {
                let token = self.bump().cloned()?;
                BodyBindingPatternKind::Identifier {
                    name: self.module.source[token.range.clone()].to_owned(),
                    mutable: false,
                }
            }
            TokenKind::LeftBracket => {
                self.bump();
                let mut elements = Vec::new();
                let mut rest = None;
                while self.kind().is_some() && self.kind() != Some(TokenKind::RightBracket) {
                    if self.eat(TokenKind::Comma) {
                        elements.push(None);
                        continue;
                    }
                    if self.eat(TokenKind::Ellipsis) {
                        rest = self.body_binding_pattern().map(Box::new);
                        self.eat(TokenKind::Comma);
                        break;
                    }
                    let element = self.body_binding_pattern()?;
                    let mut element = element;
                    if self.eat(TokenKind::Equal) {
                        element.default = self.skip_body_binding_default(&[
                            TokenKind::Comma,
                            TokenKind::RightBracket,
                        ]);
                    }
                    elements.push(Some(element));
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.eat(TokenKind::RightBracket);
                BodyBindingPatternKind::Array { elements, rest }
            }
            TokenKind::LeftBrace => {
                self.bump();
                let mut properties = Vec::new();
                let mut rest = None;
                while self.kind().is_some() && self.kind() != Some(TokenKind::RightBrace) {
                    if self.eat(TokenKind::Ellipsis) {
                        rest = self.body_binding_pattern().map(Box::new);
                        self.eat(TokenKind::Comma);
                        break;
                    }
                    let property_start = self.index;
                    let key_token = self.bump().cloned()?;
                    let key = self.module.source[key_token.range.clone()].to_owned();
                    let mut pattern = if self.eat(TokenKind::Colon) {
                        self.body_binding_pattern()?
                    } else {
                        BodyBindingPattern {
                            start: property_start,
                            kind: BodyBindingPatternKind::Identifier {
                                name: key.clone(),
                                mutable: false,
                            },
                            span: self.token_span(&key_token),
                            default: None,
                        }
                    };
                    if self.eat(TokenKind::Equal) {
                        pattern.default = self
                            .skip_body_binding_default(&[TokenKind::Comma, TokenKind::RightBrace]);
                    }
                    properties.push(BodyBindingProperty {
                        key,
                        pattern,
                        span: self.body_binding_span(property_start, self.index),
                    });
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.eat(TokenKind::RightBrace);
                BodyBindingPatternKind::Object { properties, rest }
            }
            _ => return None,
        };
        let mut pattern = BodyBindingPattern {
            start,
            kind,
            span: self.body_binding_span(start, self.index),
            default: None,
        };
        if mutable {
            make_body_binding_mutable(&mut pattern);
        }
        Some(pattern)
    }

    fn skip_body_binding_default(&mut self, stops: &[TokenKind]) -> Option<SourceSpan> {
        let start = self.tokens.get(self.index)?.range.start;
        let mut end = start;
        let mut delimiters = Vec::new();
        while let Some(kind) = self.kind() {
            if delimiters.is_empty() && stops.contains(&kind) {
                break;
            }
            match kind {
                TokenKind::LeftParen => delimiters.push(TokenKind::RightParen),
                TokenKind::LeftBracket => delimiters.push(TokenKind::RightBracket),
                TokenKind::LeftBrace => delimiters.push(TokenKind::RightBrace),
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    if delimiters.last() == Some(&kind) {
                        delimiters.pop();
                    } else if delimiters.is_empty() {
                        break;
                    }
                }
                _ => {}
            }
            if let Some(token) = self.tokens.get(self.index) {
                end = token.range.end;
            }
            self.bump();
        }
        Some(SourceSpan::new(
            self.module.path.to_string_lossy(),
            start..end,
            &self.module.source,
        ))
    }

    fn check_binding_default(&mut self, span: &SourceSpan, expected: &Type) {
        if span.byte_start == span.byte_end {
            return;
        }
        let Some(start) = self
            .tokens
            .iter()
            .position(|token| token.range.start >= span.byte_start as usize)
        else {
            return;
        };
        let end = self
            .tokens
            .iter()
            .position(|token| token.range.end > span.byte_end as usize)
            .unwrap_or(self.tokens.len());
        if start >= end {
            return;
        }
        let saved = self.index;
        self.index = start;
        let actual = self.expression(0, Some(expected));
        self.index = saved;
        if let Some(actual) = actual
            && !compatible(self.program, &actual.ty, expected)
            && actual.ty != Type::Error
            && *expected != Type::Error
        {
            self.error_span(
                "TYPE_MISMATCH",
                format!(
                    "binding default has type {:?}, expected {expected:?}",
                    actual.ty
                ),
                span,
                "use a value compatible with the binding type",
            );
        }
    }

    fn body_binding_span(&self, start: usize, end: usize) -> SourceSpan {
        let byte_start = self
            .tokens
            .get(start)
            .map_or(self.module.source.len(), |token| token.range.start);
        let byte_end = self
            .tokens
            .get(end.saturating_sub(1))
            .map_or(byte_start, |token| token.range.end);
        SourceSpan::new(
            self.module.path.to_string_lossy(),
            byte_start..byte_end,
            &self.module.source,
        )
    }

    fn collect_body_binding_pattern(
        &mut self,
        pattern: &BodyBindingPattern,
        pattern_type: &Type,
        projection: &mut Vec<HirPatternProjection>,
        bindings: &mut Vec<HirPatternBinding>,
    ) {
        let pattern_type = binding_default_type(pattern_type, pattern.default.is_some());
        match &pattern.kind {
            BodyBindingPatternKind::Identifier { name, mutable } => {
                if name == "_" {
                    return;
                }
                let ty = pattern_type.clone();
                if let Some(default) = pattern.default.as_ref() {
                    self.check_binding_default(default, &ty);
                }
                let duplicate = self
                    .scopes
                    .last_mut()
                    .is_some_and(|scope| scope.insert(name.clone(), ty.clone()).is_some());
                if duplicate {
                    self.error_span(
                        "RESOLVE_DUPLICATE_LOCAL",
                        format!("local `{name}` is already declared in this scope"),
                        &pattern.span,
                        "choose a distinct binding name",
                    );
                }
                let id = self.declare_hir_local(
                    name.clone(),
                    ty.clone(),
                    *mutable,
                    pattern.span.clone(),
                );
                bindings.push(HirPatternBinding {
                    local: id,
                    ty,
                    projection: projection.clone(),
                    default: pattern.default.clone(),
                });
            }
            BodyBindingPatternKind::Array { elements, rest } => {
                for (index, element) in elements.iter().enumerate() {
                    let Some(element) = element else {
                        continue;
                    };
                    let element_type = binding_projection_type(
                        self.program,
                        &pattern_type,
                        &[HirPatternProjection::Index(
                            u32::try_from(index).unwrap_or(u32::MAX),
                        )],
                    );
                    projection.push(HirPatternProjection::Index(
                        u32::try_from(index).unwrap_or(u32::MAX),
                    ));
                    self.collect_body_binding_pattern(element, &element_type, projection, bindings);
                    projection.pop();
                }
                if let Some(rest) = rest {
                    let start = u32::try_from(elements.len()).unwrap_or(u32::MAX);
                    let rest_type = binding_projection_type(
                        self.program,
                        &pattern_type,
                        &[HirPatternProjection::Rest { start }],
                    );
                    projection.push(HirPatternProjection::Rest { start });
                    self.collect_body_binding_pattern(rest, &rest_type, projection, bindings);
                    projection.pop();
                }
            }
            BodyBindingPatternKind::Object { properties, rest } => {
                let parent_type = pattern_type.clone();
                for property in properties {
                    let Some((field_index, field_type)) =
                        binding_field(self.program, &parent_type, &property.key)
                    else {
                        if !matches!(parent_type, Type::Unknown | Type::Error) {
                            self.error_span(
                                "TYPE_UNKNOWN_PROPERTY",
                                format!(
                                    "property `{}` is not present in the binding type",
                                    property.key
                                ),
                                &property.span,
                                "bind a declared property or change the source type",
                            );
                        }
                        continue;
                    };
                    let field_type =
                        binding_default_type(&field_type, property.pattern.default.is_some());
                    projection.push(HirPatternProjection::Field(field_index));
                    self.collect_body_binding_pattern(
                        &property.pattern,
                        &field_type,
                        projection,
                        bindings,
                    );
                    projection.pop();
                }
                if let Some(rest) = rest {
                    let rest_type = binding_projection_type(
                        self.program,
                        &pattern_type,
                        &[HirPatternProjection::Rest { start: 0 }],
                    );
                    projection.push(HirPatternProjection::Rest { start: 0 });
                    self.collect_body_binding_pattern(rest, &rest_type, projection, bindings);
                    projection.pop();
                }
            }
        }
    }

    fn local_declaration(&mut self) {
        let mutable = self.eat(TokenKind::Let);
        if !mutable {
            self.eat(TokenKind::Const);
        }
        let Some(mut pattern) = self.body_binding_pattern() else {
            return;
        };
        if mutable {
            make_body_binding_mutable(&mut pattern);
        }
        let pattern_token = self.tokens.get(pattern.start).cloned();
        let annotation = if self.eat(TokenKind::Colon) {
            self.parse_local_type()
        } else {
            None
        };
        self.eat(TokenKind::Equal);
        let value = self.expression(0, annotation.as_ref());
        let captured_closure = value
            .as_ref()
            .map(|value| value.captures.clone())
            .unwrap_or_default();
        let ty = match (annotation, value) {
            (Some(expected), Some(actual)) => {
                if !compatible(self.program, &actual.ty, &expected) {
                    if let Some(token) = pattern_token.as_ref() {
                        self.error(
                            "TYPE_MISMATCH",
                            format!(
                                "initializer has type {:?}, expected {expected:?}",
                                actual.ty
                            ),
                            token,
                            "use a value of the annotated type; numeric widening is explicit",
                        );
                    }
                }
                expected
            }
            (None, Some(actual)) if actual.ty != Type::Error => actual.ty,
            _ => Type::Error,
        };
        let simple_name = body_binding_identifier(&pattern);
        let root_name = simple_name
            .as_ref()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| format!("$binding{}", self.hir_locals.len()));
        let root_origin = pattern.span.clone();
        let root = if let Some((name, pattern_mutable)) = simple_name {
            if let Some(scope) = self.scopes.last_mut()
                && scope.insert(name.clone(), ty.clone()).is_some()
                && let Some(token) = pattern_token.as_ref()
            {
                self.error(
                    "RESOLVE_DUPLICATE_LOCAL",
                    format!("local `{name}` is already declared in this scope"),
                    token,
                    "choose a distinct local name",
                );
            }
            self.declare_hir_local(name, ty.clone(), pattern_mutable, root_origin.clone())
        } else {
            self.push_hir_local(root_name, ty.clone(), false, root_origin.clone())
        };
        if !pattern.is_simple_identifier() {
            let mut bindings = Vec::new();
            self.collect_body_binding_pattern(&pattern, &ty, &mut Vec::new(), &mut bindings);
            let id = HirBindingPatternId(
                u32::try_from(self.hir_binding_patterns.len()).expect("HIR binding pattern limit"),
            );
            self.hir_binding_patterns.push(HirBindingPattern {
                id,
                root,
                bindings,
                origin: pattern.span.clone(),
            });
        } else if let Some((name, _)) = body_binding_identifier(&pattern) {
            if captured_closure.is_empty() {
                self.closure_bindings.remove(&name);
            } else {
                self.closure_bindings.insert(name, captured_closure);
            }
        }
        self.eat(TokenKind::Semicolon);
    }

    fn using_statement(&mut self, awaited: bool) {
        self.eat(TokenKind::Using);
        let Some(name_token) = self.bump().cloned() else {
            return;
        };
        let name = self.module.source[name_token.range.clone()].to_owned();
        self.eat(TokenKind::Equal);
        let value = self.expression(0, None);
        let ty = value.as_ref().map_or(Type::Error, |value| value.ty.clone());
        let protocol_name = if awaited {
            "AsyncDisposable"
        } else {
            "Disposable"
        };
        let conforms = standard_interface(self.program, protocol_name).is_some_and(|interface| {
            nominal_id(&ty).is_some_and(|nominal| {
                nominal_conforms_to_interface(self.program, nominal, interface)
            })
        });
        if ty != Type::Error && !conforms {
            self.error(
                "TYPE_USING_REQUIRES_DISPOSABLE",
                format!("managed binding requires `{protocol_name}`"),
                &name_token,
                if awaited {
                    "implement `[Symbol.asyncDispose]` and declare `implements AsyncDisposable`"
                } else {
                    "implement `[Symbol.dispose]` and declare `implements Disposable`"
                },
            );
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), ty.clone());
        }
        self.declare_hir_local(name, ty, false, self.token_span(&name_token));
        self.eat(TokenKind::Semicolon);
        if awaited
            && !self.function.is_async
            && let Some(token) = self.tokens.get(self.index.saturating_sub(1)).cloned()
        {
            self.error(
                "TYPE_AWAIT_OUTSIDE_ASYNC",
                "await using is valid only in an async declaration",
                &token,
                "mark the enclosing function async",
            );
        }
    }

    fn return_statement(&mut self) {
        let token = self.bump().cloned();
        let generator_completion = Type::Primitive(PrimitiveType::Void);
        let expected_result = if self.function.is_generator {
            &generator_completion
        } else if self.function.is_async {
            match &self.function.result {
                Type::Promise { result, .. } => result.as_ref(),
                result => result,
            }
        } else {
            &self.function.result
        };
        let value = if self.kind() == Some(TokenKind::Semicolon) {
            None
        } else {
            self.expression(0, Some(expected_result))
        };
        let valid = if *expected_result == Type::Primitive(PrimitiveType::Void) {
            value.is_none()
        } else {
            value
                .as_ref()
                .is_some_and(|actual| compatible(self.program, &actual.ty, expected_result))
        };
        if !valid && let Some(token) = token {
            self.error(
                "TYPE_INVALID_RETURN",
                format!("return does not produce {expected_result:?}"),
                &token,
                "return a value compatible with the declaration result",
            );
        }
        self.eat(TokenKind::Semicolon);
    }

    fn yield_statement(&mut self) {
        let token = self.bump().cloned();
        let protocol = if self.function.is_async {
            "AsyncIterable"
        } else {
            "Iterable"
        };
        let item = match &self.function.result {
            Type::Nominal(declaration, arguments)
            | Type::DynamicInterface(declaration, arguments)
                if declaration_name(self.program, *declaration) == Some(protocol)
                    && arguments.len() == 1 =>
            {
                arguments.first()
            }
            _ => None,
        };
        if !self.function.is_generator
            && let Some(token) = token.as_ref()
        {
            self.error(
                "TYPE_YIELD_OUTSIDE_GENERATOR",
                "yield is valid only in a generator declaration",
                token,
                "add `*` after `function` or remove the yield statement",
            );
        } else if item.is_none()
            && let Some(token) = token.as_ref()
        {
            self.error(
                "TYPE_INVALID_GENERATOR_RESULT",
                format!("generator result must be {protocol}<Item>"),
                token,
                "declare the generator's yielded item type",
            );
        }
        if self.kind() == Some(TokenKind::Semicolon) {
            if let Some(token) = token.as_ref() {
                self.error(
                    "TYPE_YIELD_VALUE_REQUIRED",
                    "yield requires a value",
                    token,
                    "provide a value compatible with the generator item type",
                );
            }
        } else {
            self.expression(0, item);
        }
        self.eat(TokenKind::Semicolon);
    }

    fn throw_statement(&mut self) {
        let token = self.bump().cloned();
        let value = self.expression(0, None);
        let declared = value
            .as_ref()
            .and_then(|value| nominal_id(&value.ty))
            .is_some_and(|effect| {
                self.function.effects.contains(&effect)
                    || self.caught_effects.iter().any(|set| {
                        set.iter()
                            .any(|caught| catch_handles(self.program, *caught, effect))
                    })
            });
        if !declared && let Some(token) = token {
            self.error(
                "TYPE_UNDECLARED_THROW",
                "thrown value is not in the enclosing closed error set",
                &token,
                "declare this error type or throw it inside a matching handler",
            );
        }
        self.eat(TokenKind::Semicolon);
    }

    fn if_statement(&mut self) {
        self.bump();
        self.eat(TokenKind::LeftParen);
        let condition = self.expression(0, Some(&Type::Primitive(PrimitiveType::Bool)));
        self.require_bool(condition);
        self.eat(TokenKind::RightParen);
        self.statement();
        if self.eat(TokenKind::Else) {
            self.statement();
        }
    }

    fn while_statement(&mut self) {
        self.bump();
        self.eat(TokenKind::LeftParen);
        let condition = self.expression(0, Some(&Type::Primitive(PrimitiveType::Bool)));
        self.require_bool(condition);
        self.eat(TokenKind::RightParen);
        self.loop_depth += 1;
        self.statement();
        self.loop_depth = self.loop_depth.saturating_sub(1);
    }

    fn for_statement(&mut self) {
        self.bump();
        let awaited = self.eat(TokenKind::Await);
        if awaited
            && !self.function.is_async
            && let Some(token) = self.tokens.get(self.index.saturating_sub(1)).cloned()
        {
            self.error(
                "TYPE_AWAIT_OUTSIDE_ASYNC",
                "for await is valid only in an async declaration",
                &token,
                "mark the enclosing function async",
            );
        }
        self.eat(TokenKind::LeftParen);
        let mutable = self.eat(TokenKind::Let);
        if !mutable {
            self.eat(TokenKind::Const);
        }
        let mut pattern = self.body_binding_pattern();
        if mutable && let Some(pattern) = pattern.as_mut() {
            make_body_binding_mutable(pattern);
        }
        let binding_token = pattern
            .as_ref()
            .and_then(|pattern| self.tokens.get(pattern.start).cloned());
        self.eat(TokenKind::Of);
        let iterable = self.expression(0, None);
        self.eat(TokenKind::RightParen);
        self.scopes.push(BTreeMap::new());
        self.hir_scopes.push(BTreeMap::new());
        if let Some(pattern) = pattern {
            let binding_name = body_binding_identifier(&pattern)
                .map(|(name, _)| name)
                .unwrap_or_else(|| format!("$binding{}", self.hir_locals.len()));
            let iteration = iterable
                .as_ref()
                .map_or(Err(IterationError::NotIterable), |iterable| {
                    for_iteration(self.program, &iterable.ty, awaited)
                });
            let (item, witness) = match iteration {
                Ok(iteration) => iteration,
                Err(error) => {
                    if let Some(binding) = binding_token.as_ref() {
                        match error {
                            IterationError::NotIterable => self.error(
                                "TYPE_NOT_ITERABLE",
                                "for-of expression does not implement IntoIterator<Item, Iter>",
                                binding,
                                "iterate an array, slice, string view, or explicit IntoIterator type",
                            ),
                            IterationError::InvalidProtocol => self.error(
                                "TYPE_INVALID_ITERATOR_PROTOCOL",
                                "selected iterator implementations do not provide the required operations",
                                binding,
                                "provide infallible move intoIterator() and mut next() with exact types",
                            ),
                        }
                    }
                    (Type::Error, None)
                }
            };
            let local = if pattern.is_simple_identifier() {
                let (name, pattern_mutable) = body_binding_identifier(&pattern)
                    .unwrap_or_else(|| (binding_name.clone(), mutable));
                self.scopes
                    .last_mut()
                    .expect("loop scope")
                    .insert(name.clone(), item.clone());
                self.declare_hir_local(
                    name,
                    item.clone(),
                    pattern_mutable,
                    binding_token
                        .as_ref()
                        .map_or_else(|| pattern.span.clone(), |token| self.token_span(token)),
                )
            } else {
                let root =
                    self.push_hir_local(binding_name, item.clone(), false, pattern.span.clone());
                let mut bindings = Vec::new();
                self.collect_body_binding_pattern(&pattern, &item, &mut Vec::new(), &mut bindings);
                let id = HirBindingPatternId(
                    u32::try_from(self.hir_binding_patterns.len())
                        .expect("HIR binding pattern limit"),
                );
                self.hir_binding_patterns.push(HirBindingPattern {
                    id,
                    root,
                    bindings,
                    origin: pattern.span.clone(),
                });
                root
            };
            if let Some(witness) = witness {
                self.iteration_witnesses.insert(local, witness);
            }
        }
        self.loop_depth += 1;
        self.statement();
        self.loop_depth = self.loop_depth.saturating_sub(1);
        self.scopes.pop();
        self.hir_scopes.pop();
    }

    fn try_statement(&mut self) {
        self.bump();
        self.try_block_effects.push(BTreeSet::new());
        self.block();
        let reaching = self.try_block_effects.pop().unwrap_or_default();
        let mut caught = Vec::new();
        while self.eat(TokenKind::Catch) {
            self.eat(TokenKind::LeftParen);
            let binding_token = self.bump().cloned();
            self.eat(TokenKind::Colon);
            let caught_type = self.parse_local_type();
            if let Some(id) = caught_type.as_ref().and_then(nominal_id) {
                if caught
                    .iter()
                    .any(|earlier| catch_handles(self.program, *earlier, id))
                    && let Some(token) = self.token().cloned()
                {
                    self.error(
                        "TYPE_REDUNDANT_CATCH",
                        "catch type is already handled",
                        &token,
                        "remove this unreachable catch clause",
                    );
                }
                caught.push(id);
            }
            self.eat(TokenKind::RightParen);
            self.caught_effects.push(caught.clone());
            let mut catch_scope = BTreeMap::new();
            if let (Some(binding), Some(ty)) = (binding_token, caught_type) {
                let name = self.module.source[binding.range.clone()].to_owned();
                catch_scope.insert(name.clone(), ty.clone());
                self.hir_scopes.push(BTreeMap::new());
                self.declare_hir_local(name, ty, false, self.token_span(&binding));
            } else {
                self.hir_scopes.push(BTreeMap::new());
            }
            self.scopes.push(catch_scope);
            self.block();
            self.scopes.pop();
            self.hir_scopes.pop();
            self.caught_effects.pop();
        }
        if let Some(missing) = reaching.iter().find(|effect| {
            !caught
                .iter()
                .any(|caught| catch_handles(self.program, *caught, **effect))
        }) && let Some(declaration) = self.program.graph.declaration(*missing)
            && let Some(token) = self.tokens.last().cloned()
        {
            self.error(
                "TYPE_MISSING_CATCH",
                format!(
                    "try block does not catch `{}`",
                    declaration.name.as_deref().unwrap_or("error")
                ),
                &token,
                "add a catch clause for every reachable error type",
            );
        }
    }

    fn expression(&mut self, minimum: u8, expected: Option<&Type>) -> Option<ExpressionType> {
        let start = self.index;
        let expression = self.expression_inner(minimum, expected);
        if let Some(expression) = expression.as_ref() {
            self.record_hir_expression(start, expression);
        }
        expression
    }

    #[allow(clippy::too_many_lines)]
    fn expression_inner(&mut self, minimum: u8, expected: Option<&Type>) -> Option<ExpressionType> {
        let start = self.index;
        let mut left = self.prefix(expected)?;
        loop {
            let explicit_generics =
                if self.kind() == Some(TokenKind::Less) && left.callable.is_some() {
                    Some(self.call_generic_arguments())
                } else {
                    None
                };
            if self.kind() == Some(TokenKind::LeftParen) {
                left = self.call_expression(&left, expected, explicit_generics);
                self.record_hir_expression(start, &left);
                continue;
            }
            if matches!(self.kind(), Some(TokenKind::Dot | TokenKind::QuestionDot)) {
                left = self.member_expression(&left);
                self.record_hir_expression(start, &left);
                continue;
            }
            if self.kind() == Some(TokenKind::LeftBracket)
                && self.nth(1) == Some(TokenKind::Identifier)
                && self.nth(2) == Some(TokenKind::Dot)
                && self.nth(3) == Some(TokenKind::Identifier)
                && self.nth(4) == Some(TokenKind::RightBracket)
            {
                left = self.computed_member_expression(&left);
                self.record_hir_expression(start, &left);
                continue;
            }
            if self.eat(TokenKind::Bang) {
                let token = self.tokens.get(self.index.saturating_sub(1)).cloned();
                if let Type::Optional(inner) = left.ty {
                    left.ty = *inner;
                    left.optional_chain_value = None;
                } else if let Some(token) = token.as_ref() {
                    self.error(
                        "TYPE_FORCE_UNWRAP_NON_OPTIONAL",
                        "postfix `!` requires an optional value",
                        token,
                        "use `!` only after a value of type `T | undefined`",
                    );
                    left.ty = Type::Error;
                    left.optional_chain_value = None;
                }
                self.record_hir_expression(start, &left);
                continue;
            }
            if self.eat(TokenKind::LeftBracket) {
                self.expression(0, Some(&Type::Primitive(PrimitiveType::Usize)));
                self.eat(TokenKind::RightBracket);
                let active = left
                    .optional_chain_value
                    .as_ref()
                    .unwrap_or(&left.ty)
                    .clone();
                let element = indexed_element_type(&active);
                if left.optional_chain_value.is_some() {
                    left.ty = optional_type(element.clone());
                    left.optional_chain_value = Some(element);
                } else {
                    left.ty = element;
                }
                left.place = None;
                self.record_hir_expression(start, &left);
                continue;
            }
            if matches!(self.kind(), Some(TokenKind::As | TokenKind::AsQuestion)) {
                let operator = self.bump().map(|token| token.kind)?;
                let target_token = self.token().cloned();
                let target = self.parse_local_type().unwrap_or(Type::Error);
                left = self.cast_expression(operator, &left, target, target_token.as_ref());
                self.record_hir_expression(start, &left);
                continue;
            }
            if self.kind() == Some(TokenKind::Question) && minimum <= 2 {
                self.bump();
                let then_value = self.expression(0, expected);
                self.eat(TokenKind::Colon);
                let else_value = self.expression(2, expected);
                left = merge_branches(then_value, else_value);
                self.record_hir_expression(start, &left);
                continue;
            }
            let Some(kind) = self.kind() else {
                break;
            };
            let Some((left_power, right_power)) = binding_power(kind) else {
                break;
            };
            if left_power < minimum {
                break;
            }
            let operator_token = self.bump().cloned()?;
            let operator = operator_token.kind;
            if operator == TokenKind::InstanceOf {
                let target_token = self.token().cloned();
                let target = self.parse_local_type().unwrap_or(Type::Error);
                left = self.instance_of_expression(&left, &target, target_token.as_ref());
                self.record_hir_expression(start, &left);
                continue;
            }
            let right = self.expression(right_power, binary_right_expected(operator, &left.ty));
            left = self.binary(operator, left, right, &operator_token);
            self.record_hir_expression(start, &left);
        }
        Some(left)
    }

    fn prefix(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        let start = self.index;
        let expression = self.prefix_inner(expected);
        if let Some(expression) = expression.as_ref() {
            self.record_hir_expression(start, expression);
        }
        expression
    }

    #[allow(clippy::too_many_lines)]
    fn prefix_inner(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        match self.kind()? {
            TokenKind::Bang => {
                self.bump();
                let value = self.expression(24, Some(&Type::Primitive(PrimitiveType::Bool)));
                self.require_bool(value);
                Some(value_type(Type::Primitive(PrimitiveType::Bool)))
            }
            TokenKind::Minus | TokenKind::Tilde => {
                let operator = self.bump()?.kind;
                let value = self.expression(24, expected)?;
                if !is_numeric(&value.ty)
                    && let Some(token) = self.token().cloned()
                {
                    self.error(
                        "TYPE_INVALID_UNARY_OPERAND",
                        format!("operator {operator:?} requires a numeric operand"),
                        &token,
                        "use a numeric value or an explicit operator interface",
                    );
                }
                Some(value_type(value.ty))
            }
            TokenKind::Amp => {
                self.bump();
                let mutable = self.eat(TokenKind::Mut);
                let value = self.expression(24, None)?;
                Some(value_type(Type::Reference {
                    mutable,
                    lifetime: "scope".into(),
                    referent: Box::new(value.ty),
                }))
            }
            TokenKind::Star => {
                let token = self.bump().cloned();
                let value = self.expression(24, None)?;
                if self.unsafe_depth == 0
                    && let Some(token) = token
                {
                    self.error(
                        "TYPE_RAW_POINTER_REQUIRES_UNSAFE",
                        "raw pointer dereference requires an unsafe block",
                        &token,
                        "wrap the operation in `unsafe { ... }`",
                    );
                }
                Some(value_type(match value.ty {
                    Type::RawPointer { pointee, .. }
                    | Type::Reference {
                        referent: pointee, ..
                    } => *pointee,
                    _ => Type::Error,
                }))
            }
            TokenKind::Move if self.nth(1) == Some(TokenKind::LeftParen) => {
                self.bump();
                self.lambda_expression(expected, true)
            }
            TokenKind::Move => {
                self.bump();
                self.expression(24, expected)
            }
            TokenKind::Await => {
                let token = self.bump().cloned();
                if !self.function.is_async
                    && let Some(token) = token.as_ref()
                {
                    self.error(
                        "TYPE_AWAIT_OUTSIDE_ASYNC",
                        "await is valid only in an async declaration",
                        token,
                        "mark the enclosing function async",
                    );
                }
                let value = self.expression(24, None)?;
                let awaited = promise_like(self.program, &value.ty);
                let effects = awaited
                    .as_ref()
                    .map_or_else(|| value.effects.clone(), |(_, effects)| effects.clone());
                if !effects.is_empty()
                    && let Some(token) = token.as_ref()
                {
                    self.error(
                        "TYPE_MISSING_TRY_AWAIT",
                        "fallible async completion requires `try await`",
                        token,
                        "write `try await` and handle or declare its closed error set",
                    );
                }
                Some(value_type(
                    awaited.map_or(Type::Error, |(result, _)| result),
                ))
            }
            TokenKind::Try => {
                let token = self.bump().cloned();
                if self.eat(TokenKind::Await) {
                    let value = self.expression(24, None)?;
                    let awaited = promise_like(self.program, &value.ty);
                    let effects = awaited
                        .as_ref()
                        .map_or_else(|| value.effects.clone(), |(_, effects)| effects.clone());
                    self.record_effects(&effects, token.as_ref());
                    Some(value_type(
                        awaited.map_or(Type::Error, |(result, _)| result),
                    ))
                } else {
                    self.try_prefix_depth += 1;
                    let value = self.expression(24, expected);
                    self.try_prefix_depth = self.try_prefix_depth.saturating_sub(1);
                    value
                }
            }
            TokenKind::IntegerLiteral => {
                let text = self.text()?.to_owned();
                self.bump();
                Some(value_type(integer_literal_type(&text, expected)))
            }
            TokenKind::FloatLiteral => {
                let text = self.text()?.to_owned();
                self.bump();
                Some(value_type(float_literal_type(&text, expected)))
            }
            TokenKind::True | TokenKind::False => {
                self.bump();
                Some(value_type(Type::Primitive(PrimitiveType::Bool)))
            }
            TokenKind::CharacterLiteral => {
                self.bump();
                Some(value_type(Type::Primitive(PrimitiveType::Char)))
            }
            TokenKind::StringLiteral => {
                self.bump();
                if expected.is_some_and(expected_owned_string) {
                    Some(value_type(Type::String))
                } else {
                    Some(value_type(Type::Reference {
                        mutable: false,
                        lifetime: "static".into(),
                        referent: Box::new(Type::Str),
                    }))
                }
            }
            TokenKind::Less
                if self.module.path.extension().is_some_and(|ext| ext == "tnx")
                    && self.looks_like_jsx_start() =>
            {
                self.jsx_element_expression(expected)
            }
            TokenKind::TemplateLiteral => self.template_literal(),
            TokenKind::Undefined => {
                let token = self.bump().cloned();
                if let Some(Type::Optional(inner)) = expected {
                    Some(value_type(Type::Optional(inner.clone())))
                } else {
                    if let Some(token) = token {
                        self.error(
                            "TYPE_UNCONSTRAINED_UNDEFINED",
                            "undefined requires an expected optional type",
                            &token,
                            "add an optional annotation or use this in an optional context",
                        );
                    }
                    Some(value_type(Type::Error))
                }
            }
            TokenKind::Identifier | TokenKind::This | TokenKind::Super => self.name_expression(),
            TokenKind::LeftParen if self.is_lambda_start() => {
                self.lambda_expression(expected, false)
            }
            TokenKind::LeftParen => self.parenthesized(expected),
            TokenKind::LeftBracket => Some(self.array_literal(expected)),
            TokenKind::LeftBrace => self.object_literal(expected),
            TokenKind::New => self.new_expression(),
            TokenKind::Switch => self.match_expression(expected),
            TokenKind::Match => {
                let token = self.bump().cloned();
                if let Some(token) = token.as_ref() {
                    self.error(
                        "TYPE_EXCLUDED_MATCH",
                        "`match` is not part of canonical TypeNative",
                        token,
                        "use `switch`",
                    );
                }
                Some(value_type(Type::Error))
            }
            _ => {
                let token = self.bump().cloned();
                if let Some(token) = token {
                    self.error(
                        "TYPE_EXPECTED_EXPRESSION",
                        "expected a typed expression",
                        &token,
                        "this token cannot begin an expression",
                    );
                }
                Some(value_type(Type::Error))
            }
        }
    }

    fn name_expression(&mut self) -> Option<ExpressionType> {
        let token = self.bump().cloned()?;
        let name = self.module.source[token.range.clone()].to_owned();
        if token.kind == TokenKind::Super {
            return Some(self.super_expression(&token));
        }
        if let Some((scope_index, ty)) = self.lookup_scoped(&name) {
            self.record_capture(&name, &ty, scope_index, &token);
            return Some(ExpressionType {
                ty,
                optional_chain_value: None,
                place: Some(name),
                effects: Vec::new(),
                callable: None,
                call_name: None,
                captures: self
                    .closure_bindings
                    .get(&self.module.source[token.range.clone()])
                    .cloned()
                    .unwrap_or_default(),
                resolution: self
                    .lookup_hir(&self.module.source[token.range.clone()])
                    .map(ResolvedValue::Local),
                type_qualifier: false,
                jsx: None,
            });
        }
        if let Some((declaration, ty, mutable_static)) = self.resolve_global_value(&name) {
            if mutable_static && self.unsafe_depth == 0 {
                self.error(
                    "TYPE_STATIC_MUT_REQUIRES_UNSAFE",
                    format!("access to mutable static `{name}` requires unsafe"),
                    &token,
                    "access mutable static storage inside an unsafe block",
                );
            }
            let mut expression = value_type(ty);
            expression.resolution = Some(ResolvedValue::Declaration(declaration));
            return Some(expression);
        }
        if self.kind() == Some(TokenKind::Dot)
            && let Some(ty) = primitive(&name)
        {
            let mut expression = value_type(ty);
            expression.type_qualifier = true;
            return Some(expression);
        }
        if let Some((declaration, function)) = self.callable.get(&(self.module.id, name.clone())) {
            let mut expression = value_type(Type::Function(tn_hir::FunctionType {
                parameters: function
                    .parameters
                    .iter()
                    .map(|parameter| normalize_alias_deep(self.program, &parameter.ty))
                    .collect(),
                result: Box::new(normalize_alias_deep(self.program, &function.result)),
                effects: function.effects.clone(),
                generics: function
                    .generics
                    .iter()
                    .map(|parameter| tn_hir::GenericConstraint {
                        name: parameter.name.clone(),
                        namespace: parameter.namespace,
                        bounds: parameter.bounds.clone(),
                    })
                    .collect(),
                is_async: function.is_async && !function.is_generator,
                is_unsafe: function.is_unsafe,
            }));
            expression.callable = Some(CallableIdentity::Function(*declaration));
            expression.call_name = Some(name);
            expression.resolution = Some(ResolvedValue::Declaration(*declaration));
            return Some(expression);
        }
        if self.kind() == Some(TokenKind::Dot)
            && let Some(Type::Nominal(declaration, _)) = self.resolve_type_name(&name)
            && let Some(definition) = self.program.definition(declaration)
        {
            let arguments = definition
                .generics
                .iter()
                .filter(|parameter| parameter.namespace == tn_hir::Namespace::Type)
                .map(|parameter| Type::Generic(parameter.name.clone()))
                .collect();
            let mut expression = value_type(Type::Nominal(declaration, arguments));
            expression.resolution = Some(ResolvedValue::Declaration(declaration));
            expression.type_qualifier = true;
            return Some(expression);
        }
        self.error(
            "RESOLVE_UNRESOLVED_VALUE",
            format!("unresolved value name `{name}`"),
            &token,
            "declare or import this value before use",
        );
        Some(value_type(Type::Error))
    }

    fn super_expression(&mut self, token: &Token) -> ExpressionType {
        let constructor = self.program.definition(self.owner).and_then(|definition| {
            let DefinitionData::Class {
                base: Some(base), ..
            } = definition.data
            else {
                return None;
            };
            self.program.definition(base).and_then(|definition| {
                let DefinitionData::Class { constructor, .. } = &definition.data else {
                    return None;
                };
                constructor.as_ref().map_or_else(
                    || {
                        Some(Function {
                            parameters: Vec::new(),
                            result: Type::Primitive(PrimitiveType::Void),
                            effects: Vec::new(),
                            generics: Vec::new(),
                            is_async: false,
                            is_generator: false,
                            is_unsafe: false,
                            body_start: 0,
                            body_end: 0,
                        })
                    },
                    |constructor| Some(constructor.function.clone()),
                )
            })
        });
        if let Some(constructor) = constructor {
            return value_type(function_type(&constructor));
        }
        self.error(
            "TYPE_SUPER_WITHOUT_BASE",
            "super is unavailable without a base class",
            token,
            "remove this super reference",
        );
        value_type(Type::Error)
    }

    fn parenthesized(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        self.bump();
        let first = self.expression(0, expected)?;
        if !self.eat(TokenKind::Comma) {
            self.eat(TokenKind::RightParen);
            return Some(first);
        }
        let mut elements = vec![first.ty];
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightParen) {
            if let Some(element) = self.expression(0, None) {
                elements.push(element.ty);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightParen);
        Some(value_type(Type::Tuple(elements)))
    }

    fn is_lambda_start(&self) -> bool {
        let mut depth = 0_u32;
        for (offset, token) in self.tokens.iter().skip(self.index).enumerate() {
            match token.kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self.tokens[self.index + offset + 1..]
                            .iter()
                            .take(4)
                            .any(|next| next.kind == TokenKind::FatArrow);
                    }
                }
                _ => {}
            }
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    fn lambda_expression(
        &mut self,
        expected: Option<&Type>,
        moved: bool,
    ) -> Option<ExpressionType> {
        let token = self.token().cloned()?;
        let expected_function = match expected {
            Some(Type::Function(function)) => Some(function),
            _ => None,
        };
        self.eat(TokenKind::LeftParen);
        let mut parameters = Vec::new();
        let mut parameter_ids = Vec::new();
        let mut scope = BTreeMap::new();
        let mut hir_scope = BTreeMap::new();
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightParen) {
            let name_token = self.bump().cloned()?;
            let name = self.module.source[name_token.range.clone()].to_owned();
            let position = parameters.len();
            let ty = if self.eat(TokenKind::Colon) {
                self.parse_local_type().unwrap_or(Type::Error)
            } else {
                expected_function
                    .and_then(|function| function.parameters.get(position))
                    .cloned()
                    .unwrap_or(Type::Error)
            };
            parameters.push(ty.clone());
            let id = HirLocalId(u32::try_from(self.hir_locals.len()).expect("HIR local limit"));
            parameter_ids.push(id);
            hir_scope.insert(name.clone(), id);
            self.hir_locals.push(HirLocal {
                id,
                name: name.clone(),
                ty: ty.clone(),
                mutable: false,
                origin: self.token_span(&name_token),
            });
            scope.insert(name, ty);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightParen);
        let declared_result = if self.eat(TokenKind::Colon) {
            self.parse_local_type()
        } else {
            None
        };
        self.eat(TokenKind::FatArrow);
        let body_start = self.index;
        let expected_result = declared_result
            .as_ref()
            .or_else(|| expected_function.map(|function| function.result.as_ref()));
        self.capture_contexts.push(CaptureContext {
            scope_base: self.scopes.len(),
            captures: BTreeMap::new(),
        });
        self.scopes.push(scope);
        self.hir_scopes.push(hir_scope);
        let result = if self.kind() == Some(TokenKind::LeftBrace) {
            self.block();
            Type::Primitive(PrimitiveType::Void)
        } else {
            self.expression(0, expected_result)
                .map_or(Type::Error, |expression| expression.ty)
        };
        let body_end = self.index;
        self.scopes.pop();
        self.hir_scopes.pop();
        let mut captures = self
            .capture_contexts
            .pop()
            .expect("lambda capture context")
            .captures
            .into_values()
            .collect::<Vec<_>>();
        if moved {
            for capture in &mut captures {
                capture.kind = CaptureKind::Move;
                if let Type::Reference { referent, .. } = &capture.ty {
                    capture.ty = referent.as_ref().clone();
                }
            }
        }
        if expected_result.is_some_and(|expected| !compatible(self.program, &result, expected)) {
            self.error(
                "TYPE_LAMBDA_RESULT_MISMATCH",
                "lambda result does not match its expected function type",
                &token,
                "return a value compatible with the contextual result type",
            );
        }
        if parameters.contains(&Type::Error) {
            self.error(
                "TYPE_LAMBDA_PARAMETER_ANNOTATION_REQUIRED",
                "lambda parameters require annotations without an expected function type",
                &token,
                "add parameter types or pass the lambda in a typed context",
            );
        }
        let body_origin = self.tokens[body_start..body_end]
            .first()
            .zip(self.tokens[body_start..body_end].last())
            .map_or_else(
                || self.token_span(&token),
                |(first, last)| {
                    SourceSpan::new(
                        self.module.path.to_string_lossy(),
                        first.range.start..last.range.end,
                        &self.module.source,
                    )
                },
            );
        let origin = self.tokens.get(body_end.saturating_sub(1)).map_or_else(
            || self.token_span(&token),
            |last| {
                SourceSpan::new(
                    self.module.path.to_string_lossy(),
                    token.range.start..last.range.end,
                    &self.module.source,
                )
            },
        );
        self.closures.push(ClosureAnalysis {
            owner: self.owner,
            captures: captures.clone(),
            moved,
            span: origin.clone(),
        });
        let function = tn_hir::FunctionType {
            parameters,
            result: Box::new(declared_result.unwrap_or(result)),
            effects: expected_function.map_or_else(Vec::new, |function| function.effects.clone()),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        };
        let id = HirClosureId(
            u32::try_from(self.hir_closures.len()).expect("HIR closure identity limit"),
        );
        let hir_captures = captures
            .iter()
            .filter_map(|capture| {
                Some(HirClosureCapture {
                    local: self.lookup_hir(&capture.name)?,
                    name: capture.name.clone(),
                    ty: capture.ty.clone(),
                    mode: match capture.kind {
                        CaptureKind::SharedBorrow => HirCaptureMode::SharedBorrow,
                        CaptureKind::MutableBorrow => HirCaptureMode::MutableBorrow,
                        CaptureKind::Move => HirCaptureMode::Move,
                    },
                    origin: capture.span.clone(),
                })
            })
            .collect();
        self.hir_closures.push(HirClosure {
            id,
            function: function.clone(),
            parameters: parameter_ids,
            captures: hir_captures,
            moved,
            body: body_origin,
            origin,
        });
        let mut closure = value_type(Type::Function(function));
        closure.captures = captures;
        closure.resolution = Some(ResolvedValue::Closure(id));
        Some(closure)
    }

    fn array_literal(&mut self, expected: Option<&Type>) -> ExpressionType {
        self.bump();
        let expected_element = match expected {
            Some(Type::Array(element, _) | Type::Slice(element)) => Some(element.as_ref()),
            _ => None,
        };
        let mut element_type = None;
        let mut length = 0_u64;
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightBracket) {
            if let Some(element) = self.expression(0, expected_element) {
                if let Some(existing) = &element_type {
                    if !compatible(self.program, &element.ty, existing)
                        && let Some(token) = self.token().cloned()
                    {
                        self.error(
                            "TYPE_ARRAY_ELEMENT_MISMATCH",
                            "array literal elements have incompatible types",
                            &token,
                            "use one element type",
                        );
                    }
                } else {
                    element_type = Some(element.ty);
                }
            }
            length += 1;
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightBracket);
        value_type(Type::Array(
            Box::new(element_type.unwrap_or(Type::Error)),
            length,
        ))
    }

    fn jsx_element_expression(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        let start = self.index;
        let (id, element_type) = self.parse_jsx_element(expected)?;
        let mut expression = value_type(element_type);
        expression.jsx = Some(id);
        self.record_hir_expression_range(start, self.index, &expression);
        Some(expression)
    }

    fn parse_jsx_element(&mut self, _expected: Option<&Type>) -> Option<(HirJsxId, Type)> {
        let start = self.index;
        if !self.eat(TokenKind::Less) {
            return None;
        }
        if self.eat(TokenKind::Greater) {
            let children = self.parse_jsx_children(None);
            if self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash) {
                self.bump();
                self.bump();
                if !self.eat(TokenKind::Greater) {
                    self.error_current(
                        "SYNTAX_JSX_EXPECTED_FRAGMENT_CLOSE",
                        "expected `>` after a fragment closing tag",
                        "close the fragment with `</>`",
                    );
                }
            } else {
                self.error_current(
                    "SYNTAX_JSX_EXPECTED_FRAGMENT_CLOSE",
                    "expected a fragment closing tag",
                    "close the fragment with `</>`",
                );
            }
            let id = self.push_jsx_element(
                None,
                Vec::new(),
                children,
                Type::Unknown,
                self.jsx_element_type(),
                None,
                None,
                true,
                start,
            );
            let element_type = self
                .hir_jsx_elements
                .get(id.0 as usize)
                .map_or(Type::Unknown, |element| element.element_type.clone());
            return Some((id, element_type));
        }

        let Some((name, name_start, name_end)) = self.parse_jsx_name() else {
            self.error_current(
                "SYNTAX_JSX_EXPECTED_NAME",
                "expected a JSX element name",
                "use an identifier or a component member expression",
            );
            self.recover_jsx_element(start);
            return None;
        };
        let (component, props_type, element_type) =
            self.resolve_jsx_component(&name, name_start, name_end);
        let fields = self.jsx_fields(&props_type);
        let mut properties = Vec::new();
        let mut provided = BTreeSet::new();
        let mut spread_provided = BTreeSet::new();
        let mut key = None;
        let mut reference = None;
        while self.kind().is_some()
            && !matches!(self.kind(), Some(TokenKind::Greater | TokenKind::Slash))
        {
            if self.at(TokenKind::LeftBrace) && self.nth(1) == Some(TokenKind::Ellipsis) {
                let origin_start = self.index;
                self.bump();
                self.bump();
                let value_start = self.index;
                let value = self.expression(0, Some(&props_type));
                let value_end = self.index;
                self.eat(TokenKind::RightBrace);
                if let Some(value) = value {
                    if props_type != Type::Unknown
                        && !compatible(self.program, &value.ty, &props_type)
                    {
                        self.error_span(
                            "TYPE_JSX_SPREAD_MISMATCH",
                            format!(
                                "JSX spread has type {:?}, expected {:?}",
                                value.ty, props_type
                            ),
                            &self.span_from_tokens(value_start, value_end),
                            "spread a value compatible with the component properties",
                        );
                    } else {
                        spread_provided.extend(fields.iter().map(|(name, _, _, _)| name.clone()));
                    }
                    if let Some(expression) =
                        self.hir_expression_id_for_range(value_start, value_end)
                    {
                        properties.push(HirJsxProperty {
                            name: None,
                            value: HirJsxValue::Expression(expression),
                            spread: true,
                            origin: self.span_from_tokens(origin_start, self.index),
                        });
                    }
                }
                continue;
            }
            let attr_start = self.index;
            let Some((attr_name, _, attr_end)) = self.parse_jsx_name() else {
                self.error_current(
                    "SYNTAX_JSX_EXPECTED_ATTRIBUTE",
                    "expected a JSX attribute",
                    "use a name, a spread attribute, or close the opening tag",
                );
                self.bump();
                continue;
            };
            let expected_field = fields
                .iter()
                .find(|(name, _, _, _)| name == &attr_name)
                .cloned();
            let expected_type = expected_field.as_ref().map(|(_, ty, _, _)| ty);
            let (value, value_type, value_span) =
                self.parse_jsx_attribute_value(expected_type, attr_start);
            if attr_name == "key" || attr_name == "ref" {
                let target = if attr_name == "key" {
                    &mut key
                } else {
                    &mut reference
                };
                if target.is_some() {
                    self.error_span(
                        "TYPE_DUPLICATE_JSX_ATTRIBUTE",
                        format!("duplicate JSX attribute `{attr_name}`"),
                        &self.span_from_tokens(attr_start, self.index),
                        "provide `key` and `ref` at most once",
                    );
                } else if let HirJsxValue::Expression(expression) = value {
                    if attr_name == "key"
                        && !is_jsx_key_type(&value_type)
                        && value_type != Type::Error
                    {
                        self.error_span(
                            "TYPE_JSX_INVALID_KEY",
                            "JSX keys must be strings or integer values",
                            &value_span,
                            "use a stable string or integer key",
                        );
                    }
                    if attr_name == "ref"
                        && !is_jsx_ref_type(self.program, &value_type, &element_type)
                        && value_type != Type::Error
                    {
                        self.error_span(
                            "TYPE_JSX_INVALID_REF",
                            "JSX refs must target the produced element type",
                            &value_span,
                            "use a `Ref<Element>` or mutable reference compatible with this component",
                        );
                    }
                    *target = Some(expression);
                } else {
                    self.error_span(
                        "TYPE_JSX_ATTRIBUTE_REQUIRES_EXPRESSION",
                        format!("JSX `{attr_name}` requires an expression value"),
                        &self.span_from_tokens(attr_start, self.index),
                        "write the value inside braces",
                    );
                }
                continue;
            }
            if !provided.insert(attr_name.clone()) {
                self.error_span(
                    "TYPE_DUPLICATE_JSX_ATTRIBUTE",
                    format!("duplicate JSX attribute `{attr_name}`"),
                    &self.span_from_tokens(attr_start, self.index),
                    "provide each property at most once",
                );
            }
            if let Some((_, field_type, _, _)) = expected_field {
                if !compatible(self.program, &value_type, &field_type) {
                    self.error_span(
                        "TYPE_MISMATCH",
                        format!(
                            "JSX property `{attr_name}` has type {:?}, expected {:?}",
                            value_type, field_type
                        ),
                        &value_span,
                        "provide a value compatible with the declared property type",
                    );
                }
            } else if props_type != Type::Unknown {
                self.error_span(
                    "TYPE_UNKNOWN_JSX_PROPERTY",
                    format!("unknown JSX property `{attr_name}`"),
                    &self.span_from_tokens(attr_start, attr_end.max(attr_start + 1)),
                    "use a property declared by the component's props type",
                );
            }
            properties.push(HirJsxProperty {
                name: Some(attr_name),
                value,
                spread: false,
                origin: self.span_from_tokens(attr_start, self.index),
            });
        }
        let self_closing = self.eat(TokenKind::Slash);
        if !self.eat(TokenKind::Greater) {
            self.error_current(
                "SYNTAX_JSX_EXPECTED_OPENING_CLOSE",
                "expected `>` to close the JSX opening tag",
                "close the opening tag before adding children",
            );
        }
        let child_type = fields
            .iter()
            .find(|(name, _, _, _)| name == "children")
            .map(|(_, ty, _, _)| ty.clone());
        let children = if self_closing {
            Vec::new()
        } else {
            let children = self.parse_jsx_children(child_type.as_ref());
            if self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash) {
                let close_start = self.index;
                self.bump();
                self.bump();
                let Some((closing_name, _, _)) = self.parse_jsx_name() else {
                    self.error_current(
                        "SYNTAX_JSX_EXPECTED_CLOSE_NAME",
                        "expected a JSX closing tag name",
                        "repeat the opening component name",
                    );
                    self.recover_jsx_closing_tag();
                    return None;
                };
                if closing_name != name {
                    self.error_span(
                        "TYPE_JSX_TAG_MISMATCH",
                        "JSX closing tag does not match its opening tag",
                        &self.span_from_tokens(close_start, self.index),
                        "close the element with the same component name",
                    );
                }
                if !self.eat(TokenKind::Greater) {
                    self.error_current(
                        "SYNTAX_JSX_EXPECTED_CLOSE",
                        "expected `>` after a JSX closing tag",
                        "finish the closing tag",
                    );
                }
            } else {
                self.error_current(
                    "SYNTAX_JSX_EXPECTED_CLOSE",
                    "expected a JSX closing tag",
                    "close the element before the end of the function",
                );
            }
            children
        };
        if !children.is_empty() {
            if provided.contains("children") {
                self.error_span(
                    "TYPE_DUPLICATE_JSX_ATTRIBUTE",
                    "JSX children cannot be supplied both as an attribute and as child nodes",
                    &self.span_from_tokens(start, self.index),
                    "use either the `children` property or nested child nodes",
                );
            } else {
                if let Some(child_type) = child_type.as_ref() {
                    self.validate_jsx_children(&children, child_type);
                }
                properties.push(HirJsxProperty {
                    name: Some("children".into()),
                    value: HirJsxValue::Children(children.clone()),
                    spread: false,
                    origin: self.span_from_tokens(start, self.index),
                });
                provided.insert("children".into());
            }
        }
        for (field_name, _, optional, has_initializer) in &fields {
            if !optional
                && !has_initializer
                && !provided.contains(field_name)
                && !spread_provided.contains(field_name)
            {
                self.error_span(
                    "TYPE_MISSING_JSX_PROPERTY",
                    format!("missing required JSX property `{field_name}`"),
                    &self.span_from_tokens(start, self.index),
                    "provide the required property or mark it optional",
                );
            }
        }
        let id = self.push_jsx_element(
            component,
            properties,
            children,
            props_type,
            element_type,
            key,
            reference,
            false,
            start,
        );
        let result_type = self
            .hir_jsx_elements
            .get(id.0 as usize)
            .map_or(Type::Error, |element| element.element_type.clone());
        Some((id, result_type))
    }

    fn parse_jsx_children(&mut self, expected: Option<&Type>) -> Vec<HirJsxChild> {
        let expected_child = expected.map(|expected| match expected {
            Type::Array(element, _) | Type::Slice(element) => element.as_ref(),
            _ => expected,
        });
        let mut children = Vec::new();
        while self.kind().is_some() {
            if self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash) {
                break;
            }
            if self.at(TokenKind::LeftBrace) {
                self.bump();
                if self.at(TokenKind::RightBrace) {
                    self.bump();
                    continue;
                }
                let expression_start = self.index;
                let expression = self.expression(0, expected_child);
                let expression_end = self.index;
                if !self.eat(TokenKind::RightBrace) {
                    self.error_current(
                        "SYNTAX_JSX_EXPECTED_EXPRESSION_CLOSE",
                        "expected `}` after a JSX expression child",
                        "close the expression container",
                    );
                }
                if let Some(expression) = expression
                    && let Some(id) =
                        self.hir_expression_id_for_range(expression_start, expression_end)
                {
                    if let Some(expected) = expected_child
                        && !compatible(self.program, &expression.ty, expected)
                    {
                        self.error_span(
                            "TYPE_MISMATCH",
                            format!(
                                "JSX child has type {:?}, expected {:?}",
                                expression.ty, expected
                            ),
                            &self.span_from_tokens(expression_start, expression_end),
                            "provide a child compatible with the component's children type",
                        );
                    }
                    children.push(HirJsxChild::Expression(id));
                }
                continue;
            }
            if self.looks_like_jsx_start() && self.nth(1) != Some(TokenKind::Greater) {
                let child_start = self.index;
                if let Some(expression) = self.jsx_element_expression(expected_child)
                    && let Some(id) = expression.jsx
                {
                    let child_type = expression.ty.clone();
                    children.push(HirJsxChild::Element(id));
                    if let Some(expected) = expected_child
                        && !compatible(self.program, &child_type, expected)
                    {
                        self.error_span(
                            "TYPE_MISMATCH",
                            format!(
                                "JSX child has type {:?}, expected {:?}",
                                child_type, expected
                            ),
                            &self.span_from_tokens(child_start, self.index),
                            "provide a child compatible with the component's children type",
                        );
                    }
                }
                continue;
            }
            let text_start = self.index;
            let mut text_end = text_start;
            while self.kind().is_some()
                && !self.at(TokenKind::LeftBrace)
                && !(self.at(TokenKind::Less)
                    && (self.nth(1) == Some(TokenKind::Slash)
                        || matches!(
                            self.nth(1),
                            Some(TokenKind::Identifier | TokenKind::From | TokenKind::Greater)
                        )))
            {
                self.bump();
                text_end = self.index;
            }
            if text_end == text_start {
                self.error_current(
                    "SYNTAX_JSX_EXPECTED_CHILD",
                    "expected a JSX child",
                    "use text, a braced expression, or a nested element",
                );
                self.bump();
                continue;
            }
            let first = &self.tokens[text_start];
            let last = &self.tokens[text_end - 1];
            let value = self.module.source[first.range.start..last.range.end].to_owned();
            children.push(HirJsxChild::Text {
                value,
                origin: self.span_from_tokens(text_start, text_end),
            });
        }
        children
    }

    fn parse_jsx_attribute_value(
        &mut self,
        expected: Option<&Type>,
        attribute_start: usize,
    ) -> (HirJsxValue, Type, SourceSpan) {
        if !self.eat(TokenKind::Equal) {
            return (
                HirJsxValue::Boolean(true),
                Type::Primitive(PrimitiveType::Bool),
                self.span_from_tokens(attribute_start, self.index),
            );
        }
        if self.at(TokenKind::LeftBrace) {
            self.bump();
            let start = self.index;
            let value = if self.at(TokenKind::RightBrace) {
                None
            } else {
                self.expression(0, expected)
            };
            let end = self.index;
            if !self.eat(TokenKind::RightBrace) {
                self.error_current(
                    "SYNTAX_JSX_EXPECTED_ATTRIBUTE_CLOSE",
                    "expected `}` after a JSX attribute expression",
                    "close the expression container",
                );
            }
            let ty = value.as_ref().map_or(Type::Error, |value| value.ty.clone());
            let value_span = if start < end {
                self.span_from_tokens(start, end)
            } else {
                self.span_from_tokens(start.saturating_sub(1), self.index)
            };
            let expression = self
                .hir_expression_id_for_range(start, end)
                .map(HirJsxValue::Expression)
                .unwrap_or(HirJsxValue::Boolean(true));
            return (expression, ty, value_span);
        }
        if self.at(TokenKind::StringLiteral) {
            let start = self.index;
            let literal = self
                .text()
                .and_then(decode_quoted_literal)
                .unwrap_or_default();
            let value = self.prefix(expected);
            let expression = self
                .hir_expression_id_for_range(start, self.index)
                .map(HirJsxValue::Expression)
                .unwrap_or(HirJsxValue::String(literal));
            let ty = value.as_ref().map_or(Type::Error, |value| value.ty.clone());
            return (expression, ty, self.span_from_tokens(start, self.index));
        }
        self.error_current(
            "SYNTAX_JSX_EXPECTED_ATTRIBUTE_VALUE",
            "expected a JSX attribute value",
            "use a quoted string or a braced expression",
        );
        (
            HirJsxValue::Boolean(true),
            Type::Primitive(PrimitiveType::Bool),
            self.span_from_tokens(self.index, self.index),
        )
    }

    fn parse_jsx_name(&mut self) -> Option<(String, usize, usize)> {
        let start = self.index;
        if !matches!(self.kind(), Some(TokenKind::Identifier | TokenKind::From)) {
            return None;
        }
        let mut name = self.text()?.to_owned();
        self.bump();
        loop {
            let separator = match self.kind() {
                Some(TokenKind::Dot) => '.',
                Some(TokenKind::Minus) => '-',
                _ => break,
            };
            self.bump();
            let Some(part) = self.text() else {
                break;
            };
            if !matches!(self.kind(), Some(TokenKind::Identifier | TokenKind::From)) {
                break;
            }
            name.push(separator);
            name.push_str(part);
            self.bump();
        }
        Some((name, start, self.index))
    }

    fn resolve_jsx_component(
        &mut self,
        name: &str,
        start: usize,
        end: usize,
    ) -> (Option<HirExpressionId>, Type, Type) {
        let mut expression_candidate = None;
        if !name.contains('-') {
            let diagnostics_before = self.diagnostics.len();
            let expressions_before = self.hir_expressions.len();
            let saved_index = self.index;
            self.index = start;
            let candidate = self.expression(0, None);
            let consumed = self.index;
            self.index = saved_index;
            if consumed == end
                && candidate
                    .as_ref()
                    .is_some_and(|expression| expression.ty != Type::Error)
            {
                expression_candidate = candidate;
            } else {
                self.diagnostics.truncate(diagnostics_before);
                self.hir_expressions.truncate(expressions_before);
            }
        }
        if let Some(expression) = expression_candidate {
            let component_type = expression.ty.clone();
            let id = self
                .hir_expression_id_for_range(start, end)
                .or_else(|| self.record_hir_expression_range(start, end, &expression));
            return self.resolve_jsx_component_type(id, component_type, name, start);
        }
        let mut declaration_and_function = None;
        if let Some((declaration, function)) = self.callable.get(&(self.module.id, name.to_owned()))
        {
            declaration_and_function = Some((*declaration, function.clone()));
        } else if let Some((namespace, member)) = name.split_once('.') {
            if let Some(import) = self.module.imports.iter().find(|import| {
                matches!(
                    &import.clause,
                    ImportClause::Namespace { local, .. } if local == namespace
                )
            }) {
                if let Some((declaration, function)) =
                    self.callable.get(&(import.target, member.to_owned()))
                {
                    declaration_and_function = Some((*declaration, function.clone()));
                }
            }
        }
        let (declaration, component_type) =
            if let Some((declaration, function)) = declaration_and_function {
                (declaration, function_type(&function))
            } else if let Some((declaration, ty, _)) = self.resolve_global_value(name) {
                (declaration, ty)
            } else {
                let token = self.tokens.get(start).cloned();
                if let Some(token) = token.as_ref() {
                    self.error(
                        "RESOLVE_UNRESOLVED_JSX_COMPONENT",
                        format!("unresolved JSX component `{name}`"),
                        token,
                        "declare or import the component before using it",
                    );
                }
                return (None, Type::Error, Type::Error);
            };
        let mut expression = value_type(component_type.clone());
        expression.resolution = Some(ResolvedValue::Declaration(declaration));
        expression.callable = matches!(component_type, Type::Function(_))
            .then_some(CallableIdentity::Function(declaration));
        expression.call_name = Some(name.into());
        let id = self.record_hir_expression_range(start, end, &expression);
        self.resolve_jsx_component_type(id, component_type, name, start)
    }

    fn resolve_jsx_component_type(
        &mut self,
        id: Option<HirExpressionId>,
        component_type: Type,
        name: &str,
        start: usize,
    ) -> (Option<HirExpressionId>, Type, Type) {
        let (props_type, element_type) = match &component_type {
            Type::Function(function) => (
                function
                    .parameters
                    .first()
                    .cloned()
                    .unwrap_or(Type::Unknown),
                (*function.result).clone(),
            ),
            Type::Nominal(component, arguments)
                if matches!(
                    declaration_name(self.program, *component),
                    Some("ComponentType" | "HostComponent")
                ) =>
            {
                (
                    arguments.first().cloned().unwrap_or(Type::Unknown),
                    self.jsx_element_type(),
                )
            }
            _ => {
                let token = self.tokens.get(start).cloned();
                if let Some(token) = token.as_ref() {
                    self.error(
                        "TYPE_JSX_COMPONENT_NOT_CALLABLE",
                        format!("JSX tag `{name}` does not resolve to a component value"),
                        token,
                        "use a function or a ComponentType value as the tag",
                    );
                }
                (Type::Error, Type::Error)
            }
        };
        (id, props_type, element_type)
    }

    fn jsx_fields(&self, props_type: &Type) -> Vec<(String, Type, bool, bool)> {
        let Type::Nominal(declaration, arguments) = normalize_alias(self.program, props_type)
        else {
            return Vec::new();
        };
        let Some(definition) = self.program.definition(declaration) else {
            return Vec::new();
        };
        let substitutions = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace != tn_hir::Namespace::Value)
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument))
            .collect::<BTreeMap<_, _>>();
        let fields = match &definition.data {
            DefinitionData::Struct { fields, .. } | DefinitionData::Class { fields, .. } => fields,
            _ => return Vec::new(),
        };
        fields
            .iter()
            .map(|field| {
                (
                    field.name.clone(),
                    substitute_type(&field.ty, &substitutions),
                    field.optional,
                    field.has_initializer,
                )
            })
            .collect()
    }

    fn validate_jsx_children(&mut self, children: &[HirJsxChild], expected: &Type) {
        let element_type = match expected {
            Type::Array(element, _) | Type::Slice(element) => element.as_ref(),
            _ => expected,
        };
        for child in children {
            let actual = match child {
                HirJsxChild::Element(id) => self
                    .hir_jsx_elements
                    .get(id.0 as usize)
                    .map_or(Type::Error, |element| element.element_type.clone()),
                HirJsxChild::Expression(id) => self
                    .hir_expressions
                    .get(id.0 as usize)
                    .map_or(Type::Error, |expression| expression.ty.clone()),
                HirJsxChild::Text { .. } => Type::String,
            };
            if !compatible(self.program, &actual, element_type)
                && !matches!(element_type, Type::Unknown | Type::Error)
            {
                self.error_span(
                    "TYPE_MISMATCH",
                    format!(
                        "JSX child has type {:?}, expected {:?}",
                        actual, element_type
                    ),
                    &self.jsx_child_origin(child),
                    "provide a compatible child value",
                );
            }
        }
    }

    fn jsx_child_origin(&self, child: &HirJsxChild) -> SourceSpan {
        match child {
            HirJsxChild::Element(id) => self
                .hir_jsx_elements
                .get(id.0 as usize)
                .map_or_else(|| self.empty_span(), |element| element.origin.clone()),
            HirJsxChild::Expression(id) => self
                .hir_expressions
                .get(id.0 as usize)
                .map_or_else(|| self.empty_span(), |expression| expression.origin.clone()),
            HirJsxChild::Text { origin, .. } => origin.clone(),
        }
    }

    fn jsx_element_type(&self) -> Type {
        self.resolve_type_name("Element").unwrap_or(Type::Unknown)
    }

    fn resolve_jsx_runtime(
        &mut self,
        operation: &str,
        component_type: Option<&Type>,
        properties_type: &Type,
        children: &[HirJsxChild],
        key: Option<HirExpressionId>,
        element_type: &Type,
        start: usize,
    ) -> (Option<DeclarationId>, Option<tn_hir::FunctionType>) {
        let span = self.span_from_tokens(start, self.index);
        let Some(declaration) = self.program.jsx_runtime_export(operation) else {
            self.error_span(
                "TYPE_JSX_RUNTIME_MISSING_EXPORT",
                format!("configured JSX runtime does not export `{operation}`"),
                &span,
                "export this callable from the configured JSX runtime module",
            );
            return (None, None);
        };
        let Some(item) = self.program.graph.declaration(declaration) else {
            self.error_span(
                "TYPE_JSX_RUNTIME_MISSING_DECLARATION",
                format!("configured JSX runtime declaration `{operation}` is missing"),
                &span,
                "provide a resolved TypeNative declaration for this runtime operation",
            );
            return (Some(declaration), None);
        };
        if item.kind != DeclarationKind::Function {
            self.error_span(
                "TYPE_JSX_RUNTIME_NOT_FUNCTION",
                format!("JSX runtime export `{operation}` is not a TypeNative function"),
                &item.span,
                "export a function declaration for this runtime operation",
            );
            return (Some(declaration), None);
        }
        let Some(definition) = self.program.definition(declaration) else {
            self.error_span(
                "TYPE_JSX_RUNTIME_MISSING_DECLARATION",
                format!("configured JSX runtime declaration `{operation}` has no definition"),
                &item.span,
                "provide the function body in the configured runtime module",
            );
            return (Some(declaration), None);
        };
        let DefinitionData::Function(runtime_function) = &definition.data else {
            self.error_span(
                "TYPE_JSX_RUNTIME_NOT_FUNCTION",
                format!("JSX runtime export `{operation}` is not callable"),
                &item.span,
                "export a function declaration for this runtime operation",
            );
            return (Some(declaration), None);
        };
        let runtime_function = normalize_function_aliases(self.program, runtime_function);
        let Type::Function(signature) = function_type(&runtime_function) else {
            unreachable!("function declarations always have function types");
        };
        let expected_parameters = if operation == "fragment" {
            vec![Type::Array(
                Box::new(self.jsx_children_type(children)),
                children.len() as u64,
            )]
        } else {
            let properties_type = if *properties_type == Type::Unknown {
                Type::Tuple(Vec::new())
            } else {
                properties_type.clone()
            };
            let key_type = key
                .and_then(|id| self.hir_expressions.get(id.0 as usize))
                .map_or_else(
                    || Type::Optional(Box::new(Type::String)),
                    |expression| optional_type(expression.ty.clone()),
                );
            vec![
                component_type.cloned().unwrap_or(Type::Error),
                properties_type,
                key_type,
            ]
        };
        if signature.parameters.len() != expected_parameters.len() {
            self.error_span(
                "TYPE_JSX_RUNTIME_WRONG_ARITY",
                format!(
                    "JSX runtime `{operation}` expects {} parameter(s), but the export declares {}",
                    expected_parameters.len(),
                    signature.parameters.len()
                ),
                &item.span,
                "declare the runtime operation with the required callable arity",
            );
            return (Some(declaration), None);
        }
        if signature.is_async {
            self.error_span(
                "TYPE_JSX_RUNTIME_ASYNC",
                format!("JSX runtime `{operation}` cannot be asynchronous"),
                &item.span,
                "return an Element synchronously from the runtime operation",
            );
        }
        if signature.is_unsafe || item.kind == DeclarationKind::ExternFunction {
            self.error_span(
                "TYPE_JSX_RUNTIME_FOREIGN_DECLARATION",
                format!("JSX runtime `{operation}` must be an ordinary safe TypeNative function"),
                &item.span,
                "implement the JSX runtime in TypeNative source",
            );
        }
        let mut substitutions = BTreeMap::new();
        for (parameter, expected) in signature.parameters.iter().zip(&expected_parameters) {
            infer_substitutions(parameter, expected, &mut substitutions);
        }
        infer_substitutions(&signature.result, element_type, &mut substitutions);
        let unresolved = signature
            .generics
            .iter()
            .filter(|generic| !substitutions.contains_key(&generic.name))
            .map(|generic| generic.name.clone())
            .collect::<Vec<_>>();
        if !unresolved.is_empty() {
            self.error_span(
                "TYPE_JSX_RUNTIME_GENERIC_INFERENCE",
                format!(
                    "could not infer JSX runtime generic parameter(s): {}",
                    unresolved.join(", ")
                ),
                &span,
                "make every runtime generic appear in an operation parameter or result",
            );
        }
        let runtime_token = self.tokens.get(start).cloned();
        self.validate_generic_bounds(&signature.generics, &substitutions, runtime_token.as_ref());
        let concrete_parameters = signature
            .parameters
            .iter()
            .map(|parameter| substitute_type(parameter, &substitutions))
            .collect::<Vec<_>>();
        for (index, (actual, expected)) in concrete_parameters
            .iter()
            .zip(&expected_parameters)
            .enumerate()
        {
            if *expected == Type::Error || !compatible(self.program, expected, actual) {
                let condition = match (operation, index) {
                    ("fragment", 0) => "TYPE_JSX_RUNTIME_CHILDREN_PARAMETER",
                    (_, 0) => "TYPE_JSX_RUNTIME_COMPONENT_PARAMETER",
                    (_, 1) => "TYPE_JSX_RUNTIME_PROPERTIES_PARAMETER",
                    _ => "TYPE_JSX_RUNTIME_KEY_PARAMETER",
                };
                self.error_span(
                    condition,
                    format!(
                        "JSX runtime `{operation}` parameter {} has type {actual:?}, expected {expected:?}",
                        index + 1
                    ),
                    &item.span,
                    "make the runtime operation parameter compatible with the typed JSX value",
                );
            }
        }
        let concrete_result = substitute_type(&signature.result, &substitutions);
        if *element_type != Type::Error && !compatible(self.program, &concrete_result, element_type)
        {
            self.error_span(
                "TYPE_JSX_RUNTIME_RESULT_MISMATCH",
                format!(
                    "JSX runtime `{operation}` returns {concrete_result:?}, expected {element_type:?}"
                ),
                &item.span,
                "return the Element type produced by the JSX component contract",
            );
        }
        if !signature.effects.is_empty() {
            if self.try_prefix_depth == 0 {
                self.error_span(
                    "TYPE_JSX_RUNTIME_EFFECTS",
                    format!("JSX runtime `{operation}` has undeclared effects"),
                    &span,
                    "use an infallible runtime operation or handle its effects explicitly",
                );
            } else {
                let runtime_token = self.tokens.get(start).cloned();
                self.record_effects(&signature.effects, runtime_token.as_ref());
            }
        }
        (
            Some(declaration),
            Some(tn_hir::FunctionType {
                parameters: concrete_parameters,
                result: Box::new(concrete_result),
                effects: signature.effects,
                generics: Vec::new(),
                is_async: signature.is_async,
                is_unsafe: signature.is_unsafe,
            }),
        )
    }

    fn jsx_children_type(&self, children: &[HirJsxChild]) -> Type {
        let mut result = None;
        for child in children {
            let actual = match child {
                HirJsxChild::Element(id) => self
                    .hir_jsx_elements
                    .get(id.0 as usize)
                    .map_or(Type::Error, |element| element.element_type.clone()),
                HirJsxChild::Expression(id) => self
                    .hir_expressions
                    .get(id.0 as usize)
                    .map_or(Type::Error, |expression| expression.ty.clone()),
                HirJsxChild::Text { .. } => Type::Reference {
                    mutable: false,
                    lifetime: "static".into(),
                    referent: Box::new(Type::Str),
                },
            };
            if let Some(previous) = &result
                && *previous != actual
                && !matches!(previous, Type::Unknown)
            {
                return Type::Error;
            }
            result = Some(actual);
        }
        result.unwrap_or(Type::String)
    }

    fn push_jsx_element(
        &mut self,
        component: Option<HirExpressionId>,
        properties: Vec<HirJsxProperty>,
        children: Vec<HirJsxChild>,
        properties_type: Type,
        element_type: Type,
        key: Option<HirExpressionId>,
        reference: Option<HirExpressionId>,
        fragment: bool,
        start: usize,
    ) -> HirJsxId {
        let component_type = component
            .and_then(|id| self.hir_expressions.get(id.0 as usize))
            .map(|expression| expression.ty.clone());
        let operation = if fragment {
            "fragment"
        } else if children.len() > 1 {
            "jsxs"
        } else {
            "jsx"
        };
        let (runtime, runtime_signature) = self.resolve_jsx_runtime(
            operation,
            component_type.as_ref(),
            &properties_type,
            &children,
            key,
            &element_type,
            start,
        );
        let id = HirJsxId(u32::try_from(self.hir_jsx_elements.len()).expect("HIR JSX limit"));
        self.hir_jsx_elements.push(HirJsxElement {
            id,
            component,
            properties,
            children,
            properties_type,
            element_type,
            key,
            reference,
            fragment,
            runtime,
            runtime_signature,
            origin: self.span_from_tokens(start, self.index),
        });
        id
    }

    fn hir_expression_id_for_range(&self, start: usize, end: usize) -> Option<HirExpressionId> {
        let first = self.tokens.get(start)?;
        let last = self.tokens.get(end.checked_sub(1)?)?;
        let start = u32::try_from(first.range.start).ok()?;
        let end = u32::try_from(last.range.end).ok()?;
        self.hir_expressions
            .iter()
            .rev()
            .find(|expression| {
                expression.origin.byte_start == start && expression.origin.byte_end == end
            })
            .map(|expression| expression.id)
    }

    fn span_from_tokens(&self, start: usize, end: usize) -> SourceSpan {
        let byte_start = self
            .tokens
            .get(start)
            .map_or(self.module.source.len(), |token| token.range.start);
        let byte_end = self
            .tokens
            .get(end.saturating_sub(1))
            .map_or(byte_start, |token| token.range.end);
        SourceSpan::new(
            self.module.path.to_string_lossy(),
            byte_start..byte_end,
            &self.module.source,
        )
    }

    fn empty_span(&self) -> SourceSpan {
        SourceSpan::new(
            self.module.path.to_string_lossy(),
            self.module.source.len()..self.module.source.len(),
            &self.module.source,
        )
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.kind() == Some(kind)
    }

    fn looks_like_generic_call(&self) -> bool {
        let mut depth = 0_u32;
        let mut offset = 0_usize;
        while offset < 512
            && let Some(kind) = self.nth(offset)
        {
            match kind {
                TokenKind::Less => depth += 1,
                TokenKind::Greater => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self.nth(offset + 1) == Some(TokenKind::LeftParen);
                    }
                }
                TokenKind::Semicolon | TokenKind::LeftBrace if depth == 0 => return false,
                _ => {}
            }
            offset += 1;
        }
        false
    }

    fn looks_like_jsx_start(&self) -> bool {
        self.at(TokenKind::Less)
            && !self.looks_like_generic_call()
            && matches!(
                self.nth(1),
                Some(TokenKind::Identifier | TokenKind::From | TokenKind::Greater)
            )
    }

    fn error_current(&mut self, id: &str, message: impl Into<String>, label: &str) {
        if let Some(token) = self.token().cloned() {
            self.error(id, message, &token, label);
        }
    }

    fn recover_jsx_element(&mut self, _start: usize) {
        while self.kind().is_some()
            && !matches!(
                self.kind(),
                Some(TokenKind::Greater | TokenKind::RightBrace | TokenKind::Semicolon)
            )
        {
            self.bump();
        }
        self.eat(TokenKind::Greater);
    }

    fn recover_jsx_closing_tag(&mut self) {
        while self.kind().is_some() && self.kind() != Some(TokenKind::Greater) {
            self.bump();
        }
        self.eat(TokenKind::Greater);
    }

    #[allow(clippy::too_many_lines)]
    fn template_literal(&mut self) -> Option<ExpressionType> {
        let token = self.bump()?.clone();
        let text = self.module.source[token.range.clone()].to_owned();
        let interpolations = template_interpolation_ranges(&text);
        let mut parts = Vec::new();
        let mut capture_types = Vec::new();
        let mut chunk_start = 1;
        for interpolation in interpolations {
            let chunk_end = interpolation.start.saturating_sub(2);
            parts.push(HirTemplatePart::Literal(decode_template_chunk(
                &text[chunk_start..chunk_end],
            )?));
            let absolute_start = token.range.start + interpolation.start;
            let absolute_end = token.range.start + interpolation.end;
            let absolute_start_u32 = u32::try_from(absolute_start).ok()?;
            let absolute_end_u32 = u32::try_from(absolute_end).ok()?;
            let interpolation_source = &self.module.source[absolute_start..absolute_end];
            let mut interpolation_tokens = lex(
                &self.module.path.to_string_lossy(),
                interpolation_source.as_bytes(),
            )
            .tokens
            .into_iter()
            .filter(|token| !token.kind.is_trivia())
            .collect::<Vec<_>>();
            for interpolation_token in &mut interpolation_tokens {
                interpolation_token.range.start += absolute_start;
                interpolation_token.range.end += absolute_start;
            }
            interpolation_tokens.push(Token {
                kind: TokenKind::RightBrace,
                range: absolute_end..absolute_end.saturating_add(1),
            });
            let outer_tokens = std::mem::replace(&mut self.tokens, interpolation_tokens);
            let outer_index = std::mem::replace(&mut self.index, 0);
            let expressions_before = self.hir_expressions.len();
            let expression = self.expression(0, None);
            let interpolation_consumed =
                self.index + 1 == self.tokens.len() && self.kind() == Some(TokenKind::RightBrace);
            self.tokens = outer_tokens;
            self.index = outer_index;
            let expression = expression.unwrap_or_else(|| value_type(Type::Error));
            if !interpolation_consumed {
                let interpolation_token = Token {
                    kind: TokenKind::TemplateLiteral,
                    range: absolute_start..absolute_end,
                };
                self.error(
                    "TYPE_INVALID_TEMPLATE_INTERPOLATION",
                    "template interpolation must contain one complete expression",
                    &interpolation_token,
                    "remove trailing tokens or combine them into one expression",
                );
            }
            if !template_displayable(self.program, self.function, &expression.ty) {
                let interpolation_token = Token {
                    kind: TokenKind::TemplateLiteral,
                    range: absolute_start..absolute_end,
                };
                self.error(
                    "TYPE_TEMPLATE_VALUE_NOT_DISPLAY",
                    format!(
                        "template interpolation of type {:?} does not implement Display",
                        expression.ty
                    ),
                    &interpolation_token,
                    "interpolate a displayable value or add an explicit Display implementation",
                );
            }
            let expression_id = self.hir_expressions[expressions_before..]
                .iter()
                .filter(|candidate| {
                    candidate.origin.byte_start >= absolute_start_u32
                        && candidate.origin.byte_end <= absolute_end_u32
                })
                .max_by_key(|candidate| {
                    candidate
                        .origin
                        .byte_end
                        .saturating_sub(candidate.origin.byte_start)
                })
                .map(|expression| expression.id)?;
            let storage = if expression.place.is_some() {
                HirTemplateStorage::SharedBorrow
            } else {
                HirTemplateStorage::Owned
            };
            let capture_type = if storage == HirTemplateStorage::SharedBorrow {
                Type::Reference {
                    mutable: false,
                    lifetime: "scope".into(),
                    referent: Box::new(expression.ty.clone()),
                }
            } else {
                expression.ty.clone()
            };
            capture_types.push(capture_type);
            parts.push(HirTemplatePart::Interpolation {
                expression: expression_id,
                ty: expression.ty,
                storage,
                origin: SourceSpan::new(
                    self.module.path.to_string_lossy(),
                    absolute_start..absolute_end,
                    &self.module.source,
                ),
            });
            chunk_start = interpolation.end + 1;
        }
        parts.push(HirTemplatePart::Literal(decode_template_chunk(
            &text[chunk_start..text.len().saturating_sub(1)],
        )?));
        let id = HirTemplateId(
            u32::try_from(self.hir_templates.len()).expect("HIR template identity limit"),
        );
        let origin = self.token_span(&token);
        self.hir_templates.push(HirTemplate { id, parts, origin });
        let mut template = value_type(Type::Template(capture_types));
        template.resolution = Some(ResolvedValue::Template(id));
        Some(template)
    }

    #[allow(clippy::too_many_lines)]
    fn object_literal(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        let token = self.bump().cloned()?;
        let expected = match expected {
            Some(Type::Optional(inner)) => Some(inner.as_ref()),
            expected => expected,
        };
        let Some(Type::Nominal(id, arguments)) = expected else {
            self.skip_object_fields();
            self.error(
                "TYPE_OBJECT_LITERAL_REQUIRES_CONTEXT",
                "object literal requires an expected struct type",
                &token,
                "add a struct annotation or a contextual parameter type",
            );
            return Some(value_type(Type::Error));
        };
        let fields = self.program.definition(*id).and_then(|definition| {
            let substitutions = definition
                .generics
                .iter()
                .filter(|parameter| parameter.namespace != tn_hir::Namespace::Value)
                .zip(arguments)
                .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
                .collect::<BTreeMap<_, _>>();
            match &definition.data {
                DefinitionData::Struct { fields, .. } => Some(
                    fields
                        .iter()
                        .cloned()
                        .map(|mut field| {
                            field.ty = substitute_type(&field.ty, &substitutions);
                            field
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            }
        });
        let Some(fields) = fields else {
            self.skip_object_fields();
            self.error(
                "TYPE_OBJECT_LITERAL_TARGET_NOT_STRUCT",
                "object literals construct only structs",
                &token,
                "use a class constructor or enum variant constructor",
            );
            return Some(value_type(Type::Error));
        };
        let mut provided = BTreeSet::new();
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightBrace) {
            let Some(name_token) = self.bump().cloned() else {
                break;
            };
            let name = self.module.source[name_token.range.clone()].to_owned();
            let expected_field = fields.iter().find(|field| field.name == name);
            if expected_field
                .is_some_and(|field| !self.can_initialize_member(*id, field.visibility))
            {
                self.error(
                    "TYPE_INACCESSIBLE_MEMBER",
                    format!("member `{name}` is not accessible here"),
                    &name_token,
                    "initialize private members from their declaring module",
                );
            }
            if !provided.insert(name.clone()) {
                self.error(
                    "TYPE_DUPLICATE_OBJECT_FIELD",
                    format!("duplicate object field `{name}`"),
                    &name_token,
                    "provide each struct field exactly once",
                );
            }
            if self.eat(TokenKind::Colon) {
                let value = self.expression(0, expected_field.map(|field| &field.ty));
                if let (Some(field), Some(value)) = (expected_field, value)
                    && !compatible(self.program, &value.ty, &field.ty)
                {
                    self.error(
                        "TYPE_MISMATCH",
                        format!(
                            "field `{name}` has type {:?}, expected {:?}",
                            value.ty, field.ty
                        ),
                        &name_token,
                        "initialize the field with its declared type",
                    );
                }
            } else if let Some(actual) = self.lookup(&name) {
                if let Some(field) = expected_field
                    && !compatible(self.program, &actual, &field.ty)
                {
                    self.error(
                        "TYPE_MISMATCH",
                        format!("field `{name}` does not match its declared type"),
                        &name_token,
                        "use a shorthand binding with the field's declared type",
                    );
                }
            } else {
                self.error(
                    "RESOLVE_UNRESOLVED_VALUE",
                    format!("unresolved shorthand field `{name}`"),
                    &name_token,
                    "declare a local with this name or write `name: expression`",
                );
            }
            if expected_field.is_none() {
                self.error(
                    "TYPE_UNKNOWN_OBJECT_FIELD",
                    format!("unknown struct field `{name}`"),
                    &name_token,
                    "remove this field or correct its name",
                );
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightBrace);
        for field in &fields {
            if !field.optional && !provided.contains(&field.name) {
                self.error(
                    "TYPE_MISSING_OBJECT_FIELD",
                    format!("missing required struct field `{}`", field.name),
                    &token,
                    "initialize every non-optional field",
                );
            }
        }
        Some(value_type(expected.cloned().unwrap_or(Type::Error)))
    }

    fn skip_object_fields(&mut self) {
        let mut depth = 1_u32;
        while let Some(kind) = self.kind() {
            self.bump();
            match kind {
                TokenKind::LeftBrace => depth += 1,
                TokenKind::RightBrace => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn new_expression(&mut self) -> Option<ExpressionType> {
        let token = self.bump().cloned()?;
        let name_token = self.bump().cloned()?;
        let name = &self.module.source[name_token.range.clone()];
        let resolved = self.resolve_type_name(name).unwrap_or_else(|| {
            self.error(
                "TYPE_UNRESOLVED_NAME",
                format!("unresolved constructed type `{name}`"),
                &name_token,
                "import or declare this class",
            );
            Type::Error
        });
        let ty = match resolved {
            Type::Nominal(id, _) => {
                let arguments = self.parse_local_generic_arguments();
                Type::Nominal(id, self.fill_elided_lifetime_arguments(id, arguments))
            }
            Type::DynamicInterface(id, _) => {
                let arguments = self.parse_local_generic_arguments();
                Type::DynamicInterface(id, self.fill_elided_lifetime_arguments(id, arguments))
            }
            resolved => resolved,
        };
        if let Type::Nominal(id, arguments) | Type::DynamicInterface(id, arguments) = &ty
            && !arguments.is_empty()
            && let Some(definition) = self.program.definition(*id)
        {
            let generics = definition
                .generics
                .iter()
                .map(|generic| tn_hir::GenericConstraint {
                    name: generic.name.clone(),
                    namespace: generic.namespace,
                    bounds: generic.bounds.clone(),
                })
                .collect::<Vec<_>>();
            let substitutions = generics
                .iter()
                .filter(|generic| generic.namespace != tn_hir::Namespace::Value)
                .zip(arguments)
                .map(|(generic, argument)| (generic.name.clone(), argument.clone()))
                .collect::<BTreeMap<_, _>>();
            self.validate_generic_bounds(&generics, &substitutions, Some(&name_token));
        }
        let class = nominal_id(&ty).and_then(|id| {
            self.program.definition(id).and_then(|definition| {
                if let DefinitionData::Class {
                    constructor,
                    is_abstract,
                    ..
                } = &definition.data
                {
                    Some((id, constructor.clone(), *is_abstract))
                } else {
                    None
                }
            })
        });
        let value_struct = nominal_id(&ty).is_some_and(|id| {
            self.program
                .definition(id)
                .is_some_and(|definition| matches!(definition.data, DefinitionData::Struct { .. }))
        });
        if class.is_none() && !value_struct {
            self.error(
                "TYPE_NEW_REQUIRES_CONSTRUCTIBLE_TYPE",
                "new constructs classes and canonical value types",
                &token,
                "use `new Type(...)` for a fresh owned value",
            );
        }
        if class
            .as_ref()
            .is_some_and(|(_, _, is_abstract)| *is_abstract)
        {
            self.error(
                "TYPE_CONSTRUCTS_ABSTRACT_CLASS",
                "an abstract class cannot be constructed",
                &token,
                "construct a concrete subclass",
            );
        }
        if self.eat(TokenKind::LeftParen) {
            if value_struct && self.kind() == Some(TokenKind::LeftBrace) {
                let _ = self.object_literal(Some(&ty));
                self.eat(TokenKind::RightParen);
                return Some(value_type(ty));
            }
            let arguments = if let Some((_, Some(constructor), _)) = &class {
                self.expression_list_expected(
                    TokenKind::RightParen,
                    &constructor
                        .function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.ty.clone())
                        .collect::<Vec<_>>(),
                )
            } else {
                self.expression_list(TokenKind::RightParen)
            };
            self.eat(TokenKind::RightParen);
            if let Some((owner, constructor, _)) = class {
                let parameters = constructor.as_ref().map_or(&[][..], |constructor| {
                    constructor.function.parameters.as_slice()
                });
                let required = parameters
                    .iter()
                    .rposition(|parameter| !matches!(parameter.ty, Type::Optional(_)))
                    .map_or(0, |index| index + 1);
                if arguments.len() < required || arguments.len() > parameters.len() {
                    self.error(
                        "TYPE_CONSTRUCTOR_ARGUMENT_MISMATCH",
                        "constructor arguments do not match the class constructor",
                        &token,
                        "provide every required constructor argument",
                    );
                }
                if let Some(constructor) = constructor {
                    if !self.can_access(owner, constructor.visibility) {
                        self.error(
                            "TYPE_INACCESSIBLE_CONSTRUCTOR",
                            "class constructor is not accessible here",
                            &token,
                            "construct the class from an allowed context",
                        );
                    }
                    if !constructor.function.effects.is_empty() {
                        if self.try_prefix_depth == 0 {
                            self.error(
                                "TYPE_MISSING_TRY",
                                "throwing constructor requires prefix try",
                                &token,
                                "write `try` before new",
                            );
                        } else {
                            self.record_effects(&constructor.function.effects, Some(&token));
                        }
                    }
                }
            }
        }
        Some(value_type(ty))
    }

    #[allow(clippy::too_many_lines)]
    fn match_expression(&mut self, expected: Option<&Type>) -> Option<ExpressionType> {
        let token = self.bump().cloned()?;
        self.eat(TokenKind::LeftParen);
        let scrutinee = self.expression(0, None)?;
        self.eat(TokenKind::RightParen);
        self.eat(TokenKind::LeftBrace);
        let (mut patterns, closed_domain) = pattern_space(self.program, &scrutinee.ty);
        let mut catch_all = false;
        let mut result = None;
        while matches!(self.kind(), Some(TokenKind::Case | TokenKind::Default)) {
            let default_arm = self.eat(TokenKind::Default);
            if !default_arm {
                self.eat(TokenKind::Case);
            }
            let pattern_token = self.token().cloned();
            let pattern_start = self.index;
            let (key, bindings, problems) = if default_arm {
                (None, BTreeMap::new(), Vec::new())
            } else {
                let mut depth = 0_u32;
                while let Some(kind) = self.kind() {
                    if depth == 0 && matches!(kind, TokenKind::Colon | TokenKind::If) {
                        break;
                    }
                    match kind {
                        TokenKind::LeftParen | TokenKind::LeftBrace => depth += 1,
                        TokenKind::RightParen | TokenKind::RightBrace => {
                            depth = depth.saturating_sub(1);
                        }
                        _ => {}
                    }
                    self.bump();
                }
                classify_pattern(
                    self.program,
                    &scrutinee.ty,
                    &self.tokens[pattern_start..self.index],
                    &self.module.source,
                )
            };
            for problem in problems {
                if let Some(problem_token) = self.tokens[pattern_start..self.index]
                    .get(problem.token_index)
                    .cloned()
                {
                    self.error(
                        problem.condition,
                        problem.message,
                        &problem_token,
                        problem.label,
                    );
                }
            }
            let constructor = pattern_constructor(self.program, &scrutinee.ty, key.as_deref());
            let projections = pattern_binding_projections(
                self.program,
                &scrutinee.ty,
                constructor,
                &self.tokens[pattern_start..self.index],
                &self.module.source,
            );
            self.scopes.push(bindings.clone());
            self.hir_scopes.push(BTreeMap::new());
            let binding_origin = pattern_token
                .as_ref()
                .map_or_else(|| self.token_span(&token), |token| self.token_span(token));
            let hir_bindings = bindings
                .into_iter()
                .map(|(name, ty)| {
                    let local = self.declare_hir_local(
                        name.clone(),
                        ty.clone(),
                        false,
                        binding_origin.clone(),
                    );
                    HirPatternBinding {
                        local,
                        ty,
                        projection: projections.get(&name).cloned().unwrap_or_default(),
                        default: None,
                    }
                })
                .collect::<Vec<_>>();
            let guarded = self.eat(TokenKind::If);
            if guarded {
                let guard = self.expression(0, Some(&Type::Primitive(PrimitiveType::Bool)));
                self.require_bool(guard);
            }
            if !guarded {
                let already_covered = catch_all
                    || key
                        .as_ref()
                        .is_some_and(|key| patterns.get(key).copied().unwrap_or(false));
                if already_covered && let Some(pattern_token) = pattern_token.as_ref() {
                    self.error(
                        "TYPE_UNREACHABLE_PATTERN",
                        "switch pattern is unreachable",
                        pattern_token,
                        "remove this arm or place it before the covering pattern",
                    );
                }
                if let Some(key) = key.as_ref() {
                    if let Some(covered) = patterns.get_mut(key) {
                        *covered = true;
                    } else if !closed_domain {
                        patterns.insert(key.clone(), true);
                    } else {
                        catch_all = true;
                    }
                } else {
                    catch_all = true;
                }
            }
            self.eat(TokenKind::Colon);
            let pattern_origin = self.tokens[pattern_start..self.index]
                .first()
                .zip(self.tokens[pattern_start..self.index].last())
                .map_or_else(
                    || binding_origin.clone(),
                    |(first, last)| {
                        SourceSpan::new(
                            self.module.path.to_string_lossy(),
                            first.range.start..last.range.end,
                            &self.module.source,
                        )
                    },
                );
            self.hir_patterns.push(HirPattern {
                scrutinee: scrutinee.ty.clone(),
                constructor,
                bindings: hir_bindings,
                guarded,
                origin: pattern_origin,
            });
            let arm = if self.kind() == Some(TokenKind::LeftBrace) {
                self.block();
                Some(value_type(Type::Primitive(PrimitiveType::Void)))
            } else {
                let arm = self.expression(0, expected);
                self.eat(TokenKind::Comma);
                arm
            };
            self.scopes.pop();
            self.hir_scopes.pop();
            let merged = merge_branches(result, arm);
            if merged.ty == Type::Error
                && let Some(pattern_token) = pattern_token.as_ref()
            {
                self.error(
                    "TYPE_SWITCH_ARM_MISMATCH",
                    "switch arms produce incompatible result types",
                    pattern_token,
                    "make every reachable arm produce the same type",
                );
            }
            result = Some(merged);
        }
        self.eat(TokenKind::RightBrace);
        let missing = patterns
            .iter()
            .find(|(_, covered)| !**covered)
            .map(|(pattern, _)| pattern.as_str())
            .or((!closed_domain).then_some("_"));
        if !catch_all && let Some(missing) = missing {
            self.error(
                "TYPE_NON_EXHAUSTIVE_SWITCH",
                format!("switch is missing constructible pattern `{missing}`"),
                &token,
                "add the missing arm or an explicit wildcard",
            );
        }
        result.or_else(|| Some(value_type(Type::Error)))
    }

    #[allow(clippy::too_many_lines)]
    fn call_expression(
        &mut self,
        callee: &ExpressionType,
        expected: Option<&Type>,
        explicit_generics: Option<Vec<Type>>,
    ) -> ExpressionType {
        let token = self.token().cloned();
        let callable = callee.callable.clone();
        let call_name = callee.call_name.clone();
        self.bump();
        let active_callee_type = callee
            .optional_chain_value
            .as_ref()
            .unwrap_or(&callee.ty)
            .clone();
        let Type::Function(function) = active_callee_type else {
            self.expression_list(TokenKind::RightParen);
            self.eat(TokenKind::RightParen);
            if let Some(token) = token {
                self.error(
                    "TYPE_NOT_CALLABLE",
                    "expression is not callable",
                    &token,
                    "call a function, method, closure, or foreign function value",
                );
            }
            return value_type(Type::Error);
        };
        let mut substitutions = BTreeMap::new();
        if let Some(arguments) = explicit_generics {
            let type_generics = function
                .generics
                .iter()
                .filter(|generic| generic.namespace == tn_hir::Namespace::Type)
                .collect::<Vec<_>>();
            if arguments.len() != type_generics.len()
                && let Some(token) = token.as_ref()
            {
                self.error(
                    "TYPE_GENERIC_ARGUMENT_ARITY",
                    "explicit generic arguments do not match the function signature",
                    token,
                    "provide one argument for each type generic parameter",
                );
            }
            for (generic, argument) in type_generics.into_iter().zip(arguments) {
                substitutions.insert(generic.name.clone(), argument);
            }
        }
        if let Some(expected) = expected {
            infer_substitutions(&function.result, expected, &mut substitutions);
        }
        let contextual_parameters = function
            .parameters
            .iter()
            .map(|parameter| substitute_type(parameter, &substitutions))
            .collect::<Vec<_>>();
        let arguments =
            self.expression_list_expected(TokenKind::RightParen, &contextual_parameters);
        self.eat(TokenKind::RightParen);
        for (parameter, argument) in function.parameters.iter().zip(&arguments) {
            infer_substitutions(parameter, &argument.ty, &mut substitutions);
        }
        let parameters = function
            .parameters
            .iter()
            .map(|parameter| substitute_type(parameter, &substitutions))
            .collect::<Vec<_>>();
        let inherited_effects = (call_name.as_deref() == Some("run"))
            .then(|| {
                arguments.first().and_then(|argument| match &argument.ty {
                    Type::Promise { effects, .. } => Some(effects.clone()),
                    _ => None,
                })
            })
            .flatten();
        if (arguments.len() != parameters.len()
            || arguments
                .iter()
                .zip(&parameters)
                .enumerate()
                .any(|(index, (actual, expected))| {
                    let inherited_promise = inherited_effects.is_some()
                        && index == 0
                        && matches!((&actual.ty, expected), (
                            Type::Promise { result: actual, .. },
                            Type::Promise { result: expected, .. }
                        ) if compatible(self.program, actual, expected));
                    !inherited_promise && !compatible(self.program, &actual.ty, expected)
                }))
            && let Some(token) = token.as_ref()
        {
            self.error(
                "TYPE_CALL_ARGUMENT_MISMATCH",
                "call arguments do not match the function signature",
                token,
                "provide one compatible argument for each parameter",
            );
        }
        self.validate_generic_bounds(&function.generics, &substitutions, token.as_ref());
        if !function.generics.is_empty()
            && let Some(callable) = callable
        {
            self.monomorphizations.insert(MonomorphizationInstance {
                callable,
                arguments: function
                    .generics
                    .iter()
                    .filter(|generic| generic.namespace == tn_hir::Namespace::Type)
                    .filter_map(|generic| substitutions.get(&generic.name).cloned())
                    .collect(),
            });
        }
        if function.is_unsafe
            && self.unsafe_depth == 0
            && let Some(token) = token.as_ref()
        {
            self.error(
                "TYPE_UNSAFE_CALL_REQUIRES_UNSAFE",
                "calling an unsafe or foreign function requires an unsafe block",
                token,
                "wrap the call in `unsafe { ... }`",
            );
        }
        let call_effects = inherited_effects
            .clone()
            .unwrap_or_else(|| function.effects.clone());
        if !function.is_async && !call_effects.is_empty() {
            if self.try_prefix_depth == 0 {
                if let Some(token) = token.as_ref() {
                    self.error(
                        "TYPE_MISSING_TRY",
                        "throwing synchronous call requires prefix try",
                        token,
                        "write `try` before this call",
                    );
                }
            } else {
                self.record_effects(&call_effects, token.as_ref());
            }
        }
        let result_type = substitute_type(&function.result, &substitutions);
        let mut result = if callee.optional_chain_value.is_some() {
            let mut result = value_type(optional_type(result_type.clone()));
            result.optional_chain_value = Some(result_type);
            result
        } else {
            value_type(result_type)
        };
        let mut captures = BTreeMap::new();
        for capture in arguments
            .iter()
            .flat_map(|argument| argument.captures.iter().cloned())
        {
            captures.insert(capture.name.clone(), capture);
        }
        result.captures = captures.values().cloned().collect();
        if matches!(
            call_name.as_deref(),
            Some("spawn" | "spawnBlocking" | "detach")
        ) {
            let detached = call_name.as_deref() == Some("detach");
            let checked =
                check_capture_requirements(&result.captures, detached, self.ownership_facts);
            self.diagnostics.extend(checked.diagnostics);
        }
        result.effects = if function.is_async {
            match &result.ty {
                Type::Promise { effects, .. } => effects.clone(),
                _ => function.effects,
            }
        } else {
            call_effects
        };
        result
    }

    fn call_generic_arguments(&mut self) -> Vec<Type> {
        let mut arguments = Vec::new();
        self.bump();
        while self.kind().is_some() && self.kind() != Some(TokenKind::Greater) {
            if let Some(argument) = self.parse_local_type() {
                arguments.push(argument);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::Greater);
        arguments
    }

    fn validate_generic_bounds(
        &mut self,
        generics: &[tn_hir::GenericConstraint],
        substitutions: &BTreeMap<String, Type>,
        token: Option<&Token>,
    ) {
        for generic in generics {
            let Some(argument) = substitutions.get(&generic.name) else {
                if let Some(token) = token {
                    self.error(
                        "TYPE_CANNOT_INFER_GENERIC_ARGUMENT",
                        format!("cannot infer generic argument `{}`", generic.name),
                        token,
                        "provide an argument that constrains this type parameter",
                    );
                }
                continue;
            };
            for bound in &generic.bounds {
                if !satisfies_bound(self.program, self.function, self.owner, argument, bound)
                    && let Some(token) = token
                {
                    self.error(
                        "TYPE_UNSATISFIED_GENERIC_BOUND",
                        format!("inferred type does not satisfy `{}`", generic.name),
                        token,
                        "use a type with the required explicit interface implementation",
                    );
                }
            }
        }
    }

    fn expression_list(&mut self, end: TokenKind) -> Vec<ExpressionType> {
        let mut values = Vec::new();
        while self.kind().is_some() && self.kind() != Some(end) {
            if let Some(value) = self.expression(0, None) {
                values.push(value);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        values
    }

    fn expression_list_expected(
        &mut self,
        end: TokenKind,
        expected: &[Type],
    ) -> Vec<ExpressionType> {
        let mut values = Vec::new();
        while self.kind().is_some() && self.kind() != Some(end) {
            let expected_type = expected.get(values.len());
            if let Some(value) = self.expression(0, expected_type) {
                values.push(value);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        values
    }

    #[allow(clippy::too_many_lines)]
    fn member_expression(&mut self, receiver: &ExpressionType) -> ExpressionType {
        let optional = self.eat(TokenKind::QuestionDot);
        if !optional {
            self.eat(TokenKind::Dot);
        }
        let Some(name_token) = self.bump().cloned() else {
            return value_type(Type::Error);
        };
        let name = self.module.source[name_token.range.clone()].to_owned();
        self.resolve_named_member(receiver, optional, &name_token, name)
    }

    fn computed_member_expression(&mut self, receiver: &ExpressionType) -> ExpressionType {
        self.eat(TokenKind::LeftBracket);
        let root = self.text().unwrap_or_default().to_owned();
        self.bump();
        self.eat(TokenKind::Dot);
        let Some(name_token) = self.bump().cloned() else {
            return value_type(Type::Error);
        };
        let member = self.module.source[name_token.range.clone()].to_owned();
        self.eat(TokenKind::RightBracket);
        self.resolve_named_member(receiver, false, &name_token, format!("[{root}.{member}]"))
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_named_member(
        &mut self,
        receiver: &ExpressionType,
        optional: bool,
        name_token: &Token,
        name: String,
    ) -> ExpressionType {
        let active_receiver = receiver
            .optional_chain_value
            .as_ref()
            .unwrap_or(&receiver.ty);
        let accessed = match active_receiver {
            Type::Optional(inner) if optional => inner.as_ref(),
            ty if optional => {
                self.error(
                    "TYPE_OPTIONAL_CHAIN_NON_OPTIONAL",
                    "optional chaining requires an optional receiver",
                    name_token,
                    "remove `?` or use a value whose type includes undefined",
                );
                ty
            }
            ty => ty,
        };
        let (base, borrowed_receiver) = match accessed {
            Type::Reference {
                mutable,
                lifetime,
                referent,
            } => (referent.as_ref(), Some((*mutable, lifetime.as_str()))),
            ty => (ty, None),
        };
        if !optional && matches!(base, Type::String | Type::Str) {
            let resolution = match name.as_str() {
                "length" => Some(ResolvedValue::StringLength),
                "byteLength" => Some(ResolvedValue::StringByteLength),
                _ => None,
            };
            if let Some(resolution) = resolution {
                let mut expression = value_type(Type::Primitive(PrimitiveType::Usize));
                expression.resolution = Some(resolution);
                return expression;
            }
        }
        let Some(id) = nominal_id(base).or_else(|| self.program.intrinsic_type_declaration(base))
        else {
            self.error(
                "TYPE_UNKNOWN_MEMBER",
                format!("type {:?} has no member `{name}`", receiver.ty),
                name_token,
                "use a member declared by this nominal type",
            );
            return value_type(Type::Error);
        };
        let Some(member) = resolve_member(self.program, id, &name) else {
            self.error(
                "TYPE_UNKNOWN_MEMBER",
                format!("unknown member `{name}`"),
                name_token,
                "correct the member name",
            );
            return value_type(Type::Error);
        };
        if let Some(receiver_mode) = resolved_method_receiver(self.program, member.id) {
            if receiver_mode == ReceiverMode::Static && !receiver.type_qualifier {
                self.error(
                    "TYPE_STATIC_METHOD_REQUIRES_TYPE",
                    format!("static method `{name}` requires a type qualifier"),
                    name_token,
                    "call this method through its declaring type",
                );
            } else if receiver_mode != ReceiverMode::Static && receiver.type_qualifier {
                self.error(
                    "TYPE_INSTANCE_METHOD_REQUIRES_VALUE",
                    format!("instance method `{name}` requires a value receiver"),
                    name_token,
                    "call this method through an instance",
                );
            }
        }
        if !self.can_access(member.owner, member.visibility) {
            self.error(
                "TYPE_INACCESSIBLE_MEMBER",
                format!("member `{name}` is not accessible here"),
                name_token,
                "use a public member or access it from the declaring type",
            );
        }
        let mut ty = specialize_nominal_member_type(self.program, base, &member.ty);
        if matches!(
            self.program
                .definition(id)
                .map(|definition| &definition.data),
            Some(DefinitionData::Enum { .. })
        ) {
            if let Type::Function(function) = &mut ty {
                *function.result = base.clone();
            } else {
                ty = base.clone();
            }
        }
        if let Some((mutable, lifetime)) = borrowed_receiver {
            if let Type::Function(function) = &mut ty {
                *function.result = relate_elided_lifetime(&function.result, lifetime);
            } else {
                ty = borrowed_field_type(self.ownership_facts, ty, mutable, lifetime);
            }
        }
        let callable = member.callable;
        let chain = optional || receiver.optional_chain_value.is_some();
        let mut expression = if chain {
            let mut expression = value_type(optional_type(ty.clone()));
            expression.optional_chain_value = Some(ty);
            expression
        } else {
            value_type(ty)
        };
        expression.callable = callable;
        expression.call_name = Some(name);
        expression.resolution = Some(ResolvedValue::Member(member.id));
        if !chain && !matches!(expression.ty, Type::Function(_)) {
            expression.place.clone_from(&receiver.place);
        }
        expression
    }

    fn binary(
        &mut self,
        operator: TokenKind,
        left: ExpressionType,
        right: Option<ExpressionType>,
        token: &Token,
    ) -> ExpressionType {
        let Some(right) = right else {
            return value_type(Type::Error);
        };
        let assignment = matches!(
            operator,
            TokenKind::Equal
                | TokenKind::PlusEqual
                | TokenKind::MinusEqual
                | TokenKind::StarEqual
                | TokenKind::SlashEqual
                | TokenKind::PercentEqual
                | TokenKind::AmpEqual
                | TokenKind::PipeEqual
                | TokenKind::CaretEqual
                | TokenKind::ShiftLeftEqual
                | TokenKind::ShiftRightEqual
        );
        if assignment
            && let Some(ResolvedValue::Member(member)) = left.resolution.as_ref()
            && readonly_field_owner(self.program, *member).is_some_and(|owner| owner != self.owner)
        {
            self.error(
                "TYPE_READONLY_FIELD_ASSIGNMENT",
                "readonly fields cannot be assigned outside their declaring type",
                token,
                "mutate the value through a declared method",
            );
        }
        if assignment && let Some(place) = left.place.as_deref() {
            self.upgrade_capture(place);
        }
        let operands_match = compatible(self.program, &right.ty, &left.ty)
            || string_comparison_compatible(&left.ty, &right.ty);
        let ty = match operator {
            TokenKind::EqualEqualEqual
            | TokenKind::BangEqualEqual
            | TokenKind::Less
            | TokenKind::LessEqual
            | TokenKind::Greater
            | TokenKind::GreaterEqual => {
                if !operands_match
                    || !supports_operator(
                        self.program,
                        self.function,
                        self.owner,
                        &left.ty,
                        operator,
                    )
                {
                    self.unsupported_operator(operator, &left.ty, token);
                }
                Type::Primitive(PrimitiveType::Bool)
            }
            TokenKind::AmpAmp | TokenKind::PipePipe => {
                self.require_bool(Some(left));
                self.require_bool(Some(right));
                Type::Primitive(PrimitiveType::Bool)
            }
            TokenKind::QuestionQuestion => match left.ty {
                Type::Optional(inner) if compatible(self.program, &right.ty, &inner) => *inner,
                _ => Type::Error,
            },
            TokenKind::Equal
            | TokenKind::PlusEqual
            | TokenKind::MinusEqual
            | TokenKind::StarEqual
            | TokenKind::SlashEqual
            | TokenKind::PercentEqual
            | TokenKind::AmpEqual
            | TokenKind::PipeEqual
            | TokenKind::CaretEqual
            | TokenKind::ShiftLeftEqual
            | TokenKind::ShiftRightEqual => {
                if compatible(self.program, &right.ty, &left.ty) {
                    left.ty
                } else {
                    Type::Error
                }
            }
            _ if operands_match
                && supports_operator(
                    self.program,
                    self.function,
                    self.owner,
                    &left.ty,
                    operator,
                ) =>
            {
                left.ty
            }
            _ => {
                self.unsupported_operator(operator, &left.ty, token);
                Type::Error
            }
        };
        value_type(ty)
    }

    fn unsupported_operator(&mut self, operator: TokenKind, ty: &Type, token: &Token) {
        self.error(
            "TYPE_OPERATOR_NOT_SUPPORTED",
            format!("operator {operator:?} is not supported by {ty:?}"),
            token,
            "add the explicit operator interface constraint or implementation",
        );
    }

    fn cast_expression(
        &mut self,
        operator: TokenKind,
        source: &ExpressionType,
        target: Type,
        token: Option<&Token>,
    ) -> ExpressionType {
        if operator == TokenKind::AsQuestion {
            let (source_type, mutable, lifetime) = match &source.ty {
                Type::Reference {
                    mutable,
                    lifetime,
                    referent,
                } => (referent.as_ref(), *mutable, lifetime.clone()),
                ty => (ty, false, "scope".into()),
            };
            let related = match (source_type, &target) {
                (Type::Nominal(source, _), Type::Nominal(target, _)) => {
                    class_is_or_extends(self.program, *source, *target)
                        || class_is_or_extends(self.program, *target, *source)
                }
                (Type::DynamicInterface(_, _), Type::Nominal(_, _)) => true,
                _ => false,
            };
            if !related && let Some(token) = token {
                self.error(
                    "TYPE_INVALID_CHECKED_DOWNCAST",
                    "checked downcast requires related class or interface types",
                    token,
                    "cast between a base, subclass, or implemented interface",
                );
            }
            return value_type(if related {
                Type::Optional(Box::new(Type::Reference {
                    mutable,
                    lifetime,
                    referent: Box::new(target),
                }))
            } else {
                Type::Error
            });
        }
        // Unsafe code is the explicit boundary for representation-level access.  Once the
        // target is a raw pointer, permit casts from any resolved value representation; the
        // backend still receives the concrete source type and the unsafe diagnostic remains
        // mandatory outside an unsafe region.
        let raw_pointer_cast =
            matches!(target, Type::RawPointer { .. }) && !matches!(source.ty, Type::Error);
        let runtime_numeric_cast = self
            .module
            .path
            .to_string_lossy()
            .ends_with("runtime/runtime.tn")
            && is_numeric(&source.ty)
            && is_numeric(&target);
        let valid = compatible(self.program, &source.ty, &target)
            || runtime_numeric_cast
            || raw_pointer_cast;
        let valid = valid
            || matches!(
                (&source.ty, &target),
                (
                    Type::Generic(name),
                    Type::DynamicInterface(interface, _)
                ) if generic_supports_interface(
                    self.program,
                    self.function,
                    self.owner,
                    name,
                    *interface,
                )
            );
        if raw_pointer_cast
            && self.unsafe_depth == 0
            && let Some(token) = token
        {
            self.error(
                "TYPE_RAW_POINTER_CAST_REQUIRES_UNSAFE",
                "raw pointer conversion requires an unsafe block",
                token,
                "perform this cast inside unsafe",
            );
        } else if !valid && let Some(token) = token {
            self.error(
                "TYPE_INVALID_CAST",
                "as supports only upcasts and raw pointer conversions",
                token,
                "use a named numeric conversion or a related class/interface type",
            );
        }
        value_type(if valid { target } else { Type::Error })
    }

    fn instance_of_expression(
        &mut self,
        source: &ExpressionType,
        target: &Type,
        token: Option<&Token>,
    ) -> ExpressionType {
        let valid = matches!(
            source.ty,
            Type::Nominal(_, _) | Type::DynamicInterface(_, _)
        ) && matches!(target, Type::Nominal(_, _) | Type::DynamicInterface(_, _));
        if !valid && let Some(token) = token {
            self.error(
                "TYPE_INVALID_INSTANCEOF",
                "instanceof is defined only for class and dynamic interface values",
                token,
                "test a class owner or dynamic interface against a class/interface type",
            );
        }
        value_type(Type::Primitive(PrimitiveType::Bool))
    }

    fn parse_local_type(&mut self) -> Option<Type> {
        let mut ty = self.parse_local_primary_type()?;
        if self.eat(TokenKind::Pipe) {
            if self.eat(TokenKind::Undefined) {
                ty = Type::Optional(Box::new(ty));
            } else {
                return Some(Type::Error);
            }
        }
        Some(ty)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_local_primary_type(&mut self) -> Option<Type> {
        Some(match self.kind()? {
            TokenKind::Amp => {
                self.bump();
                let lifetime = if self.kind() == Some(TokenKind::Static) {
                    self.bump();
                    "static".into()
                } else if self.kind() == Some(TokenKind::Scope) {
                    self.bump();
                    "scope".into()
                } else if self.text().is_some_and(|name| {
                    self.generic_namespace(name) == Some(tn_hir::Namespace::Lifetime)
                }) {
                    let lifetime = self.text()?.to_owned();
                    self.bump();
                    lifetime
                } else {
                    "scope".into()
                };
                let mutable = self.eat(TokenKind::Mut);
                Type::Reference {
                    mutable,
                    lifetime,
                    referent: Box::new(self.parse_local_primary_type()?),
                }
            }
            TokenKind::Star => {
                self.bump();
                let mutable = self.eat(TokenKind::Mut);
                if !mutable {
                    self.eat(TokenKind::Const);
                }
                Type::RawPointer {
                    mutable,
                    pointee: Box::new(self.parse_local_primary_type()?),
                }
            }
            TokenKind::LeftBracket => {
                self.bump();
                let element = self.parse_local_type()?;
                if self.eat(TokenKind::Semicolon) {
                    let length = self
                        .text()
                        .and_then(|text| text.replace('_', "").parse().ok())
                        .unwrap_or_default();
                    self.bump();
                    self.eat(TokenKind::RightBracket);
                    Type::Array(Box::new(element), length)
                } else {
                    self.eat(TokenKind::RightBracket);
                    Type::Slice(Box::new(element))
                }
            }
            TokenKind::LeftParen => self.parse_local_tuple_or_function()?,
            TokenKind::Identifier => {
                let name = self.text()?.to_owned();
                self.bump();
                if name == "Promise" {
                    self.eat(TokenKind::Less);
                    let result = self.parse_local_type()?;
                    self.eat(TokenKind::Comma);
                    let error = self.parse_local_type()?;
                    self.eat(TokenKind::Greater);
                    let effects = match error {
                        Type::Nominal(id, _) => vec![id],
                        _ => Vec::new(),
                    };
                    Type::Promise {
                        result: Box::new(result),
                        error: Box::new(error),
                        effects,
                    }
                } else if let Some(namespace) = self.generic_namespace(&name) {
                    match namespace {
                        tn_hir::Namespace::Lifetime => Type::Lifetime(name),
                        tn_hir::Namespace::Type => Type::Generic(name),
                        _ => Type::Error,
                    }
                } else {
                    let resolved = primitive(&name).or_else(|| self.resolve_type_name(&name))?;
                    match resolved {
                        Type::Nominal(id, _) => {
                            let arguments = self.parse_local_generic_arguments();
                            Type::Nominal(id, self.fill_elided_lifetime_arguments(id, arguments))
                        }
                        Type::DynamicInterface(id, _) => {
                            let arguments = self.parse_local_generic_arguments();
                            Type::DynamicInterface(
                                id,
                                self.fill_elided_lifetime_arguments(id, arguments),
                            )
                        }
                        resolved => resolved,
                    }
                }
            }
            TokenKind::Unknown => {
                self.bump();
                Type::Unknown
            }
            TokenKind::Dyn => {
                self.bump();
                return Some(Type::Error);
            }
            _ => return None,
        })
    }

    fn parse_local_tuple_or_function(&mut self) -> Option<Type> {
        self.bump();
        let mut elements = Vec::new();
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightParen) {
            elements.push(self.parse_local_type()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightParen);
        if !self.eat(TokenKind::FatArrow) {
            return Some(Type::Tuple(elements));
        }
        Some(Type::Function(tn_hir::FunctionType {
            parameters: elements,
            result: Box::new(self.parse_local_type()?),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        }))
    }

    fn parse_local_generic_arguments(&mut self) -> Vec<Type> {
        let mut arguments = Vec::new();
        if !self.eat(TokenKind::Less) {
            return arguments;
        }
        while self.kind().is_some() && self.kind() != Some(TokenKind::Greater) {
            if matches!(self.kind(), Some(TokenKind::Static | TokenKind::Scope)) {
                let lifetime = self.text().unwrap_or("scope").to_owned();
                self.bump();
                arguments.push(Type::Lifetime(lifetime));
            } else if let Some(argument) = self.parse_local_type() {
                arguments.push(argument);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::Greater);
        arguments
    }

    fn fill_elided_lifetime_arguments(
        &self,
        declaration: DeclarationId,
        arguments: Vec<Type>,
    ) -> Vec<Type> {
        if !arguments.is_empty() {
            return arguments;
        }
        let Some(definition) = self.program.definition(declaration) else {
            return arguments;
        };
        if definition
            .generics
            .iter()
            .all(|parameter| parameter.namespace == tn_hir::Namespace::Lifetime)
        {
            definition
                .generics
                .iter()
                .map(|_| Type::Lifetime("scope".into()))
                .collect()
        } else {
            arguments
        }
    }

    fn generic_namespace(&self, name: &str) -> Option<tn_hir::Namespace> {
        self.function
            .generics
            .iter()
            .find(|parameter| parameter.name == name)
            .map(|parameter| parameter.namespace)
            .or_else(|| {
                self.program
                    .definition(self.owner)
                    .and_then(|definition| {
                        definition
                            .generics
                            .iter()
                            .find(|parameter| parameter.name == name)
                    })
                    .map(|parameter| parameter.namespace)
            })
    }

    fn require_bool(&mut self, value: Option<ExpressionType>) {
        if value.is_some_and(|value| {
            !compatible(
                self.program,
                &value.ty,
                &Type::Primitive(PrimitiveType::Bool),
            )
        }) && let Some(token) = self.token().cloned()
        {
            self.error(
                "TYPE_CONDITION_NOT_BOOL",
                "condition must have type bool",
                &token,
                "TypeNative has no implicit truthiness",
            );
        }
    }

    fn lookup(&self, name: &str) -> Option<Type> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn lookup_scoped(&self, name: &str) -> Option<(usize, Type)> {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| scope.get(name).cloned().map(|ty| (index, ty)))
    }

    fn lookup_hir(&self, name: &str) -> Option<HirLocalId> {
        self.hir_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn declare_hir_local(
        &mut self,
        name: String,
        ty: Type,
        mutable: bool,
        origin: SourceSpan,
    ) -> HirLocalId {
        let id = self.push_hir_local(name.clone(), ty, mutable, origin);
        self.hir_scopes
            .last_mut()
            .expect("HIR local scope")
            .insert(name, id);
        id
    }

    fn push_hir_local(
        &mut self,
        name: String,
        ty: Type,
        mutable: bool,
        origin: SourceSpan,
    ) -> HirLocalId {
        let id = HirLocalId(u32::try_from(self.hir_locals.len()).expect("HIR local limit"));
        self.hir_locals.push(HirLocal {
            id,
            name,
            ty,
            mutable,
            origin,
        });
        id
    }

    fn record_capture(&mut self, name: &str, ty: &Type, scope_index: usize, token: &Token) {
        let span = self.token_span(token);
        for context in &mut self.capture_contexts {
            if scope_index >= context.scope_base {
                continue;
            }
            context
                .captures
                .entry(name.to_owned())
                .or_insert_with(|| Capture {
                    name: name.to_owned(),
                    ty: Type::Reference {
                        mutable: false,
                        lifetime: "scope".into(),
                        referent: Box::new(ty.clone()),
                    },
                    kind: CaptureKind::SharedBorrow,
                    span: span.clone(),
                });
        }
    }

    fn upgrade_capture(&mut self, name: &str) {
        for context in &mut self.capture_contexts {
            let Some(capture) = context.captures.get_mut(name) else {
                continue;
            };
            capture.kind = CaptureKind::MutableBorrow;
            if let Type::Reference { referent, .. } = &capture.ty {
                let referent = referent.clone();
                capture.ty = Type::Reference {
                    mutable: true,
                    lifetime: "scope".into(),
                    referent,
                };
            }
        }
    }

    fn token_span(&self, token: &Token) -> SourceSpan {
        SourceSpan::new(
            self.module.path.to_string_lossy(),
            token.range.clone(),
            &self.module.source,
        )
    }

    fn finish_hir(&mut self) {
        let roots = maximal_statement_ids(&self.hir_statements, None);
        self.hir_bodies.push(BodyHir {
            owner: self.hir_owner,
            locals: std::mem::take(&mut self.hir_locals),
            parameter_roots: std::mem::take(&mut self.parameter_roots),
            expressions: std::mem::take(&mut self.hir_expressions),
            patterns: std::mem::take(&mut self.hir_patterns),
            binding_patterns: std::mem::take(&mut self.hir_binding_patterns),
            jsx_elements: std::mem::take(&mut self.hir_jsx_elements),
            statements: std::mem::take(&mut self.hir_statements),
            closures: std::mem::take(&mut self.hir_closures),
            templates: std::mem::take(&mut self.hir_templates),
            roots,
        });
    }

    fn record_hir_statement(&mut self, start: usize, kind: TokenKind, locals_before: usize) {
        let Some(first) = self.tokens.get(start) else {
            return;
        };
        let end = self.index.max(start + 1).min(self.tokens.len());
        let origin = SourceSpan::new(
            self.module.path.to_string_lossy(),
            first.range.start..self.tokens[end - 1].range.end,
            &self.module.source,
        );
        let children = maximal_statement_ids(&self.hir_statements, Some(&origin));
        let expressions = maximal_expression_ids(
            &self.hir_expressions,
            Some(&origin),
            &children,
            &self.hir_statements,
        );
        let new_local = self.hir_locals.get(locals_before).map(|local| local.id);
        let statement_kind = match kind {
            TokenKind::LeftBrace => HirStatementKind::Block,
            TokenKind::Const | TokenKind::Let => {
                HirStatementKind::Local(new_local.expect("local declaration creates HIR local"))
            }
            TokenKind::Return => HirStatementKind::Return,
            TokenKind::Yield => HirStatementKind::Yield,
            TokenKind::Throw => HirStatementKind::Throw,
            TokenKind::If => HirStatementKind::If,
            TokenKind::While => HirStatementKind::While,
            TokenKind::For => {
                let binding = new_local.expect("for binding creates HIR local");
                HirStatementKind::For {
                    binding,
                    awaited: self
                        .tokens
                        .get(start + 1)
                        .is_some_and(|token| token.kind == TokenKind::Await),
                    witness: self.iteration_witnesses.remove(&binding).map(Box::new),
                }
            }
            TokenKind::Using => HirStatementKind::Using {
                local: new_local.expect("using declaration creates HIR local"),
                awaited: false,
            },
            TokenKind::Await if self.nth(1) == Some(TokenKind::Using) => HirStatementKind::Using {
                local: new_local.expect("await using declaration creates HIR local"),
                awaited: true,
            },
            TokenKind::Try => HirStatementKind::Try,
            TokenKind::Unsafe => HirStatementKind::Unsafe,
            TokenKind::Break => HirStatementKind::Break,
            TokenKind::Continue => HirStatementKind::Continue,
            _ => HirStatementKind::Expression,
        };
        let id =
            HirStatementId(u32::try_from(self.hir_statements.len()).expect("HIR statement limit"));
        self.hir_statements.push(HirStatement {
            id,
            kind: statement_kind,
            expressions,
            children,
            origin,
        });
    }

    fn record_hir_expression(&mut self, start: usize, expression: &ExpressionType) {
        let end = self.index.max(start + 1).min(self.tokens.len());
        self.record_hir_expression_range(start, end, expression);
    }

    fn record_hir_expression_range(
        &mut self,
        start: usize,
        end: usize,
        expression: &ExpressionType,
    ) -> Option<HirExpressionId> {
        let Some(first) = self.tokens.get(start) else {
            return None;
        };
        if start >= end || end > self.tokens.len() {
            return None;
        }
        let last = &self.tokens[end - 1];
        let tokens = &self.tokens[start..end];
        if tokens
            .first()
            .is_some_and(|token| token.kind == TokenKind::LeftParen)
            && tokens
                .last()
                .is_some_and(|token| token.kind == TokenKind::RightParen)
            && !tokens
                .iter()
                .any(|token| matches!(token.kind, TokenKind::Comma | TokenKind::FatArrow))
        {
            return self.hir_expressions.last().map(|expression| expression.id);
        }
        let origin = SourceSpan::new(
            self.module.path.to_string_lossy(),
            first.range.start..last.range.end,
            &self.module.source,
        );
        if self.hir_expressions.last().is_some_and(|recorded| {
            recorded.origin == origin
                && recorded.ty == expression.ty
                && recorded.resolution == expression.resolution
        }) {
            return self.hir_expressions.last().map(|expression| expression.id);
        }
        let children = maximal_expression_ids(&self.hir_expressions, Some(&origin), &[], &[]);
        let id = HirExpressionId(
            u32::try_from(self.hir_expressions.len()).expect("HIR expression limit"),
        );
        self.hir_expressions.push(HirExpression {
            id,
            kind: hir_expression_kind(tokens, expression),
            ty: expression.ty.clone(),
            optional_chain_value: expression.optional_chain_value.clone(),
            effects: expression.effects.clone(),
            resolution: expression.resolution,
            children,
            origin,
        });
        Some(id)
    }

    fn resolve_type_name(&self, name: &str) -> Option<Type> {
        if let Some(ty) = primitive(name) {
            return Some(ty);
        }
        let local = self.module.declarations.iter().find(|declaration| {
            declaration.name.as_deref() == Some(name)
                && declaration.kind.namespace() == Some(tn_hir::Namespace::Type)
        });
        let imported = self.module.imports.iter().find_map(|import| {
            let ImportClause::Named(names) = &import.clause else {
                return None;
            };
            let imported_name = names.iter().find(|imported| imported.local == name)?;
            self.program
                .graph
                .module(import.target)?
                .declarations
                .iter()
                .find(|declaration| {
                    declaration.exported
                        && declaration.name.as_deref() == Some(&imported_name.imported)
                        && declaration.kind.namespace() == Some(tn_hir::Namespace::Type)
                })
        });
        let declaration = local.or(imported)?;
        match &self.program.definition(declaration.id)?.data {
            DefinitionData::TypeAlias(ty) => Some(ty.clone()),
            DefinitionData::Interface { .. } => {
                Some(Type::DynamicInterface(declaration.id, Vec::new()))
            }
            _ => Some(Type::Nominal(declaration.id, Vec::new())),
        }
    }

    fn resolve_global_value(&self, name: &str) -> Option<(DeclarationId, Type, bool)> {
        let local = self.module.declarations.iter().find(|declaration| {
            declaration.name.as_deref() == Some(name)
                && matches!(
                    declaration.kind,
                    tn_hir::DeclarationKind::Const | tn_hir::DeclarationKind::Static
                )
        });
        let imported = self.module.imports.iter().find_map(|import| {
            let ImportClause::Named(names) = &import.clause else {
                return None;
            };
            let imported_name = names.iter().find(|imported| imported.local == name)?;
            self.program
                .graph
                .module(import.target)?
                .declarations
                .iter()
                .find(|declaration| {
                    declaration.exported
                        && declaration.name.as_deref() == Some(&imported_name.imported)
                        && matches!(
                            declaration.kind,
                            tn_hir::DeclarationKind::Const | tn_hir::DeclarationKind::Static
                        )
                })
        });
        let declaration = local.or(imported)?;
        let DefinitionData::Constant { ty, mutable_static } =
            &self.program.definition(declaration.id)?.data
        else {
            return None;
        };
        Some((declaration.id, ty.clone(), *mutable_static))
    }

    fn can_access(&self, declaring: DeclarationId, visibility: Visibility) -> bool {
        if visibility == Visibility::Public {
            return true;
        }
        let current = self
            .program
            .definition(self.owner)
            .and_then(|definition| match &definition.data {
                DefinitionData::Struct { .. }
                | DefinitionData::Enum { .. }
                | DefinitionData::Interface { .. }
                | DefinitionData::Class { .. } => Some(definition.declaration),
                DefinitionData::Implementation { target, .. } => nominal_id(target),
                _ => None,
            });
        match visibility {
            Visibility::Private => current == Some(declaring),
            Visibility::Protected => {
                current.is_some_and(|current| class_is_or_extends(self.program, current, declaring))
            }
            Visibility::Public => true,
        }
    }

    fn can_initialize_member(&self, declaring: DeclarationId, visibility: Visibility) -> bool {
        visibility == Visibility::Public
            || self
                .program
                .graph
                .declaration(declaring)
                .is_some_and(|declaration| declaration.module == self.module.id)
            || self.can_access(declaring, visibility)
    }

    fn error(&mut self, id: &str, message: impl Into<String>, token: &Token, label: &str) {
        self.diagnostics.push(Diagnostic::error(
            ConditionId::new(id).expect("static condition is valid"),
            message,
            Label {
                span: SourceSpan::new(
                    self.module.path.to_string_lossy(),
                    token.range.clone(),
                    &self.module.source,
                ),
                message: label.into(),
            },
            id.to_ascii_lowercase().replace('_', "/"),
        ));
    }

    fn error_span(&mut self, id: &str, message: impl Into<String>, span: &SourceSpan, label: &str) {
        self.diagnostics.push(Diagnostic::error(
            ConditionId::new(id).expect("static condition is valid"),
            message,
            Label {
                span: span.clone(),
                message: label.into(),
            },
            id.to_ascii_lowercase().replace('_', "/"),
        ));
    }

    fn record_effects(&mut self, effects: &[DeclarationId], token: Option<&Token>) {
        if let Some(reaching) = self.try_block_effects.last_mut() {
            reaching.extend(effects.iter().copied());
            return;
        }
        if effects
            .iter()
            .any(|effect| !self.function.effects.contains(effect))
            && let Some(token) = token
        {
            self.error(
                "TYPE_UNDECLARED_ERROR_EFFECT",
                "call can propagate an error absent from the enclosing declaration",
                token,
                "declare the error or catch it exhaustively",
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn callable_index(program: &Program) -> BTreeMap<(ModuleId, String), (DeclarationId, Function)> {
    let mut functions = BTreeMap::new();
    for definition in &program.definitions {
        let Some(declaration) = program.graph.declaration(definition.declaration) else {
            continue;
        };
        match &definition.data {
            DefinitionData::Function(function) => {
                if let Some(name) = &declaration.name {
                    functions.insert(
                        (declaration.module, name.clone()),
                        (
                            definition.declaration,
                            normalize_function_aliases(program, function),
                        ),
                    );
                }
            }
            DefinitionData::Extern { functions: methods } => {
                for method in methods {
                    functions.insert(
                        (declaration.module, method.name.clone()),
                        (
                            definition.declaration,
                            normalize_function_aliases(program, &method.function),
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    for module in &program.graph.modules {
        for import in &module.imports {
            let ImportClause::Named(names) = &import.clause else {
                continue;
            };
            for imported in names {
                let Some(declaration) = program.graph.module(import.target).and_then(|target| {
                    target.declarations.iter().find(|declaration| {
                        declaration.exported
                            && declaration.name.as_deref() == Some(&imported.imported)
                    })
                }) else {
                    continue;
                };
                if let Some(definition) = program.definition(declaration.id) {
                    match &definition.data {
                        DefinitionData::Function(function) => {
                            functions.insert(
                                (module.id, imported.local.clone()),
                                (
                                    declaration.id,
                                    normalize_function_aliases(program, function),
                                ),
                            );
                        }
                        DefinitionData::Extern { functions: methods } => {
                            if let Some(method) = methods
                                .iter()
                                .find(|method| method.name == imported.imported)
                            {
                                functions.insert(
                                    (module.id, imported.local.clone()),
                                    (
                                        declaration.id,
                                        normalize_function_aliases(program, &method.function),
                                    ),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let prelude = program
        .graph
        .modules
        .iter()
        .find(|module| program.graph.is_bundled_module(module.id, "string.tn"))
        .and_then(|module| {
            module.declarations.iter().find(|declaration| {
                declaration.exported && declaration.name.as_deref() == Some("String")
            })
        })
        .and_then(|declaration| {
            program.definition(declaration.id).and_then(|definition| {
                if let DefinitionData::Function(function) = &definition.data {
                    Some((
                        declaration.id,
                        normalize_function_aliases(program, function),
                    ))
                } else {
                    None
                }
            })
        });
    if let Some(prelude) = prelude {
        for module in &program.graph.modules {
            functions.insert((module.id, "String".into()), prelude.clone());
        }
    }
    functions
}

fn normalize_function_aliases(program: &Program, function: &Function) -> Function {
    let mut normalized = function.clone();
    for parameter in &mut normalized.parameters {
        parameter.ty = normalize_alias_deep(program, &parameter.ty);
    }
    normalized.result = normalize_alias_deep(program, &normalized.result);
    normalized
}

fn normalize_alias_deep(program: &Program, ty: &Type) -> Type {
    let ty = normalize_alias(program, ty);
    match ty {
        Type::Nominal(declaration, arguments) => Type::Nominal(
            declaration,
            arguments
                .iter()
                .map(|argument| normalize_alias_deep(program, argument))
                .collect(),
        ),
        Type::DynamicInterface(declaration, arguments) => Type::DynamicInterface(
            declaration,
            arguments
                .iter()
                .map(|argument| normalize_alias_deep(program, argument))
                .collect(),
        ),
        Type::Promise {
            result,
            error,
            effects,
        } => {
            let error = normalize_alias_deep(program, &error);
            Type::Promise {
                result: Box::new(normalize_alias_deep(program, &result)),
                error: Box::new(error.clone()),
                effects: tn_hir::promise_effects(&error, &effects),
            }
        }
        Type::Optional(inner) => Type::Optional(Box::new(normalize_alias_deep(program, &inner))),
        Type::Array(inner, length) => {
            Type::Array(Box::new(normalize_alias_deep(program, &inner)), length)
        }
        Type::Slice(inner) => Type::Slice(Box::new(normalize_alias_deep(program, &inner))),
        Type::Tuple(elements) => Type::Tuple(
            elements
                .iter()
                .map(|element| normalize_alias_deep(program, element))
                .collect(),
        ),
        Type::Reference {
            mutable,
            lifetime,
            referent,
        } => Type::Reference {
            mutable,
            lifetime,
            referent: Box::new(normalize_alias_deep(program, &referent)),
        },
        Type::RawPointer { mutable, pointee } => Type::RawPointer {
            mutable,
            pointee: Box::new(normalize_alias_deep(program, &pointee)),
        },
        Type::Function(function) => Type::Function(tn_hir::FunctionType {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| normalize_alias_deep(program, parameter))
                .collect(),
            result: Box::new(normalize_alias_deep(program, &function.result)),
            effects: function.effects,
            generics: function.generics,
            is_async: function.is_async,
            is_unsafe: function.is_unsafe,
        }),
        Type::Template(elements) => Type::Template(
            elements
                .iter()
                .map(|element| normalize_alias_deep(program, element))
                .collect(),
        ),
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => ty,
    }
}

#[allow(clippy::too_many_lines)]
fn guaranteed_sequence(tokens: &[Token]) -> bool {
    let mut index = 0_usize;
    while index < tokens.len() {
        match tokens[index].kind {
            TokenKind::Return | TokenKind::Throw => return true,
            TokenKind::If => {
                index += 1;
                index = skip_balanced_tokens(
                    tokens,
                    index,
                    TokenKind::LeftParen,
                    TokenKind::RightParen,
                );
                let (then_start, then_end) = statement_range(tokens, index);
                let then_returns = guaranteed_statement(&tokens[then_start..then_end]);
                index = then_end;
                if tokens
                    .get(index)
                    .is_some_and(|token| token.kind == TokenKind::Else)
                {
                    let (else_start, else_end) = statement_range(tokens, index + 1);
                    if then_returns && guaranteed_statement(&tokens[else_start..else_end]) {
                        return true;
                    }
                    index = else_end;
                }
            }
            TokenKind::While | TokenKind::For => {
                index += 1;
                index = skip_balanced_tokens(
                    tokens,
                    index,
                    TokenKind::LeftParen,
                    TokenKind::RightParen,
                );
                let (_, end) = statement_range(tokens, index);
                index = end;
            }
            TokenKind::Try => {
                index += 1;
                let (try_start, try_end) = statement_range(tokens, index);
                let try_returns = guaranteed_statement(&tokens[try_start..try_end]);
                index = try_end;
                let mut catches_return = true;
                let mut has_catch = false;
                while tokens
                    .get(index)
                    .is_some_and(|token| token.kind == TokenKind::Catch)
                {
                    has_catch = true;
                    index += 1;
                    index = skip_balanced_tokens(
                        tokens,
                        index,
                        TokenKind::LeftParen,
                        TokenKind::RightParen,
                    );
                    let (catch_start, catch_end) = statement_range(tokens, index);
                    catches_return &= guaranteed_statement(&tokens[catch_start..catch_end]);
                    index = catch_end;
                }
                if try_returns && (!has_catch || catches_return) {
                    return true;
                }
            }
            TokenKind::Switch => {
                let (end, returns) = guaranteed_switch(tokens, index);
                if returns {
                    return true;
                }
                index = end;
            }
            TokenKind::LeftBrace => {
                let end =
                    matching_token(tokens, index, TokenKind::LeftBrace, TokenKind::RightBrace)
                        .unwrap_or(tokens.len().saturating_sub(1));
                if guaranteed_sequence(&tokens[index + 1..end]) {
                    return true;
                }
                index = end + 1;
            }
            TokenKind::Unsafe => {
                let (start, end) = statement_range(tokens, index + 1);
                if guaranteed_statement(&tokens[start..end]) {
                    return true;
                }
                index = end;
            }
            _ => {
                let (_, end) = statement_range(tokens, index);
                index = end.max(index + 1);
            }
        }
    }
    let mut depth = 0_u32;
    for token in tokens {
        match token.kind {
            TokenKind::LeftBrace => depth = depth.saturating_add(1),
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            TokenKind::Return | TokenKind::Throw if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn guaranteed_statement(tokens: &[Token]) -> bool {
    if tokens
        .first()
        .is_some_and(|token| token.kind == TokenKind::LeftBrace)
        && tokens
            .last()
            .is_some_and(|token| token.kind == TokenKind::RightBrace)
    {
        guaranteed_sequence(&tokens[1..tokens.len().saturating_sub(1)])
    } else {
        guaranteed_sequence(tokens)
    }
}

fn guaranteed_switch(tokens: &[Token], start: usize) -> (usize, bool) {
    let Some(open) = tokens
        .get(start + 1)
        .is_some_and(|token| token.kind == TokenKind::LeftParen)
        .then_some(start + 1)
    else {
        return (start + 1, false);
    };
    let Some(close) = matching_token(tokens, open, TokenKind::LeftParen, TokenKind::RightParen)
    else {
        return (tokens.len(), false);
    };
    let Some(body_start) = tokens
        .get(close + 1)
        .is_some_and(|token| token.kind == TokenKind::LeftBrace)
        .then_some(close + 1)
    else {
        return (close + 1, false);
    };
    let Some(body_end) = matching_token(
        tokens,
        body_start,
        TokenKind::LeftBrace,
        TokenKind::RightBrace,
    ) else {
        return (tokens.len(), false);
    };

    let mut markers = Vec::new();
    let mut depth = 0_u32;
    for (index, token) in tokens
        .iter()
        .enumerate()
        .take(body_end)
        .skip(body_start + 1)
    {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
            TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::Case | TokenKind::Default if depth == 0 => markers.push(index),
            _ => {}
        }
    }
    let mut has_default = false;
    let all_return = !markers.is_empty()
        && markers.iter().enumerate().all(|(arm, marker)| {
            if tokens[*marker].kind == TokenKind::Default {
                has_default = true;
            }
            let arm_end = markers.get(arm + 1).copied().unwrap_or(body_end);
            let mut pattern_depth = 0_u32;
            let colon = (*marker + 1..arm_end).find(|index| match tokens[*index].kind {
                TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => {
                    pattern_depth += 1;
                    false
                }
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    pattern_depth = pattern_depth.saturating_sub(1);
                    false
                }
                TokenKind::Colon => pattern_depth == 0,
                _ => false,
            });
            colon.is_some_and(|colon| guaranteed_statement(&tokens[colon + 1..arm_end]))
        });
    (body_end + 1, has_default && all_return)
}

fn statement_range(tokens: &[Token], start: usize) -> (usize, usize) {
    if tokens
        .get(start)
        .is_some_and(|token| token.kind == TokenKind::LeftBrace)
    {
        let end = matching_token(tokens, start, TokenKind::LeftBrace, TokenKind::RightBrace)
            .map_or(tokens.len(), |end| end + 1);
        return (start, end);
    }
    let mut depth = 0_u32;
    for (offset, token) in tokens.iter().skip(start).enumerate() {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
            TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::Semicolon if depth == 0 => return (start, start + offset + 1),
            _ => {}
        }
    }
    (start, tokens.len())
}

fn skip_balanced_tokens(
    tokens: &[Token],
    start: usize,
    open: TokenKind,
    close: TokenKind,
) -> usize {
    matching_token(tokens, start, open, close).map_or(start, |end| end + 1)
}

fn matching_token(
    tokens: &[Token],
    start: usize,
    open: TokenKind,
    close: TokenKind,
) -> Option<usize> {
    if tokens.get(start)?.kind != open {
        return None;
    }
    let mut depth = 0_u32;
    for (offset, token) in tokens.iter().skip(start).enumerate() {
        if token.kind == open {
            depth += 1;
        } else if token.kind == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

fn value_type(ty: Type) -> ExpressionType {
    let effects = match &ty {
        Type::Promise { effects, .. } => effects.clone(),
        _ => Vec::new(),
    };
    ExpressionType {
        ty,
        optional_chain_value: None,
        place: None,
        effects,
        callable: None,
        call_name: None,
        captures: Vec::new(),
        resolution: None,
        type_qualifier: false,
        jsx: None,
    }
}

fn is_jsx_key_type(ty: &Type) -> bool {
    match ty {
        Type::String | Type::Str => true,
        Type::Reference { referent, .. } => matches!(referent.as_ref(), Type::String | Type::Str),
        Type::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::I128
            | PrimitiveType::Isize
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::U128
            | PrimitiveType::Usize,
        ) => true,
        _ => false,
    }
}

fn is_jsx_ref_type(program: &Program, actual: &Type, element: &Type) -> bool {
    if matches!(element, Type::Unknown | Type::Error)
        || matches!(actual, Type::Unknown | Type::Error)
    {
        return true;
    }
    let actual = normalize_alias(program, actual);
    match actual {
        Type::Optional(inner) => is_jsx_ref_type(program, inner.as_ref(), element),
        Type::Reference {
            mutable, referent, ..
        } => mutable && compatible(program, referent.as_ref(), element),
        Type::Nominal(declaration, arguments)
            if declaration_name(program, declaration) == Some("Ref") =>
        {
            arguments
                .first()
                .is_some_and(|target| compatible(program, target, element))
        }
        _ => false,
    }
}

fn indexed_element_type(ty: &Type) -> Type {
    match ty {
        Type::Array(element, _) | Type::Slice(element) => element.as_ref().clone(),
        Type::Reference { referent, .. } => indexed_element_type(referent),
        _ => Type::Error,
    }
}

fn optional_type(ty: Type) -> Type {
    if matches!(ty, Type::Optional(_)) {
        ty
    } else {
        Type::Optional(Box::new(ty))
    }
}

fn decode_template_chunk(chunk: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut characters = chunk.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => decoded.push('\\'),
            '`' => decoded.push('`'),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '0' => decoded.push('\0'),
            '$' if characters.next_if_eq(&'{').is_some() => decoded.push_str("${"),
            'x' => {
                let digits = [characters.next()?, characters.next()?]
                    .into_iter()
                    .collect::<String>();
                decoded.push(char::from(u8::from_str_radix(&digits, 16).ok()?));
            }
            'u' if characters.next()? == '{' => {
                let mut digits = String::new();
                loop {
                    let digit = characters.next()?;
                    if digit == '}' {
                        break;
                    }
                    digits.push(digit);
                }
                decoded.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(decoded)
}

fn hir_expression_kind(tokens: &[Token], expression: &ExpressionType) -> HirExpressionKind {
    if expression.ty == Type::Error {
        return HirExpressionKind::Error;
    }
    if let Some(jsx) = expression.jsx {
        return HirExpressionKind::Jsx(jsx);
    }
    let first = tokens.first().map(|token| token.kind);
    if first == Some(TokenKind::StringLiteral) && tokens.len() == 1 && expression.ty == Type::String
    {
        return HirExpressionKind::Conversion(tn_hir::HirConversionKind::StringLiteralToOwned);
    }
    if tokens.iter().any(|token| token.kind == TokenKind::FatArrow) {
        return HirExpressionKind::Closure;
    }
    match first {
        Some(
            TokenKind::IntegerLiteral
            | TokenKind::FloatLiteral
            | TokenKind::CharacterLiteral
            | TokenKind::StringLiteral
            | TokenKind::TemplateLiteral
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Undefined,
        ) if tokens.len() == 1 => HirExpressionKind::Literal,
        Some(TokenKind::Amp) => HirExpressionKind::Borrow {
            mutable: tokens
                .get(1)
                .is_some_and(|token| token.kind == TokenKind::Mut),
        },
        Some(TokenKind::Move) => HirExpressionKind::Move,
        Some(TokenKind::Await | TokenKind::Try)
            if tokens.iter().any(|token| token.kind == TokenKind::Await) =>
        {
            HirExpressionKind::Await
        }
        Some(TokenKind::Switch) => HirExpressionKind::Switch,
        Some(TokenKind::New | TokenKind::LeftBrace | TokenKind::LeftBracket) => {
            HirExpressionKind::Aggregate
        }
        Some(TokenKind::LeftParen) if tokens.iter().any(|token| token.kind == TokenKind::Comma) => {
            HirExpressionKind::Aggregate
        }
        _ if tokens.iter().any(|token| token.kind == TokenKind::Question) => {
            HirExpressionKind::Conditional
        }
        _ if tokens
            .iter()
            .any(|token| matches!(token.kind, TokenKind::As | TokenKind::AsQuestion)) =>
        {
            HirExpressionKind::Cast
        }
        _ if tokens
            .iter()
            .any(|token| binding_power(token.kind).is_some()) =>
        {
            HirExpressionKind::Binary
        }
        _ if tokens
            .iter()
            .any(|token| token.kind == TokenKind::LeftParen) =>
        {
            HirExpressionKind::Call
        }
        _ if tokens
            .iter()
            .any(|token| matches!(token.kind, TokenKind::Dot | TokenKind::QuestionDot)) =>
        {
            HirExpressionKind::Member
        }
        _ if tokens
            .iter()
            .any(|token| token.kind == TokenKind::LeftBracket) =>
        {
            HirExpressionKind::Index
        }
        Some(TokenKind::Bang | TokenKind::Minus | TokenKind::Tilde | TokenKind::Star) => {
            HirExpressionKind::Unary
        }
        _ if expression.resolution.is_some() => HirExpressionKind::Value,
        _ => HirExpressionKind::Error,
    }
}

fn expected_owned_string(expected: &Type) -> bool {
    match expected {
        Type::String => true,
        Type::Optional(inner) => matches!(inner.as_ref(), Type::String),
        _ => false,
    }
}

fn span_contains(outer: &SourceSpan, inner: &SourceSpan) -> bool {
    outer.file == inner.file
        && outer.byte_start <= inner.byte_start
        && outer.byte_end >= inner.byte_end
}

fn strictly_contains(outer: &SourceSpan, inner: &SourceSpan) -> bool {
    span_contains(outer, inner)
        && (outer.byte_start != inner.byte_start || outer.byte_end != inner.byte_end)
}

fn maximal_statement_ids(
    statements: &[HirStatement],
    parent: Option<&SourceSpan>,
) -> Vec<HirStatementId> {
    statements
        .iter()
        .filter(|statement| {
            parent.is_none_or(|parent| strictly_contains(parent, &statement.origin))
        })
        .filter(|statement| {
            !statements.iter().any(|other| {
                other.id != statement.id
                    && parent.is_none_or(|parent| strictly_contains(parent, &other.origin))
                    && strictly_contains(&other.origin, &statement.origin)
            })
        })
        .map(|statement| statement.id)
        .collect()
}

fn maximal_expression_ids(
    expressions: &[HirExpression],
    parent: Option<&SourceSpan>,
    child_statements: &[HirStatementId],
    statements: &[HirStatement],
) -> Vec<HirExpressionId> {
    expressions
        .iter()
        .filter(|expression| {
            parent.is_none_or(|parent| strictly_contains(parent, &expression.origin))
        })
        .filter(|expression| {
            !child_statements.iter().any(|child| {
                statements
                    .get(child.0 as usize)
                    .is_some_and(|statement| span_contains(&statement.origin, &expression.origin))
            })
        })
        .filter(|expression| {
            !expressions.iter().any(|other| {
                other.id != expression.id
                    && parent.is_none_or(|parent| strictly_contains(parent, &other.origin))
                    && !child_statements.iter().any(|child| {
                        statements.get(child.0 as usize).is_some_and(|statement| {
                            span_contains(&statement.origin, &other.origin)
                        })
                    })
                    && strictly_contains(&other.origin, &expression.origin)
            })
        })
        .map(|expression| expression.id)
        .collect()
}

fn compatible(program: &Program, actual: &Type, expected: &Type) -> bool {
    let actual = normalize_alias(program, actual);
    let expected = normalize_alias(program, expected);
    if actual == expected || matches!(actual, Type::Error | Type::Primitive(PrimitiveType::Never)) {
        return true;
    }
    match (&actual, &expected) {
        (actual, Type::Optional(expected)) if compatible(program, actual, expected) => true,
        (Type::Array(actual, _), Type::Slice(expected))
        | (Type::Optional(actual), Type::Optional(expected)) => {
            compatible(program, actual, expected)
        }
        (
            Type::Reference {
                mutable: actual_mutable,
                lifetime: actual_lifetime,
                referent: actual,
            },
            Type::Reference {
                mutable: expected_mutable,
                lifetime: expected_lifetime,
                referent: expected,
            },
        ) => {
            (!expected_mutable || *actual_mutable)
                && (actual_lifetime == expected_lifetime || actual_lifetime == "static")
                && compatible(program, actual, expected)
        }
        (Type::Nominal(actual, actual_arguments), Type::Nominal(expected, expected_arguments))
            if actual_arguments.is_empty() && expected_arguments.is_empty() =>
        {
            class_is_or_extends(program, *actual, *expected)
        }
        (Type::Nominal(actual, _), Type::DynamicInterface(interface, _)) => {
            explicitly_conforms(program, *actual, *interface)
        }
        (Type::Reference { referent, .. }, Type::DynamicInterface(interface, _)) => {
            match referent.as_ref() {
                Type::Generic(_) => true,
                Type::Nominal(actual, _) => explicitly_conforms(program, *actual, *interface),
                Type::Primitive(_) | Type::String | Type::Str => {
                    matches!(
                        program
                            .graph
                            .declaration(*interface)
                            .and_then(|declaration| declaration.name.as_deref()),
                        Some("Equal" | "Hash" | "Ord")
                    )
                }
                _ => false,
            }
        }
        (
            Type::Promise {
                result: actual_result,
                error: actual_error,
                ..
            },
            Type::Promise {
                result: expected_result,
                error: expected_error,
                ..
            },
        ) => {
            compatible(program, actual_result, expected_result)
                && compatible(program, actual_error, expected_error)
        }
        (Type::String, Type::Str) | (Type::Str, Type::String) | (_, Type::Generic(_)) => true,
        _ => false,
    }
}

fn normalize_alias(program: &Program, ty: &Type) -> Type {
    let mut current = ty.clone();
    let mut visited = BTreeSet::new();
    loop {
        let Type::Nominal(declaration, arguments) = &current else {
            return current;
        };
        if !visited.insert(*declaration) {
            return current;
        }
        let Some(definition) = program.definition(*declaration) else {
            return current;
        };
        let DefinitionData::TypeAlias(alias) = &definition.data else {
            return current;
        };
        if definition.generics.len() != arguments.len() {
            return current;
        }
        let substitutions = definition
            .generics
            .iter()
            .zip(arguments)
            .map(|(generic, argument)| (generic.name.clone(), argument.clone()))
            .collect();
        current = substitute_type(alias, &substitutions);
    }
}

fn string_comparison_compatible(left: &Type, right: &Type) -> bool {
    is_string_like(left) && is_string_like(right)
}

fn binary_right_expected(operator: TokenKind, left: &Type) -> Option<&Type> {
    if matches!(
        operator,
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
    ) && is_string_like(left)
    {
        None
    } else {
        Some(left)
    }
}

fn is_string_like(ty: &Type) -> bool {
    match ty {
        Type::String | Type::Str => true,
        Type::Reference { referent, .. } => {
            matches!(referent.as_ref(), Type::Str | Type::String)
        }
        _ => false,
    }
}

fn infer_substitutions(parameter: &Type, argument: &Type, inferred: &mut BTreeMap<String, Type>) {
    match (parameter, argument) {
        (Type::Generic(name) | Type::Lifetime(name), argument) => {
            inferred
                .entry(name.clone())
                .or_insert_with(|| argument.clone());
        }
        (Type::Nominal(_, parameters), Type::Nominal(_, arguments))
        | (Type::DynamicInterface(_, parameters), Type::DynamicInterface(_, arguments))
        | (Type::Tuple(parameters), Type::Tuple(arguments)) => {
            for (parameter, argument) in parameters.iter().zip(arguments) {
                infer_substitutions(parameter, argument, inferred);
            }
        }
        (Type::Optional(parameter), Type::Optional(argument))
        | (Type::Array(parameter, _), Type::Array(argument, _))
        | (Type::Slice(parameter), Type::Slice(argument)) => {
            infer_substitutions(parameter, argument, inferred);
        }
        (
            Type::Promise {
                result: parameter,
                error: parameter_error,
                ..
            },
            Type::Promise {
                result: argument,
                error: argument_error,
                ..
            },
        ) => {
            infer_substitutions(parameter, argument, inferred);
            infer_substitutions(parameter_error, argument_error, inferred);
        }
        (
            Type::Reference {
                lifetime: parameter_lifetime,
                referent: parameter,
                ..
            },
            Type::Reference {
                lifetime: argument_lifetime,
                referent: argument,
                ..
            },
        ) => {
            if parameter_lifetime != "scope" && parameter_lifetime != "static" {
                inferred
                    .entry(parameter_lifetime.clone())
                    .or_insert_with(|| Type::Lifetime(argument_lifetime.clone()));
            }
            infer_substitutions(parameter, argument, inferred);
        }
        (
            Type::RawPointer {
                pointee: parameter, ..
            },
            Type::RawPointer {
                pointee: argument, ..
            },
        ) => infer_substitutions(parameter, argument, inferred),
        (Type::Function(parameter), Type::Function(argument)) => {
            for (parameter, argument) in parameter.parameters.iter().zip(&argument.parameters) {
                infer_substitutions(parameter, argument, inferred);
            }
            infer_substitutions(&parameter.result, &argument.result, inferred);
        }
        _ => {}
    }
}

fn substitute_type(ty: &Type, substitutions: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Generic(name) | Type::Lifetime(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Nominal(id, arguments) => Type::Nominal(
            *id,
            arguments
                .iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
        ),
        Type::DynamicInterface(id, arguments) => Type::DynamicInterface(
            *id,
            arguments
                .iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
        ),
        Type::Optional(inner) => Type::Optional(Box::new(substitute_type(inner, substitutions))),
        Type::Array(inner, length) => {
            Type::Array(Box::new(substitute_type(inner, substitutions)), *length)
        }
        Type::Slice(inner) => Type::Slice(Box::new(substitute_type(inner, substitutions))),
        Type::Tuple(elements) => Type::Tuple(
            elements
                .iter()
                .map(|element| substitute_type(element, substitutions))
                .collect(),
        ),
        Type::Template(captures) => Type::Template(
            captures
                .iter()
                .map(|capture| substitute_type(capture, substitutions))
                .collect(),
        ),
        Type::Reference {
            mutable,
            lifetime,
            referent,
        } => Type::Reference {
            mutable: *mutable,
            lifetime: substitutions
                .get(lifetime)
                .and_then(|replacement| match replacement {
                    Type::Lifetime(lifetime) => Some(lifetime.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| lifetime.clone()),
            referent: Box::new(substitute_type(referent, substitutions)),
        },
        Type::RawPointer { mutable, pointee } => Type::RawPointer {
            mutable: *mutable,
            pointee: Box::new(substitute_type(pointee, substitutions)),
        },
        Type::Promise {
            result,
            error,
            effects,
        } => {
            let error = substitute_type(error, substitutions);
            Type::Promise {
                result: Box::new(substitute_type(result, substitutions)),
                error: Box::new(error.clone()),
                effects: tn_hir::promise_effects(&error, effects),
            }
        }
        Type::Function(function) => Type::Function(tn_hir::FunctionType {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| substitute_type(parameter, substitutions))
                .collect(),
            result: Box::new(substitute_type(&function.result, substitutions)),
            effects: function.effects.clone(),
            generics: function.generics.clone(),
            is_async: function.is_async,
            is_unsafe: function.is_unsafe,
        }),
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => ty.clone(),
    }
}

fn satisfies_bound(
    program: &Program,
    function: &Function,
    owner: DeclarationId,
    ty: &Type,
    bound: &tn_hir::GenericBound,
) -> bool {
    match bound {
        tn_hir::GenericBound::Static => !matches!(
            ty,
            Type::Reference { lifetime, .. } if lifetime != "static"
        ),
        tn_hir::GenericBound::Outlives(_) => matches!(ty, Type::Reference { .. }),
        tn_hir::GenericBound::Interface(interface, _) => {
            if let Type::Generic(name) = ty {
                return function
                    .generics
                    .iter()
                    .chain(
                        program
                            .definition(owner)
                            .into_iter()
                            .flat_map(|definition| definition.generics.iter()),
                    )
                    .find(|parameter| parameter.name == *name)
                    .is_some_and(|parameter| {
                        parameter.bounds.iter().any(|candidate| {
                            matches!(candidate, tn_hir::GenericBound::Interface(candidate, _) if candidate == interface)
                        })
                    });
            }
            let name = declaration_name(program, *interface);
            if matches!(name, Some("Equal" | "Hash" | "Ord"))
                && matches!(ty, Type::Primitive(_) | Type::String | Type::Str)
            {
                return true;
            }
            nominal_id(ty).is_some_and(|nominal| explicitly_conforms(program, nominal, *interface))
        }
    }
}

fn class_is_or_extends(
    program: &Program,
    mut actual: DeclarationId,
    expected: DeclarationId,
) -> bool {
    let mut visited = BTreeSet::new();
    while visited.insert(actual) {
        if actual == expected {
            return true;
        }
        let Some(Definition {
            data: DefinitionData::Class {
                base: Some(base), ..
            },
            ..
        }) = program.definition(actual)
        else {
            return false;
        };
        actual = *base;
    }
    false
}

fn explicitly_conforms(
    program: &Program,
    nominal: DeclarationId,
    interface: DeclarationId,
) -> bool {
    declared_conformances(program, nominal).contains(&interface)
        || program.definitions.iter().any(|definition| {
            matches!(
                &definition.data,
                DefinitionData::Implementation {
                    interface: Some(Type::Nominal(implemented, _)),
                    target: Type::Nominal(target, _),
                    ..
                } if *implemented == interface && *target == nominal
            )
        })
}

fn standard_interface(program: &Program, name: &str) -> Option<DeclarationId> {
    program.graph.modules.iter().find_map(|module| {
        program
            .graph
            .is_bundled_module(module.id, "core.tn")
            .then(|| {
                module.declarations.iter().find_map(|declaration| {
                    (declaration.kind == tn_hir::DeclarationKind::Interface
                        && declaration.name.as_deref() == Some(name))
                    .then_some(declaration.id)
                })
            })
            .flatten()
    })
}

fn nominal_conforms_to_interface(
    program: &Program,
    mut nominal: DeclarationId,
    interface: DeclarationId,
) -> bool {
    let mut visited = BTreeSet::new();
    while visited.insert(nominal) {
        if nominal == interface || explicitly_conforms(program, nominal, interface) {
            return true;
        }
        let Some(DefinitionData::Class {
            base: Some(base), ..
        }) = program
            .definition(nominal)
            .map(|definition| &definition.data)
        else {
            return false;
        };
        nominal = *base;
    }
    false
}

fn catch_handles(program: &Program, caught: DeclarationId, effect: DeclarationId) -> bool {
    if caught == effect || class_is_or_extends(program, effect, caught) {
        return true;
    }
    program.definition(caught).is_some_and(|definition| {
        matches!(definition.data, DefinitionData::Interface { .. })
            && explicitly_conforms(program, effect, caught)
    })
}

fn merge_branches(left: Option<ExpressionType>, right: Option<ExpressionType>) -> ExpressionType {
    match (left, right) {
        (Some(left), Some(right)) if left.ty == right.ty => value_type(left.ty),
        (Some(left), None) | (None, Some(left)) => value_type(left.ty),
        _ => value_type(Type::Error),
    }
}

fn binding_power(kind: TokenKind) -> Option<(u8, u8)> {
    Some(match kind {
        TokenKind::Equal
        | TokenKind::PlusEqual
        | TokenKind::MinusEqual
        | TokenKind::StarEqual
        | TokenKind::SlashEqual
        | TokenKind::PercentEqual
        | TokenKind::AmpEqual
        | TokenKind::PipeEqual
        | TokenKind::CaretEqual
        | TokenKind::ShiftLeftEqual
        | TokenKind::ShiftRightEqual => (1, 1),
        TokenKind::QuestionQuestion => (3, 4),
        TokenKind::PipePipe => (5, 6),
        TokenKind::AmpAmp => (7, 8),
        TokenKind::Pipe => (9, 10),
        TokenKind::Caret => (11, 12),
        TokenKind::Amp => (13, 14),
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual => (15, 16),
        TokenKind::Less
        | TokenKind::LessEqual
        | TokenKind::Greater
        | TokenKind::GreaterEqual
        | TokenKind::InstanceOf => (17, 18),
        TokenKind::ShiftLeft | TokenKind::ShiftRight => (19, 20),
        TokenKind::Plus | TokenKind::Minus => (21, 22),
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (23, 24),
        _ => return None,
    })
}

fn integer_literal_type(text: &str, expected: Option<&Type>) -> Type {
    const SUFFIXES: &[(&str, PrimitiveType)] = &[
        ("isize", PrimitiveType::Isize),
        ("usize", PrimitiveType::Usize),
        ("i128", PrimitiveType::I128),
        ("u128", PrimitiveType::U128),
        ("i64", PrimitiveType::I64),
        ("u64", PrimitiveType::U64),
        ("i32", PrimitiveType::I32),
        ("u32", PrimitiveType::U32),
        ("i16", PrimitiveType::I16),
        ("u16", PrimitiveType::U16),
        ("i8", PrimitiveType::I8),
        ("u8", PrimitiveType::U8),
    ];
    if let Some((_, primitive)) = SUFFIXES.iter().find(|(suffix, _)| text.ends_with(suffix)) {
        return Type::Primitive(primitive.clone());
    }
    match expected {
        Some(ty) if is_integer(ty) => ty.clone(),
        Some(Type::Optional(inner)) if is_integer(inner) => inner.as_ref().clone(),
        _ => Type::Primitive(PrimitiveType::Isize),
    }
}

fn float_literal_type(text: &str, expected: Option<&Type>) -> Type {
    if text.ends_with("f32") {
        Type::Primitive(PrimitiveType::F32)
    } else if text.ends_with("f64") {
        Type::Primitive(PrimitiveType::F64)
    } else {
        match expected {
            Some(Type::Primitive(PrimitiveType::F32)) => Type::Primitive(PrimitiveType::F32),
            Some(Type::Optional(inner))
                if matches!(inner.as_ref(), Type::Primitive(PrimitiveType::F32)) =>
            {
                Type::Primitive(PrimitiveType::F32)
            }
            _ => Type::Primitive(PrimitiveType::F64),
        }
    }
}

fn is_integer(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Primitive(
            PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::I64
                | PrimitiveType::I128
                | PrimitiveType::Isize
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32
                | PrimitiveType::U64
                | PrimitiveType::U128
                | PrimitiveType::Usize
        )
    )
}

fn is_numeric(ty: &Type) -> bool {
    is_integer(ty) || matches!(ty, Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64))
}

fn borrowed_field_type(facts: &OwnershipFacts, ty: Type, mutable: bool, lifetime: &str) -> Type {
    match ty {
        Type::Optional(inner) if !facts.is_copy(&inner) => {
            Type::Optional(Box::new(Type::Reference {
                mutable,
                lifetime: lifetime.to_owned(),
                referent: inner,
            }))
        }
        ty if !facts.is_copy(&ty) => Type::Reference {
            mutable,
            lifetime: lifetime.to_owned(),
            referent: Box::new(ty),
        },
        ty => ty,
    }
}

fn relate_elided_lifetime(ty: &Type, receiver_lifetime: &str) -> Type {
    match ty {
        Type::Reference {
            mutable,
            lifetime,
            referent,
        } => Type::Reference {
            mutable: *mutable,
            lifetime: if lifetime == "scope" {
                receiver_lifetime.to_owned()
            } else {
                lifetime.clone()
            },
            referent: Box::new(relate_elided_lifetime(referent, receiver_lifetime)),
        },
        Type::Nominal(id, arguments) => Type::Nominal(
            *id,
            arguments
                .iter()
                .map(|argument| relate_elided_lifetime(argument, receiver_lifetime))
                .collect(),
        ),
        Type::Lifetime(lifetime) if lifetime == "scope" => {
            Type::Lifetime(receiver_lifetime.to_owned())
        }
        Type::Optional(inner) => {
            Type::Optional(Box::new(relate_elided_lifetime(inner, receiver_lifetime)))
        }
        Type::Promise {
            result,
            error,
            effects,
        } => Type::Promise {
            result: Box::new(relate_elided_lifetime(result, receiver_lifetime)),
            error: Box::new(relate_elided_lifetime(error, receiver_lifetime)),
            effects: effects.clone(),
        },
        _ => ty.clone(),
    }
}

fn template_displayable(program: &Program, function: &Function, ty: &Type) -> bool {
    match ty {
        Type::Primitive(PrimitiveType::Void | PrimitiveType::Never)
        | Type::Promise { .. }
        | Type::RawPointer { .. }
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Optional(_)
        | Type::Array(_, _)
        | Type::Slice(_)
        | Type::Tuple(_)
        | Type::Function(_)
        | Type::DynamicInterface(_, _)
        | Type::Unknown => false,
        Type::Primitive(_) | Type::String | Type::Str | Type::Template(_) => true,
        Type::Reference { referent, .. } => template_displayable(program, function, referent),
        Type::Generic(name) => function
            .generics
            .iter()
            .find(|parameter| parameter.name == *name)
            .is_some_and(|parameter| {
                parameter.bounds.iter().any(|bound| {
                    let tn_hir::GenericBound::Interface(interface, _) = bound else {
                        return false;
                    };
                    declaration_name(program, *interface) == Some("Display")
                })
            }),
        Type::Nominal(nominal, _) => {
            declared_conformances(program, *nominal)
                .iter()
                .any(|interface| declaration_name(program, *interface) == Some("Display"))
                || program.definitions.iter().any(|definition| {
                    let DefinitionData::Implementation {
                        interface: Some(Type::Nominal(interface, _)),
                        target: Type::Nominal(target, _),
                        ..
                    } = &definition.data
                    else {
                        return false;
                    };
                    target == nominal && declaration_name(program, *interface) == Some("Display")
                })
        }
    }
}

fn supports_operator(
    program: &Program,
    function: &Function,
    owner: DeclarationId,
    ty: &Type,
    operator: TokenKind,
) -> bool {
    if matches!(ty, Type::String)
        && matches!(
            operator,
            TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
        )
    {
        return true;
    }
    if is_numeric(ty) {
        return true;
    }
    if matches!(
        operator,
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
    ) && matches!(
        ty,
        Type::Primitive(PrimitiveType::Bool | PrimitiveType::Char)
            | Type::String
            | Type::Str
            | Type::RawPointer { .. }
    ) {
        return true;
    }
    if matches!(
        operator,
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
    ) && matches!(
        ty,
        Type::Reference { referent, .. }
            if matches!(referent.as_ref(), Type::Str | Type::String)
    ) {
        return true;
    }
    if matches!(
        operator,
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
    ) && matches!(ty, Type::Optional(_))
    {
        return true;
    }
    if matches!(ty, Type::Nominal(id, _) if program.definition(*id).is_some_and(|definition| matches!(definition.data, DefinitionData::Class { .. })))
        && matches!(
            operator,
            TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual
        )
    {
        return true;
    }
    let required = match operator {
        TokenKind::Plus | TokenKind::PlusEqual => "Add",
        TokenKind::Minus | TokenKind::MinusEqual => "Sub",
        TokenKind::Star | TokenKind::StarEqual => "Mul",
        TokenKind::Slash | TokenKind::SlashEqual => "Div",
        TokenKind::Percent | TokenKind::PercentEqual => "Rem",
        TokenKind::Amp | TokenKind::AmpEqual => "BitAnd",
        TokenKind::Pipe | TokenKind::PipeEqual => "BitOr",
        TokenKind::Caret | TokenKind::CaretEqual => "BitXor",
        TokenKind::ShiftLeft | TokenKind::ShiftLeftEqual => "Shl",
        TokenKind::ShiftRight | TokenKind::ShiftRightEqual => "Shr",
        TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual => "Equal",
        TokenKind::Less | TokenKind::LessEqual | TokenKind::Greater | TokenKind::GreaterEqual => {
            "Ord"
        }
        _ => return false,
    };
    match ty {
        Type::Generic(name) => generic_supports_operator(program, function, owner, name, required),
        Type::Nominal(nominal, _) => {
            declared_conformances(program, *nominal)
                .iter()
                .any(|interface| declaration_name(program, *interface) == Some(required))
                || program.definitions.iter().any(|definition| {
                    let DefinitionData::Implementation {
                        interface: Some(Type::Nominal(interface, _)),
                        target: Type::Nominal(target, _),
                        ..
                    } = &definition.data
                    else {
                        return false;
                    };
                    target == nominal && declaration_name(program, *interface) == Some(required)
                })
        }
        _ => false,
    }
}

fn generic_supports_operator(
    program: &Program,
    function: &Function,
    owner: DeclarationId,
    name: &str,
    required: &str,
) -> bool {
    function
        .generics
        .iter()
        .chain(
            program
                .definition(owner)
                .into_iter()
                .flat_map(|definition| definition.generics.iter()),
        )
        .find(|parameter| parameter.name == name)
        .is_some_and(|parameter| {
            parameter.bounds.iter().any(|bound| {
                let tn_hir::GenericBound::Interface(interface, _) = bound else {
                    return false;
                };
                declaration_name(program, *interface) == Some(required)
            })
        })
}

fn generic_supports_interface(
    program: &Program,
    function: &Function,
    owner: DeclarationId,
    name: &str,
    required: DeclarationId,
) -> bool {
    function
        .generics
        .iter()
        .chain(
            program
                .definition(owner)
                .into_iter()
                .flat_map(|definition| definition.generics.iter()),
        )
        .find(|parameter| parameter.name == name)
        .is_some_and(|parameter| {
            parameter.bounds.iter().any(|bound| {
                let tn_hir::GenericBound::Interface(interface, _) = bound else {
                    return false;
                };
                *interface == required
            })
        })
}

fn declaration_name(program: &Program, declaration: DeclarationId) -> Option<&str> {
    program
        .graph
        .declaration(declaration)
        .and_then(|declaration| declaration.name.as_deref())
}

#[derive(Clone, Copy)]
enum IterationError {
    NotIterable,
    InvalidProtocol,
}

type ImplementationMatch<'program> = (
    &'program Definition,
    Vec<Type>,
    &'program [Method],
    BTreeMap<String, Type>,
);

#[allow(clippy::too_many_lines)]
fn iteration_implementation<'program>(
    program: &'program Program,
    interface_name: &str,
    target_type: &Type,
    missing: IterationError,
) -> Result<ImplementationMatch<'program>, IterationError> {
    let mut matches = Vec::new();
    for definition in &program.definitions {
        if let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, arguments)),
            target,
            methods,
            ..
        } = &definition.data
            && declaration_name(program, *interface) == Some(interface_name)
        {
            let mut substitutions = BTreeMap::new();
            infer_substitutions(target, target_type, &mut substitutions);
            if compatible(
                program,
                target_type,
                &substitute_type(target, &substitutions),
            ) {
                matches.push((
                    definition,
                    arguments.clone(),
                    methods.as_slice(),
                    substitutions,
                ));
            }
        }
    }

    for definition in &program.definitions {
        let methods = match &definition.data {
            DefinitionData::Struct { methods, .. } | DefinitionData::Class { methods, .. } => {
                methods.as_slice()
            }
            _ => continue,
        };
        let Some(interface) = declared_conformances(program, definition.declaration)
            .into_iter()
            .find(|interface| declaration_name(program, *interface) == Some(interface_name))
        else {
            continue;
        };
        let Some(DefinitionData::Interface {
            methods: interface_methods,
            ..
        }) = program
            .definition(interface)
            .map(|definition| &definition.data)
        else {
            continue;
        };
        let mut interface_substitutions = BTreeMap::new();
        for interface_method in interface_methods {
            let Some(method) = methods
                .iter()
                .find(|method| method.name == interface_method.name)
            else {
                continue;
            };
            for (parameter, actual) in interface_method
                .function
                .parameters
                .iter()
                .zip(&method.function.parameters)
            {
                infer_substitutions(&parameter.ty, &actual.ty, &mut interface_substitutions);
            }
            infer_substitutions(
                &interface_method.function.result,
                &method.function.result,
                &mut interface_substitutions,
            );
        }
        let arguments = program
            .definition(interface)
            .map(|definition| {
                definition
                    .generics
                    .iter()
                    .map(|parameter| {
                        substitute_type(
                            &Type::Generic(parameter.name.clone()),
                            &interface_substitutions,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let target = Type::Nominal(
            definition.declaration,
            definition
                .generics
                .iter()
                .map(|parameter| Type::Generic(parameter.name.clone()))
                .collect(),
        );
        let mut substitutions = BTreeMap::new();
        infer_substitutions(&target, target_type, &mut substitutions);
        if compatible(
            program,
            target_type,
            &substitute_type(&target, &substitutions),
        ) {
            matches.push((definition, arguments, methods, substitutions));
        }
    }
    let [selected] = matches.as_slice() else {
        return Err(if matches.is_empty() {
            missing
        } else {
            IterationError::InvalidProtocol
        });
    };
    Ok(selected.clone())
}

fn for_iteration(
    program: &Program,
    iterable: &Type,
    awaited: bool,
) -> Result<(Type, Option<IterationWitness>), IterationError> {
    if let Type::DynamicInterface(interface, arguments) = iterable {
        let expected = if awaited { "AsyncIterable" } else { "Iterable" };
        if declaration_name(program, *interface) == Some(expected)
            && let [item] = arguments.as_slice()
        {
            return Ok((
                item.clone(),
                Some(IterationWitness::Generator {
                    item_type: item.clone(),
                    asynchronous: awaited,
                }),
            ));
        }
    }
    match iterable {
        Type::Array(element, _) | Type::Slice(element) => {
            return Ok((element.as_ref().clone(), None));
        }
        Type::Reference { referent, .. }
            if matches!(
                referent.as_ref(),
                Type::Array(_, _) | Type::Slice(_) | Type::Str
            ) =>
        {
            return for_iteration(program, referent, awaited);
        }
        Type::Str => return Ok((Type::Primitive(PrimitiveType::Char), None)),
        _ => {}
    }
    let (into_definition, into_arguments, into_methods, into_substitutions) =
        iteration_implementation(
            program,
            "IntoIterator",
            iterable,
            IterationError::NotIterable,
        )?;
    let [item_pattern, iterator_pattern] = into_arguments.as_slice() else {
        return Err(IterationError::InvalidProtocol);
    };
    let item_type = substitute_type(item_pattern, &into_substitutions);
    let iterator_type = substitute_type(iterator_pattern, &into_substitutions);
    let Some(into_method) = into_methods
        .iter()
        .find(|method| method.name == "intoIterator")
    else {
        return Err(IterationError::InvalidProtocol);
    };
    let into_result = substitute_type(&into_method.function.result, &into_substitutions);
    if into_method.receiver != ReceiverMode::Move
        || !into_method.function.parameters.is_empty()
        || !into_method.function.effects.is_empty()
        || into_method.function.is_async
        || into_result != iterator_type
    {
        return Err(IterationError::InvalidProtocol);
    }

    let (iterator_definition, iterator_arguments, iterator_methods, iterator_substitutions) =
        iteration_implementation(
            program,
            "Iterator",
            &iterator_type,
            IterationError::InvalidProtocol,
        )?;
    let [iterator_item] = iterator_arguments.as_slice() else {
        return Err(IterationError::InvalidProtocol);
    };
    let iterator_item = substitute_type(iterator_item, &iterator_substitutions);
    let item_type = match item_type {
        Type::Generic(_) => iterator_item.clone(),
        item_type => item_type,
    };
    if iterator_item != item_type {
        return Err(IterationError::InvalidProtocol);
    }
    let Some(next_method) = iterator_methods.iter().find(|method| method.name == "next") else {
        return Err(IterationError::InvalidProtocol);
    };
    let next_result = substitute_type(&next_method.function.result, &iterator_substitutions);
    if !matches!(
        next_method.receiver,
        ReceiverMode::Shared | ReceiverMode::Mutable
    ) || !next_method.function.parameters.is_empty()
        || !next_method.function.effects.is_empty()
        || next_method.function.is_async
        || next_result != Type::Optional(Box::new(item_type.clone()))
    {
        return Err(IterationError::InvalidProtocol);
    }
    Ok((
        item_type.clone(),
        Some(IterationWitness::Declared {
            into_iterator_implementation: into_definition.declaration,
            into_iterator_method: into_method.id,
            iterator_implementation: iterator_definition.declaration,
            next_method: next_method.id,
            next_receiver: next_method.receiver,
            iterator_type,
            item_type,
        }),
    ))
}

fn nominal_id(ty: &Type) -> Option<DeclarationId> {
    match ty {
        Type::Nominal(id, _) | Type::DynamicInterface(id, _) => Some(*id),
        _ => None,
    }
}

fn promise_like(program: &Program, ty: &Type) -> Option<(Type, Vec<DeclarationId>)> {
    if let Type::Promise {
        result, effects, ..
    } = ty
    {
        return Some((result.as_ref().clone(), effects.clone()));
    }
    let Type::Nominal(declaration, arguments) = ty else {
        return None;
    };
    let item = program.graph.declaration(*declaration)?;
    let module = program.graph.module(item.module)?;
    (item.name.as_deref() == Some("Task")
        && module.path.ends_with("std/async.tn")
        && arguments.len() == 2)
        .then(|| {
            (
                arguments[0].clone(),
                tn_hir::promise_effects(&arguments[1], &[]),
            )
        })
}

fn pattern_space(program: &Program, ty: &Type) -> (BTreeMap<String, bool>, bool) {
    match ty {
        Type::Primitive(PrimitiveType::Bool) => (
            [("true".into(), false), ("false".into(), false)]
                .into_iter()
                .collect(),
            true,
        ),
        Type::Optional(_) => (
            [("undefined".into(), false), ("present".into(), false)]
                .into_iter()
                .collect(),
            true,
        ),
        Type::Nominal(id, _) => {
            let variants = program.definition(*id).and_then(|definition| {
                let DefinitionData::Enum { variants, .. } = &definition.data else {
                    return None;
                };
                Some(
                    variants
                        .iter()
                        .map(|variant| (variant.name.clone(), false))
                        .collect(),
                )
            });
            variants.map_or_else(|| (BTreeMap::new(), false), |variants| (variants, true))
        }
        _ => (BTreeMap::new(), false),
    }
}

fn pattern_constructor(program: &Program, ty: &Type, key: Option<&str>) -> Option<MemberId> {
    let (Type::Nominal(id, _), Some(key)) = (ty, key) else {
        return None;
    };
    let DefinitionData::Enum { variants, .. } = &program.definition(*id)?.data else {
        return None;
    };
    variants
        .iter()
        .find(|variant| variant.name == key)
        .map(|variant| variant.id)
}

#[allow(clippy::too_many_lines)]
fn classify_pattern(
    program: &Program,
    scrutinee: &Type,
    tokens: &[Token],
    source: &str,
) -> (Option<String>, BTreeMap<String, Type>, Vec<PatternProblem>) {
    let mut bindings = BTreeMap::new();
    let mut problems = Vec::new();
    if tokens.is_empty() {
        return (None, bindings, problems);
    }
    let first_text = &source[tokens[0].range.clone()];
    if first_text == "_" {
        return (None, bindings, problems);
    }
    match scrutinee {
        Type::Primitive(PrimitiveType::Bool) => {
            return (Some(first_text.to_owned()), bindings, problems);
        }
        Type::Optional(inner) => {
            if tokens[0].kind == TokenKind::Undefined {
                return (Some("undefined".into()), bindings, problems);
            }
            if tokens.len() == 1 && tokens[0].kind == TokenKind::Identifier {
                bindings.insert(first_text.to_owned(), inner.as_ref().clone());
                return (None, bindings, problems);
            }
            for token in tokens {
                if token.kind == TokenKind::Identifier {
                    let name = &source[token.range.clone()];
                    if name != "_" {
                        bindings.insert(name.to_owned(), inner.as_ref().clone());
                    }
                }
            }
            return (Some("present".into()), bindings, problems);
        }
        Type::Nominal(id, arguments) => {
            if let Some(definition) = program.definition(*id) {
                let substitutions = definition
                    .generics
                    .iter()
                    .filter(|parameter| parameter.namespace != tn_hir::Namespace::Value)
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                match &definition.data {
                    DefinitionData::Enum { variants, .. } => {
                        if let Some((variant, variant_index)) =
                            variants.iter().find_map(|variant| {
                                tokens
                                    .iter()
                                    .position(|token| source[token.range.clone()] == variant.name)
                                    .map(|index| (variant, index))
                            })
                        {
                            let payload = &tokens[variant_index + 1..];
                            if payload
                                .first()
                                .is_some_and(|token| token.kind == TokenKind::LeftBrace)
                            {
                                let fields = variant
                                    .fields
                                    .iter()
                                    .filter_map(|field| {
                                        field.name.as_ref().map(|name| {
                                            (
                                                name.clone(),
                                                substitute_type(&field.ty, &substitutions),
                                            )
                                        })
                                    })
                                    .collect::<BTreeMap<_, _>>();
                                collect_record_bindings(
                                    payload,
                                    source,
                                    &fields,
                                    &mut bindings,
                                    &mut problems,
                                    variant_index + 1,
                                );
                            } else {
                                let names = payload
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, token)| token.kind == TokenKind::Identifier)
                                    .filter_map(|(index, token)| {
                                        let name = &source[token.range.clone()];
                                        (name != "_").then_some((index, name))
                                    });
                                for ((_, name), field) in names.zip(&variant.fields) {
                                    bindings.insert(
                                        name.to_owned(),
                                        substitute_type(&field.ty, &substitutions),
                                    );
                                }
                                let supplied = split_pattern_fields(payload).len();
                                if payload
                                    .first()
                                    .is_some_and(|token| token.kind == TokenKind::LeftParen)
                                    && supplied != variant.fields.len()
                                {
                                    problems.push(PatternProblem {
                                        condition: "TYPE_PATTERN_ARITY_MISMATCH",
                                        message: format!(
                                            "variant `{}` expects {} payload pattern(s), but {} were supplied",
                                            variant.name,
                                            variant.fields.len(),
                                            supplied
                                        ),
                                        label: "match the declared variant payload arity",
                                        token_index: variant_index,
                                    });
                                }
                            }
                            return (Some(variant.name.clone()), bindings, problems);
                        }
                        if tokens.iter().any(|token| {
                            matches!(token.kind, TokenKind::LeftParen | TokenKind::LeftBrace)
                        }) {
                            problems.push(PatternProblem {
                                condition: "TYPE_UNKNOWN_PATTERN_CONSTRUCTOR",
                                message: "pattern does not name a variant of the scrutinee enum"
                                    .into(),
                                label: "use a variant declared by this enum",
                                token_index: 0,
                            });
                        }
                    }
                    DefinitionData::Struct { fields, .. }
                        if tokens
                            .iter()
                            .any(|token| token.kind == TokenKind::LeftBrace) =>
                    {
                        let field_types = fields
                            .iter()
                            .map(|field| {
                                (
                                    field.name.clone(),
                                    substitute_type(&field.ty, &substitutions),
                                )
                            })
                            .collect::<BTreeMap<_, _>>();
                        collect_record_bindings(
                            tokens,
                            source,
                            &field_types,
                            &mut bindings,
                            &mut problems,
                            0,
                        );
                        return (None, bindings, problems);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if tokens.len() == 1 && tokens[0].kind == TokenKind::Identifier {
        bindings.insert(first_text.to_owned(), scrutinee.clone());
        (None, bindings, problems)
    } else {
        (
            Some(
                tokens
                    .iter()
                    .map(|token| &source[token.range.clone()])
                    .collect::<String>(),
            ),
            bindings,
            problems,
        )
    }
}

struct PatternProblem {
    condition: &'static str,
    message: String,
    label: &'static str,
    token_index: usize,
}

fn split_pattern_fields(tokens: &[Token]) -> Vec<&[Token]> {
    let Some(open) = tokens
        .iter()
        .position(|token| matches!(token.kind, TokenKind::LeftParen | TokenKind::LeftBrace))
    else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    let mut depth = 0_u32;
    let mut start = open + 1;
    for (index, token) in tokens.iter().enumerate().skip(open + 1) {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBrace => depth += 1,
            TokenKind::RightParen | TokenKind::RightBrace if depth == 0 => {
                if start < index {
                    fields.push(&tokens[start..index]);
                }
                break;
            }
            TokenKind::RightParen | TokenKind::RightBrace => depth = depth.saturating_sub(1),
            TokenKind::Comma if depth == 0 => {
                fields.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    fields
}

fn pattern_binding_projections(
    program: &Program,
    scrutinee: &Type,
    constructor: Option<MemberId>,
    tokens: &[Token],
    source: &str,
) -> BTreeMap<String, Vec<HirPatternProjection>> {
    let mut projections = BTreeMap::new();
    if let Type::Optional(_) = scrutinee {
        for token in tokens {
            if token.kind == TokenKind::Identifier {
                let name = &source[token.range.clone()];
                if name != "_" {
                    projections
                        .insert(name.to_owned(), vec![HirPatternProjection::OptionalPayload]);
                }
            }
        }
        return projections;
    }
    let Type::Nominal(declaration, _) = scrutinee else {
        return projections;
    };
    let Some(definition) = program.definition(*declaration) else {
        return projections;
    };
    let fields = match (&definition.data, constructor) {
        (DefinitionData::Enum { variants, .. }, Some(constructor)) => variants
            .iter()
            .find(|variant| variant.id == constructor)
            .map(|variant| {
                variant
                    .fields
                    .iter()
                    .map(|field| field.name.as_deref())
                    .collect::<Vec<_>>()
            }),
        (DefinitionData::Struct { fields, .. }, None) => Some(
            fields
                .iter()
                .map(|field| Some(field.name.as_str()))
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    let Some(fields) = fields else {
        return projections;
    };
    for (position, pattern) in split_pattern_fields(tokens).into_iter().enumerate() {
        let identifiers = pattern
            .iter()
            .filter(|token| token.kind == TokenKind::Identifier)
            .map(|token| &source[token.range.clone()])
            .collect::<Vec<_>>();
        let Some(binding) = identifiers.last().copied().filter(|name| *name != "_") else {
            continue;
        };
        let field_index = fields
            .iter()
            .position(|field| field.is_some_and(|field| field == identifiers[0]))
            .unwrap_or(position);
        let mut projection = Vec::new();
        if let Some(constructor) = constructor {
            projection.push(HirPatternProjection::Variant(constructor));
        }
        projection.push(HirPatternProjection::Field(
            u32::try_from(field_index).expect("pattern field limit"),
        ));
        projections.insert(binding.to_owned(), projection);
    }
    projections
}

fn collect_record_bindings(
    tokens: &[Token],
    source: &str,
    fields: &BTreeMap<String, Type>,
    bindings: &mut BTreeMap<String, Type>,
    problems: &mut Vec<PatternProblem>,
    token_offset: usize,
) {
    let mut seen = BTreeSet::new();
    for pattern in split_pattern_fields(tokens) {
        let Some((relative_index, field_token)) = pattern
            .iter()
            .enumerate()
            .find(|(_, token)| token.kind == TokenKind::Identifier)
        else {
            continue;
        };
        let field_name = &source[field_token.range.clone()];
        let absolute_index = tokens
            .iter()
            .position(|candidate| std::ptr::eq(candidate, field_token))
            .unwrap_or(relative_index)
            + token_offset;
        let Some(field_type) = fields.get(field_name) else {
            problems.push(PatternProblem {
                condition: "TYPE_UNKNOWN_PATTERN_FIELD",
                message: format!("unknown record pattern field `{field_name}`"),
                label: "use a field declared by this record payload",
                token_index: absolute_index,
            });
            continue;
        };
        if !seen.insert(field_name) {
            problems.push(PatternProblem {
                condition: "TYPE_DUPLICATE_PATTERN_FIELD",
                message: format!("record pattern field `{field_name}` appears more than once"),
                label: "mention each record field at most once",
                token_index: absolute_index,
            });
            continue;
        }
        let colon = pattern
            .iter()
            .position(|token| token.kind == TokenKind::Colon);
        let binding = colon
            .and_then(|colon| pattern.get(colon + 1))
            .filter(|token| token.kind == TokenKind::Identifier)
            .map_or(field_name, |token| &source[token.range.clone()]);
        if binding != "_" {
            bindings.insert(binding.to_owned(), field_type.clone());
        }
    }
}

struct ResolvedMember {
    id: MemberId,
    owner: DeclarationId,
    visibility: Visibility,
    ty: Type,
    callable: Option<CallableIdentity>,
}

fn readonly_field_owner(program: &Program, member: MemberId) -> Option<DeclarationId> {
    program.definitions.iter().find_map(|definition| {
        let (DefinitionData::Struct { fields, .. } | DefinitionData::Class { fields, .. }) =
            &definition.data
        else {
            return None;
        };
        fields
            .iter()
            .any(|field| field.id == member && field.readonly)
            .then_some(definition.declaration)
    })
}

#[allow(clippy::too_many_lines)]
fn resolve_member(program: &Program, owner: DeclarationId, name: &str) -> Option<ResolvedMember> {
    let definition = program.definition(owner)?;
    match &definition.data {
        DefinitionData::Struct {
            fields, methods, ..
        } => fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| ResolvedMember {
                id: field.id,
                owner,
                visibility: field.visibility,
                ty: field.ty.clone(),
                callable: None,
            })
            .or_else(|| {
                methods
                    .iter()
                    .find(|method| method.name == name)
                    .map(|method| ResolvedMember {
                        id: method.id,
                        owner,
                        visibility: method.visibility,
                        ty: function_type(&method.function),
                        callable: Some(CallableIdentity::Method(method.id)),
                    })
            })
            .or_else(|| resolve_inherent_method(program, owner, name)),
        DefinitionData::Class {
            base,
            fields,
            methods,
            ..
        } => fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| ResolvedMember {
                id: field.id,
                owner,
                visibility: field.visibility,
                ty: field.ty.clone(),
                callable: None,
            })
            .or_else(|| {
                methods
                    .iter()
                    .find(|method| method.name == name)
                    .map(|method| ResolvedMember {
                        id: method.id,
                        owner,
                        visibility: method.visibility,
                        ty: function_type(&method.function),
                        callable: Some(CallableIdentity::Method(method.id)),
                    })
            })
            .or_else(|| base.and_then(|base| resolve_member(program, base, name))),
        DefinitionData::Interface { methods, .. } => methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| ResolvedMember {
                id: method.id,
                owner,
                visibility: Visibility::Public,
                ty: function_type(&method.function),
                callable: Some(CallableIdentity::Method(method.id)),
            }),
        DefinitionData::Enum {
            variants, methods, ..
        } => methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| ResolvedMember {
                id: method.id,
                owner,
                visibility: method.visibility,
                ty: function_type(&method.function),
                callable: Some(CallableIdentity::Method(method.id)),
            })
            .or_else(|| {
                variants
                    .iter()
                    .find(|variant| variant.name == name)
                    .map(|variant| ResolvedMember {
                        id: variant.id,
                        owner,
                        visibility: Visibility::Public,
                        ty: if variant.fields.is_empty() {
                            Type::Nominal(owner, Vec::new())
                        } else {
                            Type::Function(tn_hir::FunctionType {
                                parameters: variant
                                    .fields
                                    .iter()
                                    .map(|field| field.ty.clone())
                                    .collect(),
                                result: Box::new(Type::Nominal(owner, Vec::new())),
                                effects: Vec::new(),
                                generics: definition
                                    .generics
                                    .iter()
                                    .map(|parameter| tn_hir::GenericConstraint {
                                        name: parameter.name.clone(),
                                        namespace: parameter.namespace,
                                        bounds: parameter.bounds.clone(),
                                    })
                                    .collect(),
                                is_async: false,
                                is_unsafe: false,
                            })
                        },
                        callable: (!variant.fields.is_empty())
                            .then_some(CallableIdentity::Method(variant.id)),
                    })
            })
            .or_else(|| resolve_inherent_method(program, owner, name)),
        _ => None,
    }
}

fn resolve_inherent_method(
    program: &Program,
    target: DeclarationId,
    name: &str,
) -> Option<ResolvedMember> {
    program.definitions.iter().find_map(|definition| {
        let DefinitionData::Implementation {
            target: Type::Nominal(candidate, _),
            methods,
            ..
        } = &definition.data
        else {
            return None;
        };
        if *candidate != target {
            return None;
        }
        methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| ResolvedMember {
                id: method.id,
                owner: definition.declaration,
                visibility: method.visibility,
                ty: function_type(&method.function),
                callable: Some(CallableIdentity::Method(method.id)),
            })
    })
}

fn resolved_method_receiver(program: &Program, member: MemberId) -> Option<ReceiverMode> {
    program
        .definitions
        .iter()
        .find_map(|definition| match &definition.data {
            DefinitionData::Class { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Interface { methods, .. }
            | DefinitionData::Implementation { methods, .. }
            | DefinitionData::Extern { functions: methods } => methods
                .iter()
                .find(|method| method.id == member)
                .map(|method| method.receiver),
            _ => None,
        })
}

fn specialize_nominal_member_type(program: &Program, owner: &Type, ty: &Type) -> Type {
    let (declaration, arguments) = match owner {
        Type::Nominal(declaration, arguments) | Type::DynamicInterface(declaration, arguments) => {
            (*declaration, arguments)
        }
        _ => return ty.clone(),
    };
    let Some(definition) = program.definition(declaration) else {
        return ty.clone();
    };
    let substitutions = definition
        .generics
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            let argument =
                arguments
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| match parameter.namespace {
                        tn_hir::Namespace::Lifetime => Type::Lifetime("scope".into()),
                        _ => Type::Error,
                    });
            (parameter.name.clone(), argument)
        })
        .collect::<BTreeMap<_, _>>();
    substitute_type(ty, &substitutions)
}

fn generic_identity_type(parameter: &tn_hir::GenericParameter) -> Type {
    match parameter.namespace {
        tn_hir::Namespace::Lifetime => Type::Lifetime(parameter.name.clone()),
        tn_hir::Namespace::Type => Type::Generic(parameter.name.clone()),
        _ => Type::Error,
    }
}

fn function_type(function: &Function) -> Type {
    Type::Function(tn_hir::FunctionType {
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
                bounds: parameter.bounds.clone(),
            })
            .collect(),
        is_async: function.is_async && !function.is_generator,
        is_unsafe: function.is_unsafe,
    })
}

fn primitive(name: &str) -> Option<Type> {
    Some(match name {
        "bool" => Type::Primitive(PrimitiveType::Bool),
        "i8" => Type::Primitive(PrimitiveType::I8),
        "i16" => Type::Primitive(PrimitiveType::I16),
        "i32" => Type::Primitive(PrimitiveType::I32),
        "i64" => Type::Primitive(PrimitiveType::I64),
        "i128" => Type::Primitive(PrimitiveType::I128),
        "isize" | "number" => Type::Primitive(PrimitiveType::Isize),
        "u8" => Type::Primitive(PrimitiveType::U8),
        "u16" => Type::Primitive(PrimitiveType::U16),
        "u32" => Type::Primitive(PrimitiveType::U32),
        "u64" => Type::Primitive(PrimitiveType::U64),
        "u128" => Type::Primitive(PrimitiveType::U128),
        "usize" => Type::Primitive(PrimitiveType::Usize),
        "f32" => Type::Primitive(PrimitiveType::F32),
        "f64" => Type::Primitive(PrimitiveType::F64),
        "char" => Type::Primitive(PrimitiveType::Char),
        "void" => Type::Primitive(PrimitiveType::Void),
        "never" => Type::Primitive(PrimitiveType::Never),
        "string" | "String" => Type::String,
        "str" => Type::Str,
        _ => return None,
    })
}

fn decode_quoted_literal(text: &str) -> Option<String> {
    let content = text.get(1..text.len().checked_sub(1)?)?;
    let mut decoded = String::new();
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => decoded.push('\\'),
            '"' => decoded.push('"'),
            '\'' => decoded.push('\''),
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            '0' => decoded.push('\0'),
            'x' => {
                let digits = [characters.next()?, characters.next()?];
                decoded.push(char::from(
                    u8::from_str_radix(&digits.iter().collect::<String>(), 16).ok()?,
                ));
            }
            'u' => {
                if characters.next()? != '{' {
                    return None;
                }
                let mut digits = String::new();
                for character in characters.by_ref() {
                    if character == '}' {
                        break;
                    }
                    digits.push(character);
                }
                decoded.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(decoded)
}
