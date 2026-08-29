use crate::{OwnershipFacts, derive_ownership_facts};
use std::collections::{BTreeMap, BTreeSet};
use tn_diagnostics::SourceSpan;
use tn_hir::{
    BodyHir, BodyOwner, DeclarationId, DefinitionData, Function, HirBindingPattern, HirCaptureMode,
    HirClosureId, HirJsxChild, HirJsxValue, HirLocalId, HirPatternBinding, HirPatternProjection,
    HirStatementKind, HirTemplateId, HirTemplatePart, HirTemplateStorage, ImportClause,
    IterationWitness, MemberId, Namespace, PrimitiveType, Program, ReceiverMode, ResolvedValue,
    Type,
};
use tn_mir::{
    BasicBlock, BasicBlockId, Body, BorrowKind, CastKind, Local, LocalId, Operand, Place, RegionId,
    Rvalue, Statement, StatementKind, TemplatePart, Terminator, TerminatorKind,
};
use tn_syntax::{Token, TokenKind, lex};

pub fn lower_mir(program: &Program, hir_bodies: &[BodyHir]) -> Vec<Body> {
    let ownership_facts = derive_ownership_facts(program);
    lower_mir_with_ownership(program, hir_bodies, &ownership_facts)
}

pub fn lower_mir_with_ownership(
    program: &Program,
    hir_bodies: &[BodyHir],
    ownership_facts: &OwnershipFacts,
) -> Vec<Body> {
    let mut bodies = Vec::new();
    for definition in &program.definitions {
        match &definition.data {
            DefinitionData::Function(function) => {
                lower_one(
                    program,
                    hir_bodies,
                    definition.declaration,
                    None,
                    function,
                    ownership_facts,
                    &mut bodies,
                );
            }
            DefinitionData::Class {
                constructor,
                methods,
                ..
            } => {
                if let Some(constructor) = constructor {
                    lower_one(
                        program,
                        hir_bodies,
                        definition.declaration,
                        Some(constructor.id),
                        &constructor.function,
                        ownership_facts,
                        &mut bodies,
                    );
                }
                for method in methods {
                    lower_one(
                        program,
                        hir_bodies,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        ownership_facts,
                        &mut bodies,
                    );
                }
            }
            DefinitionData::Struct { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Implementation { methods, .. } => {
                for method in methods {
                    lower_one(
                        program,
                        hir_bodies,
                        definition.declaration,
                        Some(method.id),
                        &method.function,
                        ownership_facts,
                        &mut bodies,
                    );
                }
            }
            _ => {}
        }
    }
    bodies.sort_by_key(|body| (body.declaration, body.member));
    bodies
}

#[allow(clippy::too_many_lines)]
fn lower_one(
    program: &Program,
    hir_bodies: &[BodyHir],
    declaration: DeclarationId,
    member: Option<MemberId>,
    function: &Function,
    ownership_facts: &OwnershipFacts,
    bodies: &mut Vec<Body>,
) {
    if function.body_start == 0 || function.body_end <= function.body_start {
        return;
    }
    let Some(item) = program.graph.declaration(declaration) else {
        return;
    };
    let Some(module) = program.graph.module(item.module) else {
        return;
    };
    let owner = member.map_or(BodyOwner::Declaration(declaration), |member| {
        BodyOwner::Member {
            declaration,
            member,
        }
    });
    let Some(hir) = hir_bodies.iter().find(|body| body.owner == owner) else {
        return;
    };
    let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
    let all_tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia() && token.range.end <= function.body_end as usize)
        .collect::<Vec<_>>();
    let body_start = all_tokens
        .iter()
        .position(|token| token.range.start > function.body_start as usize)
        .unwrap_or(all_tokens.len());
    let body_limit = all_tokens
        .iter()
        .position(|token| token.range.end >= function.body_end as usize)
        .unwrap_or(all_tokens.len());
    let generic_call_ends = generic_call_ends(&all_tokens);
    let function_result = specialize_owner_result(program, declaration, &function.result);
    let return_type = if function.is_async {
        match &function_result {
            Type::Promise { result, .. } => result.as_ref().clone(),
            result => result.clone(),
        }
    } else {
        function_result.clone()
    };
    let mut generics = function
        .generics
        .iter()
        .map(|parameter| (parameter.name.clone(), parameter.namespace))
        .collect::<BTreeMap<_, _>>();
    if let Some(definition) = program.definition(declaration) {
        generics.extend(
            definition
                .generics
                .iter()
                .map(|parameter| (parameter.name.clone(), parameter.namespace)),
        );
    }
    let mut lowerer = OwnershipMirLowerer {
        program,
        module,
        hir,
        tokens: all_tokens,
        body_limit,
        generic_call_ends,
        index: body_start,
        locals: Vec::new(),
        temporary_locals: BTreeSet::new(),
        names: BTreeMap::new(),
        hir_local_ids: BTreeMap::new(),
        capture_references: BTreeSet::new(),
        bound_receivers: BTreeMap::new(),
        blocks: vec![OpenBlock::default()],
        current: 0,
        next_region: 0,
        return_type,
        declared_effects: function.effects.clone(),
        generics,
        loop_targets: Vec::new(),
        error_contexts: Vec::new(),
        async_managed_scopes: vec![Vec::new()],
        disposing_async: false,
        ownership_facts: ownership_facts.clone(),
        generator_item_type: function
            .is_generator
            .then(|| generator_item_type(program, &function_result, function.is_async))
            .flatten(),
        generator_async: function.is_async,
        generator_buffer: None,
        generator_finish_block: None,
    };
    let mut argument_ids = hir.parameter_roots.clone();
    if member.is_some()
        && let Some(this) = hir.locals.iter().find(|local| local.name == "this")
    {
        argument_ids.insert(0, this.id);
    }
    for id in argument_ids {
        let Some(parameter) = hir.locals.get(id.0 as usize) else {
            continue;
        };
        let local = lowerer.add_local(
            parameter.name.clone(),
            parameter.ty.clone(),
            parameter.mutable,
            true,
            parameter.origin.clone(),
        );
        lowerer.hir_local_ids.insert(parameter.id, local);
    }
    let binding_locals = hir
        .binding_patterns
        .iter()
        .flat_map(|pattern| pattern.bindings.iter().map(|binding| binding.local))
        .collect::<BTreeSet<_>>();
    for hir_local in hir
        .locals
        .iter()
        .filter(|local| binding_locals.contains(&local.id))
    {
        if lowerer.hir_local_ids.contains_key(&hir_local.id) {
            continue;
        }
        let local = lowerer.add_local(
            hir_local.name.clone(),
            hir_local.ty.clone(),
            hir_local.mutable,
            false,
            hir_local.origin.clone(),
        );
        lowerer.hir_local_ids.insert(hir_local.id, local);
    }
    lowerer.lower();
    bodies.push(lowerer.finish(declaration, member, function.effects.clone()));
}

fn specialize_owner_result(program: &Program, declaration: DeclarationId, result: &Type) -> Type {
    let Some(definition) = program.definition(declaration) else {
        return result.clone();
    };
    let substitutions = definition
        .generics
        .iter()
        .filter(|parameter| parameter.namespace == Namespace::Type)
        .map(|parameter| {
            (
                parameter.name.clone(),
                Type::Generic(parameter.name.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let result = substitute_mir_type(result, &substitutions);
    if let Type::Nominal(owner, arguments) = &result
        && *owner == declaration
        && arguments.is_empty()
    {
        let arguments = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace == Namespace::Type)
            .map(|parameter| Type::Generic(parameter.name.clone()))
            .collect();
        return Type::Nominal(*owner, arguments);
    }
    result
}

fn generator_item_type(program: &Program, result: &Type, asynchronous: bool) -> Option<Type> {
    let expected = if asynchronous {
        "AsyncIterable"
    } else {
        "Iterable"
    };
    let (Type::Nominal(declaration, arguments) | Type::DynamicInterface(declaration, arguments)) =
        result
    else {
        return None;
    };
    (program
        .graph
        .declaration(*declaration)
        .and_then(|declaration| declaration.name.as_deref())
        == Some(expected))
    .then(|| arguments.first().cloned())
    .flatten()
}

fn task_promise_type(program: &Program, ty: &Type) -> Option<Type> {
    let Type::Nominal(declaration, arguments) = ty else {
        return None;
    };
    let item = program.graph.declaration(*declaration)?;
    let module = program.graph.module(item.module)?;
    (item.name.as_deref() == Some("Task")
        && module.path.ends_with("std/async.tn")
        && arguments.len() == 2)
        .then(|| Type::Promise {
            result: Box::new(arguments[0].clone()),
            error: Box::new(arguments[1].clone()),
            effects: tn_hir::promise_effects(&arguments[1], &[]),
        })
}

fn promise_effects_for_type(program: &Program, ty: &Type) -> Vec<DeclarationId> {
    if let Type::Promise { effects, error, .. } = ty {
        return tn_hir::promise_effects(error, effects);
    }
    let Type::Nominal(declaration, arguments) = ty else {
        return Vec::new();
    };
    let Some(item) = program.graph.declaration(*declaration) else {
        return Vec::new();
    };
    let Some(module) = program.graph.module(item.module) else {
        return Vec::new();
    };
    if item.name.as_deref() != Some("Task")
        || !module.path.ends_with("std/async.tn")
        || arguments.len() != 2
    {
        return Vec::new();
    }
    tn_hir::promise_effects(&arguments[1], &[])
}

fn mir_nominal_id(ty: &Type) -> Option<DeclarationId> {
    match ty {
        Type::Nominal(declaration, _) | Type::DynamicInterface(declaration, _) => {
            Some(*declaration)
        }
        _ => None,
    }
}

#[derive(Default)]
struct OpenBlock {
    statements: Vec<Statement>,
    terminator: Option<Terminator>,
}

struct OwnershipMirLowerer<'a> {
    program: &'a Program,
    module: &'a tn_hir::Module,
    hir: &'a BodyHir,
    tokens: Vec<&'a Token>,
    body_limit: usize,
    generic_call_ends: BTreeMap<usize, usize>,
    index: usize,
    locals: Vec<Local>,
    temporary_locals: BTreeSet<LocalId>,
    names: BTreeMap<String, LocalId>,
    hir_local_ids: BTreeMap<HirLocalId, LocalId>,
    capture_references: BTreeSet<LocalId>,
    bound_receivers: BTreeMap<LocalId, Operand>,
    blocks: Vec<OpenBlock>,
    current: usize,
    next_region: u32,
    return_type: Type,
    declared_effects: Vec<DeclarationId>,
    generics: BTreeMap<String, Namespace>,
    loop_targets: Vec<(BasicBlockId, BasicBlockId, usize)>,
    error_contexts: Vec<ErrorContext>,
    async_managed_scopes: Vec<Vec<(LocalId, usize)>>,
    disposing_async: bool,
    ownership_facts: OwnershipFacts,
    generator_item_type: Option<Type>,
    generator_async: bool,
    generator_buffer: Option<LocalId>,
    generator_finish_block: Option<usize>,
}

impl OwnershipMirLowerer<'_> {
    fn lower(&mut self) {
        if self.generator_item_type.is_some() {
            self.start_generator();
        }
        self.lower_parameter_bindings();
        self.lower_sequence(None);
        if self.blocks[self.current].terminator.is_none() {
            let managed = self.active_async_disposals();
            self.lower_async_disposals(&managed);
        }
        if let Some(finish) = self.generator_finish_block {
            if self.current != finish && self.blocks[self.current].terminator.is_none() {
                self.terminate(
                    TerminatorKind::Goto(Self::block_id(finish)),
                    self.locals.first().map_or_else(
                        || SourceSpan::new("<generator>", 0..0, ""),
                        |local| local.span.clone(),
                    ),
                );
            }
            self.current = finish;
            self.finish_generator();
        }
    }

    fn lower_parameter_bindings(&mut self) {
        let patterns = self.hir.binding_patterns.clone();
        for pattern in &patterns {
            if self.hir_local_ids.contains_key(&pattern.root) {
                self.lower_binding_pattern(pattern);
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_binding_pattern(&mut self, pattern: &HirBindingPattern) {
        let Some(root) = self.hir_local_ids.get(&pattern.root).copied() else {
            return;
        };
        let root_type = self
            .hir
            .locals
            .get(pattern.root.0 as usize)
            .map_or(Type::Error, |local| local.ty.clone());
        for binding in &pattern.bindings {
            let destination = if let Some(destination) = self.hir_local_ids.get(&binding.local) {
                *destination
            } else {
                let Some(hir_local) = self.hir.locals.get(binding.local.0 as usize).cloned() else {
                    continue;
                };
                let destination = self.add_local(
                    hir_local.name,
                    hir_local.ty,
                    hir_local.mutable,
                    false,
                    hir_local.origin,
                );
                self.hir_local_ids.insert(binding.local, destination);
                destination
            };
            self.statement(
                StatementKind::StorageLive(destination),
                self.span_from_hir_local(binding.local),
            );
            let mut source = Place::local(root);
            let mut source_type = root_type.clone();
            let mut rest = false;
            let mut rest_start = 0_u32;
            for projection in &binding.projection {
                match projection {
                    HirPatternProjection::Field(index) => {
                        let field_type = self
                            .binding_projection_type(&source_type, projection)
                            .unwrap_or_else(|| binding.ty.clone());
                        source.projection.push(tn_mir::Projection::Field {
                            index: *index,
                            ty: field_type.clone(),
                        });
                        source_type = field_type;
                    }
                    HirPatternProjection::Index(index) => {
                        let element_type = self
                            .binding_projection_type(&source_type, projection)
                            .unwrap_or_else(|| binding.ty.clone());
                        if matches!(source_type, Type::Tuple(_)) {
                            source.projection.push(tn_mir::Projection::Field {
                                index: *index,
                                ty: element_type.clone(),
                            });
                        } else {
                            let index_local = self.temporary(
                                Type::Primitive(PrimitiveType::Usize),
                                self.span_from_hir_local(binding.local),
                            );
                            self.statement(
                                StatementKind::StorageLive(index_local),
                                self.span_from_hir_local(binding.local),
                            );
                            self.statement(
                                StatementKind::Assign(
                                    Place::local(index_local),
                                    Box::new(Rvalue::Use(Operand::Constant(
                                        tn_mir::Constant::Integer {
                                            value: i128::from(*index),
                                            ty: Type::Primitive(PrimitiveType::Usize),
                                        },
                                    ))),
                                ),
                                self.span_from_hir_local(binding.local),
                            );
                            source
                                .projection
                                .push(tn_mir::Projection::Index(index_local));
                        }
                        source_type = element_type;
                    }
                    HirPatternProjection::Rest { start } => {
                        rest = true;
                        rest_start = *start;
                        source_type = self
                            .binding_projection_type(&source_type, projection)
                            .unwrap_or(Type::Unknown);
                    }
                    HirPatternProjection::Variant(_) | HirPatternProjection::OptionalPayload => {}
                }
            }
            if let Type::Optional(inner) = &source_type
                && binding.default.is_some()
            {
                self.lower_binding_default(
                    destination,
                    source,
                    inner,
                    binding.default.as_ref(),
                    binding.local,
                );
                continue;
            }
            let rvalue = if rest {
                Rvalue::RawOperation {
                    operation: "binding_rest".into(),
                    operands: vec![
                        Operand::Copy(source),
                        Operand::Constant(tn_mir::Constant::Integer {
                            value: i128::from(rest_start),
                            ty: Type::Primitive(PrimitiveType::Usize),
                        }),
                    ],
                    ty: binding.ty.clone(),
                }
            } else {
                Rvalue::Use(Operand::Move(source))
            };
            self.statement(
                StatementKind::Assign(Place::local(destination), Box::new(rvalue)),
                self.span_from_hir_local(binding.local),
            );
        }
    }

    fn lower_binding_default(
        &mut self,
        destination: LocalId,
        source: Place,
        value_type: &Type,
        default: Option<&SourceSpan>,
        binding: HirLocalId,
    ) {
        let value_type = value_type.clone();
        let span = self.span_from_hir_local(binding);
        let present = self.new_block();
        let fallback = self.new_block();
        let join = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(source.clone()),
                targets: vec![(1, Self::block_id(present))],
                otherwise: Self::block_id(fallback),
            },
            span.clone(),
        );

        self.current = present;
        let mut payload = source;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Use(Operand::Move(payload))),
            ),
            span.clone(),
        );
        self.terminate(TerminatorKind::Goto(Self::block_id(join)), span.clone());

        self.current = fallback;
        let saved_index = self.index;
        let fallback = default
            .and_then(|default| {
                let (start, end) = self.token_range_for_bytes(
                    default.byte_start as usize,
                    default.byte_end as usize,
                )?;
                self.lower_expression_range(start, end, Some(&value_type))
            })
            .unwrap_or_else(|| {
                (
                    Operand::Constant(tn_mir::Constant::Undefined(value_type.clone())),
                    value_type.clone(),
                )
            });
        self.index = saved_index;
        let value = if fallback.1 == value_type {
            Rvalue::Use(fallback.0)
        } else {
            Rvalue::Cast {
                operand: fallback.0,
                ty: value_type.clone(),
                kind: mir_cast_kind(&fallback.1, &value_type),
            }
        };
        self.statement(
            StatementKind::Assign(Place::local(destination), Box::new(value)),
            span.clone(),
        );
        self.terminate(TerminatorKind::Goto(Self::block_id(join)), span);
        self.current = join;
    }

    fn binding_projection_type(
        &self,
        ty: &Type,
        projection: &HirPatternProjection,
    ) -> Option<Type> {
        match projection {
            HirPatternProjection::Field(index) => self
                .aggregate_schema(ty)
                .get(*index as usize)
                .map(|(_, ty)| ty.clone()),
            HirPatternProjection::Index(index) => match ty {
                Type::Tuple(elements) => elements.get(*index as usize).cloned(),
                Type::Array(element, _) | Type::Slice(element) => Some(element.as_ref().clone()),
                _ => None,
            },
            HirPatternProjection::Rest { .. } => match ty {
                Type::Array(element, _) | Type::Slice(element) => {
                    Some(Type::Slice(element.clone()))
                }
                _ => None,
            },
            HirPatternProjection::Variant(_) | HirPatternProjection::OptionalPayload => None,
        }
    }

    fn start_generator(&mut self) {
        let Some(item_type) = self.generator_item_type.clone() else {
            return;
        };
        let Some(token) = self.tokens.first().copied() else {
            return;
        };
        let Some(array) = self.nominal_named("Array") else {
            return;
        };
        let Some(options) = self.nominal_named("ArrayOptions") else {
            return;
        };
        let array_type = Type::Nominal(array, vec![item_type.clone()]);
        let options_type = Type::Nominal(options, Vec::new());
        let buffer = self.add_local(
            "$generator_buffer".into(),
            array_type.clone(),
            true,
            false,
            self.span(token),
        );
        self.generator_buffer = Some(buffer);
        self.statement(StatementKind::StorageLive(buffer), self.span(token));
        let options_local = self.temporary(options_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(options_local), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(options_local),
                Box::new(Rvalue::Aggregate {
                    ty: options_type.clone(),
                    variant: None,
                    fields: vec![Operand::Constant(tn_mir::Constant::Integer {
                        value: 0,
                        ty: Type::Primitive(PrimitiveType::Usize),
                    })],
                    field_types: vec![Type::Primitive(PrimitiveType::Usize)],
                }),
            ),
            self.span(token),
        );
        let Some(DefinitionData::Class { constructor, .. }) = self
            .program
            .definition(array)
            .map(|definition| &definition.data)
        else {
            return;
        };
        let Some(constructor) = constructor else {
            return;
        };
        let signature = tn_hir::FunctionType {
            parameters: vec![options_type],
            result: Box::new(array_type.clone()),
            effects: constructor.function.effects.clone(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: constructor.function.is_unsafe,
        };
        let function = Operand::Constant(tn_mir::Constant::Constructor {
            owner: array,
            member: Some(constructor.id),
            ty: Type::Function(signature.clone()),
        });
        if let Some((value, _)) = self.emit_call(
            function,
            None,
            &signature,
            vec![Operand::Move(Place::local(options_local))],
            0,
        ) {
            self.statement(
                StatementKind::Assign(Place::local(buffer), Box::new(Rvalue::Use(value))),
                self.span(token),
            );
        }
        self.generator_finish_block = Some(self.new_block());
    }

    fn finish_generator(&mut self) {
        let Some(item_type) = self.generator_item_type.clone() else {
            return;
        };
        let Some(token) = self.tokens.first().copied() else {
            self.terminate(
                TerminatorKind::Unreachable,
                SourceSpan::new("<generator>", 0..0, ""),
            );
            return;
        };
        let Some(buffer) = self.generator_buffer else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
            return;
        };
        let iterator_name = if self.generator_async {
            "AsyncArrayIterator"
        } else {
            "ArrayIterator"
        };
        let Some(iterator) = self.nominal_named(iterator_name) else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
            return;
        };
        let iterator_type = Type::Nominal(iterator, vec![item_type.clone()]);
        let Some(DefinitionData::Class { constructor, .. }) = self
            .program
            .definition(iterator)
            .map(|definition| &definition.data)
        else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
            return;
        };
        let Some(constructor) = constructor else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
            return;
        };
        let array_type = self.locals[buffer.0 as usize].ty.clone();
        let signature = tn_hir::FunctionType {
            parameters: vec![array_type],
            result: Box::new(iterator_type.clone()),
            effects: constructor.function.effects.clone(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: constructor.function.is_unsafe,
        };
        let function = Operand::Constant(tn_mir::Constant::Constructor {
            owner: iterator,
            member: Some(constructor.id),
            ty: Type::Function(signature.clone()),
        });
        let Some((iterator_value, _)) = self.emit_call(
            function,
            None,
            &signature,
            vec![Operand::Move(Place::local(buffer))],
            0,
        ) else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
            return;
        };
        let result = self.temporary(self.return_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(result), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(result),
                Box::new(Rvalue::Cast {
                    operand: iterator_value,
                    ty: self.return_type.clone(),
                    kind: CastKind::InterfaceCoercion,
                }),
            ),
            self.span(token),
        );
        self.terminate(
            TerminatorKind::Return(Some(Operand::Move(Place::local(result)))),
            self.span(token),
        );
    }

    fn nominal_named(&self, name: &str) -> Option<DeclarationId> {
        self.program
            .graph
            .modules
            .iter()
            .flat_map(|module| &module.declarations)
            .find_map(|declaration| {
                (matches!(
                    declaration.kind,
                    tn_hir::DeclarationKind::Class | tn_hir::DeclarationKind::Struct
                ) && declaration.name.as_deref() == Some(name))
                .then_some(declaration.id)
            })
    }

    fn lower_sequence(&mut self, end: Option<TokenKind>) {
        while self.kind().is_some() && self.kind() != end {
            if self.blocks[self.current].terminator.is_some() {
                self.current = self.new_block();
            }
            self.lower_statement();
        }
    }

    fn lower_statement(&mut self) {
        match self.kind() {
            Some(TokenKind::LeftBrace) => {
                let saved_names = self.names.clone();
                let first_local = self.locals.len();
                self.async_managed_scopes.push(Vec::new());
                self.index += 1;
                self.lower_sequence(Some(TokenKind::RightBrace));
                let managed = self.async_managed_scopes.pop().unwrap_or_default();
                if self.blocks[self.current].terminator.is_none() {
                    self.lower_async_disposals(&managed);
                }
                for index in (first_local..self.locals.len()).rev() {
                    let local = LocalId(u32::try_from(index).expect("MIR local limit"));
                    self.statement(
                        StatementKind::StorageDead(local),
                        self.locals[index].span.clone(),
                    );
                }
                self.names = saved_names;
                self.index += usize::from(self.kind() == Some(TokenKind::RightBrace));
            }
            Some(TokenKind::Const | TokenKind::Let) => self.lower_local(),
            Some(TokenKind::If) => self.lower_if(),
            Some(TokenKind::While) => self.lower_while(),
            Some(TokenKind::For) => self.lower_for(),
            Some(TokenKind::Yield) => self.lower_yield(),
            Some(
                TokenKind::Switch
                | TokenKind::Identifier
                | TokenKind::This
                | TokenKind::Star
                | TokenKind::Less,
            ) => {
                self.lower_expression_statement();
            }
            Some(TokenKind::Try)
                if self
                    .tokens
                    .get(self.index + 1)
                    .is_some_and(|token| token.kind == TokenKind::LeftBrace) =>
            {
                self.lower_try();
            }
            Some(TokenKind::Try)
                if self
                    .tokens
                    .get(self.index + 1)
                    .is_some_and(|token| token.kind == TokenKind::Await) =>
            {
                self.lower_suspend();
            }
            Some(TokenKind::Unsafe) => {
                self.index += 1;
                self.lower_statement();
            }
            Some(TokenKind::Move) => self.lower_explicit_move(),
            Some(TokenKind::Await)
                if self
                    .tokens
                    .get(self.index + 1)
                    .is_some_and(|token| token.kind == TokenKind::Using) =>
            {
                self.lower_using(true);
            }
            Some(TokenKind::Await) => self.lower_suspend(),
            Some(TokenKind::Return) => self.lower_return(),
            Some(TokenKind::Throw) => self.lower_throw(),
            Some(TokenKind::Break | TokenKind::Continue) => self.lower_loop_control(),
            Some(TokenKind::Using) => self.lower_using(false),
            Some(_) => self.index += 1,
            None => {}
        }
    }

    fn lower_yield(&mut self) {
        let token = self.tokens[self.index];
        let end = self.statement_end(self.index);
        let Some(item_type) = self.generator_item_type.clone() else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let Some(buffer) = self.generator_buffer else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let Some((value, _value_type)) =
            self.lower_expression_range(self.index + 1, end, Some(&item_type))
        else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let Type::Nominal(array, arguments) = self.locals[buffer.0 as usize].ty.clone() else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let Some(DefinitionData::Class { methods, .. }) = self
            .program
            .definition(array)
            .map(|definition| &definition.data)
        else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let Some(push) = methods.iter().find(|method| method.name == "push") else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let signature = tn_hir::FunctionType {
            parameters: vec![arguments.first().cloned().unwrap_or(item_type)],
            result: Box::new(Type::Primitive(PrimitiveType::Void)),
            effects: push.function.effects.clone(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: push.function.is_unsafe,
        };
        let function_type = Type::Function(signature.clone());
        let method = self.temporary(function_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(method), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(method),
                Box::new(Rvalue::DirectMethod {
                    object: Place::local(buffer),
                    implementation: array,
                    member: push.id,
                    receiver: ReceiverMode::Mutable,
                    ty: function_type,
                }),
            ),
            self.span(token),
        );
        let _ = self.emit_call(
            Operand::Move(Place::local(method)),
            Some(Operand::Copy(Place::local(buffer))),
            &signature,
            vec![value],
            self.index,
        );
        self.index = end + usize::from(end < self.tokens.len());
    }

    fn lower_if(&mut self) {
        let token = self.tokens[self.index];
        let condition_start = self.index + 1;
        let Some(condition_end) =
            self.matching_token(condition_start, TokenKind::LeftParen, TokenKind::RightParen)
        else {
            self.index += 1;
            return;
        };
        let condition = self
            .lower_expression_range(
                condition_start + 1,
                condition_end,
                Some(&Type::Primitive(PrimitiveType::Bool)),
            )
            .map_or_else(
                || Operand::Constant(tn_mir::Constant::Bool(false)),
                |(operand, _)| operand,
            );
        self.index = condition_end + 1;
        let then_block = self.new_block();
        let else_block = self.new_block();
        let join_block = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: condition,
                targets: vec![(1, Self::block_id(then_block))],
                otherwise: Self::block_id(else_block),
            },
            self.span(token),
        );
        self.current = then_block;
        self.lower_statement();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(join_block)),
                self.span(token),
            );
        }
        self.current = else_block;
        if self.kind() == Some(TokenKind::Else) {
            self.index += 1;
            self.lower_statement();
        }
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(join_block)),
                self.span(token),
            );
        }
        self.current = join_block;
    }

    fn lower_while(&mut self) {
        let token = self.tokens[self.index];
        let condition_start = self.index + 1;
        let Some(condition_end) =
            self.matching_token(condition_start, TokenKind::LeftParen, TokenKind::RightParen)
        else {
            self.index += 1;
            return;
        };
        let condition_block = self.new_block();
        let body_block = self.new_block();
        let exit_block = self.new_block();
        self.terminate(
            TerminatorKind::Goto(Self::block_id(condition_block)),
            self.span(token),
        );
        self.current = condition_block;
        let condition = self
            .lower_expression_range(
                condition_start + 1,
                condition_end,
                Some(&Type::Primitive(PrimitiveType::Bool)),
            )
            .map_or_else(
                || Operand::Constant(tn_mir::Constant::Bool(false)),
                |(operand, _)| operand,
            );
        self.terminate(
            TerminatorKind::Switch {
                value: condition,
                targets: vec![(1, Self::block_id(body_block))],
                otherwise: Self::block_id(exit_block),
            },
            self.span(token),
        );
        self.index = condition_end + 1;
        self.current = body_block;
        self.loop_targets.push((
            Self::block_id(condition_block),
            Self::block_id(exit_block),
            self.async_managed_scopes.len(),
        ));
        self.lower_statement();
        self.loop_targets.pop();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(condition_block)),
                self.span(token),
            );
        }
        self.current = exit_block;
    }

    #[allow(clippy::too_many_lines)]
    fn lower_for(&mut self) {
        let token = self.tokens[self.index];
        let left_paren = self.index
            + 1
            + usize::from(
                self.tokens
                    .get(self.index + 1)
                    .is_some_and(|token| token.kind == TokenKind::Await),
            );
        let Some(header_end) =
            self.matching_token(left_paren, TokenKind::LeftParen, TokenKind::RightParen)
        else {
            self.index += 1;
            return;
        };
        let binding_index = left_paren + 2;
        let Some(binding_token) = self.tokens.get(binding_index).copied() else {
            self.index = header_end + 1;
            return;
        };
        let binding_pattern = self.for_binding_pattern(token);
        let Some(of_index) = self.find_top_level(binding_index, header_end, TokenKind::Of) else {
            self.index = header_end + 1;
            return;
        };
        let iterable_start = of_index + 1;
        let Some((iterable, iterable_type)) =
            self.lower_expression_range(iterable_start, header_end, None)
        else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        if let Some(witness) = self.iteration_witness(token).cloned() {
            self.lower_witness_for(
                token,
                binding_token,
                header_end,
                iterable_start,
                iterable,
                iterable_type,
                &witness,
            );
            return;
        }
        let Some(mut iterable) = operand_place(iterable) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let Some(item_type) = builtin_iterable_item(&iterable_type) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        if matches!(iterable_type, Type::Reference { .. }) {
            iterable.projection.push(tn_mir::Projection::Dereference);
        }
        let binding = self.add_local(
            binding_pattern
                .as_ref()
                .and_then(|pattern| self.hir.locals.get(pattern.root.0 as usize))
                .map_or_else(
                    || self.text(binding_token).to_owned(),
                    |local| local.name.clone(),
                ),
            item_type.clone(),
            false,
            false,
            self.span(binding_token),
        );
        self.prepare_for_binding(binding_pattern.as_ref(), binding_token, binding);
        let usize_type = Type::Primitive(PrimitiveType::Usize);
        let index = self.temporary(usize_type.clone(), self.span(token));
        let length = self.temporary(usize_type.clone(), self.span(token));
        for local in [index, length] {
            self.statement(StatementKind::StorageLive(local), self.span(token));
        }
        self.statement(
            StatementKind::Assign(
                Place::local(index),
                Box::new(Rvalue::Use(Operand::Constant(tn_mir::Constant::Integer {
                    value: 0,
                    ty: usize_type.clone(),
                }))),
            ),
            self.span(token),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(length),
                Box::new(Rvalue::Length(iterable.clone())),
            ),
            self.span(token),
        );
        let condition_block = self.new_block();
        let body_block = self.new_block();
        let increment_block = self.new_block();
        let exit_block = self.new_block();
        self.terminate(
            TerminatorKind::Goto(Self::block_id(condition_block)),
            self.span(token),
        );
        self.current = condition_block;
        let condition = self.temporary(Type::Primitive(PrimitiveType::Bool), self.span(token));
        self.statement(StatementKind::StorageLive(condition), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(condition),
                Box::new(Rvalue::CheckedBinary {
                    operator: tn_mir::BinaryOperator::Less,
                    left: Operand::Copy(Place::local(index)),
                    right: Operand::Copy(Place::local(length)),
                    operand_type: usize_type.clone(),
                    result_type: Type::Primitive(PrimitiveType::Bool),
                }),
            ),
            self.span(token),
        );
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Move(Place::local(condition)),
                targets: vec![(1, Self::block_id(body_block))],
                otherwise: Self::block_id(exit_block),
            },
            self.span(token),
        );
        self.current = body_block;
        self.statement(
            StatementKind::StorageLive(binding),
            self.span(binding_token),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(binding),
                Box::new(Rvalue::CheckedIndex {
                    collection: iterable,
                    index: Operand::Copy(Place::local(index)),
                }),
            ),
            self.span(binding_token),
        );
        if let Some(pattern) = &binding_pattern {
            self.lower_binding_pattern(pattern);
        }
        self.index = header_end + 1;
        self.loop_targets.push((
            Self::block_id(increment_block),
            Self::block_id(exit_block),
            self.async_managed_scopes.len(),
        ));
        self.lower_statement();
        self.loop_targets.pop();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(increment_block)),
                self.span(token),
            );
        }
        self.current = increment_block;
        self.statement(
            StatementKind::Assign(
                Place::local(index),
                Box::new(Rvalue::CheckedBinary {
                    operator: tn_mir::BinaryOperator::Add,
                    left: Operand::Copy(Place::local(index)),
                    right: Operand::Constant(tn_mir::Constant::Integer {
                        value: 1,
                        ty: usize_type.clone(),
                    }),
                    operand_type: usize_type.clone(),
                    result_type: usize_type,
                }),
            ),
            self.span(token),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(condition_block)),
            self.span(token),
        );
        self.current = exit_block;
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn lower_witness_for(
        &mut self,
        token: &Token,
        binding_token: &Token,
        header_end: usize,
        iterable_start: usize,
        iterable: Operand,
        iterable_type: Type,
        witness: &IterationWitness,
    ) {
        if let IterationWitness::Generator {
            item_type,
            asynchronous,
        } = witness
        {
            self.lower_generator_for(
                token,
                binding_token,
                header_end,
                iterable_start,
                iterable,
                iterable_type,
                item_type,
                *asynchronous,
            );
            return;
        }
        let IterationWitness::Declared {
            into_iterator_implementation,
            into_iterator_method,
            iterator_implementation,
            next_method: next_member,
            next_receiver,
            iterator_type,
            item_type,
        } = witness
        else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let iterable = self.materialize_operand(iterable, iterable_type, token);
        let into_signature = tn_hir::FunctionType {
            parameters: Vec::new(),
            result: Box::new(iterator_type.clone()),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        };
        let into_type = Type::Function(into_signature.clone());
        let into_method = self.temporary(into_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(into_method), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(into_method),
                Box::new(Rvalue::DirectMethod {
                    object: iterable.clone(),
                    implementation: *into_iterator_implementation,
                    member: *into_iterator_method,
                    receiver: ReceiverMode::Move,
                    ty: into_type,
                }),
            ),
            self.span(token),
        );
        let Some((iterator, _)) = self.emit_call(
            Operand::Move(Place::local(into_method)),
            Some(Operand::Move(iterable)),
            &into_signature,
            Vec::new(),
            iterable_start,
        ) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let iterator = self.materialize_operand(iterator, iterator_type.clone(), token);
        let binding = self.add_local(
            self.for_binding_pattern(token)
                .as_ref()
                .and_then(|pattern| self.hir.locals.get(pattern.root.0 as usize))
                .map_or_else(
                    || self.text(binding_token).to_owned(),
                    |local| local.name.clone(),
                ),
            item_type.clone(),
            false,
            false,
            self.span(binding_token),
        );
        let binding_pattern = self.for_binding_pattern(token);
        self.prepare_for_binding(binding_pattern.as_ref(), binding_token, binding);

        let condition_block = self.new_block();
        let body_block = self.new_block();
        let exit_block = self.new_block();
        self.terminate(
            TerminatorKind::Goto(Self::block_id(condition_block)),
            self.span(token),
        );
        self.current = condition_block;
        let optional_item = Type::Optional(Box::new(item_type.clone()));
        let next_signature = tn_hir::FunctionType {
            parameters: Vec::new(),
            result: Box::new(optional_item.clone()),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        };
        let next_type = Type::Function(next_signature.clone());
        let next_method = self.temporary(next_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(next_method), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(next_method),
                Box::new(Rvalue::DirectMethod {
                    object: iterator.clone(),
                    implementation: *iterator_implementation,
                    member: *next_member,
                    receiver: *next_receiver,
                    ty: next_type,
                }),
            ),
            self.span(token),
        );
        let Some((next, _)) = self.emit_call(
            Operand::Move(Place::local(next_method)),
            Some(Operand::Copy(iterator)),
            &next_signature,
            Vec::new(),
            iterable_start,
        ) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let next = self.materialize_operand(next, optional_item, token);
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(next.clone()),
                targets: vec![(1, Self::block_id(body_block))],
                otherwise: Self::block_id(exit_block),
            },
            self.span(token),
        );

        self.current = body_block;
        self.statement(
            StatementKind::StorageLive(binding),
            self.span(binding_token),
        );
        let mut payload = next;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        self.statement(
            StatementKind::Assign(
                Place::local(binding),
                Box::new(Rvalue::Use(Operand::Move(payload))),
            ),
            self.span(binding_token),
        );
        if let Some(pattern) = &binding_pattern {
            self.lower_binding_pattern(pattern);
        }
        self.index = header_end + 1;
        self.loop_targets.push((
            Self::block_id(condition_block),
            Self::block_id(exit_block),
            self.async_managed_scopes.len(),
        ));
        self.lower_statement();
        self.loop_targets.pop();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(condition_block)),
                self.span(token),
            );
        }
        self.current = exit_block;
    }

    #[allow(
        clippy::needless_pass_by_value,
        clippy::too_many_arguments,
        clippy::too_many_lines
    )]
    fn lower_generator_for(
        &mut self,
        token: &Token,
        binding_token: &Token,
        header_end: usize,
        iterable_start: usize,
        iterable: Operand,
        iterable_type: Type,
        item_type: &Type,
        asynchronous: bool,
    ) {
        let Type::DynamicInterface(interface, _) = iterable_type.clone() else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let Some(DefinitionData::Interface { methods, .. }) = self
            .program
            .definition(interface)
            .map(|definition| &definition.data)
        else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let Some(next_member) = methods.iter().find(|method| method.name == "next") else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let Some(slot) = self.interface_witness_slot(interface, next_member.id) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let iterable = self.materialize_operand(iterable, iterable_type.clone(), token);
        let binding = self.add_local(
            self.for_binding_pattern(token)
                .as_ref()
                .and_then(|pattern| self.hir.locals.get(pattern.root.0 as usize))
                .map_or_else(
                    || self.text(binding_token).to_owned(),
                    |local| local.name.clone(),
                ),
            item_type.clone(),
            false,
            false,
            self.span(binding_token),
        );
        let binding_pattern = self.for_binding_pattern(token);
        self.prepare_for_binding(binding_pattern.as_ref(), binding_token, binding);
        let condition_block = self.new_block();
        let resume_block = self.new_block();
        let body_block = self.new_block();
        let exit_block = self.new_block();
        self.terminate(
            TerminatorKind::Goto(Self::block_id(condition_block)),
            self.span(token),
        );
        self.current = condition_block;
        let optional_item = Type::Optional(Box::new(item_type.clone()));
        let next_result = if asynchronous {
            Type::Promise {
                result: Box::new(optional_item.clone()),
                error: Box::new(Type::Primitive(PrimitiveType::Never)),
                effects: Vec::new(),
            }
        } else {
            optional_item.clone()
        };
        let next_signature = tn_hir::FunctionType {
            parameters: Vec::new(),
            result: Box::new(next_result.clone()),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: asynchronous,
            is_unsafe: next_member.function.is_unsafe,
        };
        let next_type = Type::Function(next_signature.clone());
        let next_method = self.temporary(next_type.clone(), self.span(token));
        self.statement(StatementKind::StorageLive(next_method), self.span(token));
        self.statement(
            StatementKind::Assign(
                Place::local(next_method),
                Box::new(Rvalue::WitnessLookup {
                    object: iterable.clone(),
                    interface,
                    slot,
                    receiver: ReceiverMode::Mutable,
                    ty: next_type,
                }),
            ),
            self.span(token),
        );
        let Some((next, _)) = self.emit_call(
            Operand::Move(Place::local(next_method)),
            Some(Operand::Copy(iterable.clone())),
            &next_signature,
            Vec::new(),
            iterable_start,
        ) else {
            self.index = header_end + 1;
            self.lower_statement();
            return;
        };
        let next = self.materialize_operand(next, next_result, token);
        let optional = if asynchronous {
            let value = self.temporary(optional_item.clone(), self.span(token));
            self.statement(StatementKind::StorageLive(value), self.span(token));
            self.terminate(
                TerminatorKind::Suspend {
                    value: Operand::Move(next),
                    destination: Some(Place::local(value)),
                    error_destination: None,
                    resume: Self::block_id(resume_block),
                    error: None,
                    cancel: Self::block_id(exit_block),
                },
                self.span(token),
            );
            self.current = resume_block;
            Place::local(value)
        } else {
            next
        };
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(optional.clone()),
                targets: vec![(1, Self::block_id(body_block))],
                otherwise: Self::block_id(exit_block),
            },
            self.span(token),
        );
        self.current = body_block;
        self.statement(
            StatementKind::StorageLive(binding),
            self.span(binding_token),
        );
        let mut payload = optional;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        self.statement(
            StatementKind::Assign(
                Place::local(binding),
                Box::new(Rvalue::Use(Operand::Move(payload))),
            ),
            self.span(binding_token),
        );
        if let Some(pattern) = &binding_pattern {
            self.lower_binding_pattern(pattern);
        }
        self.index = header_end + 1;
        self.loop_targets.push((
            Self::block_id(condition_block),
            Self::block_id(exit_block),
            self.async_managed_scopes.len(),
        ));
        self.lower_statement();
        self.loop_targets.pop();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(condition_block)),
                self.span(token),
            );
        }
        self.current = exit_block;
    }

    fn iteration_witness(&self, token: &Token) -> Option<&IterationWitness> {
        let start = u32::try_from(token.range.start).ok()?;
        self.hir.statements.iter().find_map(|statement| {
            if statement.origin.byte_start != start {
                return None;
            }
            let HirStatementKind::For {
                witness: Some(witness),
                ..
            } = &statement.kind
            else {
                return None;
            };
            Some(witness.as_ref())
        })
    }

    #[allow(clippy::too_many_lines)]
    fn lower_try(&mut self) {
        let token = self.tokens[self.index];
        let block_start = self.index + 1;
        let Some(block_end) =
            self.matching_token(block_start, TokenKind::LeftBrace, TokenKind::RightBrace)
        else {
            self.index += 1;
            return;
        };
        let catches = self.catch_arms(block_end + 1);
        let mut effects = self
            .hir
            .expressions
            .iter()
            .filter(|expression| {
                expression.origin.byte_start
                    >= u32::try_from(self.tokens[block_start].range.start).unwrap_or(u32::MAX)
                    && expression.origin.byte_end
                        <= u32::try_from(self.tokens[block_end].range.end).unwrap_or(0)
            })
            .flat_map(|expression| {
                expression
                    .effects
                    .iter()
                    .copied()
                    .chain(
                        match &expression.ty {
                            Type::Function(function) => function.effects.as_slice(),
                            _ => &[],
                        }
                        .iter()
                        .copied(),
                    )
                    .chain(promise_effects_for_type(self.program, &expression.ty))
            })
            .collect::<Vec<_>>();
        effects.sort();
        effects.dedup();
        let payload = self.temporary(Type::ErrorUnion(effects.clone()), self.span(token));
        self.statement(StatementKind::StorageLive(payload), self.span(token));
        let dispatch = self.new_block();
        let join = self.new_block();
        let catch_blocks = catches.iter().map(|_| self.new_block()).collect::<Vec<_>>();
        self.error_contexts.push(ErrorContext {
            dispatch: Self::block_id(dispatch),
            payload,
            effects: effects.clone(),
            scope_depth: self.async_managed_scopes.len(),
        });
        self.index = block_start;
        self.lower_statement();
        self.error_contexts.pop();
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(TerminatorKind::Goto(Self::block_id(join)), self.span(token));
        }
        self.current = dispatch;
        let mut targets = Vec::new();
        for (tag, effect) in effects.iter().enumerate() {
            if let Some((catch_index, _)) = catches
                .iter()
                .enumerate()
                .find(|(_, catch)| self.catch_handles(&catch.ty, *effect))
            {
                targets.push((
                    u128::try_from(tag).expect("error tag limit"),
                    Self::block_id(catch_blocks[catch_index]),
                ));
            }
        }
        let otherwise = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(Place::local(payload)),
                targets,
                otherwise: Self::block_id(otherwise),
            },
            self.span(token),
        );
        for (catch, block) in catches.iter().zip(catch_blocks) {
            self.current = block;
            let local = self.add_local(
                self.text(self.tokens[catch.binding]).to_owned(),
                catch.ty.clone(),
                false,
                false,
                self.span(self.tokens[catch.binding]),
            );
            self.bind_hir_local(self.tokens[catch.binding], local);
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[catch.binding]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(local),
                    Box::new(Rvalue::Cast {
                        operand: Operand::Move(Place::local(payload)),
                        ty: catch.ty.clone(),
                        kind: CastKind::CheckedDowncast,
                    }),
                ),
                self.span(self.tokens[catch.binding]),
            );
            self.index = catch.block_start;
            self.lower_statement();
            if self.blocks[self.current].terminator.is_none() {
                self.terminate(TerminatorKind::Goto(Self::block_id(join)), self.span(token));
            }
        }
        self.current = otherwise;
        if effects.iter().all(|effect| self.return_effect(*effect)) {
            self.terminate(
                TerminatorKind::Throw(Operand::Move(Place::local(payload))),
                self.span(token),
            );
        } else {
            self.terminate(TerminatorKind::Unreachable, self.span(token));
        }
        self.current = join;
        self.index = catches
            .last()
            .map_or(block_end + 1, |catch| catch.block_end + 1);
    }

    fn catch_arms(&self, start: usize) -> Vec<CatchArm> {
        let mut catches = Vec::new();
        let mut cursor = start;
        while self
            .tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::Catch)
        {
            let open = cursor + 1;
            let Some(close) =
                self.matching_token(open, TokenKind::LeftParen, TokenKind::RightParen)
            else {
                break;
            };
            let binding = open + 1;
            let Some(colon) = self.find_top_level(binding + 1, close, TokenKind::Colon) else {
                break;
            };
            let Some(ty) = self.parse_type_range(colon + 1, close) else {
                break;
            };
            let block_start = close + 1;
            let Some(block_end) =
                self.matching_token(block_start, TokenKind::LeftBrace, TokenKind::RightBrace)
            else {
                break;
            };
            catches.push(CatchArm {
                binding,
                ty,
                block_start,
                block_end,
            });
            cursor = block_end + 1;
        }
        catches
    }

    fn catch_handles(&self, caught: &Type, effect: DeclarationId) -> bool {
        let (Type::Nominal(caught, _) | Type::DynamicInterface(caught, _)) = caught else {
            return false;
        };
        if *caught == effect {
            return true;
        }
        let mut current = effect;
        while let Some(DefinitionData::Class {
            base: Some(base), ..
        }) = self
            .program
            .definition(current)
            .map(|definition| &definition.data)
        {
            if base == caught {
                return true;
            }
            current = *base;
        }
        self.program
            .definitions
            .iter()
            .any(|definition| match &definition.data {
                DefinitionData::Class { interfaces, .. } if definition.declaration == effect => {
                    interfaces.iter().any(
                        |interface| matches!(interface, Type::Nominal(id, _) if *id == *caught),
                    )
                }
                DefinitionData::Implementation {
                    interface: Some(Type::Nominal(interface, _)),
                    target: Type::Nominal(target, _),
                    ..
                } => *interface == *caught && *target == effect,
                _ => false,
            })
    }

    fn return_effect(&self, effect: DeclarationId) -> bool {
        self.declared_effects.contains(&effect)
    }

    fn lower_loop_control(&mut self) {
        let token = self.tokens[self.index];
        let target =
            self.loop_targets
                .last()
                .map(|(continue_target, break_target, scope_depth)| {
                    if token.kind == TokenKind::Break {
                        (*break_target, *scope_depth)
                    } else {
                        (*continue_target, *scope_depth)
                    }
                });
        if let Some((target, scope_depth)) = target {
            let managed = self.active_async_disposals_from(scope_depth);
            self.lower_async_disposals(&managed);
            if self.blocks[self.current].terminator.is_none() {
                self.terminate(TerminatorKind::Goto(target), self.span(token));
            }
        }
        self.index += 1;
        self.index += usize::from(self.kind() == Some(TokenKind::Semicolon));
    }

    fn kind(&self) -> Option<TokenKind> {
        (self.index < self.body_limit)
            .then(|| self.tokens.get(self.index).map(|token| token.kind))
            .flatten()
    }

    fn text(&self, token: &Token) -> &str {
        &self.module.source[token.range.clone()]
    }

    fn span(&self, token: &Token) -> SourceSpan {
        SourceSpan::new(
            self.module.path.to_string_lossy(),
            token.range.clone(),
            &self.module.source,
        )
    }

    fn span_from_hir_local(&self, local: HirLocalId) -> SourceSpan {
        self.hir.locals.get(local.0 as usize).map_or_else(
            || SourceSpan::new("<binding>", 0..0, ""),
            |hir_local| hir_local.origin.clone(),
        )
    }

    fn for_binding_pattern(&self, token: &Token) -> Option<HirBindingPattern> {
        let start = u32::try_from(token.range.start).ok()?;
        let root = self.hir.statements.iter().find_map(|statement| {
            if statement.origin.byte_start != start {
                return None;
            }
            let HirStatementKind::For { binding, .. } = statement.kind else {
                return None;
            };
            Some(binding)
        })?;
        self.hir
            .binding_patterns
            .iter()
            .find(|pattern| pattern.root == root)
            .cloned()
    }

    fn prepare_for_binding(
        &mut self,
        pattern: Option<&HirBindingPattern>,
        binding_token: &Token,
        binding: LocalId,
    ) {
        if let Some(pattern) = pattern {
            self.hir_local_ids.insert(pattern.root, binding);
        } else {
            self.bind_hir_local(binding_token, binding);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_local(&mut self) {
        let mutable = self.kind() == Some(TokenKind::Let);
        let start = self.index;
        let pattern_start = start
            + 1
            + usize::from(
                self.tokens
                    .get(start + 1)
                    .is_some_and(|token| token.kind == TokenKind::Mut),
            );
        let end = self.statement_end(start);
        let Some(pattern_token) = self.tokens.get(pattern_start).copied() else {
            self.index += 1;
            return;
        };
        let pattern_origin_start = if self
            .tokens
            .get(start + 1)
            .is_some_and(|token| token.kind == TokenKind::Mut)
        {
            start + 1
        } else {
            pattern_start
        };
        let binding_pattern = self
            .hir
            .binding_patterns
            .iter()
            .find(|pattern| {
                pattern.origin.byte_start
                    == self
                        .tokens
                        .get(pattern_origin_start)
                        .map_or(u32::MAX, |token| {
                            u32::try_from(token.range.start).unwrap_or(u32::MAX)
                        })
            })
            .cloned();
        let equal = self.find_top_level(pattern_start, end, TokenKind::Equal);
        let initializer = equal.and_then(|equal| self.tokens.get(equal + 1).copied());
        let annotation = equal.and_then(|equal| {
            self.find_top_level(pattern_start, equal, TokenKind::Colon)
                .map(|colon| colon + 1)
                .and_then(|type_start| self.parse_type_range(type_start, equal))
        });
        let (simple_type, source) = self.infer_initializer(initializer, equal, end);
        let contextual_conversion = initializer.is_some_and(|token| {
            self.hir_expression_at(token).is_some_and(|expression| {
                matches!(
                    expression.kind,
                    tn_hir::HirExpressionKind::Conversion(
                        tn_hir::HirConversionKind::StringLiteralToOwned
                    )
                )
            })
        });
        let single_constant = !contextual_conversion
            && equal.is_some_and(|equal| equal + 2 == end)
            && initializer.is_some_and(|token| self.constant(token, &simple_type).is_some());
        let complex = if source.is_none() && !single_constant {
            equal.and_then(|equal| self.lower_expression_range(equal + 1, end, annotation.as_ref()))
        } else {
            None
        };
        let inferred = complex
            .as_ref()
            .map_or_else(|| simple_type.clone(), |(_, ty)| ty.clone());
        let ty = annotation.unwrap_or_else(|| inferred.clone());
        let (name, pattern_mutable, origin) = binding_pattern
            .as_ref()
            .and_then(|pattern| {
                self.hir
                    .locals
                    .get(pattern.root.0 as usize)
                    .map(|local| (local.name.clone(), local.mutable, local.origin.clone()))
            })
            .unwrap_or_else(|| {
                (
                    self.text(pattern_token).to_owned(),
                    mutable,
                    self.span(pattern_token),
                )
            });
        let local = self.add_local(name, ty.clone(), pattern_mutable, false, origin.clone());
        if let Some(pattern) = &binding_pattern {
            self.hir_local_ids.insert(pattern.root, local);
        } else {
            self.bind_hir_local(pattern_token, local);
        }
        self.statement(StatementKind::StorageLive(local), origin.clone());
        if !self.assign_simple_initializer(local, &ty, &inferred, source) {
            self.assign_remaining_initializer(local, &ty, initializer, complex, pattern_token);
        }
        if let Some(pattern) = binding_pattern {
            self.lower_binding_pattern(&pattern);
        }
        self.index = end
            + usize::from(
                self.tokens
                    .get(end)
                    .is_some_and(|token| token.kind == TokenKind::Semicolon),
            );
    }

    fn lower_using(&mut self, awaited: bool) {
        let start = self.index;
        let name_index = start + if awaited { 2 } else { 1 };
        let Some(name_token) = self.tokens.get(name_index).copied() else {
            self.index += 1;
            return;
        };
        let name = self.text(name_token).to_owned();
        let end = self.statement_end(start);
        let Some(equal) = self.tokens[start..end]
            .iter()
            .position(|token| token.kind == TokenKind::Equal)
            .map(|offset| start + offset)
        else {
            self.index = end + usize::from(end < self.tokens.len());
            return;
        };
        let (operand, inferred) = self
            .lower_expression_range(equal + 1, end, None)
            .unwrap_or((
                Operand::Constant(tn_mir::Constant::Undefined(Type::Error)),
                Type::Error,
            ));
        let local = self.add_local(name, inferred.clone(), false, false, self.span(name_token));
        self.bind_hir_local(name_token, local);
        self.statement(StatementKind::StorageLive(local), self.span(name_token));
        self.statement(
            StatementKind::Assign(Place::local(local), Box::new(Rvalue::Use(operand))),
            self.span(name_token),
        );
        if awaited && let Some(scope) = self.async_managed_scopes.last_mut() {
            scope.push((local, name_index));
        }
        self.index = end + usize::from(end < self.tokens.len());
    }

    fn lower_async_disposals(&mut self, managed: &[(LocalId, usize)]) {
        let was_disposing = self.disposing_async;
        self.disposing_async = true;
        for (local, token) in managed.iter().rev() {
            let Some(method) = self.disposal_method(&self.locals[local.0 as usize].ty) else {
                continue;
            };
            let signature = tn_hir::FunctionType {
                parameters: method
                    .function
                    .parameters
                    .iter()
                    .map(|parameter| parameter.ty.clone())
                    .collect(),
                result: Box::new(method.function.result.clone()),
                effects: method.function.effects.clone(),
                generics: Vec::new(),
                is_async: true,
                is_unsafe: method.function.is_unsafe,
            };
            let function_type = Type::Function(signature.clone());
            let function = self.temporary(function_type.clone(), self.span(self.tokens[*token]));
            self.statement(
                StatementKind::StorageLive(function),
                self.span(self.tokens[*token]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(function),
                    Box::new(Rvalue::DirectMethod {
                        object: Place::local(*local),
                        implementation: self.method_owner(method.id).unwrap_or_else(|| {
                            mir_nominal_id(&self.locals[local.0 as usize].ty)
                                .expect("async disposable nominal owner")
                        }),
                        member: method.id,
                        receiver: method.receiver,
                        ty: function_type,
                    }),
                ),
                self.span(self.tokens[*token]),
            );
            let Some((promise, promise_type)) = self.emit_call(
                Operand::Move(Place::local(function)),
                Some(Operand::Copy(Place::local(*local))),
                &signature,
                Vec::new(),
                *token,
            ) else {
                continue;
            };
            let _ = self.lower_await_operand(promise, promise_type, *token);
        }
        self.disposing_async = was_disposing;
    }

    fn active_async_disposals(&self) -> Vec<(LocalId, usize)> {
        self.async_managed_scopes
            .iter()
            .flat_map(|scope| scope.iter().copied())
            .collect()
    }

    fn active_async_disposals_from(&self, scope_depth: usize) -> Vec<(LocalId, usize)> {
        self.async_managed_scopes
            .iter()
            .skip(scope_depth)
            .flat_map(|scope| scope.iter().copied())
            .collect()
    }

    fn disposal_method(&self, ty: &Type) -> Option<tn_hir::Method> {
        let declaration = mir_nominal_id(ty)?;
        let definition = self.program.definition(declaration)?;
        let (DefinitionData::Struct { methods, .. }
        | DefinitionData::Class { methods, .. }
        | DefinitionData::Implementation { methods, .. }) = &definition.data
        else {
            return None;
        };
        methods
            .iter()
            .find(|method| method.name == "[Symbol.asyncDispose]")
            .cloned()
    }

    fn assign_remaining_initializer(
        &mut self,
        local: LocalId,
        ty: &Type,
        initializer: Option<&Token>,
        complex: Option<(Operand, Type)>,
        name_token: &Token,
    ) {
        if let Some((operand, source_type)) = complex {
            let rvalue = if &source_type == ty {
                Rvalue::Use(operand)
            } else {
                Rvalue::Cast {
                    operand,
                    ty: ty.clone(),
                    kind: mir_cast_kind(&source_type, ty),
                }
            };
            self.statement(
                StatementKind::Assign(Place::local(local), Box::new(rvalue)),
                self.span(name_token),
            );
        } else if let Some(constant) = initializer.and_then(|token| self.constant(token, ty)) {
            self.statement(
                StatementKind::Assign(
                    Place::local(local),
                    Box::new(Rvalue::Use(Operand::Constant(constant))),
                ),
                self.span(initializer.expect("constant initializer")),
            );
        }
    }

    fn bind_hir_local(&mut self, name_token: &Token, local: LocalId) {
        if let Some(hir_local) = self.hir.locals.iter().find(|hir_local| {
            hir_local.name == self.text(name_token)
                && !self.hir_local_ids.contains_key(&hir_local.id)
        }) {
            self.hir_local_ids.insert(hir_local.id, local);
        }
    }

    fn assign_simple_initializer(
        &mut self,
        local: LocalId,
        ty: &Type,
        inferred: &Type,
        source: Option<(InitializerKind, Place, SourceSpan)>,
    ) -> bool {
        let Some((kind, place, span)) = source else {
            return false;
        };
        if let InitializerKind::Borrow(kind) = kind {
            self.statement(
                StatementKind::Borrow {
                    destination: local,
                    kind,
                    place,
                    region: RegionId(self.next_region),
                },
                span,
            );
            self.next_region += 1;
            return true;
        }
        let operand = if matches!(kind, InitializerKind::Move) {
            Operand::Move(place)
        } else {
            Operand::Copy(place)
        };
        let rvalue = if inferred == ty {
            Rvalue::Use(operand)
        } else {
            Rvalue::Cast {
                operand,
                ty: ty.clone(),
                kind: mir_cast_kind(inferred, ty),
            }
        };
        self.statement(
            StatementKind::Assign(Place::local(local), Box::new(rvalue)),
            span,
        );
        true
    }

    fn infer_initializer(
        &self,
        initializer: Option<&Token>,
        equal: Option<usize>,
        end: usize,
    ) -> (Type, Option<(InitializerKind, Place, SourceSpan)>) {
        let Some(initializer) = initializer else {
            return (Type::Error, None);
        };
        if initializer.kind == TokenKind::Amp {
            let mutable = equal
                .and_then(|equal| self.tokens.get(equal + 2))
                .is_some_and(|token| token.kind == TokenKind::Mut);
            let offset = 2 + usize::from(mutable);
            if equal.is_none_or(|equal| equal + offset + 1 != end) {
                return (Type::Error, None);
            }
            let referent = equal
                .and_then(|equal| self.tokens.get(equal + offset))
                .and_then(|token| self.names.get(self.text(token)).copied());
            if let Some(referent) = referent {
                let referent_type = self.locals[referent.0 as usize].ty.clone();
                return (
                    Type::Reference {
                        mutable,
                        lifetime: "scope".into(),
                        referent: Box::new(referent_type),
                    },
                    Some((
                        InitializerKind::Borrow(if mutable {
                            BorrowKind::Mutable
                        } else {
                            BorrowKind::Shared
                        }),
                        Place::local(referent),
                        self.span(initializer),
                    )),
                );
            }
        }
        if initializer.kind == TokenKind::Move {
            if equal.is_none_or(|equal| equal + 3 != end) {
                return (Type::Error, None);
            }
            let source = equal
                .and_then(|equal| self.tokens.get(equal + 2))
                .and_then(|token| self.names.get(self.text(token)).copied());
            if let Some(source) = source {
                return (
                    self.locals[source.0 as usize].ty.clone(),
                    Some((
                        InitializerKind::Move,
                        Place::local(source),
                        self.span(initializer),
                    )),
                );
            }
        }
        if equal.is_some_and(|equal| equal + 2 == end)
            && let Some(source) = self.names.get(self.text(initializer)).copied()
        {
            let ty = self.locals[source.0 as usize].ty.clone();
            return (
                ty.clone(),
                Some((
                    if self.ownership_facts.is_copy(&ty) {
                        InitializerKind::Copy
                    } else {
                        InitializerKind::Move
                    },
                    Place::local(source),
                    self.span(initializer),
                )),
            );
        }
        (
            literal_suffix_type(self.text(initializer), initializer.kind)
                .unwrap_or_else(|| atom_type(initializer.kind)),
            None,
        )
    }

    fn lower_explicit_move(&mut self) {
        let token = self.tokens[self.index];
        let source = self
            .tokens
            .get(self.index + 1)
            .and_then(|name| self.names.get(self.text(name)).copied());
        if let Some(source) = source {
            let sink = self.temporary(self.locals[source.0 as usize].ty.clone(), self.span(token));
            self.statement(StatementKind::StorageLive(sink), self.span(token));
            self.statement(
                StatementKind::Assign(
                    Place::local(sink),
                    Box::new(Rvalue::Use(Operand::Move(Place::local(source)))),
                ),
                self.span(token),
            );
        }
        self.index += 2;
    }

    fn constant(&self, token: &Token, ty: &Type) -> Option<tn_mir::Constant> {
        Some(match token.kind {
            TokenKind::True => tn_mir::Constant::Bool(true),
            TokenKind::False => tn_mir::Constant::Bool(false),
            TokenKind::IntegerLiteral => tn_mir::Constant::Integer {
                value: parse_integer(self.text(token))?,
                ty: ty.clone(),
            },
            TokenKind::FloatLiteral => {
                let value = self
                    .text(token)
                    .trim_end_matches("f32")
                    .trim_end_matches("f64")
                    .replace('_', "")
                    .parse::<f64>()
                    .ok()?;
                let bits = if matches!(ty, Type::Primitive(PrimitiveType::F32)) {
                    u64::from((value as f32).to_bits())
                } else {
                    value.to_bits()
                };
                tn_mir::Constant::Float {
                    bits,
                    ty: ty.clone(),
                }
            }
            TokenKind::CharacterLiteral => {
                let decoded = decode_quoted_literal(self.text(token))?;
                let mut characters = decoded.chars();
                let character = characters.next()?;
                if characters.next().is_some() {
                    return None;
                }
                tn_mir::Constant::Character(character)
            }
            TokenKind::StringLiteral => {
                tn_mir::Constant::String(decode_quoted_literal(self.text(token))?)
            }
            _ => return None,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn lower_expression_statement(&mut self) {
        let token = self.tokens[self.index];
        let end = self.statement_end(self.index);
        if let Some((assignment, compound)) = self.find_assignment(self.index, end) {
            if let Some((declaration, destination_type)) =
                self.global_declaration(self.index, assignment)
            {
                let left = compound.and_then(|operator| {
                    let (left, left_type) = self.lower_expression_range(
                        self.index,
                        assignment,
                        Some(&destination_type),
                    )?;
                    Some((operator, left, left_type))
                });
                let value =
                    self.lower_expression_range(assignment + 1, end, Some(&destination_type));
                if let Some((operand, source_type)) = value {
                    let value = if let Some((operator, left_operator, left_type)) = left {
                        Rvalue::CheckedBinary {
                            operator,
                            left: left_operator,
                            right: operand,
                            operand_type: binary_operand_type(&left_type, &source_type),
                            result_type: destination_type.clone(),
                        }
                    } else if source_type == destination_type {
                        Rvalue::Use(operand)
                    } else {
                        Rvalue::Cast {
                            operand,
                            ty: destination_type.clone(),
                            kind: mir_cast_kind(&source_type, &destination_type),
                        }
                    };
                    let value_local = self.temporary(destination_type.clone(), self.span(token));
                    self.statement(StatementKind::StorageLive(value_local), self.span(token));
                    self.statement(
                        StatementKind::Assign(Place::local(value_local), Box::new(value)),
                        self.span(token),
                    );
                    let stored =
                        self.temporary(Type::Primitive(PrimitiveType::Bool), self.span(token));
                    self.statement(StatementKind::StorageLive(stored), self.span(token));
                    self.statement(
                        StatementKind::Assign(
                            Place::local(stored),
                            Box::new(Rvalue::RawOperation {
                                operation: format!("global_store:{}", declaration.0),
                                operands: vec![Operand::Move(Place::local(value_local))],
                                ty: Type::Primitive(PrimitiveType::Bool),
                            }),
                        ),
                        self.span(token),
                    );
                }
            } else {
                let destination = self.lower_assignment_place(self.index, assignment);
                if let Some((destination, destination_type)) = destination {
                    let value =
                        self.lower_expression_range(assignment + 1, end, Some(&destination_type));
                    if let Some((operand, source_type)) = value {
                        let value = if let Some(operator) = compound {
                            Rvalue::CheckedBinary {
                                operator,
                                left: Operand::Copy(destination.clone()),
                                right: operand,
                                operand_type: destination_type.clone(),
                                result_type: destination_type.clone(),
                            }
                        } else if source_type == destination_type {
                            Rvalue::Use(operand)
                        } else {
                            Rvalue::Cast {
                                operand,
                                ty: destination_type.clone(),
                                kind: mir_cast_kind(&source_type, &destination_type),
                            }
                        };
                        self.statement(
                            StatementKind::Assign(destination, Box::new(value)),
                            self.span(token),
                        );
                    }
                }
            }
        } else if let Some((operand, ty)) = self
            .lower_expression_range(self.index, end, None)
            .filter(|(_, ty)| *ty != Type::Primitive(PrimitiveType::Void))
        {
            let sink = self.temporary(ty, self.span(token));
            self.statement(StatementKind::StorageLive(sink), self.span(token));
            self.statement(
                StatementKind::Assign(Place::local(sink), Box::new(Rvalue::Use(operand))),
                self.span(token),
            );
        }
        self.index = end
            + usize::from(
                self.tokens
                    .get(end)
                    .is_some_and(|token| token.kind == TokenKind::Semicolon),
            );
    }

    fn lower_assignment_place(&mut self, start: usize, end: usize) -> Option<(Place, Type)> {
        if self
            .tokens
            .get(start)
            .is_some_and(|token| token.kind == TokenKind::Star)
        {
            let (operand, pointer_type) = self.lower_expression_range(start + 1, end, None)?;
            let mut place = operand_place(operand)?;
            let pointee = match pointer_type {
                Type::RawPointer { pointee, .. }
                | Type::Reference {
                    referent: pointee, ..
                } => *pointee,
                _ => return None,
            };
            place.projection.push(tn_mir::Projection::Dereference);
            return Some((place, pointee));
        }
        self.lower_expression_range(start, end, None)
            .and_then(|(operand, ty)| operand_place(operand).map(|place| (place, ty)))
    }

    fn lower_throw(&mut self) {
        let token = self.tokens[self.index];
        let end = self.statement_end(self.index);
        let terminator = self
            .lower_expression_range(self.index + 1, end, None)
            .map_or(TerminatorKind::Unreachable, |(operand, _)| {
                TerminatorKind::Throw(operand)
            });
        let managed = self.active_async_disposals();
        self.lower_async_disposals(&managed);
        self.terminate(terminator, self.span(token));
        self.index = end + usize::from(end < self.tokens.len());
    }

    fn lower_suspend(&mut self) {
        let end = self.statement_end(self.index);
        let await_index = self.index
            + usize::from(
                self.tokens[self.index].kind == TokenKind::Try
                    && self
                        .tokens
                        .get(self.index + 1)
                        .is_some_and(|next| next.kind == TokenKind::Await),
            );
        self.lower_await(await_index, end);
        self.index = end + usize::from(end < self.tokens.len());
    }

    fn lower_return(&mut self) {
        let token = self.tokens[self.index];
        let end = self.statement_end(self.index);
        if let Some(finish) = self.generator_finish_block {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(finish)),
                self.span(token),
            );
            self.index = end + usize::from(end < self.tokens.len());
            return;
        }
        let expected = self.return_type.clone();
        let lowered = self.lower_expression_range(self.index + 1, end, Some(&expected));
        let mut operand = lowered.as_ref().map(|(operand, _)| operand.clone());
        if let Some((source_operand, source_type)) = lowered
            && source_type != self.return_type
        {
            let temporary = self.temporary(self.return_type.clone(), self.span(token));
            self.statement(StatementKind::StorageLive(temporary), self.span(token));
            self.statement(
                StatementKind::Assign(
                    Place::local(temporary),
                    Box::new(Rvalue::Cast {
                        operand: source_operand,
                        ty: self.return_type.clone(),
                        kind: mir_cast_kind(&source_type, &self.return_type),
                    }),
                ),
                self.span(token),
            );
            operand = Some(Operand::Move(Place::local(temporary)));
        }
        let terminator =
            if operand.is_none() && self.return_type != Type::Primitive(PrimitiveType::Void) {
                TerminatorKind::Unreachable
            } else {
                TerminatorKind::Return(operand)
            };
        let managed = self.active_async_disposals();
        self.lower_async_disposals(&managed);
        self.terminate(terminator, self.span(token));
        self.index = end + usize::from(end < self.tokens.len());
    }

    fn lower_jsx(&mut self, id: tn_hir::HirJsxId, start: usize) -> Option<(Operand, Type)> {
        let element = self.hir.jsx_elements.get(id.0 as usize)?.clone();
        if element.fragment {
            let (children, child_type) = self.lower_jsx_children(&element.children, None, start)?;
            let array_type =
                Type::Array(Box::new(child_type.clone()), element.children.len() as u64);
            let children =
                self.materialize_jsx_aggregate(array_type.clone(), children, child_type, start)?;
            let signature = element.runtime_signature.clone()?;
            return self.emit_call(
                Operand::Constant(tn_mir::Constant::Function(
                    element.runtime?,
                    Type::Function(signature.clone()),
                )),
                None,
                &signature,
                vec![children],
                start,
            );
        }
        let component_id = element.component?;
        let component_origin = self
            .hir
            .expressions
            .get(component_id.0 as usize)?
            .origin
            .clone();
        let component_expression = self.hir.expressions.get(component_id.0 as usize)?;
        let (component_start, component_end) = self.token_range_for_bytes(
            component_origin.byte_start as usize,
            component_origin.byte_end as usize,
        )?;
        let component = match (component_expression.resolution, &component_expression.ty) {
            (Some(ResolvedValue::Declaration(declaration)), Type::Function(_)) => (
                Operand::Constant(tn_mir::Constant::Function(
                    declaration,
                    component_expression.ty.clone(),
                )),
                component_expression.ty.clone(),
            ),
            (Some(ResolvedValue::Member(member)), Type::Function(_))
                if self.method_receiver(member) == Some(tn_hir::ReceiverMode::Static) =>
            {
                let owner = self.method_owner(member)?;
                (
                    Operand::Constant(tn_mir::Constant::Method {
                        owner,
                        member,
                        ty: component_expression.ty.clone(),
                    }),
                    component_expression.ty.clone(),
                )
            }
            _ => self.lower_expression_range(component_start, component_end, None)?,
        };
        let (component, _component_type) = component;
        let props = self.lower_jsx_properties(&element, start)?;
        let key = if let Some(key_id) = element.key {
            let origin = self.hir.expressions.get(key_id.0 as usize)?.origin.clone();
            let (key_start, key_end) =
                self.token_range_for_bytes(origin.byte_start as usize, origin.byte_end as usize)?;
            let (value, value_type) = self.lower_expression_range(key_start, key_end, None)?;
            let optional_type = Type::Optional(Box::new(value_type.clone()));
            self.materialize_jsx_optional(optional_type.clone(), value, start)?
                .map(|value| (value, optional_type))?
        } else {
            let key_type = Type::Optional(Box::new(Type::String));
            (
                Operand::Constant(tn_mir::Constant::Undefined(key_type.clone())),
                key_type,
            )
        };
        let signature = element.runtime_signature.clone()?;
        self.emit_call(
            Operand::Constant(tn_mir::Constant::Function(
                element.runtime?,
                Type::Function(signature.clone()),
            )),
            None,
            &signature,
            vec![component, props, key.0],
            start,
        )
    }

    fn lower_jsx_properties(
        &mut self,
        element: &tn_hir::HirJsxElement,
        start: usize,
    ) -> Option<Operand> {
        let props_type = if element.properties_type == Type::Unknown {
            Type::Tuple(Vec::new())
        } else {
            element.properties_type.clone()
        };
        let schema = self.aggregate_schema(&props_type);
        if schema.is_empty() && !element.properties.is_empty() {
            let spread = element.properties.iter().find(|property| property.spread)?;
            let HirJsxValue::Expression(expression) = spread.value else {
                return None;
            };
            let origin = self
                .hir
                .expressions
                .get(expression.0 as usize)?
                .origin
                .clone();
            let (value_start, value_end) =
                self.token_range_for_bytes(origin.byte_start as usize, origin.byte_end as usize)?;
            return self
                .lower_expression_range(value_start, value_end, Some(&props_type))
                .map(|(operand, _)| operand);
        }
        let mut spread_sources = vec![None; element.properties.len()];
        for (property_index, property) in element.properties.iter().enumerate() {
            if !property.spread {
                continue;
            }
            let HirJsxValue::Expression(expression) = &property.value else {
                return None;
            };
            let origin = self
                .hir
                .expressions
                .get(expression.0 as usize)?
                .origin
                .clone();
            let (value_start, value_end) =
                self.token_range_for_bytes(origin.byte_start as usize, origin.byte_end as usize)?;
            let (operand, value_type) =
                self.lower_expression_range(value_start, value_end, Some(&props_type))?;
            let token = (*self.tokens.get(start)?).clone();
            let place = self.materialize_operand(operand, value_type.clone(), &token);
            spread_sources[property_index] = Some((place, value_type));
        }
        let mut fields = Vec::with_capacity(schema.len());
        for (field_name, field_type) in &schema {
            let property_index =
                element
                    .properties
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, property)| {
                        if property.name.as_deref() == Some(field_name)
                            || (property.spread && spread_sources[index].is_some())
                        {
                            Some(index)
                        } else {
                            None
                        }
                    });
            let value = match property_index {
                Some(index) if !element.properties[index].spread => {
                    self.lower_jsx_value(&element.properties[index].value, field_type, start)?
                }
                Some(index) => {
                    let (place, source_type) = spread_sources[index].as_ref()?;
                    let source_field_index = self
                        .aggregate_schema(source_type)
                        .iter()
                        .position(|(name, _)| name == field_name)?;
                    let mut field = place.clone();
                    field.projection.push(tn_mir::Projection::Field {
                        index: u32::try_from(source_field_index).ok()?,
                        ty: field_type.clone(),
                    });
                    Operand::Move(field)
                }
                None => Operand::Constant(tn_mir::Constant::Undefined(field_type.clone())),
            };
            fields.push(value);
        }
        self.materialize_jsx_aggregate_with_types(
            props_type,
            fields,
            schema.into_iter().map(|(_, ty)| ty).collect(),
            start,
        )
    }

    fn lower_jsx_value(
        &mut self,
        value: &HirJsxValue,
        expected: &Type,
        start: usize,
    ) -> Option<Operand> {
        match value {
            HirJsxValue::Boolean(value) => Some(Operand::Constant(tn_mir::Constant::Bool(*value))),
            HirJsxValue::String(value) if *expected == Type::String => {
                let literal = Operand::Constant(tn_mir::Constant::String(value.clone()));
                let length = Operand::Constant(tn_mir::Constant::Integer {
                    value: i128::try_from(value.len()).unwrap_or(i128::MAX),
                    ty: Type::Primitive(PrimitiveType::Usize),
                });
                let destination = self.temporary(Type::String, self.span(self.tokens[start]));
                self.statement(
                    StatementKind::StorageLive(destination),
                    self.span(self.tokens[start]),
                );
                self.statement(
                    StatementKind::Assign(
                        Place::local(destination),
                        Box::new(Rvalue::RawOperation {
                            operation: "string_from_static".into(),
                            operands: vec![literal, length],
                            ty: Type::String,
                        }),
                    ),
                    self.span(self.tokens[start]),
                );
                Some(Operand::Move(Place::local(destination)))
            }
            HirJsxValue::String(value) => {
                Some(Operand::Constant(tn_mir::Constant::String(value.clone())))
            }
            HirJsxValue::Expression(expression) => {
                let origin = self
                    .hir
                    .expressions
                    .get(expression.0 as usize)?
                    .origin
                    .clone();
                let (value_start, value_end) = self
                    .token_range_for_bytes(origin.byte_start as usize, origin.byte_end as usize)?;
                self.lower_expression_range(value_start, value_end, Some(expected))
                    .map(|(operand, _)| operand)
            }
            HirJsxValue::Children(children) => {
                let (operands, child_type) =
                    self.lower_jsx_children(children, Some(expected), start)?;
                if children.len() == 1 && !matches!(expected, Type::Array(_, _) | Type::Slice(_)) {
                    return operands.into_iter().next();
                }
                let array_type = match expected {
                    Type::Array(_, length) => Type::Array(Box::new(child_type.clone()), *length),
                    _ => Type::Array(Box::new(child_type.clone()), children.len() as u64),
                };
                let children = self.materialize_jsx_aggregate(
                    array_type.clone(),
                    operands,
                    child_type,
                    start,
                )?;
                if !matches!(expected, Type::Array(_, _) | Type::Slice(_)) {
                    return self
                        .lower_jsx_fragment(children, array_type, expected.clone(), start)
                        .map(|(operand, _)| operand);
                }
                Some(children)
            }
        }
    }

    fn lower_jsx_fragment(
        &mut self,
        children: Operand,
        children_type: Type,
        result_type: Type,
        start: usize,
    ) -> Option<(Operand, Type)> {
        let runtime = self.program.jsx_runtime_declaration("fragment")?;
        let signature = tn_hir::FunctionType {
            parameters: vec![children_type],
            result: Box::new(result_type),
            effects: Vec::new(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        };
        self.emit_call(
            Operand::Constant(tn_mir::Constant::Function(
                runtime,
                Type::Function(signature.clone()),
            )),
            None,
            &signature,
            vec![children],
            start,
        )
    }

    fn lower_jsx_children(
        &mut self,
        children: &[HirJsxChild],
        expected: Option<&Type>,
        start: usize,
    ) -> Option<(Vec<Operand>, Type)> {
        let expected_element = expected.map(|expected| match expected {
            Type::Array(element, _) | Type::Slice(element) => element.as_ref(),
            _ => expected,
        });
        let mut operands = Vec::with_capacity(children.len());
        let mut child_type = None;
        for child in children {
            let (operand, actual) = match child {
                HirJsxChild::Element(id) => {
                    let origin = self.hir.jsx_elements.get(id.0 as usize)?.origin.clone();
                    let (child_start, child_end) = self.token_range_for_bytes(
                        origin.byte_start as usize,
                        origin.byte_end as usize,
                    )?;
                    self.lower_expression_range(child_start, child_end, expected_element)?
                }
                HirJsxChild::Expression(id) => {
                    let origin = self.hir.expressions.get(id.0 as usize)?.origin.clone();
                    let (child_start, child_end) = self.token_range_for_bytes(
                        origin.byte_start as usize,
                        origin.byte_end as usize,
                    )?;
                    self.lower_expression_range(child_start, child_end, expected_element)?
                }
                HirJsxChild::Text { value, .. } => {
                    let ty = expected_element.cloned().unwrap_or(Type::Reference {
                        mutable: false,
                        lifetime: "static".into(),
                        referent: Box::new(Type::Str),
                    });
                    if ty == Type::String {
                        let literal = Operand::Constant(tn_mir::Constant::String(value.clone()));
                        let length = Operand::Constant(tn_mir::Constant::Integer {
                            value: i128::try_from(value.len()).unwrap_or(i128::MAX),
                            ty: Type::Primitive(PrimitiveType::Usize),
                        });
                        let destination =
                            self.temporary(Type::String, self.span(self.tokens[start]));
                        self.statement(
                            StatementKind::StorageLive(destination),
                            self.span(self.tokens[start]),
                        );
                        self.statement(
                            StatementKind::Assign(
                                Place::local(destination),
                                Box::new(Rvalue::RawOperation {
                                    operation: "string_from_static".into(),
                                    operands: vec![literal, length],
                                    ty: Type::String,
                                }),
                            ),
                            self.span(self.tokens[start]),
                        );
                        (Operand::Move(Place::local(destination)), ty)
                    } else {
                        (
                            Operand::Constant(tn_mir::Constant::String(value.clone())),
                            ty,
                        )
                    }
                }
            };
            if let Some(previous) = &child_type
                && *previous != actual
                && !matches!(previous, Type::Unknown)
            {
                return None;
            }
            child_type = Some(actual.clone());
            operands.push(operand);
        }
        Some((
            operands,
            child_type
                .or_else(|| expected_element.cloned())
                .unwrap_or(Type::String),
        ))
    }

    fn materialize_jsx_aggregate(
        &mut self,
        ty: Type,
        fields: Vec<Operand>,
        field_type: Type,
        start: usize,
    ) -> Option<Operand> {
        let field_types = vec![field_type; fields.len()];
        self.materialize_jsx_aggregate_with_types(ty, fields, field_types, start)
    }

    #[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
    fn materialize_jsx_aggregate_with_types(
        &mut self,
        ty: Type,
        fields: Vec<Operand>,
        field_types: Vec<Type>,
        start: usize,
    ) -> Option<Operand> {
        let destination = self.temporary(ty.clone(), self.span(self.tokens[start]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[start]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Aggregate {
                    ty: ty.clone(),
                    variant: None,
                    fields,
                    field_types,
                }),
            ),
            self.span(self.tokens[start]),
        );
        Some(Operand::Move(Place::local(destination)))
    }

    #[allow(clippy::option_option)]
    fn materialize_jsx_optional(
        &mut self,
        ty: Type,
        value: Operand,
        start: usize,
    ) -> Option<Option<Operand>> {
        let Type::Optional(inner) = &ty else {
            return None;
        };
        let inner = inner.as_ref().clone();
        let operand =
            self.materialize_jsx_aggregate_with_types(ty, vec![value], vec![inner], start)?;
        Some(Some(operand))
    }

    fn token_range_for_bytes(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        let first = self
            .tokens
            .iter()
            .position(|token| token.range.start == start || token.range.start >= start)?;
        let last = self
            .tokens
            .iter()
            .enumerate()
            .skip(first)
            .find(|(_, token)| token.range.end >= end)
            .map(|(index, _)| index + 1)?;
        Some((first, last))
    }

    #[allow(clippy::too_many_lines)]
    fn lower_expression_range(
        &mut self,
        start: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        if start >= end {
            return None;
        }
        if let Some(tn_hir::HirExpressionKind::Jsx(id)) = self
            .hir_expression_range(start, end)
            .map(|expression| &expression.kind)
        {
            return self.lower_jsx(*id, start);
        }
        if let Some(ResolvedValue::Closure(closure)) = self
            .hir_expression_range(start, end)
            .and_then(|expression| expression.resolution)
        {
            return self.lower_closure(start, end, closure);
        }
        if let Some(ResolvedValue::Template(template)) = self
            .hir_expression_range(start, end)
            .and_then(|expression| expression.resolution)
        {
            return self.lower_template(start, template);
        }
        match self
            .hir_expression_range(start, end)
            .and_then(|expression| expression.resolution)
        {
            Some(ResolvedValue::StringLength) => return self.lower_string_length(start, end),
            Some(ResolvedValue::StringByteLength) => {
                return self.lower_string_byte_length(start, end);
            }
            _ => {}
        }
        if self.tokens[start].kind == TokenKind::Try
            && self
                .tokens
                .get(start + 1)
                .is_some_and(|token| token.kind == TokenKind::Await)
        {
            return self.lower_await(start + 1, end);
        }
        if self.tokens[start].kind == TokenKind::Await {
            return self.lower_await(start, end);
        }
        if self.tokens[start].kind == TokenKind::Try {
            return self.lower_expression_range(start + 1, end, expected);
        }
        if end > start + 1
            && self.tokens[end - 1].kind == TokenKind::Bang
            && self.find_binary_operator(start, end).is_none()
            && !matches!(
                self.tokens[start].kind,
                TokenKind::Amp
                    | TokenKind::Bang
                    | TokenKind::Minus
                    | TokenKind::Star
                    | TokenKind::Tilde
            )
        {
            return self.lower_force_unwrap(start, end - 1, expected);
        }
        if self.tokens[start].kind == TokenKind::Switch {
            return self.lower_match(start, end, expected);
        }
        if self.tokens[start].kind == TokenKind::New {
            return self.lower_new(start, end);
        }
        if let Some(operator) = self.find_top_level(start, end, TokenKind::QuestionQuestion) {
            return self.lower_coalesce(start, operator, end, expected);
        }
        if let Some(question) = self.find_top_level(start, end, TokenKind::Question)
            && let Some(colon) = self.find_top_level(question + 1, end, TokenKind::Colon)
        {
            return self.lower_conditional(start, question, colon, end, expected);
        }
        if matches!(
            self.tokens[start].kind,
            TokenKind::LeftBracket | TokenKind::LeftBrace
        ) && self.matching_token(
            start,
            self.tokens[start].kind,
            if self.tokens[start].kind == TokenKind::LeftBracket {
                TokenKind::RightBracket
            } else {
                TokenKind::RightBrace
            },
        ) == Some(end - 1)
        {
            return self.lower_aggregate(start, end, expected);
        }
        if self.tokens[start].kind == TokenKind::LeftParen
            && self.matching_token(start, TokenKind::LeftParen, TokenKind::RightParen)
                == Some(end - 1)
        {
            if self
                .find_top_level(start + 1, end - 1, TokenKind::Comma)
                .is_some()
            {
                return self.lower_aggregate(start, end, expected);
            }
            return self.lower_expression_range(start + 1, end - 1, expected);
        }
        if let Some((operator_index, operator)) = self.find_binary_operator(start, end)
            && operator_index > start
            && matches!(
                operator,
                tn_mir::BinaryOperator::LogicalAnd | tn_mir::BinaryOperator::LogicalOr
            )
        {
            return self.lower_short_circuit(start, operator_index, end, operator);
        }
        if matches!(
            self.tokens[start].kind,
            TokenKind::Bang | TokenKind::Minus | TokenKind::Tilde
        ) && self
            .find_binary_operator(start, end)
            .is_some_and(|(operator_index, _)| operator_index > start + 1)
        {
            return self.lower_binary_expression(start, end, expected);
        }
        if matches!(
            self.tokens[start].kind,
            TokenKind::Bang | TokenKind::Minus | TokenKind::Tilde
        ) {
            return self.lower_unary(start, end, expected);
        }
        if let Some(question_dot) = self.find_top_level(start, end, TokenKind::QuestionDot) {
            return self.lower_optional_chain(start, question_dot, end);
        }
        if let Some(open) = self.find_top_level(start, end, TokenKind::LeftParen)
            && open > start
            && !matches!(
                self.tokens[start].kind,
                TokenKind::Bang
                    | TokenKind::Minus
                    | TokenKind::Tilde
                    | TokenKind::Amp
                    | TokenKind::Star
                    | TokenKind::Move
                    | TokenKind::Try
                    | TokenKind::Await
            )
            && self.generic_call_bounds(start, open).map_or_else(
                || self.find_binary_operator(start, open).is_none(),
                |(less, _)| self.find_binary_operator(start, less).is_none(),
            )
            && self.matching_token(open, TokenKind::LeftParen, TokenKind::RightParen)
                == Some(end - 1)
        {
            return self.lower_call(start, open, end);
        }
        if let Some(open) = self.find_top_level(start, end, TokenKind::LeftBracket)
            && open > start
            && self.find_binary_operator(start, open).is_none()
            && self.matching_token(open, TokenKind::LeftBracket, TokenKind::RightBracket)
                == Some(end - 1)
        {
            if self.direct_member_access(start, end).is_some() {
                return self.lower_computed_member(start, open, end);
            }
            return self.lower_index(start, open, end);
        }
        if let Some(dot) = self.find_top_level(start, end, TokenKind::Dot)
            && dot > start
            && !matches!(self.tokens[start].kind, TokenKind::Amp | TokenKind::Star)
            && self.find_top_level(start, end, TokenKind::As).is_none()
            && self
                .find_top_level(start, end, TokenKind::AsQuestion)
                .is_none()
            && self.find_binary_operator(start, end).is_none()
        {
            if dot + 2 == end {
                return self.lower_field(start, dot, end);
            }
            return self.lower_member_chain(start, end);
        }
        if let Some(cast) = self
            .find_top_level(start, end, TokenKind::As)
            .or_else(|| self.find_top_level(start, end, TokenKind::AsQuestion))
        {
            let (mut operand, source_type) = self.lower_expression_range(start, cast, None)?;
            let hir_target = self
                .hir_expression_range(start, end)
                .and_then(|expression| {
                    (!matches!(expression.ty, Type::Error)).then(|| expression.ty.clone())
                });
            let target = if self.tokens[cast].kind == TokenKind::As
                && matches!(hir_target, Some(Type::RawPointer { .. }))
            {
                self.parse_type_range(cast + 1, end).or(hir_target)
            } else {
                hir_target.or_else(|| self.parse_type_range(cast + 1, end))
            }?;
            let cast_source_type = if self.tokens[cast].kind == TokenKind::AsQuestion {
                let (borrowed, borrowed_type) =
                    self.lower_checked_downcast_borrow(operand, &source_type, &target, cast)?;
                operand = borrowed;
                borrowed_type
            } else {
                if matches!(source_type, Type::Promise { .. })
                    && matches!(target, Type::RawPointer { .. })
                    && let Some(place) = operand_place(operand.clone())
                {
                    operand = Operand::Copy(place);
                }
                source_type.clone()
            };
            let temporary = self.temporary(target.clone(), self.span(self.tokens[cast]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[cast]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(temporary),
                    Box::new(Rvalue::Cast {
                        operand,
                        ty: target.clone(),
                        kind: mir_cast_kind(&cast_source_type, &target),
                    }),
                ),
                self.span(self.tokens[cast]),
            );
            return Some((Operand::Move(Place::local(temporary)), target));
        }
        if let Some(operator) = self.find_top_level(start, end, TokenKind::InstanceOf) {
            let (operand, _) = self.lower_expression_range(start, operator, None)?;
            let target = self.parse_type_range(operator + 1, end)?;
            let result_type = Type::Primitive(PrimitiveType::Bool);
            let temporary = self.temporary(result_type.clone(), self.span(self.tokens[operator]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[operator]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(temporary),
                    Box::new(Rvalue::TypeTest { operand, target }),
                ),
                self.span(self.tokens[operator]),
            );
            return Some((Operand::Move(Place::local(temporary)), result_type));
        }
        if let Some((operator_index, _operator)) = self.find_binary_operator(start, end)
            && operator_index > start
        {
            return self.lower_binary_expression(start, end, expected);
        }
        if self.tokens[start].kind == TokenKind::Star {
            let (operand, pointee) = if self
                .tokens
                .get(end.saturating_sub(1))
                .is_some_and(|token| token.kind == TokenKind::Bang)
            {
                let (operand, pointer_type) =
                    self.lower_expression_range(start + 1, end, expected)?;
                let pointee = match pointer_type {
                    Type::RawPointer { pointee, .. }
                    | Type::Reference {
                        referent: pointee, ..
                    } => *pointee,
                    _ => return None,
                };
                (operand, pointee)
            } else if let Some(source) = self
                .tokens
                .get(start + 1)
                .and_then(|name| self.names.get(self.text(name)).copied())
            {
                match &self.locals[source.0 as usize].ty {
                    Type::RawPointer { pointee, .. }
                    | Type::Reference {
                        referent: pointee, ..
                    } => (
                        Operand::Copy(Place::local(source)),
                        pointee.as_ref().clone(),
                    ),
                    _ => {
                        let (operand, pointer_type) =
                            self.lower_expression_range(start + 1, end, None)?;
                        let pointee = match pointer_type {
                            Type::RawPointer { pointee, .. }
                            | Type::Reference {
                                referent: pointee, ..
                            } => *pointee,
                            _ => return None,
                        };
                        (operand, pointee)
                    }
                }
            } else {
                let (operand, pointer_type) = self.lower_expression_range(start + 1, end, None)?;
                let pointee = match pointer_type {
                    Type::RawPointer { pointee, .. }
                    | Type::Reference {
                        referent: pointee, ..
                    } => *pointee,
                    _ => return None,
                };
                (operand, pointee)
            };
            let ty = pointee;
            let temporary = self.temporary(ty.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(temporary),
                    Box::new(Rvalue::RawOperation {
                        operation: "dereference".into(),
                        operands: vec![operand],
                        ty: ty.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(temporary)), ty));
        }
        if self.tokens[start].kind == TokenKind::Amp {
            let mutable = self
                .tokens
                .get(start + 1)
                .is_some_and(|token| token.kind == TokenKind::Mut);
            let name_index = start + 1 + usize::from(mutable);
            if self
                .tokens
                .get(name_index)
                .is_some_and(|token| token.kind == TokenKind::Star)
                && let Some((place, referent)) = self.lower_dereference_place(name_index, end)
            {
                let ty = Type::Reference {
                    mutable,
                    lifetime: "scope".into(),
                    referent: Box::new(referent),
                };
                let temporary = self.temporary(ty.clone(), self.span(self.tokens[start]));
                self.statement(
                    StatementKind::StorageLive(temporary),
                    self.span(self.tokens[start]),
                );
                self.statement(
                    StatementKind::Borrow {
                        destination: temporary,
                        kind: if mutable {
                            BorrowKind::Mutable
                        } else {
                            BorrowKind::Shared
                        },
                        place,
                        region: RegionId(self.next_region),
                    },
                    self.span(self.tokens[start]),
                );
                self.next_region += 1;
                return Some((Operand::Move(Place::local(temporary)), ty));
            }
            if let Some((declaration, referent)) = self.global_declaration(name_index, end) {
                let ty = Type::Reference {
                    mutable,
                    lifetime: "scope".into(),
                    referent: Box::new(referent),
                };
                let temporary = self.temporary(ty.clone(), self.span(self.tokens[start]));
                self.statement(
                    StatementKind::StorageLive(temporary),
                    self.span(self.tokens[start]),
                );
                self.statement(
                    StatementKind::Assign(
                        Place::local(temporary),
                        Box::new(Rvalue::RawOperation {
                            operation: format!("global_address:{}", declaration.0),
                            operands: Vec::new(),
                            ty: ty.clone(),
                        }),
                    ),
                    self.span(self.tokens[start]),
                );
                return Some((Operand::Move(Place::local(temporary)), ty));
            }
            let (source, referent) = self.lower_expression_range(name_index, end, None)?;
            let source = operand_place(source)?;
            let ty = Type::Reference {
                mutable,
                lifetime: "scope".into(),
                referent: Box::new(referent.clone()),
            };
            let temporary = self.temporary(ty.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Borrow {
                    destination: temporary,
                    kind: if mutable {
                        BorrowKind::Mutable
                    } else {
                        BorrowKind::Shared
                    },
                    place: source,
                    region: RegionId(self.next_region),
                },
                self.span(self.tokens[start]),
            );
            self.next_region += 1;
            return Some((Operand::Move(Place::local(temporary)), ty));
        }
        if self.tokens[start].kind == TokenKind::Move {
            let source = self
                .tokens
                .get(start + 1)
                .and_then(|name| self.names.get(self.text(name)).copied())?;
            return Some((
                Operand::Move(Place::local(source)),
                self.locals[source.0 as usize].ty.clone(),
            ));
        }
        self.lower_atom(start, expected)
    }

    fn lower_dereference_place(&mut self, start: usize, end: usize) -> Option<(Place, Type)> {
        if self
            .tokens
            .get(start)
            .is_none_or(|token| token.kind != TokenKind::Star)
        {
            return None;
        }
        let inner_start = start + 1;
        let (operand, pointee) = if let Some(cast) = self
            .find_top_level(inner_start, end, TokenKind::As)
            .or_else(|| self.find_top_level(inner_start, end, TokenKind::AsQuestion))
        {
            let (operand, _) = self.lower_expression_range(inner_start, cast, None)?;
            let target = self
                .hir_expression_range(inner_start, end)
                .map(|expression| expression.ty.clone())
                .or_else(|| self.parse_type_range(cast + 1, end))?;
            let Type::RawPointer { pointee, .. } = target else {
                return None;
            };
            (operand, *pointee)
        } else {
            let (operand, pointer_type) = self.lower_expression_range(inner_start, end, None)?;
            let pointee = match pointer_type {
                Type::RawPointer { pointee, .. }
                | Type::Reference {
                    referent: pointee, ..
                } => *pointee,
                _ => return None,
            };
            (operand, pointee)
        };
        let mut place = operand_place(operand)?.clone();
        place.projection.push(tn_mir::Projection::Dereference);
        Some((place, pointee))
    }

    fn lower_checked_downcast_borrow(
        &mut self,
        source: Operand,
        source_type: &Type,
        target: &Type,
        token: usize,
    ) -> Option<(Operand, Type)> {
        let Type::Optional(target) = target else {
            return None;
        };
        let Type::Reference {
            mutable, lifetime, ..
        } = target.as_ref()
        else {
            return None;
        };
        let source_referent = match source_type {
            Type::Reference { referent, .. } => referent.as_ref(),
            ty => ty,
        };
        let borrow_type = Type::Reference {
            mutable: *mutable,
            lifetime: lifetime.clone(),
            referent: Box::new(source_referent.clone()),
        };
        let mut source = operand_place(source)?;
        if matches!(source_type, Type::Reference { .. }) {
            source.projection.push(tn_mir::Projection::Dereference);
        }
        let destination = self.temporary(borrow_type.clone(), self.span(self.tokens[token]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[token]),
        );
        self.statement(
            StatementKind::Borrow {
                destination,
                kind: if *mutable {
                    BorrowKind::Mutable
                } else {
                    BorrowKind::Shared
                },
                place: source,
                region: RegionId(self.next_region),
            },
            self.span(self.tokens[token]),
        );
        self.next_region += 1;
        Some((Operand::Move(Place::local(destination)), borrow_type))
    }

    fn lower_unary(
        &mut self,
        start: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let (operand, operand_type) = self.lower_expression_range(start + 1, end, expected)?;
        let (operator, result_type) = match self.tokens[start].kind {
            TokenKind::Bang => (
                tn_mir::UnaryOperator::LogicalNot,
                Type::Primitive(PrimitiveType::Bool),
            ),
            TokenKind::Minus => (tn_mir::UnaryOperator::Negate, operand_type.clone()),
            TokenKind::Tilde => (tn_mir::UnaryOperator::BitNot, operand_type.clone()),
            _ => return None,
        };
        let temporary = self.temporary(result_type.clone(), self.span(self.tokens[start]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[start]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::Unary {
                    operator,
                    operand,
                    operand_type,
                    result_type: result_type.clone(),
                }),
            ),
            self.span(self.tokens[start]),
        );
        Some((Operand::Move(Place::local(temporary)), result_type))
    }

    fn lower_binary_expression(
        &mut self,
        start: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let (operator_index, operator) = self.find_binary_operator(start, end)?;
        if operator_index <= start {
            return None;
        }
        let (left, left_type) = self.lower_expression_range(start, operator_index, expected)?;
        let (right, right_type) =
            self.lower_expression_range(operator_index + 1, end, Some(&left_type))?;
        let (left, right) = if matches!(
            operator,
            tn_mir::BinaryOperator::Equal | tn_mir::BinaryOperator::NotEqual
        ) {
            (non_consuming_operand(left), non_consuming_operand(right))
        } else {
            (left, right)
        };
        let (left, left_type) = normalize_string_comparison_operand(left, left_type);
        let (right, right_type) = normalize_string_comparison_operand(right, right_type);
        let operand_type = binary_operand_type(&left_type, &right_type);
        let result_type = if matches!(
            operator,
            tn_mir::BinaryOperator::Equal
                | tn_mir::BinaryOperator::NotEqual
                | tn_mir::BinaryOperator::Less
                | tn_mir::BinaryOperator::LessEqual
                | tn_mir::BinaryOperator::Greater
                | tn_mir::BinaryOperator::GreaterEqual
                | tn_mir::BinaryOperator::LogicalAnd
                | tn_mir::BinaryOperator::LogicalOr
        ) {
            Type::Primitive(PrimitiveType::Bool)
        } else {
            operand_type.clone()
        };
        let temporary = self.temporary(result_type.clone(), self.span(self.tokens[operator_index]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[operator_index]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::CheckedBinary {
                    operator,
                    left,
                    right,
                    operand_type,
                    result_type: result_type.clone(),
                }),
            ),
            self.span(self.tokens[operator_index]),
        );
        Some((Operand::Move(Place::local(temporary)), result_type))
    }

    fn lower_conditional(
        &mut self,
        start: usize,
        question: usize,
        colon: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let bool_type = Type::Primitive(PrimitiveType::Bool);
        let condition = self
            .lower_expression_range(start, question, Some(&bool_type))?
            .0;
        let result_type = self
            .hir_expression_range(start, end)
            .map(|expression| expression.ty.clone())
            .or_else(|| expected.cloned())?;
        let destination = self.temporary(result_type.clone(), self.span(self.tokens[question]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[question]),
        );
        let then_block = self.new_block();
        let else_block = self.new_block();
        let join = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: condition,
                targets: vec![(1, Self::block_id(then_block))],
                otherwise: Self::block_id(else_block),
            },
            self.span(self.tokens[question]),
        );
        for (block, range) in [
            (then_block, (question + 1, colon)),
            (else_block, (colon + 1, end)),
        ] {
            self.current = block;
            let (operand, source_type) =
                self.lower_expression_range(range.0, range.1, Some(&result_type))?;
            let rvalue = if source_type == result_type {
                Rvalue::Use(operand)
            } else {
                Rvalue::Cast {
                    operand,
                    ty: result_type.clone(),
                    kind: mir_cast_kind(&source_type, &result_type),
                }
            };
            self.statement(
                StatementKind::Assign(Place::local(destination), Box::new(rvalue)),
                self.span(self.tokens[range.0]),
            );
            self.terminate(
                TerminatorKind::Goto(Self::block_id(join)),
                self.span(self.tokens[range.0]),
            );
        }
        self.current = join;
        Some((Operand::Move(Place::local(destination)), result_type))
    }

    fn lower_short_circuit(
        &mut self,
        start: usize,
        operator_index: usize,
        end: usize,
        operator: tn_mir::BinaryOperator,
    ) -> Option<(Operand, Type)> {
        let bool_type = Type::Primitive(PrimitiveType::Bool);
        let left = self
            .lower_expression_range(start, operator_index, Some(&bool_type))?
            .0;
        let destination = self.temporary(bool_type.clone(), self.span(self.tokens[operator_index]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[operator_index]),
        );
        let right_block = self.new_block();
        let short_block = self.new_block();
        let join = self.new_block();
        let (targets, otherwise, short_value) = if operator == tn_mir::BinaryOperator::LogicalAnd {
            (
                vec![(1, Self::block_id(right_block))],
                Self::block_id(short_block),
                false,
            )
        } else {
            (
                vec![(1, Self::block_id(short_block))],
                Self::block_id(right_block),
                true,
            )
        };
        self.terminate(
            TerminatorKind::Switch {
                value: left,
                targets,
                otherwise,
            },
            self.span(self.tokens[operator_index]),
        );
        self.current = short_block;
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Use(Operand::Constant(tn_mir::Constant::Bool(
                    short_value,
                )))),
            ),
            self.span(self.tokens[operator_index]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[operator_index]),
        );
        self.current = right_block;
        let right = self
            .lower_expression_range(operator_index + 1, end, Some(&bool_type))?
            .0;
        self.statement(
            StatementKind::Assign(Place::local(destination), Box::new(Rvalue::Use(right))),
            self.span(self.tokens[operator_index]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[operator_index]),
        );
        self.current = join;
        Some((Operand::Move(Place::local(destination)), bool_type))
    }

    fn lower_await(&mut self, start: usize, end: usize) -> Option<(Operand, Type)> {
        let (mut value, mut promise_type) = self.lower_expression_range(start + 1, end, None)?;
        if let Some(task_promise) = task_promise_type(self.program, &promise_type) {
            let promise = self.temporary(task_promise.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(promise),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(promise),
                    Box::new(Rvalue::RawOperation {
                        operation: "task_into_promise".into(),
                        operands: vec![value],
                        ty: task_promise.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            value = Operand::Move(Place::local(promise));
            promise_type = task_promise;
        }
        self.lower_await_operand(value, promise_type, start)
    }

    fn lower_await_operand(
        &mut self,
        value: Operand,
        promise_type: Type,
        start: usize,
    ) -> Option<(Operand, Type)> {
        let Type::Promise {
            result, effects, ..
        } = promise_type
        else {
            return None;
        };
        let result_type = *result;
        let value = operand_place(value.clone()).map_or(value, Operand::Move);
        let destination = (result_type != Type::Primitive(PrimitiveType::Void)).then(|| {
            let local = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[start]),
            );
            Place::local(local)
        });
        let error_destination = (!effects.is_empty()).then(|| {
            let local = self.temporary(
                Type::ErrorUnion(effects.clone()),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[start]),
            );
            Place::local(local)
        });
        let resume = self.new_block();
        let error = (!effects.is_empty()).then(|| self.new_block());
        let cancel = self.new_block();
        self.terminate(
            TerminatorKind::Suspend {
                value,
                destination: destination.clone(),
                error_destination: error_destination.clone(),
                resume: Self::block_id(resume),
                error: error.map(Self::block_id),
                cancel: Self::block_id(cancel),
            },
            self.span(self.tokens[start]),
        );
        if let Some(error) = error {
            self.current = error;
            self.route_error(
                Operand::Move(error_destination.expect("fallible suspension payload")),
                &effects,
                start,
            );
        }
        self.current = cancel;
        if !self.disposing_async {
            let managed = self.active_async_disposals();
            self.lower_async_disposals(&managed);
        }
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(TerminatorKind::Unreachable, self.span(self.tokens[start]));
        }
        self.current = resume;
        Some((
            destination.map_or_else(
                || Operand::Constant(tn_mir::Constant::Undefined(result_type.clone())),
                Operand::Move,
            ),
            result_type,
        ))
    }

    fn lower_coalesce(
        &mut self,
        start: usize,
        operator_index: usize,
        end: usize,
        _expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let (left, left_type) = self.lower_expression_range(start, operator_index, None)?;
        let Type::Optional(inner) = left_type else {
            return None;
        };
        let result_type = inner.as_ref().clone();
        let left = self.materialize_operand(left, Type::Optional(inner), self.tokens[start]);
        let destination =
            self.temporary(result_type.clone(), self.span(self.tokens[operator_index]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[operator_index]),
        );
        let fallback = self.new_block();
        let present = self.new_block();
        let join = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(left.clone()),
                targets: vec![(1, Self::block_id(present))],
                otherwise: Self::block_id(fallback),
            },
            self.span(self.tokens[operator_index]),
        );

        self.current = present;
        let mut payload = left;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Use(Operand::Copy(payload))),
            ),
            self.span(self.tokens[operator_index]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[operator_index]),
        );

        self.current = fallback;
        let (fallback_value, fallback_type) =
            self.lower_expression_range(operator_index + 1, end, Some(&result_type))?;
        let fallback_value = if fallback_type == result_type {
            Rvalue::Use(fallback_value)
        } else {
            Rvalue::Cast {
                operand: fallback_value,
                ty: result_type.clone(),
                kind: mir_cast_kind(&fallback_type, &result_type),
            }
        };
        self.statement(
            StatementKind::Assign(Place::local(destination), Box::new(fallback_value)),
            self.span(self.tokens[operator_index]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[operator_index]),
        );
        self.current = join;
        Some((Operand::Move(Place::local(destination)), result_type))
    }

    fn lower_force_unwrap(
        &mut self,
        start: usize,
        end: usize,
        _expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let (operand, optional_type) = self.lower_expression_range(start, end, None)?;
        let Type::Optional(inner) = optional_type else {
            return Some((operand, optional_type));
        };
        let mut payload = operand_place(operand.clone())?;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        let result_type = inner.as_ref().clone();
        let result = match operand {
            Operand::Copy(_) => Operand::Copy(payload),
            Operand::Move(_) => Operand::Move(payload),
            Operand::Constant(_) => return None,
        };
        Some((result, result_type))
    }

    fn lower_optional_chain(
        &mut self,
        start: usize,
        question_dot: usize,
        end: usize,
    ) -> Option<(Operand, Type)> {
        let (source, source_type) = self.lower_expression_range(start, question_dot, None)?;
        self.lower_optional_from_value(source, source_type, question_dot + 1, end, start)
    }

    fn lower_optional_from_value(
        &mut self,
        source: Operand,
        source_type: Type,
        member_start: usize,
        end: usize,
        origin_start: usize,
    ) -> Option<(Operand, Type)> {
        let Type::Optional(inner) = source_type else {
            return None;
        };
        let result_type = self.hir_expression_range(origin_start, end)?.ty.clone();
        let source = self.materialize_operand(
            source,
            Type::Optional(inner.clone()),
            self.tokens[member_start],
        );
        let destination = self.temporary(result_type.clone(), self.span(self.tokens[member_start]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[member_start]),
        );
        let absent = self.new_block();
        let present = self.new_block();
        let join = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(source.clone()),
                targets: vec![(1, Self::block_id(present))],
                otherwise: Self::block_id(absent),
            },
            self.span(self.tokens[member_start]),
        );

        self.current = absent;
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Use(Operand::Constant(tn_mir::Constant::Undefined(
                    result_type.clone(),
                )))),
            ),
            self.span(self.tokens[member_start]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[member_start]),
        );

        self.current = present;
        let mut payload = source;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        let (value, value_type) = self.lower_optional_tail_present(
            Operand::Copy(payload),
            inner.as_ref(),
            member_start,
            end,
            origin_start,
        )?;
        if value_type == result_type {
            self.statement(
                StatementKind::Assign(Place::local(destination), Box::new(Rvalue::Use(value))),
                self.span(self.tokens[member_start]),
            );
        } else {
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::Aggregate {
                        ty: result_type.clone(),
                        variant: Some(1),
                        fields: vec![value],
                        field_types: vec![value_type],
                    }),
                ),
                self.span(self.tokens[member_start]),
            );
            self.statement(
                StatementKind::SetDiscriminant(Place::local(destination), 1),
                self.span(self.tokens[member_start]),
            );
        }
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(
                TerminatorKind::Goto(Self::block_id(join)),
                self.span(self.tokens[member_start]),
            );
        }
        self.current = join;
        Some((Operand::Move(Place::local(destination)), result_type))
    }

    #[allow(clippy::too_many_lines)]
    fn lower_optional_tail_present(
        &mut self,
        owner: Operand,
        owner_type: &Type,
        member_start: usize,
        end: usize,
        origin_start: usize,
    ) -> Option<(Operand, Type)> {
        let member_end = member_start + 1;
        let member_expression = self.hir_expression_range(origin_start, member_end)?;
        let Some(ResolvedValue::Member(member)) = member_expression.resolution else {
            return None;
        };
        let member_type = member_expression
            .optional_chain_value
            .clone()
            .unwrap_or_else(|| member_expression.ty.clone());
        let (mut value, mut value_type) =
            self.lower_member_from(owner, owner_type, member, member_type, member_start)?;
        let mut cursor = member_end;
        while cursor < end {
            match self.tokens[cursor].kind {
                TokenKind::LeftParen => {
                    let close =
                        self.matching_token(cursor, TokenKind::LeftParen, TokenKind::RightParen)?;
                    let Type::Function(signature) = value_type else {
                        return None;
                    };
                    let arguments = self
                        .argument_ranges(cursor + 1, close)
                        .into_iter()
                        .enumerate()
                        .map(|(index, (argument_start, argument_end))| {
                            let (argument, actual) = self.lower_expression_range(
                                argument_start,
                                argument_end,
                                signature.parameters.get(index),
                            )?;
                            Some(self.reborrow_argument(
                                argument,
                                &actual,
                                signature.parameters.get(index),
                                argument_start,
                            ))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    let receiver = operand_place(value.clone())
                        .and_then(|place| self.bound_receivers.get(&place.local).cloned());
                    (value, value_type) =
                        self.emit_call(value, receiver, &signature, arguments, cursor)?;
                    cursor = close + 1;
                }
                TokenKind::Dot => {
                    let next_member = cursor + 1;
                    let next_end = next_member + 1;
                    let expression = self.hir_expression_range(origin_start, next_end)?;
                    let Some(ResolvedValue::Member(member)) = expression.resolution else {
                        return None;
                    };
                    let member_type = expression
                        .optional_chain_value
                        .clone()
                        .unwrap_or_else(|| expression.ty.clone());
                    (value, value_type) = self.lower_member_from(
                        value,
                        &value_type,
                        member,
                        member_type,
                        next_member,
                    )?;
                    cursor = next_end;
                }
                TokenKind::QuestionDot => {
                    return self.lower_optional_from_value(
                        value,
                        value_type,
                        cursor + 1,
                        end,
                        origin_start,
                    );
                }
                TokenKind::LeftBracket => {
                    let close = self.matching_token(
                        cursor,
                        TokenKind::LeftBracket,
                        TokenKind::RightBracket,
                    )?;
                    let index = self
                        .lower_expression_range(
                            cursor + 1,
                            close,
                            Some(&Type::Primitive(PrimitiveType::Usize)),
                        )?
                        .0;
                    let collection =
                        self.materialize_operand(value, value_type.clone(), self.tokens[cursor]);
                    let expression = self.hir_expression_range(origin_start, close + 1)?;
                    let element_type = expression
                        .optional_chain_value
                        .clone()
                        .unwrap_or_else(|| expression.ty.clone());
                    let temporary =
                        self.temporary(element_type.clone(), self.span(self.tokens[cursor]));
                    self.statement(
                        StatementKind::StorageLive(temporary),
                        self.span(self.tokens[cursor]),
                    );
                    self.statement(
                        StatementKind::Assign(
                            Place::local(temporary),
                            Box::new(Rvalue::CheckedIndex { collection, index }),
                        ),
                        self.span(self.tokens[cursor]),
                    );
                    value = Operand::Move(Place::local(temporary));
                    value_type = element_type;
                    cursor = close + 1;
                }
                _ => return None,
            }
        }
        Some((value, value_type))
    }

    fn matching_token(&self, start: usize, open: TokenKind, close: TokenKind) -> Option<usize> {
        if self.tokens.get(start).map(|token| token.kind) != Some(open) {
            return None;
        }
        let mut depth = 0_u32;
        for (index, token) in self.tokens.iter().enumerate().skip(start) {
            if token.kind == open {
                depth = depth.checked_add(1)?;
            } else if token.kind == close {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
        }
        None
    }

    fn find_top_level(&self, start: usize, end: usize, needle: TokenKind) -> Option<usize> {
        let mut depth = 0_u32;
        for index in start..end {
            let kind = self.tokens[index].kind;
            if kind == needle && depth == 0 {
                return Some(index);
            }
            match kind {
                TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        None
    }

    fn statement_end(&self, start: usize) -> usize {
        if self
            .tokens
            .get(start)
            .is_some_and(|token| token.kind == TokenKind::Switch)
        {
            let condition_close = self
                .matching_token(start + 1, TokenKind::LeftParen, TokenKind::RightParen)
                .and_then(|close| {
                    self.matching_token(close + 1, TokenKind::LeftBrace, TokenKind::RightBrace)
                });
            if let Some(body_close) = condition_close {
                return body_close + 1;
            }
        }
        self.find_top_level(start, self.body_limit, TokenKind::Semicolon)
            .unwrap_or(self.body_limit)
    }

    fn lower_template(
        &mut self,
        start: usize,
        template_id: HirTemplateId,
    ) -> Option<(Operand, Type)> {
        let template = self
            .hir
            .templates
            .iter()
            .find(|template| template.id == template_id)?
            .clone();
        let template_type = self
            .hir_expression_at(self.tokens[start])
            .filter(|expression| {
                expression.resolution == Some(ResolvedValue::Template(template_id))
            })?
            .ty
            .clone();
        let Type::Template(capture_types) = &template_type else {
            return None;
        };
        let mut parts = Vec::with_capacity(template.parts.len());
        let mut captures = Vec::with_capacity(capture_types.len());
        for part in template.parts {
            match part {
                HirTemplatePart::Literal(value) => parts.push(TemplatePart::Literal(value)),
                HirTemplatePart::Interpolation {
                    ty,
                    storage,
                    origin,
                    ..
                } => {
                    let capture_index = captures.len();
                    let (value, value_type) = self.lower_embedded_expression(&origin, Some(&ty))?;
                    if value_type != ty {
                        return None;
                    }
                    let capture_type = capture_types.get(capture_index)?.clone();
                    let capture = match storage {
                        HirTemplateStorage::SharedBorrow => {
                            let source = operand_place(value)?;
                            let local = self.temporary(capture_type, origin.clone());
                            self.statement(StatementKind::StorageLive(local), origin.clone());
                            self.statement(
                                StatementKind::Borrow {
                                    destination: local,
                                    kind: BorrowKind::Shared,
                                    place: source,
                                    region: RegionId(self.next_region),
                                },
                                origin.clone(),
                            );
                            self.next_region += 1;
                            Operand::Move(Place::local(local))
                        }
                        HirTemplateStorage::Owned => value,
                    };
                    captures.push(capture);
                    parts.push(TemplatePart::Interpolation {
                        capture: u32::try_from(capture_index).ok()?,
                        value_type: ty,
                    });
                }
            }
        }
        let destination = self.temporary(template_type.clone(), template.origin.clone());
        self.statement(
            StatementKind::StorageLive(destination),
            template.origin.clone(),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Template {
                    id: template_id,
                    parts,
                    captures,
                    ty: template_type.clone(),
                }),
            ),
            template.origin,
        );
        Some((Operand::Move(Place::local(destination)), template_type))
    }

    fn lower_embedded_expression(
        &mut self,
        origin: &SourceSpan,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let absolute_start = usize::try_from(origin.byte_start).ok()?;
        let absolute_end = usize::try_from(origin.byte_end).ok()?;
        let source = self.module.source.get(absolute_start..absolute_end)?;
        let mut owned_tokens = lex(&origin.file, source.as_bytes())
            .tokens
            .into_iter()
            .filter(|token| !token.kind.is_trivia())
            .collect::<Vec<_>>();
        for token in &mut owned_tokens {
            token.range.start += absolute_start;
            token.range.end += absolute_start;
        }
        let expression_end = owned_tokens.len();
        owned_tokens.push(Token {
            kind: TokenKind::RightBrace,
            range: absolute_end..absolute_end.saturating_add(1),
        });
        let tokens = owned_tokens.iter().collect::<Vec<_>>();
        let body_limit = tokens.len();
        let generic_call_ends = generic_call_ends(&tokens);
        let mut nested = OwnershipMirLowerer {
            program: self.program,
            module: self.module,
            hir: self.hir,
            tokens,
            body_limit,
            generic_call_ends,
            index: 0,
            locals: std::mem::take(&mut self.locals),
            temporary_locals: std::mem::take(&mut self.temporary_locals),
            names: std::mem::take(&mut self.names),
            hir_local_ids: std::mem::take(&mut self.hir_local_ids),
            capture_references: std::mem::take(&mut self.capture_references),
            bound_receivers: std::mem::take(&mut self.bound_receivers),
            blocks: std::mem::take(&mut self.blocks),
            current: self.current,
            next_region: self.next_region,
            return_type: self.return_type.clone(),
            declared_effects: self.declared_effects.clone(),
            generics: self.generics.clone(),
            loop_targets: std::mem::take(&mut self.loop_targets),
            error_contexts: std::mem::take(&mut self.error_contexts),
            async_managed_scopes: std::mem::take(&mut self.async_managed_scopes),
            disposing_async: self.disposing_async,
            ownership_facts: self.ownership_facts.clone(),
            generator_item_type: self.generator_item_type.clone(),
            generator_async: self.generator_async,
            generator_buffer: self.generator_buffer,
            generator_finish_block: self.generator_finish_block,
        };
        let lowered = nested.lower_expression_range(0, expression_end, expected);
        self.locals = nested.locals;
        self.temporary_locals = nested.temporary_locals;
        self.names = nested.names;
        self.hir_local_ids = nested.hir_local_ids;
        self.capture_references = nested.capture_references;
        self.bound_receivers = nested.bound_receivers;
        self.blocks = nested.blocks;
        self.current = nested.current;
        self.next_region = nested.next_region;
        self.loop_targets = nested.loop_targets;
        self.error_contexts = nested.error_contexts;
        self.async_managed_scopes = nested.async_managed_scopes;
        self.disposing_async = nested.disposing_async;
        self.generator_item_type = nested.generator_item_type;
        self.generator_async = nested.generator_async;
        self.generator_buffer = nested.generator_buffer;
        self.generator_finish_block = nested.generator_finish_block;
        lowered
    }

    #[allow(clippy::too_many_lines)]
    fn lower_closure(
        &mut self,
        start: usize,
        _end: usize,
        closure_id: HirClosureId,
    ) -> Option<(Operand, Type)> {
        let closure = self
            .hir
            .closures
            .iter()
            .find(|closure| closure.id == closure_id)?
            .clone();
        let mut captures = Vec::new();
        for capture in &closure.captures {
            let source_local = *self.hir_local_ids.get(&capture.local)?;
            let mut source = Place::local(source_local);
            if self.capture_references.contains(&source_local) {
                source.projection.push(tn_mir::Projection::Dereference);
            }
            let operand = match capture.mode {
                HirCaptureMode::Move => Operand::Move(source),
                HirCaptureMode::SharedBorrow | HirCaptureMode::MutableBorrow => {
                    let local = self.temporary(capture.ty.clone(), capture.origin.clone());
                    self.statement(StatementKind::StorageLive(local), capture.origin.clone());
                    self.statement(
                        StatementKind::Borrow {
                            destination: local,
                            kind: if capture.mode == HirCaptureMode::MutableBorrow {
                                BorrowKind::Mutable
                            } else {
                                BorrowKind::Shared
                            },
                            place: source,
                            region: RegionId(self.next_region),
                        },
                        capture.origin.clone(),
                    );
                    self.next_region += 1;
                    Operand::Move(Place::local(local))
                }
            };
            captures.push(operand);
        }

        let mut body_tokens = self
            .tokens
            .iter()
            .copied()
            .filter(|token| {
                token.range.start >= closure.body.byte_start as usize
                    && token.range.end <= closure.body.byte_end as usize
            })
            .collect::<Vec<_>>();
        let block_body = body_tokens
            .first()
            .is_some_and(|token| token.kind == TokenKind::LeftBrace)
            && body_tokens
                .last()
                .is_some_and(|token| token.kind == TokenKind::RightBrace);
        if block_body {
            body_tokens.remove(0);
            body_tokens.pop();
        }
        let body_limit = body_tokens.len();
        let generic_call_ends = generic_call_ends(&body_tokens);
        let mut nested = OwnershipMirLowerer {
            program: self.program,
            module: self.module,
            hir: self.hir,
            tokens: body_tokens,
            body_limit,
            generic_call_ends,
            index: 0,
            locals: Vec::new(),
            temporary_locals: BTreeSet::new(),
            names: BTreeMap::new(),
            hir_local_ids: BTreeMap::new(),
            capture_references: BTreeSet::new(),
            bound_receivers: BTreeMap::new(),
            blocks: vec![OpenBlock::default()],
            current: 0,
            next_region: 0,
            return_type: closure.function.result.as_ref().clone(),
            declared_effects: closure.function.effects.clone(),
            generics: self.generics.clone(),
            loop_targets: Vec::new(),
            error_contexts: Vec::new(),
            async_managed_scopes: vec![Vec::new()],
            disposing_async: false,
            ownership_facts: self.ownership_facts.clone(),
            generator_item_type: None,
            generator_async: false,
            generator_buffer: None,
            generator_finish_block: None,
        };
        for capture in &closure.captures {
            let local = nested.add_local(
                capture.name.clone(),
                capture.ty.clone(),
                capture.mode == HirCaptureMode::MutableBorrow,
                true,
                capture.origin.clone(),
            );
            nested.hir_local_ids.insert(capture.local, local);
            if capture.mode != HirCaptureMode::Move {
                nested.capture_references.insert(local);
            }
        }
        for parameter in &closure.parameters {
            let parameter = nested.hir.locals.get(parameter.0 as usize)?;
            let local = nested.add_local(
                parameter.name.clone(),
                parameter.ty.clone(),
                parameter.mutable,
                true,
                parameter.origin.clone(),
            );
            nested.hir_local_ids.insert(parameter.id, local);
        }
        if block_body {
            nested.lower();
        } else if !nested.tokens.is_empty() {
            let token = nested.tokens[0];
            let expected = nested.return_type.clone();
            let end = nested.tokens.len();
            let value = nested.lower_expression_range(0, end, Some(&expected));
            nested.terminate(
                value.map_or(TerminatorKind::Unreachable, |(operand, _)| {
                    TerminatorKind::Return(Some(operand))
                }),
                nested.span(token),
            );
        }
        let body = nested.finish(
            self.program
                .graph
                .declaration(self.hir_owner_declaration())?
                .id,
            self.hir_owner_member(),
            closure.function.effects.clone(),
        );
        let function_type = Type::Function(closure.function.clone());
        let destination = self.temporary(function_type.clone(), closure.origin.clone());
        self.statement(
            StatementKind::StorageLive(destination),
            closure.origin.clone(),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Closure {
                    id: closure.id,
                    function: closure.function,
                    captures,
                    body: Box::new(body),
                }),
            ),
            self.span(self.tokens[start]),
        );
        Some((Operand::Move(Place::local(destination)), function_type))
    }

    fn hir_owner_declaration(&self) -> DeclarationId {
        match self.hir.owner {
            BodyOwner::Declaration(declaration) | BodyOwner::Member { declaration, .. } => {
                declaration
            }
        }
    }

    fn hir_owner_member(&self) -> Option<MemberId> {
        match self.hir.owner {
            BodyOwner::Declaration(_) => None,
            BodyOwner::Member { member, .. } => Some(member),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_call(&mut self, start: usize, open: usize, end: usize) -> Option<(Operand, Type)> {
        if let Some(ResolvedValue::Member(member)) = self
            .hir_expression_range(start, open)
            .and_then(|expression| expression.resolution)
            && self.enum_variant(member).is_some()
        {
            return self.lower_enum_variant(start, open + 1, end - 1, member);
        }
        let generic_bounds = self.generic_call_bounds(start, open);
        let callee_end = generic_bounds.map_or(open, |(less, _)| less);
        let direct_member =
            self.direct_member_access(start, callee_end)
                .and_then(|(owner_end, member_token)| {
                    let expression = self.hir_expression_range(start, callee_end)?;
                    let ResolvedValue::Member(member) = expression.resolution? else {
                        return None;
                    };
                    if self.method_receiver(member) == Some(tn_hir::ReceiverMode::Static) {
                        return None;
                    }
                    matches!(&expression.ty, Type::Function(_)).then_some((
                        owner_end,
                        member_token,
                        member,
                        expression.ty.clone(),
                    ))
                });
        let callee_is_run = self
            .hir_expression_range(start, callee_end)
            .and_then(|expression| expression.resolution)
            .and_then(|resolution| match resolution {
                tn_hir::ResolvedValue::Declaration(declaration) => {
                    self.program.graph.declaration(declaration)
                }
                _ => None,
            })
            .and_then(|declaration| declaration.name.as_deref())
            == Some("run");
        let has_direct_member = direct_member.is_some();
        let (mut function, function_type, prelowered_arguments) =
            if let Some((owner_end, member_token, member, ty)) = direct_member {
                let (owner, owner_type) = self.lower_expression_range(start, owner_end, None)?;
                let Type::Function(signature) = ty.clone() else {
                    return None;
                };
                let arguments =
                    self.lower_call_arguments(self.argument_ranges(open + 1, end - 1), &signature)?;
                let lowered =
                    self.lower_member_from(owner, &owner_type, member, ty, member_token)?;
                (lowered.0, lowered.1, Some(arguments))
            } else {
                let lowered = self.lower_expression_range(start, callee_end, None)?;
                (lowered.0, lowered.1, None)
            };
        let Type::Function(signature) = function_type else {
            return None;
        };
        let (arguments, argument_types) = match prelowered_arguments {
            Some(arguments) => arguments,
            None => {
                self.lower_call_arguments(self.argument_ranges(open + 1, end - 1), &signature)?
            }
        };
        let mut substitutions = BTreeMap::new();
        if let Some((less, greater)) = generic_bounds {
            let explicit = self
                .argument_ranges(less + 1, greater)
                .into_iter()
                .filter_map(|(argument_start, argument_end)| {
                    self.parse_type_range(argument_start, argument_end)
                })
                .collect::<Vec<_>>();
            for (generic, argument) in signature
                .generics
                .iter()
                .filter(|generic| generic.namespace == tn_hir::Namespace::Type)
                .zip(explicit)
            {
                substitutions.insert(generic.name.clone(), argument);
            }
        }
        for (parameter, actual) in signature.parameters.iter().zip(&argument_types) {
            infer_mir_substitutions(parameter, actual, &mut substitutions);
        }
        if let Some(result) = self
            .hir_expression_range(start, end)
            .map(|expression| &expression.ty)
        {
            infer_mir_substitutions(&signature.result, result, &mut substitutions);
        }
        let mut concrete = tn_hir::FunctionType {
            parameters: signature
                .parameters
                .iter()
                .map(|parameter| substitute_mir_type(parameter, &substitutions))
                .collect(),
            result: Box::new(substitute_mir_type(&signature.result, &substitutions)),
            effects: signature.effects.clone(),
            generics: Vec::new(),
            is_async: signature.is_async,
            is_unsafe: signature.is_unsafe,
        };
        if concrete.effects.is_empty()
            && let Some(expression) = self.hir_expression_range(start, end)
        {
            concrete.effects = expression.effects.clone();
        }
        if callee_is_run
            && let (Some(Type::Promise { .. }), Some(actual)) =
                (concrete.parameters.first(), argument_types.first())
            && matches!(actual, Type::Promise { .. })
        {
            concrete.parameters[0] = actual.clone();
        }
        if self.is_intrinsic_operation(start, callee_end, "thread_spawn") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "thread_spawn".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "size_of") {
            let type_argument = substitutions
                .values()
                .next()
                .cloned()
                .unwrap_or(Type::Error);
            let result_type = Type::Primitive(PrimitiveType::Usize);
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "size_of".into(),
                        operands: vec![Operand::Constant(tn_mir::Constant::Undefined(
                            type_argument,
                        ))],
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        let platform_socket_operation = [
            "platform_sockaddr_family",
            "platform_socket_level",
            "platform_socket_reuse_address_option",
        ]
        .into_iter()
        .find(|operation| self.is_intrinsic_operation(start, callee_end, operation));
        if let Some(platform_socket_operation) = platform_socket_operation {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: platform_socket_operation.into(),
                        operands: Vec::new(),
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "checked_u16") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "checked_u16".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "string_from_raw") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "string_from_raw".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "is_string") {
            let type_argument = substitutions
                .values()
                .next()
                .cloned()
                .unwrap_or(Type::Error);
            let result_type = Type::Primitive(PrimitiveType::Bool);
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "is_string".into(),
                        operands: vec![Operand::Constant(tn_mir::Constant::Undefined(
                            type_argument,
                        ))],
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "is_null") {
            let result_type = Type::Primitive(PrimitiveType::Bool);
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "is_null".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "null_pointer") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "null_pointer".into(),
                        operands: Vec::new(),
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        let raw_call_operation = ["call_raw", "call_raw_void", "call_raw_pointer"]
            .into_iter()
            .find(|operation| self.is_intrinsic_operation(start, callee_end, operation));
        if let Some(raw_call_operation) = raw_call_operation {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: raw_call_operation.into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        let atomic_operation = [
            "atomic_i32_load",
            "atomic_i32_store",
            "atomic_i32_fetch_add",
            "atomic_i32_compare_exchange",
            "atomic_u64_load",
            "atomic_u64_store",
            "atomic_u64_fetch_add",
            "atomic_u64_compare_exchange",
            "atomic_usize_load",
            "atomic_usize_store",
            "atomic_usize_fetch_add",
            "atomic_usize_compare_exchange",
            "atomic_fence",
        ]
        .into_iter()
        .find(|operation| self.is_intrinsic_operation(start, callee_end, operation));
        if let Some(atomic_operation) = atomic_operation {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: atomic_operation.into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "is_copy") {
            let type_argument = substitutions
                .values()
                .next()
                .cloned()
                .unwrap_or(Type::Error);
            let result_type = Type::Primitive(PrimitiveType::Bool);
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "is_copy".into(),
                        operands: vec![Operand::Constant(tn_mir::Constant::Undefined(
                            type_argument,
                        ))],
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "element_initialized") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "element_initialized".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "move_element") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "move_element".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "store_element") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "store_element".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "drop_element") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "drop_element".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        let borrow_operation = [
            "borrow_mut_direct",
            "borrow_mut_storage",
            "borrow_shared_direct",
            "borrow_shared_storage",
        ]
        .into_iter()
        .find(|operation| self.is_intrinsic_operation(start, callee_end, operation));
        if let Some(borrow_operation) = borrow_operation {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: borrow_operation.into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        let byte_address_operation = ["byte_address", "byte_address_i32"]
            .into_iter()
            .find(|operation| self.is_intrinsic_operation(start, callee_end, operation));
        if let Some(byte_address_operation) = byte_address_operation {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: byte_address_operation.into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "byte_read_i32") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "byte_read_i32".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "borrow_element") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "borrow_element".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "borrow_element_mut") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "borrow_element_mut".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "slice_from_raw_parts") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "slice_from_raw_parts".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "str_from_raw_parts") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "str_from_raw_parts".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "slice_length") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "slice_length".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "store_raw") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "store_raw".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "drop_value") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "drop_value".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "u64_to_usize") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "u64_to_usize".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "usize_to_u64") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "usize_to_u64".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "arc_clone") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "arc_clone".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "weak_upgrade") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "weak_upgrade".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if self.is_intrinsic_operation(start, callee_end, "drop_initialized_elements") {
            let result_type = concrete.result.as_ref().clone();
            let destination = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(destination),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "drop_initialized_elements".into(),
                        operands: arguments,
                        ty: result_type.clone(),
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(destination)), result_type));
        }
        if has_direct_member {
            specialize_method_operand(self, &function, &Type::Function(concrete.clone()));
        }
        replace_callable_type(&mut function, Type::Function(concrete.clone()));
        if let Operand::Move(place) = function.clone() {
            // Calling a callable borrows its code/environment pair. The call itself does not
            // consume the callable; ownership is released by its enclosing scope.
            function = Operand::Copy(place);
        }
        let receiver = operand_place(function.clone())
            .and_then(|place| self.bound_receivers.get(&place.local).cloned());
        self.emit_call(function, receiver, &concrete, arguments, start)
    }

    fn lower_call_arguments(
        &mut self,
        ranges: Vec<(usize, usize)>,
        signature: &tn_hir::FunctionType,
    ) -> Option<(Vec<Operand>, Vec<Type>)> {
        let mut arguments = Vec::new();
        let mut argument_types = Vec::new();
        for (index, (argument_start, argument_end)) in ranges.into_iter().enumerate() {
            let expected = signature.parameters.get(index);
            let (argument, actual) =
                self.lower_expression_range(argument_start, argument_end, expected)?;
            arguments.push(self.reborrow_argument(argument, &actual, expected, argument_start));
            argument_types.push(actual);
        }
        Some((arguments, argument_types))
    }

    fn reborrow_argument(
        &mut self,
        operand: Operand,
        actual: &Type,
        expected: Option<&Type>,
        token: usize,
    ) -> Operand {
        let Some(Type::Reference {
            mutable: expected_mutable,
            lifetime: expected_lifetime,
            ..
        }) = expected
        else {
            return operand;
        };
        let Type::Reference {
            referent: actual_referent,
            ..
        } = actual
        else {
            return operand;
        };
        let Some(mut place) = operand_place(operand.clone()) else {
            return operand;
        };
        if place.projection.is_empty() && self.temporary_locals.contains(&place.local) {
            return operand;
        }
        place.projection.push(tn_mir::Projection::Dereference);
        let referent = actual_referent.clone();
        let borrow_type = Type::Reference {
            mutable: *expected_mutable,
            lifetime: expected_lifetime.clone(),
            referent,
        };
        let temporary = self.temporary(borrow_type, self.span(self.tokens[token]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[token]),
        );
        self.statement(
            StatementKind::Borrow {
                destination: temporary,
                kind: if *expected_mutable {
                    BorrowKind::Mutable
                } else {
                    BorrowKind::Shared
                },
                place,
                region: RegionId(self.next_region),
            },
            self.span(self.tokens[token]),
        );
        self.next_region += 1;
        Operand::Move(Place::local(temporary))
    }

    fn generic_call_bounds(&self, start: usize, open: usize) -> Option<(usize, usize)> {
        let less = (start..open).find(|index| self.tokens[*index].kind == TokenKind::Less)?;
        let mut depth = 0_u32;
        for index in less..open {
            match self.tokens[index].kind {
                TokenKind::Less => depth += 1,
                TokenKind::Greater => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return (index + 1 == open).then_some((less, index));
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn is_intrinsic_operation(&self, start: usize, end: usize, operation: &str) -> bool {
        let Some(ResolvedValue::Declaration(declaration)) = self
            .hir_expression_range(start, end)
            .and_then(|expression| expression.resolution)
        else {
            return false;
        };
        self.program
            .intrinsic_operation_for_declaration(declaration)
            .is_some_and(|intrinsic| intrinsic == operation)
    }

    #[allow(clippy::too_many_lines)]
    fn lower_new(&mut self, start: usize, end: usize) -> Option<(Operand, Type)> {
        let open = self.find_top_level(start + 1, end, TokenKind::LeftParen)?;
        if self.matching_token(open, TokenKind::LeftParen, TokenKind::RightParen) != Some(end - 1) {
            return None;
        }
        let result_type = self.hir_expression_range(start, end)?.ty.clone();
        let Type::Nominal(owner, type_arguments) = &result_type else {
            return None;
        };
        let owner = *owner;
        let definition = self.program.definition(owner)?;
        if matches!(definition.data, DefinitionData::Struct { .. }) {
            let schema = self.aggregate_schema(&result_type);
            let ranges = self.argument_ranges(open + 1, end - 1);
            let mut provided = BTreeMap::new();
            if ranges.len() == 1
                && self.tokens[ranges[0].0].kind == TokenKind::LeftBrace
                && let Some(close) =
                    self.matching_token(ranges[0].0, TokenKind::LeftBrace, TokenKind::RightBrace)
            {
                for (field_start, field_end) in self.argument_ranges(ranges[0].0 + 1, close) {
                    let Some(name) = self.tokens.get(field_start).map(|token| self.text(token))
                    else {
                        continue;
                    };
                    let Some(field_index) = schema.iter().position(|(field, _)| field == name)
                    else {
                        continue;
                    };
                    let value_start = self
                        .find_top_level(field_start, field_end, TokenKind::Colon)
                        .map_or(field_start, |colon| colon + 1);
                    provided.insert(field_index, (value_start, field_end));
                }
            } else {
                for (index, range) in ranges.into_iter().enumerate() {
                    provided.insert(index, range);
                }
            }
            let mut fields = Vec::with_capacity(schema.len());
            let mut field_types = Vec::with_capacity(schema.len());
            for (index, (_, field_type)) in schema.iter().enumerate() {
                let field_type = field_type.clone();
                if let Some((field_start, field_end)) = provided.get(&index).copied() {
                    let (field, actual_type) =
                        self.lower_expression_range(field_start, field_end, Some(&field_type))?;
                    fields.push(field);
                    field_types.push(actual_type);
                } else {
                    fields.push(Operand::Constant(tn_mir::Constant::Undefined(
                        field_type.clone(),
                    )));
                    field_types.push(field_type);
                }
            }
            let temporary = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[start]),
            );
            self.statement(
                StatementKind::Assign(
                    Place::local(temporary),
                    Box::new(Rvalue::Aggregate {
                        ty: result_type.clone(),
                        variant: None,
                        fields,
                        field_types,
                    }),
                ),
                self.span(self.tokens[start]),
            );
            return Some((Operand::Move(Place::local(temporary)), result_type));
        }
        let DefinitionData::Class { constructor, .. } = &definition.data else {
            return None;
        };
        let mut substitutions = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace == Namespace::Type)
            .zip(type_arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
            .collect::<BTreeMap<_, _>>();
        let parameter_templates = constructor.as_ref().map_or_else(Vec::new, |constructor| {
            constructor
                .function
                .parameters
                .iter()
                .map(|parameter| substitute_mir_type(&parameter.ty, &substitutions))
                .collect()
        });
        let ranges = self.argument_ranges(open + 1, end - 1);
        let mut lowered_arguments = Vec::new();
        for (index, (argument_start, argument_end)) in ranges.into_iter().enumerate() {
            let (argument, actual) = self.lower_expression_range(
                argument_start,
                argument_end,
                parameter_templates.get(index),
            )?;
            if let Some(parameter) = parameter_templates.get(index) {
                infer_mir_substitutions(parameter, &actual, &mut substitutions);
            }
            lowered_arguments.push((argument, actual, argument_start));
        }
        let parameters = parameter_templates
            .iter()
            .map(|parameter| substitute_mir_type(parameter, &substitutions))
            .collect::<Vec<_>>();
        let concrete_type_arguments = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace == Namespace::Type)
            .map(|parameter| {
                substitutions
                    .get(&parameter.name)
                    .cloned()
                    .unwrap_or_else(|| Type::Generic(parameter.name.clone()))
            })
            .collect::<Vec<_>>();
        let concrete_result_type = Type::Nominal(owner, concrete_type_arguments);
        let effects = constructor
            .as_ref()
            .map_or_else(Vec::new, |constructor| constructor.function.effects.clone());
        let signature = tn_hir::FunctionType {
            parameters,
            result: Box::new(concrete_result_type),
            effects,
            generics: Vec::new(),
            is_async: false,
            is_unsafe: constructor
                .as_ref()
                .is_some_and(|constructor| constructor.function.is_unsafe),
        };
        let mut arguments = Vec::new();
        for (index, (argument, actual, argument_start)) in lowered_arguments.into_iter().enumerate()
        {
            let argument = self.reborrow_argument(
                argument,
                &actual,
                signature.parameters.get(index),
                argument_start,
            );
            if let Some(parameter @ Type::Optional(inner)) = signature.parameters.get(index)
                && &actual == inner.as_ref()
            {
                let optional = self.temporary(parameter.clone(), self.span(self.tokens[start]));
                self.statement(
                    StatementKind::StorageLive(optional),
                    self.span(self.tokens[start]),
                );
                self.statement(
                    StatementKind::Assign(
                        Place::local(optional),
                        Box::new(Rvalue::Aggregate {
                            ty: parameter.clone(),
                            variant: Some(1),
                            fields: vec![argument],
                            field_types: vec![actual],
                        }),
                    ),
                    self.span(self.tokens[start]),
                );
                self.statement(
                    StatementKind::SetDiscriminant(Place::local(optional), 1),
                    self.span(self.tokens[start]),
                );
                arguments.push(Operand::Move(Place::local(optional)));
            } else {
                arguments.push(argument);
            }
        }
        for parameter in signature.parameters.iter().skip(arguments.len()) {
            if matches!(parameter, Type::Optional(_)) {
                arguments.push(Operand::Constant(tn_mir::Constant::Undefined(
                    parameter.clone(),
                )));
            }
        }
        let function = Operand::Constant(tn_mir::Constant::Constructor {
            owner,
            member: constructor.as_ref().map(|constructor| constructor.id),
            ty: Type::Function(signature.clone()),
        });
        self.emit_call(function, None, &signature, arguments, start)
    }

    fn emit_call(
        &mut self,
        function: Operand,
        receiver: Option<Operand>,
        signature: &tn_hir::FunctionType,
        arguments: Vec<Operand>,
        start: usize,
    ) -> Option<(Operand, Type)> {
        let call_temporaries = std::iter::once(&function)
            .chain(arguments.iter())
            .filter_map(operand_place_ref)
            .filter(|place| {
                place.projection.is_empty() && self.temporary_locals.contains(&place.local)
            })
            .map(|place| place.local)
            .collect::<Vec<_>>();
        let result_type = signature.result.as_ref().clone();
        let destination = (result_type != Type::Primitive(PrimitiveType::Void)).then(|| {
            let local = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[start]),
            );
            Place::local(local)
        });
        let success = self.new_block();
        let error =
            (!signature.is_async && !signature.effects.is_empty()).then(|| self.new_block());
        let error_destination = error.map(|_| {
            let mut effects = signature.effects.clone();
            effects.sort();
            effects.dedup();
            let local = self.temporary(Type::ErrorUnion(effects), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[start]),
            );
            Place::local(local)
        });
        self.terminate(
            TerminatorKind::Call {
                function,
                receiver,
                arguments,
                destination: destination.clone(),
                error_destination: error_destination.clone(),
                success: Self::block_id(success),
                error: error.map(Self::block_id),
            },
            self.span(self.tokens[start]),
        );
        if let Some(error) = error {
            self.current = error;
            for local in &call_temporaries {
                self.statement(
                    StatementKind::StorageDead(*local),
                    self.span(self.tokens[start]),
                );
            }
            let error_value = Operand::Move(error_destination.expect("fallible call payload"));
            self.route_error(error_value, &signature.effects, start);
        }
        self.current = success;
        for local in &call_temporaries {
            self.statement(
                StatementKind::StorageDead(*local),
                self.span(self.tokens[start]),
            );
        }
        destination.map(|destination| (Operand::Move(destination), result_type))
    }

    fn route_error(&mut self, error_value: Operand, effects: &[DeclarationId], start: usize) {
        if !self.disposing_async {
            let scope_depth = self
                .error_contexts
                .last()
                .map_or(0, |context| context.scope_depth);
            let managed = self.active_async_disposals_from(scope_depth);
            self.lower_async_disposals(&managed);
        }
        if let Some(context) = self.error_contexts.last().cloned() {
            self.statement(
                StatementKind::Assign(
                    Place::local(context.payload),
                    Box::new(Rvalue::Cast {
                        operand: error_value,
                        ty: Type::ErrorUnion(context.effects),
                        kind: CastKind::ErrorUnion,
                    }),
                ),
                self.span(self.tokens[start]),
            );
            self.terminate(
                TerminatorKind::Goto(context.dispatch),
                self.span(self.tokens[start]),
            );
        } else if effects.iter().all(|effect| self.return_effect(*effect)) {
            self.terminate(
                TerminatorKind::Throw(error_value),
                self.span(self.tokens[start]),
            );
        } else {
            self.terminate(TerminatorKind::Unreachable, self.span(self.tokens[start]));
        }
    }

    fn lower_index(&mut self, start: usize, open: usize, end: usize) -> Option<(Operand, Type)> {
        let (collection, collection_type) = self.lower_expression_range(start, open, None)?;
        let mut collection = operand_place(collection)?;
        let access_type = if let Type::Reference { referent, .. } = &collection_type {
            collection.projection.push(tn_mir::Projection::Dereference);
            referent.as_ref()
        } else {
            &collection_type
        };
        let index_type = Type::Primitive(PrimitiveType::Usize);
        let index = self
            .lower_expression_range(open + 1, end - 1, Some(&index_type))?
            .0;
        let ty = self
            .hir_expression_range(start, end)
            .map(|expression| expression.ty.clone())
            .or(match access_type {
                Type::Array(element, _) | Type::Slice(element) => Some(element.as_ref().clone()),
                _ => None,
            })?;
        let temporary = self.temporary(ty.clone(), self.span(self.tokens[open]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[open]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::CheckedIndex { collection, index }),
            ),
            self.span(self.tokens[open]),
        );
        Some((Operand::Move(Place::local(temporary)), ty))
    }

    fn lower_field(&mut self, start: usize, dot: usize, end: usize) -> Option<(Operand, Type)> {
        let expression = self.hir_expression_range(start, end)?;
        let Some(ResolvedValue::Member(member)) = expression.resolution else {
            return None;
        };
        let member_type = expression.ty.clone();
        if self.method_receiver(member) == Some(tn_hir::ReceiverMode::Static) {
            return Some((
                Operand::Constant(tn_mir::Constant::Method {
                    owner: self.method_owner(member)?,
                    member,
                    ty: member_type.clone(),
                }),
                member_type,
            ));
        }
        if self
            .enum_variant(member)
            .is_some_and(|(_, _, variant)| variant.fields.is_empty())
        {
            return self.lower_enum_variant(start, end, end, member);
        }
        let (owner, owner_type) = self.lower_expression_range(start, dot, None)?;
        self.lower_member_from(owner, &owner_type, member, member_type, dot)
    }

    fn direct_member_access(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if let Some(dot) = self.find_top_level(start, end, TokenKind::Dot)
            && dot + 2 == end
        {
            return Some((dot, dot + 1));
        }
        let open = self.find_top_level(start, end, TokenKind::LeftBracket)?;
        let close = self.matching_token(open, TokenKind::LeftBracket, TokenKind::RightBracket)?;
        (open > start
            && close + 1 == end
            && close == open + 4
            && self.tokens.get(open + 1)?.kind == TokenKind::Identifier
            && self.tokens.get(open + 2)?.kind == TokenKind::Dot
            && self.tokens.get(open + 3)?.kind == TokenKind::Identifier)
            .then_some((open, open + 3))
    }

    fn lower_computed_member(
        &mut self,
        start: usize,
        open: usize,
        end: usize,
    ) -> Option<(Operand, Type)> {
        let expression = self.hir_expression_range(start, end)?;
        let Some(ResolvedValue::Member(member)) = expression.resolution else {
            return None;
        };
        let member_type = expression.ty.clone();
        if self.method_receiver(member) == Some(tn_hir::ReceiverMode::Static) {
            return Some((
                Operand::Constant(tn_mir::Constant::Method {
                    owner: self.method_owner(member)?,
                    member,
                    ty: member_type.clone(),
                }),
                member_type,
            ));
        }
        let (owner, owner_type) = self.lower_expression_range(start, open, None)?;
        self.lower_member_from(owner, &owner_type, member, member_type, open + 3)
    }

    fn lower_string_length(&mut self, start: usize, end: usize) -> Option<(Operand, Type)> {
        let dot = self.find_top_level(start, end, TokenKind::Dot)?;
        let (receiver, receiver_type) = self.lower_expression_range(start, dot, None)?;
        let receiver = self.materialize_operand(receiver, receiver_type, self.tokens[dot]);
        let result_type = Type::Primitive(PrimitiveType::Usize);
        let temporary = self.temporary(result_type.clone(), self.span(self.tokens[dot + 1]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[dot + 1]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::RawOperation {
                    operation: "string_scalar_length".into(),
                    operands: vec![Operand::Copy(receiver)],
                    ty: result_type.clone(),
                }),
            ),
            self.span(self.tokens[dot + 1]),
        );
        Some((Operand::Move(Place::local(temporary)), result_type))
    }

    fn lower_string_byte_length(&mut self, start: usize, end: usize) -> Option<(Operand, Type)> {
        let dot = self.find_top_level(start, end, TokenKind::Dot)?;
        let (receiver, receiver_type) = self.lower_expression_range(start, dot, None)?;
        let receiver = self.materialize_operand(receiver, receiver_type, self.tokens[dot]);
        let result_type = Type::Primitive(PrimitiveType::Usize);
        let temporary = self.temporary(result_type.clone(), self.span(self.tokens[dot + 1]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[dot + 1]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::RawOperation {
                    operation: "string_byte_length".into(),
                    operands: vec![Operand::Copy(receiver)],
                    ty: result_type.clone(),
                }),
            ),
            self.span(self.tokens[dot + 1]),
        );
        Some((Operand::Move(Place::local(temporary)), result_type))
    }

    #[allow(clippy::too_many_lines)]
    fn lower_member_chain(&mut self, start: usize, end: usize) -> Option<(Operand, Type)> {
        let dot = self.find_top_level(start, end, TokenKind::Dot)?;
        let first_member_end = dot + 2;
        let (member, member_type) = {
            let expression = self.hir_expression_range(start, first_member_end)?;
            let Some(ResolvedValue::Member(member)) = expression.resolution else {
                return None;
            };
            (member, expression.ty.clone())
        };
        let (owner, owner_type) = self.lower_expression_range(start, dot, None)?;
        let (mut value, mut value_type) =
            self.lower_member_from(owner, &owner_type, member, member_type, dot + 1)?;
        let mut cursor = first_member_end;
        while cursor < end {
            match self.tokens[cursor].kind {
                TokenKind::LeftBracket => {
                    let close = self.matching_token(
                        cursor,
                        TokenKind::LeftBracket,
                        TokenKind::RightBracket,
                    )?;
                    let index_type = Type::Primitive(PrimitiveType::Usize);
                    let index = self
                        .lower_expression_range(cursor + 1, close, Some(&index_type))?
                        .0;
                    let collection = operand_place(value)?;
                    let element_type = self
                        .hir_expression_range(start, close + 1)
                        .map(|expression| expression.ty.clone())
                        .or_else(|| match &value_type {
                            Type::Array(element, _) | Type::Slice(element) => {
                                Some(element.as_ref().clone())
                            }
                            _ => None,
                        })?;
                    let temporary =
                        self.temporary(element_type.clone(), self.span(self.tokens[cursor]));
                    self.statement(
                        StatementKind::StorageLive(temporary),
                        self.span(self.tokens[cursor]),
                    );
                    self.statement(
                        StatementKind::Assign(
                            Place::local(temporary),
                            Box::new(Rvalue::CheckedIndex { collection, index }),
                        ),
                        self.span(self.tokens[cursor]),
                    );
                    value = Operand::Move(Place::local(temporary));
                    value_type = element_type;
                    cursor = close + 1;
                }
                TokenKind::Dot => {
                    let member_token = cursor + 1;
                    let member_end = member_token + 1;
                    let (member, member_type) = {
                        let expression = self.hir_expression_range(start, member_end)?;
                        let Some(ResolvedValue::Member(member)) = expression.resolution else {
                            return None;
                        };
                        (member, expression.ty.clone())
                    };
                    (value, value_type) = self.lower_member_from(
                        value,
                        &value_type,
                        member,
                        member_type,
                        member_token,
                    )?;
                    cursor = member_end;
                }
                TokenKind::LeftParen => {
                    let close =
                        self.matching_token(cursor, TokenKind::LeftParen, TokenKind::RightParen)?;
                    let Type::Function(signature) = value_type.clone() else {
                        return None;
                    };
                    let arguments = self
                        .argument_ranges(cursor + 1, close)
                        .into_iter()
                        .enumerate()
                        .map(|(index, (argument_start, argument_end))| {
                            let (argument, actual) = self.lower_expression_range(
                                argument_start,
                                argument_end,
                                signature.parameters.get(index),
                            )?;
                            Some(self.reborrow_argument(
                                argument,
                                &actual,
                                signature.parameters.get(index),
                                argument_start,
                            ))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    let receiver = operand_place(value.clone())
                        .and_then(|place| self.bound_receivers.get(&place.local).cloned());
                    (value, value_type) =
                        self.emit_call(value, receiver, &signature, arguments, cursor)?;
                    cursor = close + 1;
                }
                _ => return None,
            }
        }
        Some((value, value_type))
    }

    fn lower_member_from(
        &mut self,
        owner: Operand,
        owner_type: &Type,
        member: MemberId,
        member_type: Type,
        member_token: usize,
    ) -> Option<(Operand, Type)> {
        let (access_type, borrowed) = match owner_type {
            Type::Reference {
                mutable, referent, ..
            } => (referent.as_ref(), Some(*mutable)),
            ty => (ty, None),
        };
        let member_type = self.specialize_member_type(access_type, member_type);
        if matches!(member_type, Type::Function(_))
            && let Some(receiver_mode) = self.method_receiver(member)
        {
            let mut owner =
                self.materialize_operand(owner, owner_type.clone(), self.tokens[member_token]);
            if borrowed.is_some() {
                owner.projection.push(tn_mir::Projection::Dereference);
            }
            let bound_receiver = if receiver_mode == ReceiverMode::Move {
                Operand::Move(owner.clone())
            } else {
                Operand::Copy(owner.clone())
            };
            let lookup = match access_type {
                Type::Nominal(declaration, _)
                    if self.class_vtable_slot(*declaration, member).is_some()
                        && !self.method_has_decorator(*declaration, member) =>
                {
                    Rvalue::VtableLookup {
                        object: owner,
                        implementation: *declaration,
                        member,
                        slot: self.class_vtable_slot(*declaration, member)?,
                        receiver: receiver_mode,
                        ty: member_type.clone(),
                    }
                }
                Type::Nominal(_, _) => {
                    let (implementation, receiver) = self.direct_method(member)?;
                    Rvalue::DirectMethod {
                        object: owner,
                        implementation,
                        member,
                        receiver,
                        ty: member_type.clone(),
                    }
                }
                ty if self.program.intrinsic_type_declaration(ty).is_some() => {
                    let (implementation, receiver) = self.direct_method(member)?;
                    Rvalue::DirectMethod {
                        object: owner,
                        implementation,
                        member,
                        receiver,
                        ty: member_type.clone(),
                    }
                }
                Type::DynamicInterface(interface, _) => Rvalue::WitnessLookup {
                    object: owner,
                    interface: *interface,
                    slot: self.interface_witness_slot(*interface, member)?,
                    receiver: receiver_mode,
                    ty: member_type.clone(),
                },
                _ => return None,
            };
            let temporary =
                self.temporary(member_type.clone(), self.span(self.tokens[member_token]));
            self.statement(
                StatementKind::StorageLive(temporary),
                self.span(self.tokens[member_token]),
            );
            self.statement(
                StatementKind::Assign(Place::local(temporary), Box::new(lookup)),
                self.span(self.tokens[member_token]),
            );
            self.bound_receivers.insert(temporary, bound_receiver);
            return Some((Operand::Move(Place::local(temporary)), member_type));
        }
        self.lower_field_member_from(
            owner,
            access_type,
            borrowed,
            member,
            member_type,
            member_token,
        )
    }

    fn specialize_member_type(&self, owner: &Type, member_type: Type) -> Type {
        let Type::Nominal(declaration, arguments) = owner else {
            return member_type;
        };
        let Some(definition) = self.program.definition(*declaration) else {
            return member_type;
        };
        let substitutions = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace == Namespace::Type)
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
            .collect::<BTreeMap<_, _>>();
        substitute_mir_type(&member_type, &substitutions)
    }

    fn lower_field_member_from(
        &mut self,
        owner: Operand,
        access_type: &Type,
        borrowed: Option<bool>,
        member: MemberId,
        member_type: Type,
        member_token: usize,
    ) -> Option<(Operand, Type)> {
        let mut place = operand_place(owner)?;
        if borrowed.is_some() {
            place.projection.push(tn_mir::Projection::Dereference);
        }
        let index = self.field_index(access_type, member)?;
        let field_type = self.resolved_field_type(access_type, member)?;
        place.projection.push(tn_mir::Projection::Field {
            index,
            ty: field_type.clone(),
        });
        if let Some(owner_mutable) = borrowed {
            if let (Type::Optional(field_value), Type::Optional(member_value)) =
                (&field_type, &member_type)
                && let Type::Reference {
                    mutable, referent, ..
                } = member_value.as_ref()
                && referent.as_ref() == field_value.as_ref()
            {
                return self.lower_optional_field_reborrow(
                    place,
                    member_type.clone(),
                    *mutable && owner_mutable,
                    member_token,
                );
            }
            if let Type::Reference {
                mutable, referent, ..
            } = &member_type
                && referent.as_ref() == &field_type
            {
                let destination =
                    self.temporary(member_type.clone(), self.span(self.tokens[member_token]));
                self.statement(
                    StatementKind::StorageLive(destination),
                    self.span(self.tokens[member_token]),
                );
                self.statement(
                    StatementKind::Borrow {
                        destination,
                        kind: if *mutable && owner_mutable {
                            BorrowKind::Mutable
                        } else {
                            BorrowKind::Shared
                        },
                        place,
                        region: RegionId(self.next_region),
                    },
                    self.span(self.tokens[member_token]),
                );
                self.next_region += 1;
                return Some((Operand::Move(Place::local(destination)), member_type));
            }
        }
        let operand = if self.ownership_facts.is_copy(&member_type) {
            Operand::Copy(place)
        } else {
            Operand::Move(place)
        };
        Some((operand, member_type))
    }

    fn lower_optional_field_reborrow(
        &mut self,
        field: Place,
        result_type: Type,
        mutable: bool,
        member_token: usize,
    ) -> Option<(Operand, Type)> {
        let Type::Optional(result_value) = &result_type else {
            return None;
        };
        let Type::Reference { .. } = result_value.as_ref() else {
            return None;
        };
        let destination = self.temporary(result_type.clone(), self.span(self.tokens[member_token]));
        self.statement(
            StatementKind::StorageLive(destination),
            self.span(self.tokens[member_token]),
        );
        let absent = self.new_block();
        let present = self.new_block();
        let join = self.new_block();
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(field.clone()),
                targets: vec![(1, Self::block_id(present))],
                otherwise: Self::block_id(absent),
            },
            self.span(self.tokens[member_token]),
        );
        self.current = absent;
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Use(Operand::Constant(tn_mir::Constant::Undefined(
                    result_type.clone(),
                )))),
            ),
            self.span(self.tokens[member_token]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[member_token]),
        );
        self.current = present;
        let mut payload = field;
        payload.projection.push(tn_mir::Projection::Downcast(1));
        let reference = self.temporary(
            result_value.as_ref().clone(),
            self.span(self.tokens[member_token]),
        );
        self.statement(
            StatementKind::StorageLive(reference),
            self.span(self.tokens[member_token]),
        );
        self.statement(
            StatementKind::Borrow {
                destination: reference,
                kind: if mutable {
                    BorrowKind::Mutable
                } else {
                    BorrowKind::Shared
                },
                place: payload,
                region: RegionId(self.next_region),
            },
            self.span(self.tokens[member_token]),
        );
        self.next_region += 1;
        self.statement(
            StatementKind::Assign(
                Place::local(destination),
                Box::new(Rvalue::Aggregate {
                    ty: result_type.clone(),
                    variant: Some(1),
                    fields: vec![Operand::Move(Place::local(reference))],
                    field_types: vec![result_value.as_ref().clone()],
                }),
            ),
            self.span(self.tokens[member_token]),
        );
        self.statement(
            StatementKind::SetDiscriminant(Place::local(destination), 1),
            self.span(self.tokens[member_token]),
        );
        self.terminate(
            TerminatorKind::Goto(Self::block_id(join)),
            self.span(self.tokens[member_token]),
        );
        self.current = join;
        Some((Operand::Move(Place::local(destination)), result_type))
    }

    fn direct_method(&self, member: MemberId) -> Option<(DeclarationId, tn_hir::ReceiverMode)> {
        self.program.definitions.iter().find_map(|definition| {
            let (DefinitionData::Struct { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Class { methods, .. }
            | DefinitionData::Implementation { methods, .. }) = &definition.data
            else {
                return None;
            };
            methods
                .iter()
                .find(|method| method.id == member)
                .map(|method| (definition.declaration, method.receiver))
        })
    }

    fn method_receiver(&self, member: MemberId) -> Option<tn_hir::ReceiverMode> {
        self.program
            .definitions
            .iter()
            .find_map(|definition| match &definition.data {
                DefinitionData::Struct { methods, .. }
                | DefinitionData::Enum { methods, .. }
                | DefinitionData::Class { methods, .. }
                | DefinitionData::Interface { methods, .. }
                | DefinitionData::Implementation { methods, .. }
                | DefinitionData::Extern { functions: methods } => methods
                    .iter()
                    .find(|method| method.id == member)
                    .map(|method| method.receiver),
                _ => None,
            })
    }

    fn method_owner(&self, member: MemberId) -> Option<DeclarationId> {
        self.program.definitions.iter().find_map(|definition| {
            let (DefinitionData::Struct { methods, .. }
            | DefinitionData::Enum { methods, .. }
            | DefinitionData::Class { methods, .. }
            | DefinitionData::Interface { methods, .. }
            | DefinitionData::Implementation { methods, .. }
            | DefinitionData::Extern { functions: methods }) = &definition.data
            else {
                return None;
            };
            methods
                .iter()
                .any(|method| method.id == member)
                .then_some(definition.declaration)
        })
    }

    fn enum_variant(&self, member: MemberId) -> Option<(DeclarationId, u32, &tn_hir::EnumVariant)> {
        self.program.definitions.iter().find_map(|definition| {
            let DefinitionData::Enum { variants, .. } = &definition.data else {
                return None;
            };
            variants.iter().enumerate().find_map(|(index, variant)| {
                (variant.id == member).then(|| {
                    (
                        definition.declaration,
                        u32::try_from(index).expect("enum variant limit"),
                        variant,
                    )
                })
            })
        })
    }

    fn lower_enum_variant(
        &mut self,
        start: usize,
        arguments_start: usize,
        arguments_end: usize,
        member: MemberId,
    ) -> Option<(Operand, Type)> {
        let (declaration, index, variant) = self.enum_variant(member)?;
        let mut field_types = variant
            .fields
            .iter()
            .map(|field| field.ty.clone())
            .collect::<Vec<_>>();
        let result_type = self
            .hir_expression_range(
                start,
                arguments_end + usize::from(arguments_start < arguments_end),
            )
            .map(|expression| expression.ty.clone())
            .filter(|ty| matches!(ty, Type::Nominal(_, _)))
            .unwrap_or(Type::Nominal(declaration, Vec::new()));
        if let Type::Nominal(_, arguments) = &result_type
            && let Some(definition) = self.program.definition(declaration)
        {
            let substitutions = definition
                .generics
                .iter()
                .filter(|parameter| parameter.namespace == Namespace::Type)
                .zip(arguments)
                .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
                .collect::<BTreeMap<_, _>>();
            field_types = field_types
                .iter()
                .map(|field| substitute_mir_type(field, &substitutions))
                .collect();
        }
        let ranges = self.argument_ranges(arguments_start, arguments_end);
        let mut fields = Vec::new();
        for (field_index, (field_start, field_end)) in ranges.into_iter().enumerate() {
            fields.push(
                self.lower_expression_range(field_start, field_end, field_types.get(field_index))?
                    .0,
            );
        }
        let temporary = self.temporary(result_type.clone(), self.span(self.tokens[start]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[start]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::Aggregate {
                    ty: result_type.clone(),
                    variant: Some(index),
                    fields,
                    field_types,
                }),
            ),
            self.span(self.tokens[start]),
        );
        self.statement(
            StatementKind::SetDiscriminant(Place::local(temporary), index),
            self.span(self.tokens[start]),
        );
        Some((Operand::Move(Place::local(temporary)), result_type))
    }

    fn class_vtable_slot(&self, declaration: DeclarationId, member: MemberId) -> Option<u32> {
        self.class_vtable_methods(declaration)
            .iter()
            .position(|(_, candidate)| *candidate == member)
            .and_then(|index| u32::try_from(index).ok())
            .and_then(|index| index.checked_add(1))
    }

    fn method_has_decorator(&self, declaration: DeclarationId, member: MemberId) -> bool {
        self.program
            .definition(declaration)
            .is_some_and(|definition| match &definition.data {
                DefinitionData::Class { methods, .. }
                | DefinitionData::Struct { methods, .. }
                | DefinitionData::Implementation { methods, .. } => methods
                    .iter()
                    .any(|method| method.id == member && !method.attributes.is_empty()),
                _ => false,
            })
    }

    fn class_vtable_methods(&self, declaration: DeclarationId) -> Vec<(String, MemberId)> {
        let Some(DefinitionData::Class { base, methods, .. }) = self
            .program
            .definition(declaration)
            .map(|definition| &definition.data)
        else {
            return Vec::new();
        };
        let mut slots = base.map_or_else(Vec::new, |base| self.class_vtable_methods(base));
        for method in methods {
            if let Some(slot) = slots.iter_mut().find(|(name, _)| *name == method.name) {
                slot.1 = method.id;
            } else {
                slots.push((method.name.clone(), method.id));
            }
        }
        slots
    }

    fn interface_witness_slot(&self, interface: DeclarationId, member: MemberId) -> Option<u32> {
        let DefinitionData::Interface { methods, .. } = &self.program.definition(interface)?.data
        else {
            return None;
        };
        methods
            .iter()
            .position(|method| method.id == member)
            .and_then(|index| u32::try_from(index).ok())
    }

    fn field_index(&self, owner: &Type, member: MemberId) -> Option<u32> {
        let Type::Nominal(declaration, _) = owner else {
            return None;
        };
        self.field_index_in(*declaration, member)
    }

    fn resolved_field_type(&self, owner: &Type, member: MemberId) -> Option<Type> {
        let Type::Nominal(declaration, arguments) = owner else {
            return None;
        };
        self.resolved_field_type_in(*declaration, arguments, member)
    }

    fn resolved_field_type_in(
        &self,
        declaration: DeclarationId,
        arguments: &[Type],
        member: MemberId,
    ) -> Option<Type> {
        let definition = self.program.definition(declaration)?;
        let substitutions = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace != Namespace::Value)
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
            .collect::<BTreeMap<_, _>>();
        match &definition.data {
            DefinitionData::Struct { fields, .. } => fields
                .iter()
                .find(|field| field.id == member)
                .map(|field| substitute_mir_type(&field.ty, &substitutions)),
            DefinitionData::Class { base, fields, .. } => fields
                .iter()
                .find(|field| field.id == member)
                .map(|field| substitute_mir_type(&field.ty, &substitutions))
                .or_else(|| base.and_then(|base| self.resolved_field_type_in(base, &[], member))),
            _ => None,
        }
    }

    fn field_index_in(&self, declaration: DeclarationId, member: MemberId) -> Option<u32> {
        let definition = self.program.definition(declaration)?;
        match &definition.data {
            DefinitionData::Struct { fields, .. } => fields
                .iter()
                .position(|field| field.id == member)
                .and_then(|index| u32::try_from(index).ok()),
            DefinitionData::Class { base, fields, .. } => {
                let base_count = base.map_or(0, |base| self.field_count(base));
                if let Some(base) = base
                    && let Some(index) = self.field_index_in(*base, member)
                {
                    return Some(index);
                }
                fields
                    .iter()
                    .position(|field| field.id == member)
                    .and_then(|index| u32::try_from(index).ok())
                    .and_then(|index| base_count.checked_add(index))
            }
            _ => None,
        }
    }

    fn field_count(&self, declaration: DeclarationId) -> u32 {
        let Some(definition) = self.program.definition(declaration) else {
            return 0;
        };
        match &definition.data {
            DefinitionData::Struct { fields, .. } => {
                u32::try_from(fields.len()).unwrap_or(u32::MAX)
            }
            DefinitionData::Class { base, fields, .. } => base
                .map_or(0, |base| self.field_count(base))
                .saturating_add(u32::try_from(fields.len()).unwrap_or(u32::MAX)),
            _ => 0,
        }
    }

    fn lower_aggregate(
        &mut self,
        start: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let ty = self
            .hir_expression_range(start, end)
            .map(|expression| expression.ty.clone())
            .or_else(|| expected.cloned())?;
        let mut ranges = self.argument_ranges(start + 1, end - 1);
        let schema = self.aggregate_schema(&ty);
        if self.tokens[start].kind == TokenKind::LeftBrace && !schema.is_empty() {
            ranges.sort_by_key(|(field_start, _)| {
                let name = self.text(self.tokens[*field_start]);
                schema
                    .iter()
                    .position(|(field, _)| field == name)
                    .unwrap_or(usize::MAX)
            });
        }
        let mut fields = Vec::new();
        let mut field_types = Vec::new();
        for (index, (mut field_start, field_end)) in ranges.into_iter().enumerate() {
            if let Some(colon) = self.find_top_level(field_start, field_end, TokenKind::Colon) {
                field_start = colon + 1;
            }
            let field_type = schema
                .get(index)
                .map(|(_, ty)| ty.clone())
                .or_else(|| aggregate_field_type(&ty, index));
            let (field, actual_type) =
                self.lower_expression_range(field_start, field_end, field_type.as_ref())?;
            fields.push(field);
            field_types.push(field_type.unwrap_or(actual_type));
        }
        let temporary = self.temporary(ty.clone(), self.span(self.tokens[start]));
        self.statement(
            StatementKind::StorageLive(temporary),
            self.span(self.tokens[start]),
        );
        self.statement(
            StatementKind::Assign(
                Place::local(temporary),
                Box::new(Rvalue::Aggregate {
                    ty: ty.clone(),
                    variant: None,
                    fields,
                    field_types,
                }),
            ),
            self.span(self.tokens[start]),
        );
        Some((Operand::Move(Place::local(temporary)), ty))
    }

    fn aggregate_schema(&self, ty: &Type) -> Vec<(String, Type)> {
        let Type::Nominal(declaration, arguments) = ty else {
            return Vec::new();
        };
        let Some(definition) = self.program.definition(*declaration) else {
            return Vec::new();
        };
        let substitutions = definition
            .generics
            .iter()
            .filter(|parameter| parameter.namespace != Namespace::Value)
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
            .collect::<BTreeMap<_, _>>();
        match &definition.data {
            DefinitionData::Struct { fields, .. } => fields
                .iter()
                .map(|field| {
                    (
                        field.name.clone(),
                        substitute_mir_type(&field.ty, &substitutions),
                    )
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_match(
        &mut self,
        start: usize,
        end: usize,
        expected: Option<&Type>,
    ) -> Option<(Operand, Type)> {
        let condition_open = start + 1;
        let condition_close =
            self.matching_token(condition_open, TokenKind::LeftParen, TokenKind::RightParen)?;
        let (scrutinee, scrutinee_type) =
            self.lower_expression_range(condition_open + 1, condition_close, None)?;
        let scrutinee =
            self.materialize_operand(scrutinee, scrutinee_type.clone(), self.tokens[start]);
        let body_open = condition_close + 1;
        if self.tokens.get(body_open)?.kind != TokenKind::LeftBrace {
            return None;
        }
        let arms = self.match_arms(body_open + 1, end.checked_sub(1)?)?;
        let result_type = self
            .hir_expression_range(start, end)
            .map(|expression| expression.ty.clone())
            .or_else(|| expected.cloned())?;
        let destination = (result_type != Type::Primitive(PrimitiveType::Void)).then(|| {
            let local = self.temporary(result_type.clone(), self.span(self.tokens[start]));
            self.statement(
                StatementKind::StorageLive(local),
                self.span(self.tokens[start]),
            );
            local
        });
        let arm_blocks = (0..arms.len())
            .map(|_| self.new_block())
            .collect::<Vec<_>>();
        let otherwise = self.new_block();
        let join = self.new_block();
        let mut targets = Vec::new();
        let mut default = Self::block_id(otherwise);
        for (arm, block) in arms.iter().zip(&arm_blocks) {
            if let Some(value) = self.match_pattern_value(arm, &scrutinee_type) {
                targets.push((value, Self::block_id(*block)));
            } else if default == Self::block_id(otherwise) {
                default = Self::block_id(*block);
            }
        }
        targets.sort_by_key(|(value, _)| *value);
        targets.dedup_by_key(|(value, _)| *value);
        self.terminate(
            TerminatorKind::Switch {
                value: Operand::Copy(scrutinee.clone()),
                targets,
                otherwise: default,
            },
            self.span(self.tokens[start]),
        );
        let saved_index = self.index;
        for (arm_index, (arm, block)) in arms.iter().zip(&arm_blocks).enumerate() {
            self.current = *block;
            self.bind_match_payload(arm, &scrutinee, &scrutinee_type);
            if let Some((guard_start, guard_end)) = arm.guard {
                let guard = self
                    .lower_expression_range(
                        guard_start,
                        guard_end,
                        Some(&Type::Primitive(PrimitiveType::Bool)),
                    )
                    .map_or_else(
                        || Operand::Constant(tn_mir::Constant::Bool(false)),
                        |(operand, _)| operand,
                    );
                let value_block = self.new_block();
                self.terminate(
                    TerminatorKind::Switch {
                        value: guard,
                        targets: vec![(1, Self::block_id(value_block))],
                        otherwise: self.match_fallback(
                            arm_index,
                            &arms,
                            &arm_blocks,
                            &scrutinee_type,
                            otherwise,
                        ),
                    },
                    self.span(self.tokens[guard_start]),
                );
                self.current = value_block;
            }
            if arm.block {
                self.index = arm.value_start;
                self.lower_statement();
            } else if let Some((operand, source_type)) =
                self.lower_expression_range(arm.value_start, arm.value_end, Some(&result_type))
                && let Some(destination) = destination
            {
                let rvalue = if source_type == result_type {
                    Rvalue::Use(operand)
                } else {
                    Rvalue::Cast {
                        operand,
                        ty: result_type.clone(),
                        kind: mir_cast_kind(&source_type, &result_type),
                    }
                };
                self.statement(
                    StatementKind::Assign(Place::local(destination), Box::new(rvalue)),
                    self.span(self.tokens[arm.value_start]),
                );
            }
            if self.blocks[self.current].terminator.is_none() {
                self.terminate(
                    TerminatorKind::Goto(Self::block_id(join)),
                    self.span(self.tokens[arm.pattern_start]),
                );
            }
        }
        self.current = otherwise;
        if self.blocks[self.current].terminator.is_none() {
            self.terminate(TerminatorKind::Unreachable, self.span(self.tokens[start]));
        }
        self.current = join;
        self.index = saved_index;
        let value = destination.map_or_else(
            || Operand::Constant(tn_mir::Constant::Undefined(result_type.clone())),
            |destination| Operand::Move(Place::local(destination)),
        );
        Some((value, result_type))
    }

    fn match_fallback(
        &self,
        current: usize,
        arms: &[MatchArm],
        blocks: &[usize],
        scrutinee: &Type,
        otherwise: usize,
    ) -> BasicBlockId {
        let current_value = self.match_pattern_value(&arms[current], scrutinee);
        arms.iter()
            .enumerate()
            .skip(current + 1)
            .find(|(_, arm)| {
                let candidate = self.match_pattern_value(arm, scrutinee);
                candidate == current_value || candidate.is_none()
            })
            .map_or_else(
                || Self::block_id(otherwise),
                |(index, _)| Self::block_id(blocks[index]),
            )
    }

    fn materialize_operand(&mut self, operand: Operand, ty: Type, token: &Token) -> Place {
        if let Some(place) = operand_place(operand.clone()) {
            return place;
        }
        let local = self.temporary(ty, self.span(token));
        self.statement(StatementKind::StorageLive(local), self.span(token));
        self.statement(
            StatementKind::Assign(Place::local(local), Box::new(Rvalue::Use(operand))),
            self.span(token),
        );
        Place::local(local)
    }

    fn match_arms(&self, start: usize, end: usize) -> Option<Vec<MatchArm>> {
        let mut arms = Vec::new();
        let mut cursor = start;
        while cursor < end {
            cursor += usize::from(self.tokens.get(cursor)?.kind == TokenKind::Comma);
            if cursor >= end {
                break;
            }
            let is_default = self.tokens.get(cursor)?.kind == TokenKind::Default;
            if !is_default && self.tokens.get(cursor)?.kind != TokenKind::Case {
                return None;
            }
            let pattern_start = cursor + 1;
            if is_default {
                // A default arm has no pattern; it is represented as a catch-all.
                let colon = self.find_top_level(pattern_start, end, TokenKind::Colon)?;
                let value_start = colon + 1;
                let (value_end, block, next) =
                    if self.tokens.get(value_start)?.kind == TokenKind::LeftBrace {
                        let close = self.matching_token(
                            value_start,
                            TokenKind::LeftBrace,
                            TokenKind::RightBrace,
                        )?;
                        (close + 1, true, close + 1)
                    } else {
                        let comma = self.find_top_level(value_start, end, TokenKind::Comma)?;
                        (comma, false, comma + 1)
                    };
                arms.push(MatchArm {
                    pattern_start,
                    guard: None,
                    value_start,
                    value_end,
                    block,
                    constructor: None,
                    bindings: Vec::new(),
                });
                cursor = next;
                continue;
            }
            let mut depth = 0_u32;
            let mut marker = pattern_start;
            while marker < end {
                let kind = self.tokens[marker].kind;
                if depth == 0 && matches!(kind, TokenKind::If | TokenKind::Colon) {
                    break;
                }
                match kind {
                    TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => {
                        depth += 1;
                    }
                    TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                }
                marker += 1;
            }
            let guard = if self.tokens.get(marker)?.kind == TokenKind::If {
                let guard_start = marker + 1;
                let colon = self.find_top_level(guard_start, end, TokenKind::Colon)?;
                marker = colon;
                Some((guard_start, colon))
            } else {
                None
            };
            if self.tokens.get(marker)?.kind != TokenKind::Colon {
                return None;
            }
            let value_start = marker + 1;
            let (value_end, block, next) = if self.tokens.get(value_start)?.kind
                == TokenKind::LeftBrace
            {
                let close =
                    self.matching_token(value_start, TokenKind::LeftBrace, TokenKind::RightBrace)?;
                (close + 1, true, close + 1)
            } else {
                let comma = self.find_top_level(value_start, end, TokenKind::Comma)?;
                (comma, false, comma + 1)
            };
            let pattern = self.hir.patterns.iter().find(|pattern| {
                pattern.origin.byte_start
                    == u32::try_from(self.tokens[pattern_start].range.start).unwrap_or(u32::MAX)
            });
            arms.push(MatchArm {
                pattern_start,
                guard,
                value_start,
                value_end,
                block,
                constructor: pattern.and_then(|pattern| pattern.constructor),
                bindings: pattern.map_or_else(Vec::new, |pattern| pattern.bindings.clone()),
            });
            cursor = next;
        }
        Some(arms)
    }

    fn match_pattern_value(&self, arm: &MatchArm, scrutinee: &Type) -> Option<u128> {
        if let Some(constructor) = arm.constructor {
            return self.variant_discriminant(scrutinee, constructor);
        }
        let token = self.tokens.get(arm.pattern_start)?;
        Some(match token.kind {
            TokenKind::True => 1,
            TokenKind::False | TokenKind::Undefined => 0,
            TokenKind::IntegerLiteral => u128::try_from(parse_integer(self.text(token))?).ok()?,
            TokenKind::CharacterLiteral => {
                u128::from(self.text(token).trim_matches('\'').chars().next()? as u32)
            }
            _ => return None,
        })
    }

    fn variant_index(&self, scrutinee: &Type, member: MemberId) -> Option<u32> {
        let Type::Nominal(declaration, _) = scrutinee else {
            return None;
        };
        let DefinitionData::Enum { variants, .. } = &self.program.definition(*declaration)?.data
        else {
            return None;
        };
        variants
            .iter()
            .position(|variant| variant.id == member)
            .and_then(|index| u32::try_from(index).ok())
    }

    fn variant_discriminant(&self, scrutinee: &Type, member: MemberId) -> Option<u128> {
        let Type::Nominal(declaration, _) = scrutinee else {
            return None;
        };
        let DefinitionData::Enum { variants, .. } = &self.program.definition(*declaration)?.data
        else {
            return None;
        };
        let (index, variant) = variants
            .iter()
            .enumerate()
            .find(|(_, variant)| variant.id == member)?;
        let value = variant
            .discriminant
            .unwrap_or_else(|| i128::try_from(index).expect("enum variant limit"));
        Some(u128::from_ne_bytes(value.to_ne_bytes()))
    }

    fn bind_match_payload(&mut self, arm: &MatchArm, scrutinee: &Place, scrutinee_type: &Type) {
        for binding in &arm.bindings {
            let Some(hir_local) = self.hir.locals.get(binding.local.0 as usize) else {
                continue;
            };
            let local = self.add_local(
                hir_local.name.clone(),
                binding.ty.clone(),
                false,
                false,
                hir_local.origin.clone(),
            );
            self.hir_local_ids.insert(binding.local, local);
            self.statement(StatementKind::StorageLive(local), hir_local.origin.clone());
            let mut source = scrutinee.clone();
            for projection in &binding.projection {
                match projection {
                    HirPatternProjection::Variant(constructor) => {
                        if let Some(variant) = self.variant_index(scrutinee_type, *constructor) {
                            source
                                .projection
                                .push(tn_mir::Projection::Downcast(variant));
                        }
                    }
                    HirPatternProjection::Field(index) => {
                        source.projection.push(tn_mir::Projection::Field {
                            index: *index,
                            ty: binding.ty.clone(),
                        });
                    }
                    HirPatternProjection::OptionalPayload => {
                        source.projection.push(tn_mir::Projection::Downcast(1));
                    }
                    HirPatternProjection::Index(_) | HirPatternProjection::Rest { .. } => {}
                }
            }
            self.statement(
                StatementKind::Assign(
                    Place::local(local),
                    Box::new(Rvalue::Use(Operand::Copy(source))),
                ),
                hir_local.origin.clone(),
            );
        }
    }

    fn argument_ranges(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        if start >= end {
            return Vec::new();
        }
        let mut ranges = Vec::new();
        let mut range_start = start;
        let mut depth = 0_u32;
        for index in start..end {
            match self.tokens[index].kind {
                TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    depth = depth.saturating_sub(1);
                }
                TokenKind::Comma if depth == 0 => {
                    ranges.push((range_start, index));
                    range_start = index + 1;
                }
                _ => {}
            }
        }
        if range_start < end {
            ranges.push((range_start, end));
        }
        ranges
    }

    fn find_binary_operator(
        &self,
        start: usize,
        end: usize,
    ) -> Option<(usize, tn_mir::BinaryOperator)> {
        let mut delimiters = Vec::new();
        let mut candidate = None;
        let mut index = start;
        while index < end {
            let kind = self.tokens[index].kind;
            if let Some(greater) = self.generic_call_ends.get(&index).copied() {
                index = greater + 1;
                continue;
            }
            match kind {
                TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => {
                    delimiters.push(kind);
                    index += 1;
                    continue;
                }
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    delimiters.pop();
                    index += 1;
                    continue;
                }
                _ if !delimiters.is_empty() => {
                    index += 1;
                    continue;
                }
                _ => {}
            }
            let Some((precedence, operator)) = binary_operator(kind) else {
                index += 1;
                continue;
            };
            if index == start {
                index += 1;
                continue;
            }
            if kind == TokenKind::Star
                && self.tokens.get(index.wrapping_sub(1)).is_some_and(|token| {
                    matches!(
                        token.kind,
                        TokenKind::Amp
                            | TokenKind::Mut
                            | TokenKind::LeftParen
                            | TokenKind::LeftBracket
                            | TokenKind::LeftBrace
                            | TokenKind::Comma
                            | TokenKind::Plus
                            | TokenKind::Minus
                            | TokenKind::Star
                            | TokenKind::Slash
                            | TokenKind::Percent
                            | TokenKind::AmpAmp
                            | TokenKind::PipePipe
                            | TokenKind::Pipe
                            | TokenKind::Caret
                    )
                })
            {
                index += 1;
                continue;
            }
            if candidate.is_none_or(|(_, current, _)| precedence <= current) {
                candidate = Some((index, precedence, operator));
            }
            index += 1;
        }
        candidate.map(|(index, _, operator)| (index, operator))
    }

    fn find_assignment(
        &self,
        start: usize,
        end: usize,
    ) -> Option<(usize, Option<tn_mir::BinaryOperator>)> {
        let mut delimiters = Vec::new();
        for index in start..end {
            let kind = self.tokens[index].kind;
            match kind {
                TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => {
                    delimiters.push(kind);
                    continue;
                }
                TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                    delimiters.pop();
                    continue;
                }
                _ if !delimiters.is_empty() => continue,
                _ => {}
            }
            let operator = match kind {
                TokenKind::Equal => None,
                TokenKind::PlusEqual => Some(tn_mir::BinaryOperator::Add),
                TokenKind::MinusEqual => Some(tn_mir::BinaryOperator::Subtract),
                TokenKind::StarEqual => Some(tn_mir::BinaryOperator::Multiply),
                TokenKind::SlashEqual => Some(tn_mir::BinaryOperator::Divide),
                TokenKind::PercentEqual => Some(tn_mir::BinaryOperator::Remainder),
                TokenKind::AmpEqual => Some(tn_mir::BinaryOperator::BitAnd),
                TokenKind::PipeEqual => Some(tn_mir::BinaryOperator::BitOr),
                TokenKind::CaretEqual => Some(tn_mir::BinaryOperator::BitXor),
                TokenKind::ShiftLeftEqual => Some(tn_mir::BinaryOperator::ShiftLeft),
                TokenKind::ShiftRightEqual => Some(tn_mir::BinaryOperator::ShiftRight),
                _ => continue,
            };
            return Some((index, operator));
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    fn lower_atom(&mut self, index: usize, expected: Option<&Type>) -> Option<(Operand, Type)> {
        let token = *self.tokens.get(index)?;
        let hir = self.hir_expression_at(token);
        if token.kind == TokenKind::StringLiteral
            && hir.is_some_and(|expression| {
                matches!(
                    expression.kind,
                    tn_hir::HirExpressionKind::Conversion(
                        tn_hir::HirConversionKind::StringLiteralToOwned
                    )
                )
            })
        {
            let literal = Operand::Constant(self.constant(token, &Type::String)?);
            let length = match &literal {
                Operand::Constant(tn_mir::Constant::String(value)) => {
                    Operand::Constant(tn_mir::Constant::Integer {
                        value: i128::try_from(value.len()).unwrap_or(i128::MAX),
                        ty: Type::Primitive(PrimitiveType::Usize),
                    })
                }
                _ => return None,
            };
            let destination = self.temporary(Type::String, self.span(token));
            self.statement(StatementKind::StorageLive(destination), self.span(token));
            self.statement(
                StatementKind::Assign(
                    Place::local(destination),
                    Box::new(Rvalue::RawOperation {
                        operation: "string_from_static".into(),
                        operands: vec![literal, length],
                        ty: Type::String,
                    }),
                ),
                self.span(token),
            );
            return Some((Operand::Move(Place::local(destination)), Type::String));
        }
        if let Some(expression) = hir {
            match expression.resolution {
                Some(ResolvedValue::Local(local)) => {
                    let local = *self.hir_local_ids.get(&local)?;
                    return self.local_value(local);
                }
                Some(ResolvedValue::Declaration(declaration))
                    if !matches!(expression.ty, Type::Function(_))
                        && self.is_global_declaration(declaration) =>
                {
                    let ty = expression.ty.clone();
                    let destination = self.temporary(ty.clone(), self.span(token));
                    self.statement(StatementKind::StorageLive(destination), self.span(token));
                    self.statement(
                        StatementKind::Assign(
                            Place::local(destination),
                            Box::new(Rvalue::RawOperation {
                                operation: format!("global_load:{}", declaration.0),
                                operands: Vec::new(),
                                ty: ty.clone(),
                            }),
                        ),
                        self.span(token),
                    );
                    return Some((Operand::Move(Place::local(destination)), ty));
                }
                Some(ResolvedValue::Declaration(declaration))
                    if matches!(expression.ty, Type::Function(_)) =>
                {
                    if let Some(tn_hir::Definition {
                        data: tn_hir::DefinitionData::Extern { functions },
                        ..
                    }) = self.program.definition(declaration)
                        && let Some(method) = functions
                            .iter()
                            .find(|method| method.name == self.module.source[token.range.clone()])
                    {
                        return Some((
                            Operand::Constant(tn_mir::Constant::Method {
                                owner: declaration,
                                member: method.id,
                                ty: expression.ty.clone(),
                            }),
                            expression.ty.clone(),
                        ));
                    }
                    return Some((
                        Operand::Constant(tn_mir::Constant::Function(
                            declaration,
                            expression.ty.clone(),
                        )),
                        expression.ty.clone(),
                    ));
                }
                _ => {}
            }
        }
        if let Some(local) = self.names.get(self.text(token)).copied() {
            return self.local_value(local);
        }
        let explicit_literal_type = literal_suffix_type(self.text(token), token.kind);
        let inferred = explicit_literal_type.clone().unwrap_or_else(|| {
            hir.map_or_else(|| atom_type(token.kind), |expression| expression.ty.clone())
        });
        let ty = match (token.kind, expected) {
            (TokenKind::IntegerLiteral, Some(expected))
                if explicit_literal_type.is_none() && is_integer_type(expected) =>
            {
                expected.clone()
            }
            (TokenKind::FloatLiteral, Some(expected))
                if explicit_literal_type.is_none() && is_float_type(expected) =>
            {
                expected.clone()
            }
            (TokenKind::Undefined, Some(expected)) => expected.clone(),
            _ => inferred,
        };
        if token.kind == TokenKind::Undefined {
            return Some((
                Operand::Constant(tn_mir::Constant::Undefined(ty.clone())),
                ty,
            ));
        }
        self.constant(token, &ty)
            .map(|constant| (Operand::Constant(constant), ty))
    }

    fn local_value(&self, local: LocalId) -> Option<(Operand, Type)> {
        let local_type = self.locals.get(local.0 as usize)?.ty.clone();
        if self.capture_references.contains(&local) {
            let Type::Reference { referent, .. } = local_type else {
                return None;
            };
            let mut place = Place::local(local);
            place.projection.push(tn_mir::Projection::Dereference);
            Some((Operand::Copy(place), referent.as_ref().clone()))
        } else {
            let operand = if self.ownership_facts.is_copy(&local_type) {
                Operand::Copy(Place::local(local))
            } else {
                Operand::Move(Place::local(local))
            };
            Some((operand, local_type))
        }
    }

    fn hir_expression_at(&self, token: &Token) -> Option<&tn_hir::HirExpression> {
        let start = u32::try_from(token.range.start).ok()?;
        let end = u32::try_from(token.range.end).ok()?;
        self.hir
            .expressions
            .iter()
            .filter(|expression| {
                expression.origin.byte_start <= start && expression.origin.byte_end >= end
            })
            .min_by_key(|expression| expression.origin.byte_end - expression.origin.byte_start)
    }

    fn hir_expression_range(&self, start: usize, end: usize) -> Option<&tn_hir::HirExpression> {
        let first = self.tokens.get(start)?;
        let last = self.tokens.get(end.checked_sub(1)?)?;
        let byte_start = u32::try_from(first.range.start).ok()?;
        let byte_end = u32::try_from(last.range.end).ok()?;
        self.hir.expressions.iter().rev().find(|expression| {
            expression.origin.byte_start == byte_start && expression.origin.byte_end == byte_end
        })
    }

    fn global_declaration(&self, start: usize, end: usize) -> Option<(DeclarationId, Type)> {
        let declaration = self
            .hir_expression_range(start, end)
            .and_then(|expression| match expression.resolution {
                Some(ResolvedValue::Declaration(declaration)) => Some(declaration),
                _ => None,
            })?;
        let declaration_data = self.program.graph.declaration(declaration)?;
        if !matches!(
            declaration_data.kind,
            tn_hir::DeclarationKind::Const | tn_hir::DeclarationKind::Static
        ) {
            return None;
        }
        let ty = self
            .program
            .definition(declaration)
            .and_then(|definition| match &definition.data {
                DefinitionData::Constant { ty, .. } => Some(ty.clone()),
                _ => None,
            })?;
        Some((declaration, ty))
    }

    fn is_global_declaration(&self, declaration: DeclarationId) -> bool {
        self.program
            .graph
            .declaration(declaration)
            .is_some_and(|declaration| {
                matches!(
                    declaration.kind,
                    tn_hir::DeclarationKind::Const | tn_hir::DeclarationKind::Static
                )
            })
    }

    fn block_id(index: usize) -> BasicBlockId {
        BasicBlockId(u32::try_from(index).expect("MIR block limit"))
    }

    fn add_local(
        &mut self,
        name: String,
        ty: Type,
        mutable: bool,
        argument: bool,
        span: SourceSpan,
    ) -> LocalId {
        let id = LocalId(u32::try_from(self.locals.len()).expect("MIR local limit"));
        self.locals.push(Local {
            name: Some(name.clone()),
            ty,
            mutable,
            argument,
            span,
        });
        self.names.insert(name, id);
        id
    }

    fn parse_type_range(&self, start: usize, end: usize) -> Option<Type> {
        let mut parser = MirTypeParser {
            program: self.program,
            module: self.module,
            tokens: &self.tokens[start..end],
            index: 0,
            generics: &self.generics,
        };
        parser.parse_type()
    }

    fn temporary(&mut self, ty: Type, span: SourceSpan) -> LocalId {
        let id = LocalId(u32::try_from(self.locals.len()).expect("MIR local limit"));
        self.locals.push(Local {
            name: None,
            ty,
            mutable: true,
            argument: false,
            span,
        });
        self.temporary_locals.insert(id);
        id
    }

    fn statement(&mut self, kind: StatementKind, span: SourceSpan) {
        if self.blocks[self.current].terminator.is_none() {
            self.blocks[self.current]
                .statements
                .push(Statement { kind, span });
        }
    }

    fn terminate(&mut self, kind: TerminatorKind, span: SourceSpan) {
        self.blocks[self.current].terminator = Some(Terminator { kind, span });
    }

    fn new_block(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(OpenBlock::default());
        id
    }

    fn finish(
        mut self,
        declaration: DeclarationId,
        member: Option<MemberId>,
        effects: Vec<DeclarationId>,
    ) -> Body {
        let fallback = self.locals.first().map_or_else(
            || SourceSpan::new("<body>", 0..0, ""),
            |local| local.span.clone(),
        );
        for block in &mut self.blocks {
            if block.terminator.is_none() {
                block.terminator = Some(Terminator {
                    kind: if self.return_type == Type::Primitive(PrimitiveType::Void) {
                        TerminatorKind::Return(None)
                    } else {
                        TerminatorKind::Unreachable
                    },
                    span: fallback.clone(),
                });
            }
        }
        Body {
            declaration,
            member,
            locals: self.locals,
            blocks: self
                .blocks
                .into_iter()
                .map(|block| BasicBlock {
                    statements: block.statements,
                    terminator: block.terminator.expect("terminators completed"),
                })
                .collect(),
            return_type: self.return_type,
            effects,
        }
    }
}

#[derive(Clone, Copy)]
enum InitializerKind {
    Borrow(BorrowKind),
    Copy,
    Move,
}

struct MatchArm {
    pattern_start: usize,
    guard: Option<(usize, usize)>,
    value_start: usize,
    value_end: usize,
    block: bool,
    constructor: Option<MemberId>,
    bindings: Vec<HirPatternBinding>,
}

#[derive(Clone)]
struct ErrorContext {
    dispatch: BasicBlockId,
    payload: LocalId,
    effects: Vec<DeclarationId>,
    scope_depth: usize,
}

struct CatchArm {
    binding: usize,
    ty: Type,
    block_start: usize,
    block_end: usize,
}

fn operand_place(operand: Operand) -> Option<Place> {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => Some(place),
        Operand::Constant(_) => None,
    }
}

fn operand_place_ref(operand: &Operand) -> Option<&Place> {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => Some(place),
        Operand::Constant(_) => None,
    }
}

fn non_consuming_operand(operand: Operand) -> Operand {
    match operand {
        Operand::Move(place) => Operand::Copy(place),
        operand => operand,
    }
}

fn aggregate_field_type(ty: &Type, index: usize) -> Option<Type> {
    match ty {
        Type::Tuple(elements) => elements.get(index).cloned(),
        Type::Array(element, _) | Type::Slice(element) => Some(element.as_ref().clone()),
        _ => None,
    }
}

fn builtin_iterable_item(ty: &Type) -> Option<Type> {
    match ty {
        Type::Array(element, _) | Type::Slice(element) => Some(element.as_ref().clone()),
        Type::Reference { referent, .. } => builtin_iterable_item(referent),
        _ => None,
    }
}

fn substitute_mir_type(ty: &Type, substitutions: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Generic(name) | Type::Lifetime(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Nominal(id, arguments) => Type::Nominal(
            *id,
            arguments
                .iter()
                .map(|argument| substitute_mir_type(argument, substitutions))
                .collect(),
        ),
        Type::DynamicInterface(id, arguments) => Type::DynamicInterface(
            *id,
            arguments
                .iter()
                .map(|argument| substitute_mir_type(argument, substitutions))
                .collect(),
        ),
        Type::Optional(inner) => {
            Type::Optional(Box::new(substitute_mir_type(inner, substitutions)))
        }
        Type::Array(inner, length) => {
            Type::Array(Box::new(substitute_mir_type(inner, substitutions)), *length)
        }
        Type::Slice(inner) => Type::Slice(Box::new(substitute_mir_type(inner, substitutions))),
        Type::Tuple(elements) => Type::Tuple(
            elements
                .iter()
                .map(|element| substitute_mir_type(element, substitutions))
                .collect(),
        ),
        Type::Template(captures) => Type::Template(
            captures
                .iter()
                .map(|capture| substitute_mir_type(capture, substitutions))
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
            referent: Box::new(substitute_mir_type(referent, substitutions)),
        },
        Type::RawPointer { mutable, pointee } => Type::RawPointer {
            mutable: *mutable,
            pointee: Box::new(substitute_mir_type(pointee, substitutions)),
        },
        Type::Promise {
            result,
            error,
            effects,
        } => {
            let error = substitute_mir_type(error, substitutions);
            Type::Promise {
                result: Box::new(substitute_mir_type(result, substitutions)),
                error: Box::new(error.clone()),
                effects: tn_hir::promise_effects(&error, effects),
            }
        }
        Type::Function(function) => Type::Function(tn_hir::FunctionType {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| substitute_mir_type(parameter, substitutions))
                .collect(),
            result: Box::new(substitute_mir_type(&function.result, substitutions)),
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

fn infer_mir_substitutions(
    template: &Type,
    concrete: &Type,
    substitutions: &mut BTreeMap<String, Type>,
) {
    match (template, concrete) {
        (Type::Generic(name) | Type::Lifetime(name), concrete) => {
            substitutions
                .entry(name.clone())
                .or_insert_with(|| concrete.clone());
        }
        (Type::Optional(left), Type::Optional(right))
        | (Type::Array(left, _), Type::Array(right, _))
        | (Type::Slice(left), Type::Slice(right))
        | (Type::RawPointer { pointee: left, .. }, Type::RawPointer { pointee: right, .. }) => {
            infer_mir_substitutions(left, right, substitutions);
        }
        (
            Type::Promise {
                result: left,
                error: left_error,
                ..
            },
            Type::Promise {
                result: right,
                error: right_error,
                ..
            },
        ) => {
            infer_mir_substitutions(left, right, substitutions);
            infer_mir_substitutions(left_error, right_error, substitutions);
        }
        (
            Type::Reference {
                lifetime: left_lifetime,
                referent: left,
                ..
            },
            Type::Reference {
                lifetime: right_lifetime,
                referent: right,
                ..
            },
        ) => {
            if left_lifetime != "scope" && left_lifetime != "static" {
                substitutions
                    .entry(left_lifetime.clone())
                    .or_insert_with(|| Type::Lifetime(right_lifetime.clone()));
            }
            infer_mir_substitutions(left, right, substitutions);
        }
        (Type::Tuple(left), Type::Tuple(right))
        | (Type::Template(left), Type::Template(right))
        | (Type::Nominal(_, left), Type::Nominal(_, right))
        | (Type::DynamicInterface(_, left), Type::DynamicInterface(_, right)) => {
            for (left, right) in left.iter().zip(right) {
                infer_mir_substitutions(left, right, substitutions);
            }
        }
        (Type::Function(left), Type::Function(right)) => {
            for (left, right) in left.parameters.iter().zip(&right.parameters) {
                infer_mir_substitutions(left, right, substitutions);
            }
            infer_mir_substitutions(&left.result, &right.result, substitutions);
        }
        _ => {}
    }
}

fn replace_callable_type(operand: &mut Operand, ty: Type) {
    let Operand::Constant(constant) = operand else {
        return;
    };
    match constant {
        tn_mir::Constant::Function(_, function)
        | tn_mir::Constant::Method { ty: function, .. }
        | tn_mir::Constant::Constructor { ty: function, .. } => *function = ty,
        _ => {}
    }
}

fn specialize_method_operand(lowerer: &mut OwnershipMirLowerer<'_>, operand: &Operand, ty: &Type) {
    let Some(place) = operand_place_ref(operand) else {
        return;
    };
    if !place.projection.is_empty() {
        return;
    }
    let local = place.local;
    if let Some(local_data) = lowerer.locals.get_mut(local.0 as usize) {
        local_data.ty = ty.clone();
    }
    for block in &mut lowerer.blocks {
        for statement in &mut block.statements {
            let StatementKind::Assign(destination, value) = &mut statement.kind else {
                continue;
            };
            if *destination != Place::local(local) {
                continue;
            }
            match value.as_mut() {
                Rvalue::VtableLookup {
                    ty: lookup_type, ..
                }
                | Rvalue::WitnessLookup {
                    ty: lookup_type, ..
                }
                | Rvalue::DirectMethod {
                    ty: lookup_type, ..
                } => {
                    *lookup_type = ty.clone();
                }
                _ => {}
            }
        }
    }
}

fn atom_type(kind: TokenKind) -> Type {
    match kind {
        TokenKind::True | TokenKind::False => Type::Primitive(PrimitiveType::Bool),
        TokenKind::IntegerLiteral => Type::Primitive(PrimitiveType::Isize),
        TokenKind::FloatLiteral => Type::Primitive(PrimitiveType::F64),
        TokenKind::CharacterLiteral => Type::Primitive(PrimitiveType::Char),
        TokenKind::StringLiteral => Type::Reference {
            mutable: false,
            lifetime: "static".into(),
            referent: Box::new(Type::Str),
        },
        _ => Type::Error,
    }
}

fn literal_suffix_type(text: &str, kind: TokenKind) -> Option<Type> {
    if !matches!(kind, TokenKind::IntegerLiteral | TokenKind::FloatLiteral) {
        return None;
    }
    [
        "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8", "i8",
        "f32", "f64",
    ]
    .iter()
    .find_map(|suffix| text.strip_suffix(suffix).map(|_| mir_primitive(suffix)))
    .flatten()
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

fn binary_operator(kind: TokenKind) -> Option<(u8, tn_mir::BinaryOperator)> {
    use tn_mir::BinaryOperator;
    Some(match kind {
        TokenKind::PipePipe => (1, BinaryOperator::LogicalOr),
        TokenKind::AmpAmp => (2, BinaryOperator::LogicalAnd),
        TokenKind::Pipe => (3, BinaryOperator::BitOr),
        TokenKind::Caret => (4, BinaryOperator::BitXor),
        TokenKind::Amp => (5, BinaryOperator::BitAnd),
        TokenKind::EqualEqualEqual => (6, BinaryOperator::Equal),
        TokenKind::BangEqualEqual => (6, BinaryOperator::NotEqual),
        TokenKind::Less => (7, BinaryOperator::Less),
        TokenKind::LessEqual => (7, BinaryOperator::LessEqual),
        TokenKind::Greater => (7, BinaryOperator::Greater),
        TokenKind::GreaterEqual => (7, BinaryOperator::GreaterEqual),
        TokenKind::ShiftLeft => (8, BinaryOperator::ShiftLeft),
        TokenKind::ShiftRight => (8, BinaryOperator::ShiftRight),
        TokenKind::Plus => (9, BinaryOperator::Add),
        TokenKind::Minus => (9, BinaryOperator::Subtract),
        TokenKind::Star => (10, BinaryOperator::Multiply),
        TokenKind::Slash => (10, BinaryOperator::Divide),
        TokenKind::Percent => (10, BinaryOperator::Remainder),
        _ => return None,
    })
}

fn generic_call_ends(tokens: &[&Token]) -> BTreeMap<usize, usize> {
    let mut result = BTreeMap::new();
    let mut opens = Vec::new();
    for index in 0..tokens.len() {
        match tokens[index].kind {
            TokenKind::Less
                if index > 0
                    && matches!(
                        tokens[index - 1].kind,
                        TokenKind::Identifier
                            | TokenKind::This
                            | TokenKind::Greater
                            | TokenKind::RightBracket
                    ) =>
            {
                opens.push(index);
            }
            TokenKind::Greater => {
                let Some(open) = opens.pop() else {
                    continue;
                };
                if tokens
                    .get(index + 1)
                    .is_some_and(|token| token.kind == TokenKind::LeftParen)
                {
                    result.insert(open, index);
                }
            }
            _ => {}
        }
    }
    result
}

fn is_integer_type(ty: &Type) -> bool {
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

fn is_float_type(ty: &Type) -> bool {
    matches!(ty, Type::Primitive(PrimitiveType::F32 | PrimitiveType::F64))
}

fn binary_operand_type(left: &Type, right: &Type) -> Type {
    if left == right {
        return left.clone();
    }
    let string_like = |ty: &Type| match ty {
        Type::String | Type::Str => true,
        Type::Reference { referent, .. } => {
            matches!(referent.as_ref(), Type::Str | Type::String)
        }
        _ => false,
    };
    if string_like(left) && string_like(right) {
        if matches!(left, Type::Reference { referent, .. } if referent.as_ref() == &Type::Str)
            && matches!(right, Type::Reference { referent, .. } if referent.as_ref() == &Type::Str)
        {
            return left.clone();
        }
        if matches!(left, Type::String) || matches!(right, Type::String) {
            Type::String
        } else {
            Type::Str
        }
    } else {
        Type::Error
    }
}

fn normalize_string_comparison_operand(operand: Operand, ty: Type) -> (Operand, Type) {
    let Type::Reference { referent, .. } = &ty else {
        return (operand, ty);
    };
    if referent.as_ref() == &Type::Str {
        return (operand, ty);
    }
    if !matches!(referent.as_ref(), Type::String | Type::Str) {
        return (operand, ty);
    }
    if matches!(operand, Operand::Constant(tn_mir::Constant::String(_))) {
        return (operand, Type::Str);
    }
    let Some(mut place) = operand_place(operand.clone()) else {
        return (operand, ty);
    };
    place.projection.push(tn_mir::Projection::Dereference);
    (Operand::Copy(place), referent.as_ref().clone())
}

fn parse_integer(text: &str) -> Option<i128> {
    let digits = [
        "usize", "isize", "u128", "i128", "u64", "i64", "u32", "i32", "u16", "i16", "u8", "i8",
    ]
    .iter()
    .find_map(|suffix| text.strip_suffix(suffix))
    .unwrap_or(text)
    .replace('_', "");
    let (radix, digits) = if let Some(digits) = digits.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = digits.strip_prefix("0o") {
        (8, digits)
    } else if let Some(digits) = digits.strip_prefix("0b") {
        (2, digits)
    } else {
        (10, digits.as_str())
    };
    i128::from_str_radix(digits, radix).ok()
}

fn mir_cast_kind(source: &Type, target: &Type) -> CastKind {
    match (source, target) {
        (Type::Reference { .. }, Type::Reference { .. }) => CastKind::Reborrow,
        (Type::RawPointer { .. }, Type::RawPointer { .. })
        | (Type::Reference { .. }, Type::RawPointer { .. })
        | (Type::Promise { .. }, Type::RawPointer { .. })
        | (Type::RawPointer { .. }, Type::Promise { .. }) => CastKind::RawPointer,
        (
            Type::Nominal(_, _) | Type::Generic(_) | Type::Reference { .. },
            Type::DynamicInterface(_, _),
        ) => CastKind::InterfaceCoercion,
        (Type::Nominal(_, _), Type::Nominal(_, _)) => CastKind::ClassUpcast,
        _ => CastKind::CheckedDowncast,
    }
}

struct MirTypeParser<'a> {
    program: &'a Program,
    module: &'a tn_hir::Module,
    tokens: &'a [&'a Token],
    index: usize,
    generics: &'a BTreeMap<String, Namespace>,
}

impl MirTypeParser<'_> {
    fn kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.index).map(|token| token.kind)
    }

    fn text(&self) -> Option<&str> {
        self.tokens
            .get(self.index)
            .map(|token| &self.module.source[token.range.clone()])
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.kind() == Some(kind) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn parse_type(&mut self) -> Option<Type> {
        let mut ty = self.parse_primary()?;
        if self.eat(TokenKind::Pipe) {
            if !self.eat(TokenKind::Undefined) {
                return Some(Type::Error);
            }
            ty = Type::Optional(Box::new(ty));
        }
        Some(ty)
    }

    fn parse_primary(&mut self) -> Option<Type> {
        match self.kind()? {
            TokenKind::Amp => {
                self.index += 1;
                let lifetime = if self.kind() == Some(TokenKind::Static) {
                    self.index += 1;
                    "static".into()
                } else if self.kind() == Some(TokenKind::Scope) {
                    self.index += 1;
                    "scope".into()
                } else if self
                    .text()
                    .is_some_and(|name| self.generics.get(name) == Some(&Namespace::Lifetime))
                {
                    let lifetime = self.text()?.to_owned();
                    self.index += 1;
                    lifetime
                } else {
                    "scope".into()
                };
                let mutable = self.eat(TokenKind::Mut);
                Some(Type::Reference {
                    mutable,
                    lifetime,
                    referent: Box::new(self.parse_primary()?),
                })
            }
            TokenKind::Star => {
                self.index += 1;
                let mutable = self.eat(TokenKind::Mut);
                if !mutable {
                    self.eat(TokenKind::Const);
                }
                Some(Type::RawPointer {
                    mutable,
                    pointee: Box::new(self.parse_primary()?),
                })
            }
            TokenKind::LeftBracket => {
                self.index += 1;
                let element = self.parse_type()?;
                if self.eat(TokenKind::Semicolon) {
                    let length = self
                        .text()
                        .and_then(|text| text.replace('_', "").parse().ok())
                        .unwrap_or_default();
                    self.index += usize::from(self.kind().is_some());
                    self.eat(TokenKind::RightBracket);
                    Some(Type::Array(Box::new(element), length))
                } else {
                    self.eat(TokenKind::RightBracket);
                    Some(Type::Slice(Box::new(element)))
                }
            }
            TokenKind::LeftParen => self.parse_tuple_or_function(),
            TokenKind::Unknown => {
                self.index += 1;
                Some(Type::Unknown)
            }
            TokenKind::Identifier => {
                let name = self.text()?.to_owned();
                self.index += 1;
                if let Some(primitive) = mir_primitive(&name) {
                    return Some(primitive);
                }
                if let Some(namespace) = self.generics.get(&name) {
                    return Some(if *namespace == Namespace::Lifetime {
                        Type::Lifetime(name)
                    } else {
                        Type::Generic(name)
                    });
                }
                if name == "Promise" {
                    if !self.eat(TokenKind::Less) {
                        return None;
                    }
                    let result = self.parse_type()?;
                    if !self.eat(TokenKind::Comma) {
                        return None;
                    }
                    let error = self.parse_type()?;
                    self.eat(TokenKind::Greater);
                    let effects = match error {
                        Type::Primitive(PrimitiveType::Never) => Vec::new(),
                        Type::Nominal(id, _) => vec![id],
                        _ => return None,
                    };
                    return Some(Type::Promise {
                        result: Box::new(result),
                        error: Box::new(error),
                        effects,
                    });
                }
                let id = self.resolve_type(&name)?;
                Some(Type::Nominal(id, self.parse_arguments()))
            }
            _ => None,
        }
    }

    fn parse_tuple_or_function(&mut self) -> Option<Type> {
        self.eat(TokenKind::LeftParen);
        let mut elements = Vec::new();
        while self.kind().is_some() && self.kind() != Some(TokenKind::RightParen) {
            elements.push(self.parse_type()?);
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::RightParen);
        if self.eat(TokenKind::FatArrow) {
            let result = self.parse_type()?;
            Some(Type::Function(tn_hir::FunctionType {
                parameters: elements,
                result: Box::new(result),
                effects: Vec::new(),
                generics: Vec::new(),
                is_async: false,
                is_unsafe: false,
            }))
        } else {
            Some(Type::Tuple(elements))
        }
    }

    fn parse_arguments(&mut self) -> Vec<Type> {
        let mut arguments = Vec::new();
        if !self.eat(TokenKind::Less) {
            return arguments;
        }
        while self.kind().is_some() && self.kind() != Some(TokenKind::Greater) {
            if matches!(self.kind(), Some(TokenKind::Static | TokenKind::Scope)) {
                let lifetime = self.text().unwrap_or("scope").to_owned();
                self.index += 1;
                arguments.push(Type::Lifetime(lifetime));
            } else if let Some(argument) = self.parse_type() {
                arguments.push(argument);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.eat(TokenKind::Greater);
        arguments
    }

    fn resolve_type(&self, name: &str) -> Option<DeclarationId> {
        let local = self.module.declarations.iter().find(|declaration| {
            declaration.name.as_deref() == Some(name)
                && declaration.kind.namespace() == Some(Namespace::Type)
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
                        && declaration.kind.namespace() == Some(Namespace::Type)
                })
        });
        local.or(imported).map(|declaration| declaration.id)
    }
}

fn mir_primitive(name: &str) -> Option<Type> {
    Some(match name {
        "bool" => Type::Primitive(PrimitiveType::Bool),
        "i8" => Type::Primitive(PrimitiveType::I8),
        "i16" => Type::Primitive(PrimitiveType::I16),
        "i32" => Type::Primitive(PrimitiveType::I32),
        "i64" => Type::Primitive(PrimitiveType::I64),
        "i128" => Type::Primitive(PrimitiveType::I128),
        "isize" => Type::Primitive(PrimitiveType::Isize),
        "u8" => Type::Primitive(PrimitiveType::U8),
        "u16" => Type::Primitive(PrimitiveType::U16),
        "u32" => Type::Primitive(PrimitiveType::U32),
        "u64" => Type::Primitive(PrimitiveType::U64),
        "u128" => Type::Primitive(PrimitiveType::U128),
        "usize" => Type::Primitive(PrimitiveType::Usize),
        "f32" => Type::Primitive(PrimitiveType::F32),
        "f64" | "number" => Type::Primitive(PrimitiveType::F64),
        "char" => Type::Primitive(PrimitiveType::Char),
        "void" => Type::Primitive(PrimitiveType::Void),
        "never" => Type::Primitive(PrimitiveType::Never),
        "string" => Type::String,
        "str" => Type::Str,
        _ => return None,
    })
}
