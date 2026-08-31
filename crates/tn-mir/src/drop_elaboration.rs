use crate::{
    BasicBlock, BasicBlockId, Body, LocalId, Operand, Place, Rvalue, Statement, StatementKind,
    Terminator, TerminatorKind,
};
use std::collections::BTreeSet;
use tn_hir::{DeclarationId, Type};

/// Nominal layout facts needed to elaborate deterministic destruction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DropSemantics {
    pub nominal: BTreeSet<DeclarationId>,
}

impl DropSemantics {
    pub fn needs_drop(&self, ty: &Type) -> bool {
        match ty {
            Type::String
            | Type::Function(_)
            | Type::Promise { .. }
            | Type::DynamicInterface(_, _) => true,
            Type::Nominal(declaration, _) => self.nominal.contains(declaration),
            Type::ErrorUnion(effects) => effects.iter().any(|effect| self.nominal.contains(effect)),
            Type::Optional(inner) | Type::Array(inner, _) => self.needs_drop(inner),
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().any(|element| self.needs_drop(element))
            }
            Type::Primitive(_)
            | Type::Str
            | Type::Slice(_)
            | Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
            // A generic parameter may be instantiated with a move-only value. Keep its
            // ownership path live until monomorphization can substitute the concrete type;
            // primitive instantiations simply lower this conditional drop to a no-op.
            Type::Generic(_) => true,
        }
    }
}

/// Adds drop-flag updates and reverse-order cleanup chains to function exits.
///
/// The generic MIR `Drop` terminator is conditional on the compiler-maintained flag for its place.
/// Later lowering expands these edges into concrete destructor calls and field traversal.
///
/// # Panics
///
/// Panics if a compiler-generated body exceeds the representable local or basic-block identity
/// space.
pub fn elaborate_drops(body: &Body, semantics: &DropSemantics) -> Body {
    let mut result = body.clone();
    for block in &mut result.blocks {
        instrument_statements(block);
        instrument_terminator_moves(block);
    }
    let droppable = result
        .locals
        .iter()
        .enumerate()
        .filter(|(_, local)| {
            // Method receivers are borrowed/owned by the caller.  The callee may mutate or
            // explicitly close the receiver, but it must never destroy the receiver storage
            // itself.  In particular, class constructor initializers receive the freshly
            // allocated object from their wrapper; dropping `this` here would return a dangling
            // object to that wrapper.
            !matches!(local.name.as_deref(), Some("this" | "self"))
                && semantics.needs_drop(&local.ty)
        })
        .map(|(index, _)| LocalId(u32::try_from(index).expect("MIR local limit")))
        .collect::<Vec<_>>();
    initialize_argument_flags(&mut result, &droppable);
    split_storage_deaths(&mut result, &droppable);
    let (entries, exits) = active_local_sets(&result);
    let scoped_blocks = result.blocks.len();
    for (block_index, active_exit) in exits.iter().enumerate().take(scoped_blocks) {
        rewrite_scope_exit_edges(&mut result, block_index, &entries, active_exit, &droppable);
        if matches!(
            result.blocks[block_index].terminator.kind,
            TerminatorKind::Return(_)
                | TerminatorKind::Throw(_)
                | TerminatorKind::TaggedReturn { .. }
        ) {
            let active = droppable
                .iter()
                .filter(|local| active_exit.contains(local))
                .copied()
                .collect::<Vec<_>>();
            add_exit_cleanup(&mut result, block_index, &active);
        }
    }
    for block in &mut result.blocks {
        for statement in &mut block.statements {
            if let StatementKind::Assign(_, value) = &mut statement.kind
                && let Rvalue::Closure { body, .. } = value.as_mut()
            {
                **body = elaborate_drops(body, semantics);
            }
        }
    }
    result
}

fn initialize_argument_flags(body: &mut Body, droppable: &[LocalId]) {
    let Some(entry) = body.blocks.first_mut() else {
        return;
    };
    let span = entry.terminator.span.clone();
    let flags = droppable
        .iter()
        .filter(|local| {
            body.locals
                .get(local.0 as usize)
                .is_some_and(|local| local.argument)
        })
        .map(|local| Statement {
            kind: StatementKind::SetDropFlag(Place::local(*local), true),
            span: span.clone(),
        });
    entry.statements.splice(0..0, flags);
}

fn split_storage_deaths(body: &mut Body, droppable: &[LocalId]) {
    let original = std::mem::take(&mut body.blocks);
    body.blocks = original
        .iter()
        .map(|block| BasicBlock {
            statements: Vec::new(),
            terminator: block.terminator.clone(),
        })
        .collect();
    for (original_index, block) in original.into_iter().enumerate() {
        let mut current = original_index;
        for statement in block.statements {
            let StatementKind::StorageDead(local) = statement.kind else {
                body.blocks[current].statements.push(statement);
                continue;
            };
            if !droppable.contains(&local) {
                body.blocks[current].statements.push(Statement {
                    kind: StatementKind::StorageDead(local),
                    span: statement.span,
                });
                continue;
            }
            let next = BasicBlockId(u32::try_from(body.blocks.len()).expect("MIR block limit"));
            body.blocks[current].terminator = Terminator {
                kind: TerminatorKind::Drop {
                    place: Place::local(local),
                    success: next,
                },
                span: statement.span.clone(),
            };
            body.blocks.push(BasicBlock {
                statements: vec![Statement {
                    kind: StatementKind::StorageDead(local),
                    span: statement.span,
                }],
                terminator: block.terminator.clone(),
            });
            current = next.0 as usize;
        }
        body.blocks[current].terminator = block.terminator;
    }
}

fn active_local_sets(body: &Body) -> (Vec<BTreeSet<LocalId>>, Vec<BTreeSet<LocalId>>) {
    let arguments = body
        .locals
        .iter()
        .enumerate()
        .filter(|(_, local)| local.argument)
        .map(|(index, _)| LocalId(u32::try_from(index).expect("MIR local limit")))
        .collect::<BTreeSet<_>>();
    let mut entries = vec![None::<BTreeSet<LocalId>>; body.blocks.len()];
    entries[0] = Some(arguments);
    loop {
        let mut changed = false;
        for (index, block) in body.blocks.iter().enumerate() {
            let Some(mut active) = entries[index].clone() else {
                continue;
            };
            apply_storage_lifetimes(&mut active, &block.statements);
            for successor in successors(&block.terminator.kind) {
                let target = successor.0 as usize;
                let next = entries[target].as_ref().map_or_else(
                    || active.clone(),
                    |entry| entry.intersection(&active).copied().collect(),
                );
                if entries[target].as_ref() != Some(&next) {
                    entries[target] = Some(next);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let entries = entries
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect::<Vec<_>>();
    let exits = body
        .blocks
        .iter()
        .zip(&entries)
        .map(|(block, entry)| {
            let mut active = entry.clone();
            apply_storage_lifetimes(&mut active, &block.statements);
            active
        })
        .collect();
    (entries, exits)
}

fn apply_storage_lifetimes(active: &mut BTreeSet<LocalId>, statements: &[Statement]) {
    for statement in statements {
        match statement.kind {
            StatementKind::StorageLive(local) => {
                active.insert(local);
            }
            StatementKind::StorageDead(local) => {
                active.remove(&local);
            }
            _ => {}
        }
    }
}

fn rewrite_scope_exit_edges(
    body: &mut Body,
    block_index: usize,
    entries: &[BTreeSet<LocalId>],
    active: &BTreeSet<LocalId>,
    droppable: &[LocalId],
) {
    let terminator = body.blocks[block_index].terminator.clone();
    let span = terminator.span.clone();
    let leaving = |target: BasicBlockId| {
        droppable
            .iter()
            .filter(|local| active.contains(local) && !entries[target.0 as usize].contains(local))
            .copied()
            .collect::<Vec<_>>()
    };
    let rewritten = match terminator.kind {
        TerminatorKind::Goto(target) => {
            TerminatorKind::Goto(cleanup_chain(body, &leaving(target), target, &span))
        }
        TerminatorKind::Switch {
            value,
            targets,
            otherwise,
        } => TerminatorKind::Switch {
            value,
            targets: targets
                .into_iter()
                .map(|(value, target)| {
                    (value, cleanup_chain(body, &leaving(target), target, &span))
                })
                .collect(),
            otherwise: cleanup_chain(body, &leaving(otherwise), otherwise, &span),
        },
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            destination,
            error_destination,
            success,
            error,
        } => {
            let success = cleanup_chain(body, &leaving(success), success, &span);
            let success = initialize_edge_destination(body, destination.as_ref(), success, &span);
            let error = error.map(|target| {
                let cleanup = cleanup_chain(body, &leaving(target), target, &span);
                initialize_edge_destination(body, error_destination.as_ref(), cleanup, &span)
            });
            TerminatorKind::Call {
                function,
                receiver,
                arguments,
                destination,
                error_destination,
                success,
                error,
            }
        }
        TerminatorKind::Suspend {
            value,
            destination,
            error_destination,
            resume,
            error,
            cancel,
        } => {
            let resume = cleanup_chain(body, &leaving(resume), resume, &span);
            let resume = initialize_edge_destination(body, destination.as_ref(), resume, &span);
            let error = error.map(|target| {
                let cleanup = cleanup_chain(body, &leaving(target), target, &span);
                initialize_edge_destination(body, error_destination.as_ref(), cleanup, &span)
            });
            let cancel = cleanup_chain(
                body,
                &droppable
                    .iter()
                    .filter(|local| active.contains(local))
                    .copied()
                    .collect::<Vec<_>>(),
                cancel,
                &span,
            );
            TerminatorKind::Suspend {
                value,
                destination,
                error_destination,
                resume,
                error,
                cancel,
            }
        }
        kind @ (TerminatorKind::Return(_)
        | TerminatorKind::Throw(_)
        | TerminatorKind::TaggedReturn { .. }
        | TerminatorKind::Drop { .. }
        | TerminatorKind::Abort(_)
        | TerminatorKind::Unreachable) => kind,
    };
    body.blocks[block_index].terminator = Terminator {
        kind: rewritten,
        span,
    };
}

fn initialize_edge_destination(
    body: &mut Body,
    destination: Option<&Place>,
    target: BasicBlockId,
    span: &tn_diagnostics::SourceSpan,
) -> BasicBlockId {
    let Some(destination) = destination else {
        return target;
    };
    let block = BasicBlockId(u32::try_from(body.blocks.len()).expect("MIR block limit"));
    body.blocks.push(BasicBlock {
        statements: vec![Statement {
            kind: StatementKind::SetDropFlag(destination.clone(), true),
            span: span.clone(),
        }],
        terminator: Terminator {
            kind: TerminatorKind::Goto(target),
            span: span.clone(),
        },
    });
    block
}

fn cleanup_chain(
    body: &mut Body,
    locals: &[LocalId],
    final_target: BasicBlockId,
    span: &tn_diagnostics::SourceSpan,
) -> BasicBlockId {
    let mut next = final_target;
    for local in locals {
        let block = BasicBlockId(u32::try_from(body.blocks.len()).expect("MIR block limit"));
        body.blocks.push(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator {
                kind: TerminatorKind::Drop {
                    place: Place::local(*local),
                    success: next,
                },
                span: span.clone(),
            },
        });
        next = block;
    }
    next
}

fn successors(terminator: &TerminatorKind) -> Vec<BasicBlockId> {
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

fn instrument_statements(block: &mut BasicBlock) {
    let mut statements = Vec::new();
    for statement in std::mem::take(&mut block.statements) {
        let span = statement.span.clone();
        match &statement.kind {
            StatementKind::StorageLive(local) => {
                let local = *local;
                statements.push(statement);
                statements.push(Statement {
                    kind: StatementKind::SetDropFlag(Place::local(local), false),
                    span,
                });
            }
            StatementKind::Assign(destination, rvalue) => {
                let moved = moved_places_in_rvalue(rvalue);
                let destination = destination.clone();
                statements.push(statement);
                for place in moved {
                    statements.push(Statement {
                        kind: StatementKind::SetDropFlag(place, false),
                        span: span.clone(),
                    });
                }
                statements.push(Statement {
                    kind: StatementKind::SetDropFlag(destination, true),
                    span,
                });
            }
            _ => statements.push(statement),
        }
    }
    block.statements = statements;
}

fn instrument_terminator_moves(block: &mut BasicBlock) {
    let span = block.terminator.span.clone();
    for place in moved_places_in_terminator(&block.terminator.kind) {
        block.statements.push(Statement {
            kind: StatementKind::SetDropFlag(place, false),
            span: span.clone(),
        });
    }
}

fn add_exit_cleanup(body: &mut Body, block_index: usize, droppable: &[LocalId]) {
    let final_terminator = body.blocks[block_index].terminator.clone();
    let span = final_terminator.span.clone();
    let final_block = BasicBlockId(u32::try_from(body.blocks.len()).expect("MIR block limit"));
    body.blocks.push(BasicBlock {
        statements: Vec::new(),
        terminator: final_terminator,
    });
    let mut next = final_block;
    for local in droppable {
        let block = BasicBlockId(u32::try_from(body.blocks.len()).expect("MIR block limit"));
        body.blocks.push(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator {
                kind: TerminatorKind::Drop {
                    place: Place::local(*local),
                    success: next,
                },
                span: span.clone(),
            },
        });
        next = block;
    }
    body.blocks[block_index].terminator = Terminator {
        kind: TerminatorKind::Goto(next),
        span,
    };
}

fn moved_places_in_rvalue(rvalue: &Rvalue) -> Vec<Place> {
    let mut moved = Vec::new();
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::Unary { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::TypeTest { operand, .. } => collect_move(operand, &mut moved),
        Rvalue::CheckedBinary { left, right, .. } => {
            collect_move(left, &mut moved);
            collect_move(right, &mut moved);
        }
        Rvalue::CheckedIndex { index, .. } => collect_move(index, &mut moved),
        Rvalue::Aggregate { fields, .. } => collect_moves(fields, &mut moved),
        Rvalue::Closure { captures, .. } | Rvalue::Template { captures, .. } => {
            collect_moves(captures, &mut moved);
        }
        Rvalue::RawOperation { operands, .. } => collect_moves(operands, &mut moved),
        Rvalue::Length(_)
        | Rvalue::VtableLookup { .. }
        | Rvalue::WitnessLookup { .. }
        | Rvalue::DirectMethod { .. } => {}
    }
    moved
}

fn moved_places_in_terminator(terminator: &TerminatorKind) -> Vec<Place> {
    let mut moved = Vec::new();
    match terminator {
        TerminatorKind::Switch { value, .. }
        | TerminatorKind::Throw(value)
        | TerminatorKind::Suspend { value, .. }
        | TerminatorKind::Return(Some(value))
        | TerminatorKind::TaggedReturn {
            payload: Some(value),
            ..
        } => collect_move(value, &mut moved),
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            ..
        } => {
            collect_move(function, &mut moved);
            if let Some(receiver) = receiver {
                collect_move(receiver, &mut moved);
            }
            collect_moves(arguments, &mut moved);
        }
        TerminatorKind::Return(None)
        | TerminatorKind::TaggedReturn { payload: None, .. }
        | TerminatorKind::Drop { .. }
        | TerminatorKind::Goto(_)
        | TerminatorKind::Abort(_)
        | TerminatorKind::Unreachable => {}
    }
    moved
}

fn collect_moves(operands: &[Operand], moved: &mut Vec<Place>) {
    for operand in operands {
        collect_move(operand, moved);
    }
}

fn collect_move(operand: &Operand, moved: &mut Vec<Place>) {
    if let Operand::Move(place) = operand {
        moved.push(place.clone());
    }
}
