use crate::CheckResult;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_hir::{
    AttributeKind, DeclarationId, DeclarationKind, DefinitionData, ImportClause, Program, Type,
};
use tn_mir::{
    Body, BorrowKind, Completion, LocalId, Operand, Place, Projection, Rvalue, StatementKind,
    TerminatorKind, validate,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OwnershipFacts {
    pub copy: BTreeSet<DeclarationId>,
    pub drop: BTreeSet<DeclarationId>,
    pub send: BTreeSet<DeclarationId>,
    pub sync: BTreeSet<DeclarationId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureKind {
    SharedBorrow,
    MutableBorrow,
    Move,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    pub name: String,
    pub ty: Type,
    pub kind: CaptureKind,
    pub span: SourceSpan,
}

pub fn check_capture_requirements(
    captures: &[Capture],
    detached: bool,
    facts: &OwnershipFacts,
) -> CheckResult {
    let mut diagnostics = Vec::new();
    for capture in captures {
        let thread_safe = match capture.kind {
            CaptureKind::SharedBorrow => facts.is_sync(&capture.ty),
            CaptureKind::MutableBorrow | CaptureKind::Move => facts.is_send(&capture.ty),
        };
        if !thread_safe {
            diagnostics.push(diagnostic(
                "OWNERSHIP_CAPTURE_NOT_THREAD_SAFE",
                format!("capture `{}` does not satisfy Send/Sync", capture.name),
                &capture.span,
                "use a thread-safe owned value or synchronization primitive",
                Vec::new(),
            ));
        }
        if detached
            && matches!(
                capture.kind,
                CaptureKind::SharedBorrow | CaptureKind::MutableBorrow
            )
            && matches!(
                &capture.ty,
                Type::Reference { lifetime, .. } if lifetime != "static"
            )
        {
            diagnostics.push(diagnostic(
                "OWNERSHIP_DETACHED_CAPTURE_NOT_STATIC",
                format!("detached capture `{}` can outlive its borrow", capture.name),
                &capture.span,
                "move owned process-lifetime data into the detached task",
                Vec::new(),
            ));
        }
    }
    CheckResult { diagnostics }
}

pub fn check_static_requirements(program: &Program, facts: &OwnershipFacts) -> CheckResult {
    let mut diagnostics = Vec::new();
    for definition in &program.definitions {
        let Some(declaration) = program.graph.declaration(definition.declaration) else {
            continue;
        };
        if declaration.kind != tn_hir::DeclarationKind::Static {
            continue;
        }
        let DefinitionData::Constant { ty, mutable_static } = &definition.data else {
            continue;
        };
        if !mutable_static && !facts.is_sync(ty) {
            diagnostics.push(diagnostic(
                "OWNERSHIP_IMMUTABLE_STATIC_NOT_SYNC",
                "immutable static storage requires a Sync type",
                &declaration.span,
                "use a thread-safe type or an explicitly synchronized owner",
                Vec::new(),
            ));
        }
    }
    CheckResult { diagnostics }
}

impl OwnershipFacts {
    pub fn is_copy(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(_) | Type::RawPointer { .. } | Type::Function(_) => true,
            Type::Reference { mutable, .. } => !mutable,
            Type::Optional(inner) | Type::Array(inner, _) => self.is_copy(inner),
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().all(|element| self.is_copy(element))
            }
            Type::Nominal(id, _) => self.copy.contains(id),
            Type::ErrorUnion(effects) => effects.iter().all(|effect| self.copy.contains(effect)),
            Type::Promise { .. }
            | Type::String
            | Type::Str
            | Type::Slice(_)
            | Type::DynamicInterface(_, _)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
        }
    }

    pub fn has_drop(&self, ty: &Type) -> bool {
        match ty {
            Type::String => true,
            Type::Nominal(id, _) => self.drop.contains(id),
            Type::Optional(inner) | Type::Array(inner, _) => self.has_drop(inner),
            Type::Promise { result, effects } => {
                self.has_drop(result) || effects.iter().any(|effect| self.drop.contains(effect))
            }
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().any(|element| self.has_drop(element))
            }
            Type::ErrorUnion(effects) => effects.iter().any(|effect| self.drop.contains(effect)),
            _ => false,
        }
    }

    pub fn is_send(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(_) | Type::Function(_) | Type::String | Type::Str => true,
            Type::Reference {
                mutable: false,
                referent,
                ..
            } => self.is_sync(referent),
            Type::Reference {
                mutable: true,
                referent,
                ..
            } => self.is_send(referent),
            Type::RawPointer { .. }
            | Type::DynamicInterface(_, _)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
            Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
                self.is_send(inner)
            }
            Type::Promise { result, effects } => {
                self.is_send(result) && effects.iter().all(|effect| self.send.contains(effect))
            }
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().all(|element| self.is_send(element))
            }
            Type::ErrorUnion(effects) => effects.iter().all(|effect| self.send.contains(effect)),
            Type::Nominal(id, arguments) => {
                self.send.contains(id) && arguments.iter().all(|argument| self.is_send(argument))
            }
        }
    }

    pub fn is_sync(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(_) | Type::Function(_) | Type::String | Type::Str => true,
            Type::Reference { referent, .. } => self.is_sync(referent),
            Type::RawPointer { .. }
            | Type::DynamicInterface(_, _)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
            Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
                self.is_sync(inner)
            }
            Type::Promise { result, effects } => {
                self.is_sync(result) && effects.iter().all(|effect| self.sync.contains(effect))
            }
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().all(|element| self.is_sync(element))
            }
            Type::ErrorUnion(effects) => effects.iter().all(|effect| self.sync.contains(effect)),
            Type::Nominal(id, arguments) => {
                self.sync.contains(id) && arguments.iter().all(|argument| self.is_sync(argument))
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn derive_ownership_facts(program: &Program) -> OwnershipFacts {
    let mut facts = OwnershipFacts::default();
    for definition in &program.definitions {
        for marker in ["Copy", "Drop", "Send", "Sync"] {
            if has_marker(program, definition.declaration, marker) {
                match marker {
                    "Copy" => {
                        facts.copy.insert(definition.declaration);
                    }
                    "Drop" => {
                        facts.drop.insert(definition.declaration);
                    }
                    "Send" => {
                        facts.send.insert(definition.declaration);
                    }
                    "Sync" => {
                        facts.sync.insert(definition.declaration);
                    }
                    _ => {}
                }
            }
        }
    }
    for definition in &program.definitions {
        if matches!(
            definition.data,
            DefinitionData::Struct { .. }
                | DefinitionData::Enum { .. }
                | DefinitionData::Class { .. }
        ) {
            for interface in declared_conformances(program, definition.declaration) {
                insert_fact(program, &mut facts, interface, definition.declaration);
            }
        }
        if let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, _)),
            target: Type::Nominal(target, _),
            ..
        } = &definition.data
        {
            insert_fact(program, &mut facts, *interface, *target);
        }
    }
    let explicit_send = facts.send.clone();
    let explicit_sync = facts.sync.clone();
    let structural = program
        .definitions
        .iter()
        .filter(|definition| {
            matches!(
                definition.data,
                DefinitionData::Struct { .. }
                    | DefinitionData::Class { .. }
                    | DefinitionData::Enum { .. }
            )
        })
        .map(|definition| definition.declaration)
        .collect::<BTreeSet<_>>();
    facts.send.extend(structural.iter().copied());
    facts.sync.extend(structural.iter().copied());
    loop {
        let mut remove_send = Vec::new();
        let mut remove_sync = Vec::new();
        for definition in &program.definitions {
            if !structural.contains(&definition.declaration) {
                continue;
            }
            let field_types = match &definition.data {
                DefinitionData::Struct { fields, .. } | DefinitionData::Class { fields, .. } => {
                    fields.iter().map(|field| &field.ty).collect::<Vec<_>>()
                }
                DefinitionData::Enum { variants, .. } => variants
                    .iter()
                    .flat_map(|variant| variant.fields.iter().map(|field| &field.ty))
                    .collect::<Vec<_>>(),
                _ => unreachable!("structural set contains only aggregate definitions"),
            };
            if !explicit_send.contains(&definition.declaration)
                && !field_types
                    .iter()
                    .all(|ty| structurally_thread_safe(ty, false, &facts))
            {
                remove_send.push(definition.declaration);
            }
            if !explicit_sync.contains(&definition.declaration)
                && !field_types
                    .iter()
                    .all(|ty| structurally_thread_safe(ty, true, &facts))
            {
                remove_sync.push(definition.declaration);
            }
        }
        if remove_send.is_empty() && remove_sync.is_empty() {
            break;
        }
        let before = (facts.send.len(), facts.sync.len());
        for declaration in remove_send {
            facts.send.remove(&declaration);
        }
        for declaration in remove_sync {
            facts.sync.remove(&declaration);
        }
        if before == (facts.send.len(), facts.sync.len()) {
            break;
        }
    }
    facts
}

fn has_marker(program: &Program, target: DeclarationId, requested: &str) -> bool {
    let Some(declaration) = program.graph.declaration(target) else {
        return false;
    };
    declaration.attributes.iter().any(|attribute| {
        (attribute.kind.as_str() == requested
            && !matches!(requested, "Send" | "Sync")
            && attribute.arguments.is_empty())
            || (attribute.kind == AttributeKind::Conform
                && attribute
                    .arguments
                    .iter()
                    .any(|argument| argument == requested))
    })
}

/// Returns canonical interface declarations attached to a nominal type.
pub(crate) fn declared_conformances(
    program: &Program,
    nominal: DeclarationId,
) -> Vec<DeclarationId> {
    let mut interfaces = Vec::new();
    if let Some(definition) = program.definition(nominal)
        && let DefinitionData::Class {
            interfaces: declared,
            ..
        } = &definition.data
    {
        interfaces.extend(declared.iter().filter_map(|ty| match ty {
            Type::Nominal(id, _) | Type::DynamicInterface(id, _) => Some(*id),
            _ => None,
        }));
    }
    let Some(declaration) = program.graph.declaration(nominal) else {
        return interfaces;
    };
    for attribute in declaration
        .attributes
        .iter()
        .filter(|attribute| attribute.kind == AttributeKind::Conform)
    {
        for argument in &attribute.arguments {
            if let Some(interface) = resolve_interface_name(program, declaration.module, argument) {
                interfaces.push(interface);
            }
        }
    }
    interfaces.sort_unstable();
    interfaces.dedup();
    interfaces
}

fn resolve_interface_name(
    program: &Program,
    module_id: tn_hir::ModuleId,
    name: &str,
) -> Option<DeclarationId> {
    let module = program.graph.module(module_id)?;
    if let Some(local) = module.declarations.iter().find(|declaration| {
        declaration.kind == DeclarationKind::Interface && declaration.name.as_deref() == Some(name)
    }) {
        return Some(local.id);
    }
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
                declaration.kind == DeclarationKind::Interface
                    && declaration.exported
                    && declaration.name.as_deref() == Some(imported.imported.as_str())
            })
            .map(|declaration| declaration.id)
    })
}

fn structurally_thread_safe(ty: &Type, sync: bool, facts: &OwnershipFacts) -> bool {
    match ty {
        Type::Primitive(_)
        | Type::Function(_)
        | Type::String
        | Type::Str
        | Type::Lifetime(_)
        | Type::Generic(_) => true,
        Type::Reference {
            mutable, referent, ..
        } => {
            if sync || !mutable {
                structurally_thread_safe(referent, true, facts)
            } else {
                structurally_thread_safe(referent, false, facts)
            }
        }
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            structurally_thread_safe(inner, sync, facts)
        }
        Type::Promise { result, effects } => {
            structurally_thread_safe(result, sync, facts)
                && effects.iter().all(|effect| {
                    let marker = if sync { &facts.sync } else { &facts.send };
                    marker.contains(effect)
                })
        }
        Type::Tuple(elements) | Type::Template(elements) => elements
            .iter()
            .all(|element| structurally_thread_safe(element, sync, facts)),
        Type::Nominal(id, arguments) => {
            let marker = if sync { &facts.sync } else { &facts.send };
            marker.contains(id)
                && arguments
                    .iter()
                    .all(|argument| structurally_thread_safe(argument, sync, facts))
        }
        Type::ErrorUnion(effects) => {
            let marker = if sync { &facts.sync } else { &facts.send };
            effects.iter().all(|effect| marker.contains(effect))
        }
        Type::RawPointer { .. } | Type::DynamicInterface(_, _) | Type::Error | Type::Unknown => {
            false
        }
    }
}

fn insert_fact(
    program: &Program,
    facts: &mut OwnershipFacts,
    interface: DeclarationId,
    target: DeclarationId,
) {
    let name = program
        .graph
        .declaration(interface)
        .and_then(|declaration| declaration.name.as_deref());
    match name {
        Some("Copy") => {
            facts.copy.insert(target);
        }
        Some("Drop") => {
            facts.drop.insert(target);
        }
        Some("Send") => {
            facts.send.insert(target);
        }
        Some("Sync") => {
            facts.sync.insert(target);
        }
        _ => {}
    }
}

#[derive(Clone, Eq, PartialEq)]
struct Loan {
    destination: LocalId,
    kind: BorrowKind,
    place: Place,
    origin: SourceSpan,
}

#[derive(Clone, Default, Eq, PartialEq)]
struct AnalysisState {
    loans: Vec<Loan>,
    moved: Vec<(Place, SourceSpan)>,
}

/// Checks moves, partial moves, non-lexical loans, returned references, and suspension safety.
///
/// MIR must first pass the structural validator. Invalid MIR produces an internal diagnostic rather
/// than allowing ownership analysis to make assumptions about malformed control flow.
pub fn check_ownership(body: &Body, facts: &OwnershipFacts) -> CheckResult {
    if let Err(errors) = validate(body)
        && errors
            .iter()
            .any(|error| !matches!(error, tn_mir::MirValidationError::UninitializedUse { .. }))
    {
        let span = body
            .locals
            .first()
            .map_or_else(internal_span, |local| local.span.clone());
        return CheckResult {
            diagnostics: vec![diagnostic(
                "MIR_INVALID_BEFORE_BORROW_CHECK",
                format!(
                    "MIR validation failed before ownership analysis: {}",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                &span,
                "the compiler produced invalid generic MIR",
                Vec::new(),
            )],
        };
    }

    CheckResult {
        diagnostics: analyze_blocks(body, facts),
    }
}

#[allow(clippy::too_many_lines)]
fn analyze_blocks(body: &Body, facts: &OwnershipFacts) -> Vec<Diagnostic> {
    let last_uses = last_uses(body);
    let mut block_points = Vec::with_capacity(body.blocks.len());
    let mut next_point = 0_usize;
    for block in &body.blocks {
        block_points.push(next_point);
        next_point += block.statements.len() + 1;
    }
    let mut entries = vec![None::<AnalysisState>; body.blocks.len()];
    entries[0] = Some(AnalysisState::default());
    let mut work = VecDeque::from([0_usize]);
    let mut diagnostics = Vec::new();
    while let Some(block_index) = work.pop_front() {
        let block = &body.blocks[block_index];
        let mut state = entries[block_index].clone().unwrap_or_default();
        let mut point = block_points[block_index];
        for statement in &block.statements {
            state.loans.retain(|loan| {
                last_uses
                    .get(&loan.destination)
                    .is_some_and(|last| *last >= point)
            });
            match &statement.kind {
                StatementKind::Assign(place, rvalue) => {
                    check_write(place, &state.loans, &statement.span, &mut diagnostics);
                    visit_rvalue(
                        body,
                        rvalue,
                        facts,
                        &state.loans,
                        &mut state.moved,
                        &statement.span,
                        &mut diagnostics,
                    );
                    let transferred = transferred_loans(rvalue, place.local, &state.loans);
                    extend_unique(&mut state.loans, transferred);
                    if place.projection.is_empty()
                        && let Some(loan) = receiver_loan(rvalue, place.local, &statement.span)
                    {
                        extend_unique(&mut state.loans, [loan]);
                    }
                    state
                        .moved
                        .retain(|(moved_place, _)| !place_contains(place, moved_place));
                }
                StatementKind::Borrow {
                    destination,
                    kind,
                    place,
                    ..
                } => {
                    check_use(
                        place,
                        false,
                        &state.moved,
                        &statement.span,
                        &mut diagnostics,
                    );
                    for loan in state
                        .loans
                        .iter()
                        .filter(|loan| places_overlap(&loan.place, place))
                    {
                        if *kind == BorrowKind::Mutable || loan.kind == BorrowKind::Mutable {
                            diagnostics.push(diagnostic(
                                "OWNERSHIP_CONFLICTING_BORROW",
                                "borrow conflicts with an existing live loan",
                                &statement.span,
                                "this borrow overlaps a loan that is still used later",
                                vec![Label {
                                    span: loan.origin.clone(),
                                    message: "the existing loan begins here".into(),
                                }],
                            ));
                        }
                    }
                    extend_unique(
                        &mut state.loans,
                        [Loan {
                            destination: *destination,
                            kind: *kind,
                            place: place.clone(),
                            origin: statement.span.clone(),
                        }],
                    );
                }
                StatementKind::StorageDead(local) => {
                    state
                        .loans
                        .retain(|loan| loan.destination != *local && loan.place.local != *local);
                    state.moved.retain(|(place, _)| place.local != *local);
                }
                StatementKind::SetDiscriminant(place, _)
                | StatementKind::Retag(place)
                | StatementKind::SetDropFlag(place, _) => {
                    check_write(place, &state.loans, &statement.span, &mut diagnostics);
                }
                StatementKind::StorageLive(_) => {}
            }
            point += 1;
        }
        state.loans.retain(|loan| {
            last_uses
                .get(&loan.destination)
                .is_some_and(|last| *last >= point)
        });
        visit_terminator(
            body,
            &block.terminator.kind,
            facts,
            &state.loans,
            &mut state.moved,
            &block.terminator.span,
            &mut diagnostics,
        );
        let returned_loans = call_result_loans(
            body,
            &block.terminator.kind,
            &state.loans,
            &block.terminator.span,
        );
        extend_unique(&mut state.loans, returned_loans);
        for successor in ownership_successors(&block.terminator.kind) {
            let Some(entry) = entries.get_mut(successor.0 as usize) else {
                continue;
            };
            let changed = if let Some(entry) = entry {
                merge_analysis_state(entry, &state)
            } else {
                *entry = Some(state.clone());
                true
            };
            if changed {
                work.push_back(successor.0 as usize);
            }
        }
    }
    diagnostics.sort_by_key(|diagnostic| {
        (
            diagnostic.primary.span.file.clone(),
            diagnostic.primary.span.byte_start,
            diagnostic.condition.as_str().to_owned(),
            diagnostic.message.clone(),
        )
    });
    diagnostics.dedup_by(|left, right| {
        left.condition == right.condition
            && left.primary.span == right.primary.span
            && left.message == right.message
    });
    diagnostics
}

fn call_result_loans(
    body: &Body,
    terminator: &TerminatorKind,
    loans: &[Loan],
    origin: &SourceSpan,
) -> Vec<Loan> {
    let TerminatorKind::Call {
        receiver,
        arguments,
        destination: Some(destination),
        ..
    } = terminator
    else {
        return Vec::new();
    };
    let Some(destination_type) = body
        .locals
        .get(destination.local.0 as usize)
        .map(|local| &local.ty)
    else {
        return Vec::new();
    };
    if !contains_non_static_borrow(destination_type) {
        return Vec::new();
    }
    receiver
        .iter()
        .chain(arguments)
        .filter_map(operand_place)
        .flat_map(|argument| {
            loans
                .iter()
                .filter(move |loan| loan.destination == argument.local)
                .map(move |loan| Loan {
                    destination: destination.local,
                    kind: loan.kind,
                    place: loan.place.clone(),
                    origin: origin.clone(),
                })
        })
        .collect()
}

fn contains_non_static_borrow(ty: &Type) -> bool {
    match ty {
        Type::Reference {
            lifetime, referent, ..
        } => lifetime != "static" || contains_non_static_borrow(referent),
        Type::Lifetime(lifetime) => lifetime != "static",
        Type::Nominal(_, arguments)
        | Type::DynamicInterface(_, arguments)
        | Type::Tuple(arguments)
        | Type::Template(arguments) => arguments.iter().any(contains_non_static_borrow),
        Type::Optional(inner)
        | Type::Array(inner, _)
        | Type::Slice(inner)
        | Type::RawPointer { pointee: inner, .. } => contains_non_static_borrow(inner),
        Type::Promise { result, .. } => contains_non_static_borrow(result),
        Type::Function(function) => {
            function.parameters.iter().any(contains_non_static_borrow)
                || contains_non_static_borrow(&function.result)
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Generic(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => false,
    }
}

fn extend_unique<T: PartialEq>(destination: &mut Vec<T>, values: impl IntoIterator<Item = T>) {
    for value in values {
        if !destination.contains(&value) {
            destination.push(value);
        }
    }
}

fn merge_analysis_state(destination: &mut AnalysisState, source: &AnalysisState) -> bool {
    let before = (destination.loans.len(), destination.moved.len());
    extend_unique(&mut destination.loans, source.loans.iter().cloned());
    extend_unique(&mut destination.moved, source.moved.iter().cloned());
    before != (destination.loans.len(), destination.moved.len())
}

fn ownership_successors(terminator: &TerminatorKind) -> Vec<tn_mir::BasicBlockId> {
    match terminator {
        TerminatorKind::Goto(target) => vec![*target],
        TerminatorKind::Switch {
            targets, otherwise, ..
        } => targets
            .iter()
            .map(|(_, target)| *target)
            .chain(std::iter::once(*otherwise))
            .collect(),
        TerminatorKind::Call { success, error, .. } => {
            std::iter::once(*success).chain(*error).collect()
        }
        TerminatorKind::Suspend {
            resume,
            error,
            cancel,
            ..
        } => std::iter::once(*resume)
            .chain(*error)
            .chain(std::iter::once(*cancel))
            .collect(),
        TerminatorKind::Drop { success, .. } => vec![*success],
        TerminatorKind::Return(_)
        | TerminatorKind::Throw(_)
        | TerminatorKind::TaggedReturn { .. }
        | TerminatorKind::Abort(_)
        | TerminatorKind::Unreachable => Vec::new(),
    }
}

fn receiver_loan(rvalue: &Rvalue, destination: LocalId, origin: &SourceSpan) -> Option<Loan> {
    let (Rvalue::VtableLookup {
        object: place,
        receiver,
        ..
    }
    | Rvalue::WitnessLookup {
        object: place,
        receiver,
        ..
    }
    | Rvalue::DirectMethod {
        object: place,
        receiver,
        ..
    }) = rvalue
    else {
        return None;
    };
    let kind = match receiver {
        tn_hir::ReceiverMode::Shared => BorrowKind::Shared,
        tn_hir::ReceiverMode::Mutable => BorrowKind::Mutable,
        tn_hir::ReceiverMode::Move | tn_hir::ReceiverMode::Static => return None,
    };
    Some(Loan {
        destination,
        kind,
        place: place.clone(),
        origin: origin.clone(),
    })
}

fn transferred_loans(rvalue: &Rvalue, destination: LocalId, loans: &[Loan]) -> Vec<Loan> {
    let mut sources = Vec::new();
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::Unary { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::TypeTest { operand, .. } => sources.push(operand),
        Rvalue::Aggregate { fields, .. }
        | Rvalue::Template {
            captures: fields, ..
        }
        | Rvalue::Closure {
            captures: fields, ..
        } => {
            sources.extend(fields);
        }
        _ => {}
    }
    loans
        .iter()
        .filter(|loan| {
            sources.iter().any(|operand| match operand {
                Operand::Copy(place) | Operand::Move(place) => place.local == loan.destination,
                Operand::Constant(_) => false,
            })
        })
        .map(|loan| Loan {
            destination,
            kind: loan.kind,
            place: loan.place.clone(),
            origin: loan.origin.clone(),
        })
        .collect()
}

fn visit_rvalue(
    body: &Body,
    rvalue: &Rvalue,
    facts: &OwnershipFacts,
    loans: &[Loan],
    moved: &mut Vec<(Place, SourceSpan)>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::Unary { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::TypeTest { operand, .. } => {
            visit_operand(body, operand, facts, loans, moved, span, diagnostics);
        }
        Rvalue::CheckedBinary { left, right, .. } => {
            visit_operand(body, left, facts, loans, moved, span, diagnostics);
            visit_operand(body, right, facts, loans, moved, span, diagnostics);
        }
        Rvalue::CheckedIndex { collection, index } => {
            check_use(collection, false, moved, span, diagnostics);
            visit_operand(body, index, facts, loans, moved, span, diagnostics);
        }
        Rvalue::Aggregate { fields, .. } => {
            for field in fields {
                visit_operand(body, field, facts, loans, moved, span, diagnostics);
            }
        }
        Rvalue::Template { captures, .. } => {
            for capture in captures {
                visit_operand(body, capture, facts, loans, moved, span, diagnostics);
            }
        }
        Rvalue::Closure {
            captures,
            body: closure_body,
            ..
        } => {
            for capture in captures {
                visit_operand(body, capture, facts, loans, moved, span, diagnostics);
            }
            diagnostics.extend(analyze_blocks(closure_body, facts));
        }
        Rvalue::Length(place) => {
            check_read(place, loans, span, diagnostics);
            check_use(place, false, moved, span, diagnostics);
        }
        Rvalue::VtableLookup {
            object, receiver, ..
        }
        | Rvalue::WitnessLookup {
            object, receiver, ..
        }
        | Rvalue::DirectMethod {
            object, receiver, ..
        } => match receiver {
            tn_hir::ReceiverMode::Shared | tn_hir::ReceiverMode::Static => {
                check_read(object, loans, span, diagnostics);
                check_use(object, false, moved, span, diagnostics);
            }
            tn_hir::ReceiverMode::Mutable => {
                check_use(object, false, moved, span, diagnostics);
                check_write(object, loans, span, diagnostics);
            }
            tn_hir::ReceiverMode::Move => {
                visit_operand(
                    body,
                    &Operand::Move(object.clone()),
                    facts,
                    loans,
                    moved,
                    span,
                    diagnostics,
                );
            }
        },
        Rvalue::RawOperation { operands, .. } => {
            for operand in operands {
                visit_operand(body, operand, facts, loans, moved, span, diagnostics);
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn visit_terminator(
    body: &Body,
    terminator: &TerminatorKind,
    facts: &OwnershipFacts,
    loans: &[Loan],
    moved: &mut Vec<(Place, SourceSpan)>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match terminator {
        TerminatorKind::Switch { value, .. } | TerminatorKind::Throw(value) => {
            visit_operand(body, value, facts, loans, moved, span, diagnostics);
        }
        TerminatorKind::Suspend {
            value,
            destination,
            error_destination,
            ..
        } => {
            visit_operand(body, value, facts, loans, moved, span, diagnostics);
            if let Some(destination) = destination {
                check_write(destination, loans, span, diagnostics);
                moved.retain(|(moved_place, _)| !place_contains(destination, moved_place));
            }
            if let Some(destination) = error_destination {
                check_write(destination, loans, span, diagnostics);
                moved.retain(|(moved_place, _)| !place_contains(destination, moved_place));
            }
        }
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            destination,
            error_destination,
            ..
        } => {
            visit_operand(body, function, facts, loans, moved, span, diagnostics);
            if let Some(receiver) = receiver {
                let receiver_is_bound = operand_place(receiver).is_some_and(|receiver_place| {
                    method_binding_for_function(body, function)
                        .is_some_and(|bound_place| places_overlap(bound_place, receiver_place))
                });
                if !receiver_is_bound {
                    visit_operand(body, receiver, facts, loans, moved, span, diagnostics);
                }
            }
            for argument in arguments {
                visit_operand(body, argument, facts, loans, moved, span, diagnostics);
            }
            if let Some(destination) = destination {
                check_write(destination, loans, span, diagnostics);
                moved.retain(|(moved_place, _)| !place_contains(destination, moved_place));
            }
            if let Some(destination) = error_destination {
                check_write(destination, loans, span, diagnostics);
                moved.retain(|(moved_place, _)| !place_contains(destination, moved_place));
            }
        }
        TerminatorKind::Return(value)
        | TerminatorKind::TaggedReturn {
            completion: Completion::Success,
            payload: value,
        } => {
            if let Some(value) = value {
                visit_operand(body, value, facts, loans, moved, span, diagnostics);
                if let Some(returned) = operand_place(value)
                    && contains_non_static_borrow(&body.return_type)
                    && let Some(loan) = loans.iter().find(|loan| loan.destination == returned.local)
                    && !body
                        .locals
                        .get(usize::try_from(loan.place.local.0).unwrap_or(usize::MAX))
                        .is_some_and(|local| local.argument)
                    && !(matches!(
                        loan.place.projection.first(),
                        Some(tn_mir::Projection::Dereference)
                    ) && body
                        .locals
                        .get(usize::try_from(loan.place.local.0).unwrap_or(usize::MAX))
                        .is_some_and(|local| matches!(local.ty, Type::RawPointer { .. })))
                {
                    diagnostics.push(diagnostic(
                        "OWNERSHIP_RETURNED_LOCAL_REFERENCE",
                        "cannot return a reference to local storage",
                        span,
                        "this reference would outlive its referent",
                        vec![Label {
                            span: loan.origin.clone(),
                            message: "the local borrow originates here".into(),
                        }],
                    ));
                }
            }
        }
        TerminatorKind::TaggedReturn {
            completion: Completion::Error,
            payload,
        } => {
            if let Some(value) = payload {
                visit_operand(body, value, facts, loans, moved, span, diagnostics);
            }
        }
        TerminatorKind::Drop { place, .. } => {
            check_write(place, loans, span, diagnostics);
            check_use(place, true, moved, span, diagnostics);
        }
        TerminatorKind::Goto(_) | TerminatorKind::Abort(_) | TerminatorKind::Unreachable => {}
    }
    if matches!(terminator, TerminatorKind::Suspend { .. }) {
        for loan in loans {
            let referent = body
                .locals
                .get(usize::try_from(loan.place.local.0).unwrap_or(usize::MAX));
            if referent.is_some_and(|local| !local.argument) {
                diagnostics.push(diagnostic(
                    "OWNERSHIP_BORROW_ACROSS_SUSPEND",
                    "borrow of movable local storage is live across suspension",
                    span,
                    "the referent is not proven to have a stable pinned projection",
                    vec![Label {
                        span: loan.origin.clone(),
                        message: "this loan remains live at the suspension point".into(),
                    }],
                ));
            }
        }
    }
}

fn visit_operand(
    body: &Body,
    operand: &Operand,
    facts: &OwnershipFacts,
    loans: &[Loan],
    moved: &mut Vec<(Place, SourceSpan)>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(place) = operand_place(operand) else {
        return;
    };
    let is_move = matches!(operand, Operand::Move(_));
    check_use(place, is_move, moved, span, diagnostics);
    if is_move {
        check_write(place, loans, span, diagnostics);
        if let Some(ty) = place_root_type(body, place)
            && !facts.is_copy(ty)
        {
            if !place.projection.is_empty()
                && facts.has_drop(ty)
                && !is_optional_payload_move(body, place)
            {
                diagnostics.push(diagnostic(
                    "OWNERSHIP_PARTIAL_MOVE_FROM_DROP_TYPE",
                    "cannot partially move a value with a destructor",
                    span,
                    "move the complete value or borrow this field",
                    Vec::new(),
                ));
            }
            moved.push((place.clone(), span.clone()));
        }
    } else {
        check_read(place, loans, span, diagnostics);
    }
}

fn check_use(
    place: &Place,
    moving: bool,
    moved: &[(Place, SourceSpan)],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some((_, origin)) = moved
        .iter()
        .find(|(moved_place, _)| places_overlap(moved_place, place))
    {
        diagnostics.push(diagnostic(
            "OWNERSHIP_USE_AFTER_MOVE",
            if moving {
                "cannot move a value that was already moved"
            } else {
                "cannot use a value after it was moved"
            },
            span,
            "this access requires the moved ownership path",
            vec![Label {
                span: origin.clone(),
                message: "the ownership path was moved here".into(),
            }],
        ));
    }
}

fn check_read(place: &Place, loans: &[Loan], span: &SourceSpan, diagnostics: &mut Vec<Diagnostic>) {
    if let Some(loan) = loans
        .iter()
        .find(|loan| loan.kind == BorrowKind::Mutable && places_overlap(&loan.place, place))
    {
        diagnostics.push(diagnostic(
            "OWNERSHIP_READ_DURING_MUTABLE_BORROW",
            "cannot read through another access while a mutable borrow is live",
            span,
            "this read aliases the mutable loan",
            vec![Label {
                span: loan.origin.clone(),
                message: "the mutable loan starts here".into(),
            }],
        ));
    }
}

fn check_write(
    place: &Place,
    loans: &[Loan],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(loan) = loans.iter().find(|loan| places_overlap(&loan.place, place)) {
        diagnostics.push(diagnostic(
            "OWNERSHIP_WRITE_DURING_BORROW",
            "cannot mutate or move a borrowed ownership path",
            span,
            "this write overlaps a live loan",
            vec![Label {
                span: loan.origin.clone(),
                message: "the live loan starts here".into(),
            }],
        ));
    }
}

fn places_overlap(left: &Place, right: &Place) -> bool {
    if left.local != right.local {
        return false;
    }
    for (left, right) in left.projection.iter().zip(&right.projection) {
        match (left, right) {
            (Projection::Field { index: left, .. }, Projection::Field { index: right, .. })
                if left != right =>
            {
                return false;
            }
            (Projection::Downcast(left), Projection::Downcast(right)) if left != right => {
                return false;
            }
            _ => {}
        }
    }
    true
}

fn place_contains(parent: &Place, child: &Place) -> bool {
    parent.local == child.local
        && parent.projection.len() <= child.projection.len()
        && parent
            .projection
            .iter()
            .zip(&child.projection)
            .all(|(left, right)| left == right)
}

fn operand_place(operand: &Operand) -> Option<&Place> {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => Some(place),
        Operand::Constant(_) => None,
    }
}

fn method_binding_for_function<'body>(
    body: &'body Body,
    function: &Operand,
) -> Option<&'body Place> {
    let function_place = operand_place(function)?;
    if !function_place.projection.is_empty() {
        return None;
    }
    body.blocks
        .iter()
        .flat_map(|block| &block.statements)
        .rev()
        .find_map(|statement| {
            let StatementKind::Assign(destination, rvalue) = &statement.kind else {
                return None;
            };
            if destination.local != function_place.local || !destination.projection.is_empty() {
                return None;
            }
            match rvalue.as_ref() {
                Rvalue::VtableLookup { object, .. }
                | Rvalue::WitnessLookup { object, .. }
                | Rvalue::DirectMethod { object, .. } => Some(object),
                _ => None,
            }
        })
}

fn place_root_type<'body>(body: &'body Body, place: &Place) -> Option<&'body Type> {
    body.locals
        .get(usize::try_from(place.local.0).ok()?)
        .map(|local| &local.ty)
}

fn is_optional_payload_move(body: &Body, place: &Place) -> bool {
    matches!(
        (place.projection.as_slice(), place_root_type(body, place)),
        ([Projection::Downcast(1)], Some(Type::Optional(_)))
    )
}

fn last_uses(body: &Body) -> BTreeMap<LocalId, usize> {
    let mut uses = BTreeMap::new();
    let mut point = 0_usize;
    for block in &body.blocks {
        for statement in &block.statements {
            visit_statement_locals(&statement.kind, |local| {
                uses.insert(local, point);
            });
            point += 1;
        }
        visit_terminator_locals(&block.terminator.kind, |local| {
            uses.insert(local, point);
        });
        point += 1;
    }
    uses
}

fn visit_statement_locals(statement: &StatementKind, mut visit: impl FnMut(LocalId)) {
    match statement {
        StatementKind::Assign(place, rvalue) => {
            visit(place.local);
            visit_rvalue_locals(rvalue, visit);
        }
        StatementKind::SetDiscriminant(place, _)
        | StatementKind::Retag(place)
        | StatementKind::SetDropFlag(place, _) => visit(place.local),
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => visit(*local),
        StatementKind::Borrow {
            destination, place, ..
        } => {
            visit(*destination);
            visit(place.local);
        }
    }
}

fn visit_rvalue_locals(rvalue: &Rvalue, mut visit: impl FnMut(LocalId)) {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::Unary { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::TypeTest { operand, .. } => {
            visit_operand_local(operand, visit);
        }
        Rvalue::CheckedBinary { left, right, .. } => {
            visit_operand_local(left, &mut visit);
            visit_operand_local(right, visit);
        }
        Rvalue::CheckedIndex { collection, index } => {
            visit(collection.local);
            visit_operand_local(index, visit);
        }
        Rvalue::Aggregate { fields, .. } => {
            for operand in fields {
                visit_operand_local(operand, &mut visit);
            }
        }
        Rvalue::Template { captures, .. } | Rvalue::Closure { captures, .. } => {
            for operand in captures {
                visit_operand_local(operand, &mut visit);
            }
        }
        Rvalue::Length(place)
        | Rvalue::VtableLookup { object: place, .. }
        | Rvalue::WitnessLookup { object: place, .. }
        | Rvalue::DirectMethod { object: place, .. } => visit(place.local),
        Rvalue::RawOperation { operands, .. } => {
            for operand in operands {
                visit_operand_local(operand, &mut visit);
            }
        }
    }
}

fn visit_terminator_locals(terminator: &TerminatorKind, mut visit: impl FnMut(LocalId)) {
    match terminator {
        TerminatorKind::Switch { value, .. } | TerminatorKind::Throw(value) => {
            visit_operand_local(value, visit);
        }
        TerminatorKind::Suspend {
            value,
            destination,
            error_destination,
            ..
        } => {
            visit_operand_local(value, &mut visit);
            if let Some(place) = destination {
                visit(place.local);
            }
            if let Some(place) = error_destination {
                visit(place.local);
            }
        }
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            destination,
            error_destination,
            ..
        } => {
            visit_operand_local(function, &mut visit);
            if let Some(receiver) = receiver {
                visit_operand_local(receiver, &mut visit);
            }
            for argument in arguments {
                visit_operand_local(argument, &mut visit);
            }
            if let Some(place) = destination {
                visit(place.local);
            }
            if let Some(place) = error_destination {
                visit(place.local);
            }
        }
        TerminatorKind::Return(value) | TerminatorKind::TaggedReturn { payload: value, .. } => {
            if let Some(value) = value {
                visit_operand_local(value, visit);
            }
        }
        TerminatorKind::Drop { place, .. } => visit(place.local),
        TerminatorKind::Goto(_) | TerminatorKind::Abort(_) | TerminatorKind::Unreachable => {}
    }
}

fn visit_operand_local(operand: &Operand, mut visit: impl FnMut(LocalId)) {
    if let Some(place) = operand_place(operand) {
        visit(place.local);
        for projection in &place.projection {
            if let Projection::Index(local) = projection {
                visit(*local);
            }
        }
    }
}

fn internal_span() -> SourceSpan {
    SourceSpan::new("<compiler>", 0..0, "")
}

fn diagnostic(
    id: &str,
    message: impl Into<String>,
    span: &SourceSpan,
    label: &str,
    secondary: Vec<Label>,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        ConditionId::new(id).expect("static condition is valid"),
        message,
        Label {
            span: span.clone(),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    );
    diagnostic.secondary = secondary;
    diagnostic
}
