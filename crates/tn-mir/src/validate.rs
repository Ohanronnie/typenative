use crate::{
    BasicBlockId, Body, Completion, Operand, Place, Projection, Rvalue, StatementKind,
    TemplatePart, TerminatorKind,
};
use std::collections::{BTreeSet, VecDeque};
use tn_hir::{FunctionType, PrimitiveType, Type};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MirValidationError {
    #[error("MIR body has no basic blocks")]
    EmptyBody,
    #[error("basic block {block} targets missing block {target}")]
    InvalidTarget { block: u32, target: u32 },
    #[error("basic block {block} references missing local {local}")]
    InvalidLocal { block: u32, local: u32 },
    #[error("basic block {block} uses local {local} before initialization")]
    UninitializedUse { block: u32, local: u32 },
    #[error("basic block {block} assigns incompatible types {actual_type:?} and {expected_type:?}")]
    TypeMismatch {
        block: u32,
        actual_type: Type,
        expected_type: Type,
    },
    #[error("basic block {block} has an invalid projection")]
    InvalidProjection { block: u32 },
    #[error("basic block {block} has an invalid call error edge")]
    InvalidErrorEdge { block: u32 },
    #[error("basic block {block} returns {actual:?}, expected {expected:?}")]
    InvalidReturn {
        block: u32,
        actual: Option<Type>,
        expected: Type,
    },
    #[error("basic block {block} creates a borrow with an incompatible destination type")]
    InvalidBorrowType { block: u32 },
    #[error("borrow region {region} is created more than once")]
    DuplicateRegion { region: u32 },
    #[error("basic block {block} calls a non-function value")]
    InvalidCallable { block: u32 },
    #[error("basic block {block} throws a value outside the body's closed effect set")]
    InvalidThrow { block: u32 },
    #[error("basic block {block} switches on an unsupported value type")]
    InvalidSwitchType { block: u32 },
    #[error("basic block {block} references missing template capture {capture}")]
    InvalidTemplateCapture { block: u32, capture: u32 },
}

/// Validates structural, typing, control-flow, and initialization invariants of generic MIR.
///
/// # Errors
///
/// Returns every independently detectable invariant violation in deterministic block order.
pub fn validate(body: &Body) -> Result<(), Vec<MirValidationError>> {
    if body.blocks.is_empty() {
        return Err(vec![MirValidationError::EmptyBody]);
    }
    let mut errors = Vec::new();
    validate_targets(body, &mut errors);
    validate_regions(body, &mut errors);
    validate_dataflow(body, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_regions(body: &Body, errors: &mut Vec<MirValidationError>) {
    let mut regions = BTreeSet::new();
    for block in &body.blocks {
        for statement in &block.statements {
            if let StatementKind::Borrow { region, .. } = statement.kind
                && !regions.insert(region)
            {
                errors.push(MirValidationError::DuplicateRegion { region: region.0 });
            }
        }
    }
}

fn validate_targets(body: &Body, errors: &mut Vec<MirValidationError>) {
    for (index, block) in body.blocks.iter().enumerate() {
        let block_id = u32::try_from(index).unwrap_or(u32::MAX);
        let targets = terminator_targets(&block.terminator.kind);
        for target in targets {
            if usize::try_from(target.0)
                .ok()
                .is_none_or(|target| target >= body.blocks.len())
            {
                errors.push(MirValidationError::InvalidTarget {
                    block: block_id,
                    target: target.0,
                });
            }
        }
        if let TerminatorKind::Call { success, error, .. } = &block.terminator.kind
            && error.is_some_and(|error| error == *success)
        {
            errors.push(MirValidationError::InvalidErrorEdge { block: block_id });
        }
        if let TerminatorKind::Suspend {
            resume,
            error,
            cancel,
            ..
        } = &block.terminator.kind
            && (error.is_some_and(|error| error == *resume || error == *cancel) || resume == cancel)
        {
            errors.push(MirValidationError::InvalidErrorEdge { block: block_id });
        }
    }
}

fn validate_dataflow(body: &Body, errors: &mut Vec<MirValidationError>) {
    let arguments = body
        .locals
        .iter()
        .enumerate()
        .filter_map(|(index, local)| local.argument.then_some(index))
        .collect::<BTreeSet<_>>();
    let mut entry_states = vec![None; body.blocks.len()];
    entry_states[0] = Some(arguments);
    let mut work = VecDeque::from([0_usize]);
    while let Some(block_index) = work.pop_front() {
        let mut initialized = entry_states[block_index].clone().unwrap_or_default();
        let block_id = u32::try_from(block_index).unwrap_or(u32::MAX);
        let block = &body.blocks[block_index];
        for statement in &block.statements {
            validate_statement(body, block_id, &statement.kind, &mut initialized, errors);
        }
        validate_terminator(
            body,
            block_id,
            &block.terminator.kind,
            &mut initialized,
            errors,
        );
        for target in terminator_targets(&block.terminator.kind) {
            let Ok(target) = usize::try_from(target.0) else {
                continue;
            };
            if target >= body.blocks.len() {
                continue;
            }
            let mut outgoing = initialized.clone();
            if let TerminatorKind::Call {
                destination,
                error_destination,
                success,
                error,
                ..
            } = &block.terminator.kind
            {
                let initialized_destination = if usize::try_from(success.0).ok() == Some(target) {
                    destination
                } else if error.and_then(|error| usize::try_from(error.0).ok()) == Some(target) {
                    error_destination
                } else {
                    &None
                };
                if let Some(destination) = initialized_destination
                    && destination.projection.is_empty()
                    && let Ok(local) = usize::try_from(destination.local.0)
                {
                    outgoing.insert(local);
                }
            }
            if let TerminatorKind::Suspend {
                destination,
                error_destination,
                resume,
                error,
                ..
            } = &block.terminator.kind
            {
                let initialized_destination = if usize::try_from(resume.0).ok() == Some(target) {
                    destination
                } else if error.and_then(|error| usize::try_from(error.0).ok()) == Some(target) {
                    error_destination
                } else {
                    &None
                };
                if let Some(destination) = initialized_destination
                    && destination.projection.is_empty()
                    && let Ok(local) = usize::try_from(destination.local.0)
                {
                    outgoing.insert(local);
                }
            }
            let next = match &entry_states[target] {
                Some(existing) => existing.intersection(&outgoing).copied().collect(),
                None => outgoing,
            };
            if entry_states[target].as_ref() != Some(&next) {
                entry_states[target] = Some(next);
                work.push_back(target);
            }
        }
    }
}

fn validate_statement(
    body: &Body,
    block: u32,
    statement: &StatementKind,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    match statement {
        StatementKind::Assign(place, rvalue) => {
            let destination = place_type(body, block, place, errors);
            let source = rvalue_type(body, block, rvalue, initialized, errors);
            if let (Some(destination), Some(source)) = (destination, source)
                && !types_compatible(&source, &destination)
                && destination != Type::Error
                && source != Type::Error
            {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    actual_type: source,
                    expected_type: destination,
                });
            }
            if place.projection.is_empty()
                && let Ok(local) = usize::try_from(place.local.0)
            {
                initialized.insert(local);
            }
        }
        StatementKind::StorageLive(local) => {
            check_local(body, block, local.0, errors);
        }
        StatementKind::StorageDead(local) => {
            if let Some(local) = check_local(body, block, local.0, errors) {
                initialized.remove(&local);
            }
        }
        StatementKind::Borrow {
            destination,
            kind,
            place,
            ..
        } => {
            use_place(body, block, place, initialized, errors);
            let source = place_type(body, block, place, errors);
            if let Some(destination) = check_local(body, block, destination.0, errors) {
                let destination_type = &body.locals[destination].ty;
                let valid = source.is_some_and(|source| {
                    matches!(
                        destination_type,
                        Type::Reference {
                            mutable,
                            referent,
                            ..
                        } if *mutable == matches!(kind, crate::BorrowKind::Mutable)
                            && referent.as_ref() == &source
                    )
                });
                if !valid {
                    errors.push(MirValidationError::InvalidBorrowType { block });
                }
                initialized.insert(destination);
            }
        }
        StatementKind::SetDiscriminant(place, _) | StatementKind::Retag(place) => {
            use_place(body, block, place, initialized, errors);
        }
        StatementKind::SetDropFlag(place, _) => {
            place_type(body, block, place, errors);
        }
    }
}

fn validate_terminator(
    body: &Body,
    block: u32,
    terminator: &TerminatorKind,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    match terminator {
        TerminatorKind::Switch { value, .. } => {
            let ty = operand_type(body, block, value, initialized, errors);
            if !ty.as_ref().is_some_and(valid_switch_type) {
                errors.push(MirValidationError::InvalidSwitchType { block });
            }
        }
        TerminatorKind::Throw(value) => {
            let ty = operand_type(body, block, value, initialized, errors);
            if !ty.is_some_and(|ty| match ty {
                Type::Nominal(id, _) => body.effects.contains(&id),
                Type::ErrorUnion(effects) => {
                    effects.iter().all(|effect| body.effects.contains(effect))
                }
                _ => false,
            }) {
                errors.push(MirValidationError::InvalidThrow { block });
            }
        }
        TerminatorKind::Suspend {
            value,
            destination,
            error_destination,
            error,
            ..
        } => validate_suspend(
            body,
            block,
            value,
            destination.as_ref(),
            error_destination.as_ref(),
            error.as_ref(),
            initialized,
            errors,
        ),
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            destination,
            error_destination,
            error,
            ..
        } => {
            if let Some(receiver) = receiver {
                let _ = operand_type(body, block, receiver, initialized, errors);
            }
            validate_call(
                body,
                block,
                function,
                arguments,
                destination.as_ref(),
                error_destination.as_ref(),
                error.as_ref(),
                initialized,
                errors,
            );
        }
        TerminatorKind::Return(value) => {
            let actual = value
                .as_ref()
                .and_then(|value| operand_type(body, block, value, initialized, errors));
            let void = Type::Primitive(PrimitiveType::Void);
            let valid = if body.return_type == void {
                actual.is_none()
            } else {
                actual
                    .as_ref()
                    .is_some_and(|actual| types_compatible(actual, &body.return_type))
            };
            if !valid {
                errors.push(MirValidationError::InvalidReturn {
                    block,
                    actual,
                    expected: body.return_type.clone(),
                });
            }
        }
        TerminatorKind::TaggedReturn {
            completion,
            payload,
        } => validate_tagged_return(
            body,
            block,
            *completion,
            payload.as_ref(),
            initialized,
            errors,
        ),
        TerminatorKind::Drop { place, .. } => {
            place_type(body, block, place, errors);
        }
        TerminatorKind::Goto(_) | TerminatorKind::Abort(_) | TerminatorKind::Unreachable => {}
    }
}

fn validate_tagged_return(
    body: &Body,
    block: u32,
    completion: Completion,
    payload: Option<&Operand>,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    let actual = payload.and_then(|value| operand_type(body, block, value, initialized, errors));
    match completion {
        Completion::Success => {
            let void = Type::Primitive(PrimitiveType::Void);
            let valid = if body.return_type == void {
                actual.is_none()
            } else {
                actual
                    .as_ref()
                    .is_some_and(|actual| types_compatible(actual, &body.return_type))
            };
            if !valid {
                errors.push(MirValidationError::InvalidReturn {
                    block,
                    actual,
                    expected: body.return_type.clone(),
                });
            }
        }
        Completion::Error => {
            if !actual.is_some_and(|ty| match ty {
                Type::Nominal(id, _) => body.effects.contains(&id),
                Type::ErrorUnion(effects) => {
                    effects.iter().all(|effect| body.effects.contains(effect))
                }
                _ => false,
            }) {
                errors.push(MirValidationError::InvalidThrow { block });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_call(
    body: &Body,
    block: u32,
    function: &Operand,
    arguments: &[Operand],
    destination: Option<&Place>,
    error_destination: Option<&Place>,
    error: Option<&BasicBlockId>,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    let function_type = operand_type(body, block, function, initialized, errors);
    let argument_types = arguments
        .iter()
        .filter_map(|argument| operand_type(body, block, argument, initialized, errors))
        .collect::<Vec<_>>();
    let Some(Type::Function(FunctionType {
        parameters,
        result,
        effects,
        is_async,
        ..
    })) = function_type
    else {
        if function_type.is_some() {
            errors.push(MirValidationError::InvalidCallable { block });
        }
        return;
    };
    if !types_compatible_slice(&argument_types, &parameters) {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: Type::Tuple(argument_types),
            expected_type: Type::Tuple(parameters),
        });
    }
    let fallible_call = !is_async && !effects.is_empty();
    if fallible_call != error.is_some() {
        errors.push(MirValidationError::InvalidErrorEdge { block });
    }
    let expected_error = fallible_call.then(|| Type::ErrorUnion(effects.clone()));
    let actual_error =
        error_destination.and_then(|destination| place_type(body, block, destination, errors));
    if actual_error != expected_error {
        errors.push(MirValidationError::InvalidErrorEdge { block });
    }
    if let Some(destination) = destination {
        let destination_type = place_type(body, block, destination, errors);
        if !destination_type
            .as_ref()
            .is_some_and(|destination| types_compatible(&result, destination))
        {
            errors.push(MirValidationError::TypeMismatch {
                block,
                actual_type: *result,
                expected_type: destination_type.unwrap_or(Type::Error),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_suspend(
    body: &Body,
    block: u32,
    value: &Operand,
    destination: Option<&Place>,
    error_destination: Option<&Place>,
    error: Option<&BasicBlockId>,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    let promise = operand_type(body, block, value, initialized, errors);
    let Some(Type::Promise {
        result, effects, ..
    }) = promise
    else {
        if let Some(actual_type) = promise {
            errors.push(MirValidationError::TypeMismatch {
                block,
                actual_type,
                expected_type: Type::Promise {
                    result: Box::new(Type::Error),
                    error: Box::new(Type::Primitive(PrimitiveType::Never)),
                    effects: Vec::new(),
                },
            });
        }
        return;
    };
    let void = Type::Primitive(PrimitiveType::Void);
    let actual_result =
        destination.and_then(|destination| place_type(body, block, destination, errors));
    let expected_result = (*result != void).then_some(*result);
    if actual_result
        .as_ref()
        .zip(expected_result.as_ref())
        .is_some_and(|(actual, expected)| !types_compatible(actual, expected))
        || actual_result.is_some() != expected_result.is_some()
    {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: actual_result.unwrap_or(Type::Error),
            expected_type: expected_result.unwrap_or(void),
        });
    }
    let expected_error = (!effects.is_empty()).then(|| Type::ErrorUnion(effects.clone()));
    let actual_error =
        error_destination.and_then(|destination| place_type(body, block, destination, errors));
    if actual_error != expected_error || effects.is_empty() != error.is_none() {
        errors.push(MirValidationError::InvalidErrorEdge { block });
    }
}

fn valid_switch_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Primitive(
            PrimitiveType::Bool
                | PrimitiveType::I8
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
                | PrimitiveType::Char
        ) | Type::Nominal(_, _)
            | Type::Optional(_)
            | Type::Union(_)
            | Type::ErrorUnion(_)
    )
}

fn types_compatible(actual: &Type, expected: &Type) -> bool {
    if actual == expected {
        return true;
    }
    match (actual, expected) {
        (Type::String, Type::Str) | (Type::Str, Type::String) => true,
        (
            Type::Reference {
                mutable: actual_mutable,
                lifetime: actual_lifetime,
                referent: actual_referent,
            },
            Type::Reference {
                mutable: expected_mutable,
                lifetime: expected_lifetime,
                referent: expected_referent,
            },
        ) => {
            actual_mutable == expected_mutable
                && (actual_lifetime == expected_lifetime || actual_lifetime == "static")
                && types_compatible(actual_referent, expected_referent)
        }
        (Type::String, Type::Reference { referent, .. })
        | (Type::Reference { referent, .. }, Type::String) => {
            matches!(referent.as_ref(), Type::String | Type::Str)
        }
        (Type::Optional(actual), Type::Optional(expected))
        | (
            Type::Promise { result: actual, .. },
            Type::Promise {
                result: expected, ..
            },
        ) => types_compatible(actual, expected),
        (Type::Union(actual), Type::Union(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| types_compatible(actual, expected))
        }
        (actual, Type::Union(expected)) => expected
            .iter()
            .any(|expected| types_compatible(actual, expected)),
        (Type::Union(actual), expected) => actual
            .iter()
            .all(|actual| types_compatible(actual, expected)),
        _ => false,
    }
}

fn types_compatible_slice(actual: &[Type], expected: &[Type]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| types_compatible(actual, expected))
}

#[allow(clippy::too_many_lines)]
fn rvalue_type(
    body: &Body,
    block: u32,
    rvalue: &Rvalue,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Option<Type> {
    match rvalue {
        Rvalue::Use(operand) => operand_type(body, block, operand, initialized, errors),
        Rvalue::Unary {
            operand,
            operand_type: expected,
            result_type,
            ..
        } => {
            let actual = operand_type(body, block, operand, initialized, errors);
            if actual.as_ref() != Some(expected) {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    actual_type: actual.unwrap_or(Type::Error),
                    expected_type: expected.clone(),
                });
            }
            Some(result_type.clone())
        }
        Rvalue::CheckedBinary {
            left,
            right,
            operand_type: expected_operand,
            result_type,
            ..
        } => {
            let left = operand_type(body, block, left, initialized, errors);
            let right = operand_type(body, block, right, initialized, errors);
            if !left
                .as_ref()
                .is_some_and(|actual| types_compatible(actual, expected_operand))
                || !right
                    .as_ref()
                    .is_some_and(|actual| types_compatible(actual, expected_operand))
            {
                errors.push(MirValidationError::TypeMismatch {
                    block,
                    actual_type: Type::Tuple(vec![
                        left.unwrap_or(Type::Error),
                        right.unwrap_or(Type::Error),
                    ]),
                    expected_type: Type::Tuple(vec![
                        expected_operand.clone(),
                        expected_operand.clone(),
                    ]),
                });
            }
            Some(result_type.clone())
        }
        Rvalue::Aggregate {
            ty,
            fields,
            field_types,
            ..
        } => Some(aggregate_rvalue_type(
            body,
            block,
            ty,
            fields,
            field_types,
            initialized,
            errors,
        )),
        Rvalue::Closure {
            function,
            captures,
            body: closure_body,
            ..
        } => Some(closure_rvalue_type(
            body,
            block,
            function,
            captures,
            closure_body,
            initialized,
            errors,
        )),
        Rvalue::Template {
            parts,
            captures,
            ty,
            ..
        } => Some(template_rvalue_type(
            body,
            block,
            parts,
            captures,
            ty,
            initialized,
            errors,
        )),
        Rvalue::CheckedIndex { collection, index } => {
            checked_index_type(body, block, collection, index, initialized, errors)
        }
        Rvalue::Length(place) => {
            use_place(body, block, place, initialized, errors);
            Some(Type::Primitive(PrimitiveType::Usize))
        }
        Rvalue::VtableLookup { ty, object, .. }
        | Rvalue::WitnessLookup { ty, object, .. }
        | Rvalue::DirectMethod { ty, object, .. } => {
            use_place(body, block, object, initialized, errors);
            Some(ty.clone())
        }
        Rvalue::TypeTest { operand, .. } => {
            operand_type(body, block, operand, initialized, errors);
            Some(Type::Primitive(PrimitiveType::Bool))
        }
        Rvalue::RawOperation { operands, ty, .. } => {
            for operand in operands {
                operand_type(body, block, operand, initialized, errors);
            }
            Some(ty.clone())
        }
        Rvalue::Cast { operand, ty, .. } => {
            operand_type(body, block, operand, initialized, errors);
            Some(ty.clone())
        }
    }
}

fn template_rvalue_type(
    body: &Body,
    block: u32,
    parts: &[TemplatePart],
    captures: &[Operand],
    ty: &Type,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Type {
    let actual = captures
        .iter()
        .filter_map(|capture| operand_type(body, block, capture, initialized, errors))
        .collect::<Vec<_>>();
    let Type::Template(expected) = ty else {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: ty.clone(),
            expected_type: Type::Template(actual),
        });
        return ty.clone();
    };
    if actual != *expected {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: Type::Tuple(actual.clone()),
            expected_type: Type::Tuple(expected.clone()),
        });
    }
    for part in parts {
        let TemplatePart::Interpolation {
            capture,
            value_type,
        } = part
        else {
            continue;
        };
        let Some(capture_type) = actual.get(*capture as usize) else {
            errors.push(MirValidationError::InvalidTemplateCapture {
                block,
                capture: *capture,
            });
            continue;
        };
        let stored_value = match capture_type {
            Type::Reference { referent, .. } => referent.as_ref(),
            capture_type => capture_type,
        };
        if stored_value != value_type {
            errors.push(MirValidationError::TypeMismatch {
                block,
                actual_type: stored_value.clone(),
                expected_type: value_type.clone(),
            });
        }
    }
    ty.clone()
}

fn checked_index_type(
    body: &Body,
    block: u32,
    collection: &Place,
    index: &Operand,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Option<Type> {
    let collection = place_type(body, block, collection, errors)?;
    operand_type(body, block, index, initialized, errors);
    match collection {
        Type::Array(element, _) | Type::Slice(element) => Some(*element),
        _ => {
            errors.push(MirValidationError::InvalidProjection { block });
            Some(Type::Error)
        }
    }
}

fn aggregate_rvalue_type(
    body: &Body,
    block: u32,
    ty: &Type,
    fields: &[Operand],
    field_types: &[Type],
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Type {
    let actual = fields
        .iter()
        .filter_map(|field| operand_type(body, block, field, initialized, errors))
        .collect::<Vec<_>>();
    if !types_compatible_slice(&actual, field_types) {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: Type::Tuple(actual),
            expected_type: Type::Tuple(field_types.to_vec()),
        });
    }
    ty.clone()
}

fn closure_rvalue_type(
    body: &Body,
    block: u32,
    function: &FunctionType,
    captures: &[Operand],
    closure_body: &Body,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Type {
    let actual_captures = captures
        .iter()
        .filter_map(|capture| operand_type(body, block, capture, initialized, errors))
        .collect::<Vec<_>>();
    let arguments = closure_body
        .locals
        .iter()
        .filter(|local| local.argument)
        .map(|local| local.ty.clone())
        .collect::<Vec<_>>();
    let expected_capture_count = arguments.len().saturating_sub(function.parameters.len());
    if !types_compatible_slice(&actual_captures, &arguments[..expected_capture_count]) {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: Type::Tuple(actual_captures),
            expected_type: Type::Tuple(arguments[..expected_capture_count].to_vec()),
        });
    }
    if arguments[expected_capture_count..] != function.parameters
        || closure_body.return_type != *function.result
        || closure_body.effects != function.effects
    {
        errors.push(MirValidationError::TypeMismatch {
            block,
            actual_type: Type::Function(FunctionType {
                parameters: arguments[expected_capture_count..].to_vec(),
                result: Box::new(closure_body.return_type.clone()),
                effects: closure_body.effects.clone(),
                generics: Vec::new(),
                is_async: false,
                is_unsafe: false,
            }),
            expected_type: Type::Function(function.clone()),
        });
    }
    if let Err(nested) = validate(closure_body) {
        errors.extend(nested);
    }
    Type::Function(function.clone())
}

fn operand_type(
    body: &Body,
    block: u32,
    operand: &Operand,
    initialized: &mut BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) -> Option<Type> {
    match operand {
        Operand::Copy(place) => {
            use_place(body, block, place, initialized, errors);
            place_type(body, block, place, errors)
        }
        Operand::Move(place) => {
            use_place(body, block, place, initialized, errors);
            if place.projection.is_empty()
                && let Ok(local) = usize::try_from(place.local.0)
            {
                initialized.remove(&local);
            }
            place_type(body, block, place, errors)
        }
        Operand::Constant(constant) => Some(constant.ty()),
    }
}

fn use_place(
    body: &Body,
    block: u32,
    place: &Place,
    initialized: &BTreeSet<usize>,
    errors: &mut Vec<MirValidationError>,
) {
    if let Some(local) = check_local(body, block, place.local.0, errors)
        && !initialized.contains(&local)
    {
        errors.push(MirValidationError::UninitializedUse {
            block,
            local: place.local.0,
        });
    }
    for projection in &place.projection {
        if let Projection::Index(index) = projection {
            if let Some(index) = check_local(body, block, index.0, errors)
                && !initialized.contains(&index)
            {
                errors.push(MirValidationError::UninitializedUse {
                    block,
                    local: u32::try_from(index).unwrap_or(u32::MAX),
                });
            }
            if check_local(body, block, index.0, errors)
                .is_some_and(|index| body.locals[index].ty != Type::Primitive(PrimitiveType::Usize))
            {
                errors.push(MirValidationError::InvalidProjection { block });
            }
        }
    }
}

fn place_type(
    body: &Body,
    block: u32,
    place: &Place,
    errors: &mut Vec<MirValidationError>,
) -> Option<Type> {
    let local = check_local(body, block, place.local.0, errors)?;
    let mut ty = body.locals[local].ty.clone();
    for projection in &place.projection {
        ty = match (projection, ty) {
            (
                Projection::Dereference,
                Type::Reference { referent, .. }
                | Type::RawPointer {
                    pointee: referent, ..
                },
            ) => *referent,
            (Projection::Index(_), Type::Array(element, _) | Type::Slice(element)) => *element,
            (Projection::Downcast(_) | Projection::BaseClass(_), Type::Nominal(id, arguments)) => {
                Type::Nominal(id, arguments)
            }
            (Projection::Downcast(1), Type::Optional(inner)) => *inner,
            (Projection::Downcast(index), Type::Union(alternatives)) => alternatives
                .get(usize::try_from(*index).unwrap_or(usize::MAX))
                .cloned()
                .unwrap_or_else(|| {
                    errors.push(MirValidationError::InvalidProjection { block });
                    Type::Error
                }),
            (Projection::Field { ty, .. }, _) => ty.clone(),
            _ => {
                errors.push(MirValidationError::InvalidProjection { block });
                Type::Error
            }
        };
    }
    Some(ty)
}

fn check_local(
    body: &Body,
    block: u32,
    local: u32,
    errors: &mut Vec<MirValidationError>,
) -> Option<usize> {
    let local_index = usize::try_from(local).ok()?;
    if local_index >= body.locals.len() {
        errors.push(MirValidationError::InvalidLocal { block, local });
        None
    } else {
        Some(local_index)
    }
}

fn terminator_targets(terminator: &TerminatorKind) -> Vec<BasicBlockId> {
    match terminator {
        TerminatorKind::Goto(target) => vec![*target],
        TerminatorKind::Switch {
            targets, otherwise, ..
        } => targets
            .iter()
            .map(|(_, target)| *target)
            .chain([*otherwise])
            .collect(),
        TerminatorKind::Call { success, error, .. } => {
            [Some(*success), *error].into_iter().flatten().collect()
        }
        TerminatorKind::Suspend {
            resume,
            error,
            cancel,
            ..
        } => [Some(*resume), *error, Some(*cancel)]
            .into_iter()
            .flatten()
            .collect(),
        TerminatorKind::Drop { success, .. } => vec![*success],
        TerminatorKind::Return(_)
        | TerminatorKind::Throw(_)
        | TerminatorKind::TaggedReturn { .. }
        | TerminatorKind::Abort(_)
        | TerminatorKind::Unreachable => Vec::new(),
    }
}
