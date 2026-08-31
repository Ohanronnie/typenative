use crate::{
    Body, Constant, Operand, Place, Projection, Rvalue, StatementKind, TemplatePart, TerminatorKind,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tn_hir::{DeclarationId, FunctionType, MemberId, Type};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Callable {
    pub declaration: DeclarationId,
    pub member: Option<MemberId>,
}

impl Callable {
    pub const fn function(declaration: DeclarationId) -> Self {
        Self {
            declaration,
            member: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GenericBody {
    pub body: Body,
    pub type_parameters: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropImplementation {
    pub target: Type,
    pub callable: Callable,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Instance {
    pub callable: Callable,
    pub type_arguments: Vec<Type>,
    pub effects: Vec<DeclarationId>,
}

impl Instance {
    pub const fn concrete(callable: Callable) -> Self {
        Self {
            callable,
            type_arguments: Vec::new(),
            effects: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonomorphizedBody {
    pub instance: Instance,
    pub body: Body,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MonomorphizationError {
    #[error("reachable callable {callable:?} has no MIR body")]
    MissingBody { callable: Callable },
    #[error("callable {callable:?} expects {expected} type arguments but received {actual}")]
    TypeArgumentArity {
        callable: Callable,
        expected: usize,
        actual: usize,
    },
    #[error("could not infer concrete type parameter `{parameter}` for {callable:?}")]
    UnresolvedTypeParameter {
        callable: Callable,
        parameter: String,
    },
}

/// Discovers and specializes all MIR instances reachable from the supplied roots.
///
/// A sorted work queue and completed-instance set make output deterministic and allow direct or
/// mutually recursive functions without recursive compiler calls.
///
/// # Errors
///
/// Returns an error when a reachable body is missing, a root has the wrong generic arity, or a
/// concrete call signature does not determine every required type parameter.
pub fn monomorphize(
    bodies: &[GenericBody],
    roots: impl IntoIterator<Item = Instance>,
) -> Result<Vec<MonomorphizedBody>, MonomorphizationError> {
    monomorphize_with_drops(bodies, roots, &[])
}

/// Discovers and specializes MIR instances, including explicit `Drop` implementations selected
/// by drop terminators.
///
/// # Errors
///
/// Returns an error when a reachable body is missing, a root has the wrong generic arity, or a
/// concrete call signature does not determine every required type parameter.
pub fn monomorphize_with_drops(
    bodies: &[GenericBody],
    roots: impl IntoIterator<Item = Instance>,
    drop_implementations: &[DropImplementation],
) -> Result<Vec<MonomorphizedBody>, MonomorphizationError> {
    let registry = bodies
        .iter()
        .map(|body| (callable_of(&body.body), body))
        .collect::<BTreeMap<_, _>>();
    let mut pending = roots.into_iter().collect::<BTreeSet<_>>();
    let mut queue = pending.iter().cloned().collect::<VecDeque<_>>();
    let mut output = Vec::new();
    while let Some(instance) = queue.pop_front() {
        let generic =
            registry
                .get(&instance.callable)
                .ok_or(MonomorphizationError::MissingBody {
                    callable: instance.callable,
                })?;
        if generic.type_parameters.len() != instance.type_arguments.len() {
            return Err(MonomorphizationError::TypeArgumentArity {
                callable: instance.callable,
                expected: generic.type_parameters.len(),
                actual: instance.type_arguments.len(),
            });
        }
        let substitutions = generic
            .type_parameters
            .iter()
            .cloned()
            .zip(instance.type_arguments.iter().cloned())
            .collect::<BTreeMap<_, _>>();
        let mut body = generic.body.clone();
        substitute_body(&mut body, &substitutions);
        for discovered in discover_instances(&body, &registry, drop_implementations)? {
            if pending.insert(discovered.clone()) {
                queue.push_back(discovered);
            }
        }
        output.push(MonomorphizedBody { instance, body });
    }
    output.sort_by(|left, right| left.instance.cmp(&right.instance));
    Ok(output)
}

fn callable_of(body: &Body) -> Callable {
    Callable {
        declaration: body.declaration,
        member: body.member,
    }
}

fn discover_instances(
    body: &Body,
    registry: &BTreeMap<Callable, &GenericBody>,
    drop_implementations: &[DropImplementation],
) -> Result<BTreeSet<Instance>, MonomorphizationError> {
    let mut discovered = BTreeSet::new();
    for local in &body.locals {
        discover_drop_instance(&local.ty, registry, drop_implementations, &mut discovered)?;
    }
    for block in &body.blocks {
        for statement in &block.statements {
            if let StatementKind::Assign(_, value) = &statement.kind {
                visit_rvalue(body, value, registry, drop_implementations, &mut discovered)?;
            }
        }
        visit_terminator(
            body,
            &block.terminator.kind,
            registry,
            drop_implementations,
            &mut discovered,
        )?;
    }
    Ok(discovered)
}

#[allow(clippy::match_same_arms)]
fn visit_rvalue(
    body: &Body,
    value: &Rvalue,
    registry: &BTreeMap<Callable, &GenericBody>,
    drop_implementations: &[DropImplementation],
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    match value {
        Rvalue::Use(operand) | Rvalue::Unary { operand, .. } | Rvalue::TypeTest { operand, .. } => {
            visit_operand(operand, registry, drop_implementations, discovered)
        }
        Rvalue::Cast {
            operand,
            ty,
            kind: crate::CastKind::InterfaceCoercion,
        } => {
            visit_operand(operand, registry, drop_implementations, discovered)?;
            let source = match operand {
                Operand::Copy(place) | Operand::Move(place) => place_type(body, place),
                Operand::Constant(constant) => Some(constant.ty()),
            };
            if let Some(source) = source {
                discover_interface_target_methods(&source, ty, registry, discovered)?;
            }
            Ok(())
        }
        Rvalue::Cast { operand, .. } => {
            visit_operand(operand, registry, drop_implementations, discovered)
        }
        Rvalue::CheckedBinary { left, right, .. } => {
            visit_operand(left, registry, drop_implementations, discovered)?;
            visit_operand(right, registry, drop_implementations, discovered)
        }
        Rvalue::CheckedIndex { index, .. } => {
            visit_operand(index, registry, drop_implementations, discovered)
        }
        Rvalue::Closure {
            captures: fields, ..
        }
        | Rvalue::Template {
            captures: fields, ..
        }
        | Rvalue::RawOperation {
            operands: fields, ..
        } => {
            for operand in fields {
                visit_operand(operand, registry, drop_implementations, discovered)?;
            }
            if let Rvalue::Closure { body, .. } = value {
                discovered.extend(discover_instances(body, registry, drop_implementations)?);
            }
            Ok(())
        }
        Rvalue::Aggregate {
            ty,
            fields,
            field_types,
            ..
        } => {
            for operand in fields {
                visit_operand(operand, registry, drop_implementations, discovered)?;
            }
            discover_drop_instance(ty, registry, drop_implementations, discovered)?;
            for field_type in field_types {
                discover_drop_instance(field_type, registry, drop_implementations, discovered)?;
            }
            Ok(())
        }
        Rvalue::DirectMethod {
            implementation,
            member,
            ty,
            object,
            ..
        }
        | Rvalue::VtableLookup {
            implementation,
            member,
            ty,
            object,
            ..
        } => {
            let receiver_type = place_type(body, object).unwrap_or(Type::Error);
            discover_callable(
                Callable {
                    declaration: *implementation,
                    member: Some(*member),
                },
                ty,
                Some(&receiver_type),
                registry,
                discovered,
            )
        }
        Rvalue::Length(_) | Rvalue::WitnessLookup { .. } => Ok(()),
    }
}

fn discover_interface_target_methods(
    source: &Type,
    _interface: &Type,
    registry: &BTreeMap<Callable, &GenericBody>,
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    let Type::Nominal(declaration, _) = source else {
        return Ok(());
    };
    if contains_generic(source) {
        return Ok(());
    }
    for (callable, generic) in registry {
        if callable.declaration != *declaration || callable.member.is_none() {
            continue;
        }
        let parameters = generic
            .body
            .locals
            .iter()
            .filter(|local| {
                local.argument && !matches!(local.name.as_deref(), Some("self" | "this"))
            })
            .map(|local| local.ty.clone())
            .collect();
        let signature = Type::Function(FunctionType {
            parameters,
            result: Box::new(generic.body.return_type.clone()),
            effects: generic.body.effects.clone(),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        });
        discover_callable(*callable, &signature, Some(source), registry, discovered)?;
    }
    Ok(())
}

fn contains_generic(ty: &Type) -> bool {
    match ty {
        Type::Generic(_) => true,
        Type::Nominal(_, arguments) | Type::DynamicInterface(_, arguments) => {
            arguments.iter().any(contains_generic)
        }
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            contains_generic(inner)
        }
        Type::Union(alternatives) => alternatives.iter().any(contains_generic),
        Type::Promise { result, error, .. } => contains_generic(result) || contains_generic(error),
        Type::Reference {
            referent: result, ..
        } => contains_generic(result),
        Type::RawPointer { pointee, .. } => contains_generic(pointee),
        Type::Tuple(elements) | Type::Template(elements) => elements.iter().any(contains_generic),
        Type::Function(function) => {
            function.parameters.iter().any(contains_generic) || contains_generic(&function.result)
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => false,
    }
}

fn visit_terminator(
    body: &Body,
    terminator: &TerminatorKind,
    registry: &BTreeMap<Callable, &GenericBody>,
    drop_implementations: &[DropImplementation],
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    match terminator {
        TerminatorKind::Switch { value, .. }
        | TerminatorKind::Throw(value)
        | TerminatorKind::Suspend { value, .. }
        | TerminatorKind::Return(Some(value))
        | TerminatorKind::TaggedReturn {
            payload: Some(value),
            ..
        } => visit_operand(value, registry, drop_implementations, discovered),
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            ..
        } => {
            if let Operand::Constant(Constant::Method { owner, member, ty }) = function {
                let receiver_type = receiver.as_ref().and_then(|operand| match operand {
                    Operand::Copy(place) | Operand::Move(place) => place_type(body, place),
                    Operand::Constant(constant) => Some(constant.ty()),
                });
                discover_callable(
                    Callable {
                        declaration: *owner,
                        member: Some(*member),
                    },
                    ty,
                    receiver_type.as_ref(),
                    registry,
                    discovered,
                )?;
            } else {
                visit_operand(function, registry, drop_implementations, discovered)?;
            }
            if let Some(receiver) = receiver {
                visit_operand(receiver, registry, drop_implementations, discovered)?;
            }
            for argument in arguments {
                visit_operand(argument, registry, drop_implementations, discovered)?;
            }
            Ok(())
        }
        TerminatorKind::Return(None)
        | TerminatorKind::TaggedReturn { payload: None, .. }
        | TerminatorKind::Goto(_)
        | TerminatorKind::Abort(_)
        | TerminatorKind::Unreachable => Ok(()),
        TerminatorKind::Drop { place, .. } => {
            let Some(ty) = place_type(body, place) else {
                return Ok(());
            };
            discover_drop_instance(&ty, registry, drop_implementations, discovered)
        }
    }
}

fn discover_drop_instance(
    ty: &Type,
    registry: &BTreeMap<Callable, &GenericBody>,
    drop_implementations: &[DropImplementation],
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    for implementation in drop_implementations {
        let mut inferred = BTreeMap::new();
        if !matches_target(&implementation.target, ty, &mut inferred) {
            continue;
        }
        let Some(generic) = registry.get(&implementation.callable) else {
            continue;
        };
        let type_arguments = generic
            .type_parameters
            .iter()
            .map(|parameter| {
                inferred.get(parameter).cloned().ok_or_else(|| {
                    MonomorphizationError::UnresolvedTypeParameter {
                        callable: implementation.callable,
                        parameter: parameter.clone(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if type_arguments.iter().any(contains_generic) {
            continue;
        }
        discovered.insert(Instance {
            callable: implementation.callable,
            type_arguments,
            effects: Vec::new(),
        });
    }
    Ok(())
}

fn visit_operand(
    operand: &Operand,
    registry: &BTreeMap<Callable, &GenericBody>,
    _drop_implementations: &[DropImplementation],
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    let Operand::Constant(constant) = operand else {
        return Ok(());
    };
    match constant {
        Constant::Function(declaration, ty) => discover_callable(
            Callable::function(*declaration),
            ty,
            None,
            registry,
            discovered,
        ),
        Constant::Method {
            owner, member, ty, ..
        }
        | Constant::Constructor {
            owner,
            member: Some(member),
            ty,
        } => discover_callable(
            Callable {
                declaration: *owner,
                member: Some(*member),
            },
            ty,
            None,
            registry,
            discovered,
        ),
        Constant::Constructor {
            owner,
            member: None,
            ty,
        } => discover_callable(Callable::function(*owner), ty, None, registry, discovered),
        Constant::Bool(_)
        | Constant::Integer { .. }
        | Constant::Float { .. }
        | Constant::Character(_)
        | Constant::String(_)
        | Constant::Undefined(_)
        | Constant::ExternalFunction { .. } => Ok(()),
    }
}

fn place_type(body: &Body, place: &Place) -> Option<Type> {
    let mut ty = body.locals.get(place.local.0 as usize)?.ty.clone();
    for projection in &place.projection {
        ty = match projection {
            Projection::Field { ty: field, .. } => field.clone(),
            Projection::Dereference => match ty {
                Type::Reference { referent, .. }
                | Type::RawPointer {
                    pointee: referent, ..
                } => *referent,
                _ => return None,
            },
            Projection::Index(_) => match ty {
                Type::Array(element, _) | Type::Slice(element) => *element,
                _ => return None,
            },
            Projection::Downcast(_) | Projection::BaseClass(_) => ty,
        };
    }
    Some(ty)
}

fn matches_target(template: &Type, concrete: &Type, inferred: &mut BTreeMap<String, Type>) -> bool {
    match (template, concrete) {
        (Type::Generic(name), concrete) => {
            if let Some(previous) = inferred.get(name) {
                previous == concrete
            } else {
                inferred.insert(name.clone(), concrete.clone());
                true
            }
        }
        (
            Type::Nominal(template_decl, template_args),
            Type::Nominal(concrete_decl, concrete_args),
        ) => {
            template_decl == concrete_decl
                && template_args.len() == concrete_args.len()
                && template_args
                    .iter()
                    .zip(concrete_args)
                    .all(|(template, concrete)| matches_target(template, concrete, inferred))
        }
        (Type::Optional(template), Type::Optional(concrete))
        | (Type::Array(template, _), Type::Array(concrete, _))
        | (Type::Slice(template), Type::Slice(concrete))
        | (
            Type::Reference {
                referent: template, ..
            },
            Type::Reference {
                referent: concrete, ..
            },
        )
        | (
            Type::RawPointer {
                pointee: template, ..
            },
            Type::RawPointer {
                pointee: concrete, ..
            },
        ) => matches_target(template, concrete, inferred),
        (
            Type::Promise {
                result: template,
                error: template_error,
                ..
            },
            Type::Promise {
                result: concrete,
                error: concrete_error,
                ..
            },
        ) => {
            matches_target(template, concrete, inferred)
                && matches_target(template_error, concrete_error, inferred)
        }
        (Type::Tuple(template), Type::Tuple(concrete))
        | (Type::Template(template), Type::Template(concrete)) => {
            template.len() == concrete.len()
                && template
                    .iter()
                    .zip(concrete)
                    .all(|(template, concrete)| matches_target(template, concrete, inferred))
        }
        (Type::Union(template), Type::Union(concrete)) => {
            template.len() == concrete.len()
                && template
                    .iter()
                    .zip(concrete)
                    .all(|(template, concrete)| matches_target(template, concrete, inferred))
        }
        (template, concrete) => template == concrete,
    }
}

fn discover_callable(
    callable: Callable,
    concrete_type: &Type,
    receiver_type: Option<&Type>,
    registry: &BTreeMap<Callable, &GenericBody>,
    discovered: &mut BTreeSet<Instance>,
) -> Result<(), MonomorphizationError> {
    let Some(generic) = registry.get(&callable) else {
        return Ok(());
    };
    let mut inferred = BTreeMap::new();
    if let Type::Function(concrete) = concrete_type {
        let arguments = generic
            .body
            .locals
            .iter()
            .filter(|local| local.argument)
            .collect::<Vec<_>>();
        let offset = arguments.len().saturating_sub(concrete.parameters.len());
        for (template, concrete) in arguments[offset..].iter().zip(&concrete.parameters) {
            infer_type(&template.ty, concrete, &mut inferred);
        }
        if let Some(receiver) = arguments
            .iter()
            .find(|local| matches!(local.name.as_deref(), Some("this" | "self")))
        {
            if let Some(receiver_type) = receiver_type {
                infer_type(&receiver.ty, receiver_type, &mut inferred);
            } else if let Type::Nominal(_, _) = concrete.result.as_ref() {
                infer_type(&receiver.ty, &concrete.result, &mut inferred);
            }
        }
        infer_type(&generic.body.return_type, &concrete.result, &mut inferred);
        let mut type_arguments = Vec::new();
        for parameter in &generic.type_parameters {
            type_arguments.push(inferred.get(parameter).cloned().ok_or_else(|| {
                MonomorphizationError::UnresolvedTypeParameter {
                    callable,
                    parameter: parameter.clone(),
                }
            })?);
        }
        if type_arguments.iter().any(contains_generic) {
            return Ok(());
        }
        discovered.insert(Instance {
            callable,
            type_arguments,
            effects: concrete.effects.clone(),
        });
    }
    Ok(())
}

fn infer_type(template: &Type, concrete: &Type, inferred: &mut BTreeMap<String, Type>) {
    match (template, concrete) {
        (Type::Generic(name), concrete) => {
            inferred
                .entry(name.clone())
                .or_insert_with(|| concrete.clone());
        }
        (Type::Optional(left), Type::Optional(right))
        | (Type::Slice(left), Type::Slice(right))
        | (Type::Array(left, _), Type::Array(right, _)) => infer_type(left, right, inferred),
        (Type::Union(left), Type::Union(right))
        | (Type::Tuple(left), Type::Tuple(right))
        | (Type::Template(left), Type::Template(right))
        | (Type::Nominal(_, left), Type::Nominal(_, right))
        | (Type::DynamicInterface(_, left), Type::DynamicInterface(_, right)) => {
            for (left, right) in left.iter().zip(right) {
                infer_type(left, right, inferred);
            }
        }
        (
            Type::Reference { referent: left, .. },
            Type::Reference {
                referent: right, ..
            },
        )
        | (Type::RawPointer { pointee: left, .. }, Type::RawPointer { pointee: right, .. }) => {
            infer_type(left, right, inferred);
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
            infer_type(left, right, inferred);
            infer_type(left_error, right_error, inferred);
        }
        (Type::Function(left), Type::Function(right)) => {
            for (left, right) in left.parameters.iter().zip(&right.parameters) {
                infer_type(left, right, inferred);
            }
            infer_type(&left.result, &right.result, inferred);
        }
        _ => {}
    }
}

fn substitute_body(body: &mut Body, substitutions: &BTreeMap<String, Type>) {
    for local in &mut body.locals {
        substitute_type(&mut local.ty, substitutions);
    }
    substitute_type(&mut body.return_type, substitutions);
    for block in &mut body.blocks {
        for statement in &mut block.statements {
            match &mut statement.kind {
                StatementKind::Assign(place, value) => {
                    substitute_place(place, substitutions);
                    substitute_rvalue(value, substitutions);
                }
                StatementKind::SetDiscriminant(place, _)
                | StatementKind::Retag(place)
                | StatementKind::SetDropFlag(place, _)
                | StatementKind::Borrow { place, .. } => {
                    substitute_place(place, substitutions);
                }
                StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => {}
            }
        }
        substitute_terminator(&mut block.terminator.kind, substitutions);
    }
}

fn substitute_place(place: &mut Place, substitutions: &BTreeMap<String, Type>) {
    for projection in &mut place.projection {
        if let Projection::Field { ty, .. } = projection {
            substitute_type(ty, substitutions);
        }
    }
}

fn substitute_operand(operand: &mut Operand, substitutions: &BTreeMap<String, Type>) {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => substitute_place(place, substitutions),
        Operand::Constant(constant) => match constant {
            Constant::Integer { ty, .. }
            | Constant::Float { ty, .. }
            | Constant::Undefined(ty)
            | Constant::Function(_, ty)
            | Constant::ExternalFunction { ty, .. }
            | Constant::Method { ty, .. }
            | Constant::Constructor { ty, .. } => substitute_type(ty, substitutions),
            Constant::Bool(_) | Constant::Character(_) | Constant::String(_) => {}
        },
    }
}

fn substitute_rvalue(value: &mut Rvalue, substitutions: &BTreeMap<String, Type>) {
    match value {
        Rvalue::Use(operand) => substitute_operand(operand, substitutions),
        Rvalue::Unary {
            operand,
            operand_type,
            result_type,
            ..
        }
        | Rvalue::CheckedBinary {
            left: operand,
            operand_type,
            result_type,
            ..
        } => {
            substitute_operand(operand, substitutions);
            substitute_type(operand_type, substitutions);
            substitute_type(result_type, substitutions);
            if let Rvalue::CheckedBinary { right, .. } = value {
                substitute_operand(right, substitutions);
            }
        }
        Rvalue::CheckedIndex { collection, index } => {
            substitute_place(collection, substitutions);
            substitute_operand(index, substitutions);
        }
        Rvalue::Aggregate {
            ty,
            fields,
            field_types,
            ..
        } => {
            substitute_type(ty, substitutions);
            for field in fields {
                substitute_operand(field, substitutions);
            }
            for field in field_types {
                substitute_type(field, substitutions);
            }
        }
        Rvalue::Closure {
            function,
            captures,
            body,
            ..
        } => {
            substitute_function(function, substitutions);
            for capture in captures {
                substitute_operand(capture, substitutions);
            }
            substitute_body(body, substitutions);
        }
        Rvalue::Template {
            parts,
            captures,
            ty,
            ..
        } => {
            for part in parts {
                if let TemplatePart::Interpolation { value_type, .. } = part {
                    substitute_type(value_type, substitutions);
                }
            }
            for capture in captures {
                substitute_operand(capture, substitutions);
            }
            substitute_type(ty, substitutions);
        }
        Rvalue::Length(place) => substitute_place(place, substitutions),
        Rvalue::VtableLookup { object, ty, .. }
        | Rvalue::WitnessLookup { object, ty, .. }
        | Rvalue::DirectMethod { object, ty, .. } => {
            substitute_place(object, substitutions);
            substitute_type(ty, substitutions);
        }
        Rvalue::TypeTest { operand, target } => {
            substitute_operand(operand, substitutions);
            substitute_type(target, substitutions);
        }
        Rvalue::RawOperation { operands, ty, .. } => {
            for operand in operands {
                substitute_operand(operand, substitutions);
            }
            substitute_type(ty, substitutions);
        }
        Rvalue::Cast { operand, ty, .. } => {
            substitute_operand(operand, substitutions);
            substitute_type(ty, substitutions);
        }
    }
}

fn substitute_terminator(terminator: &mut TerminatorKind, substitutions: &BTreeMap<String, Type>) {
    match terminator {
        TerminatorKind::Switch { value, .. }
        | TerminatorKind::Throw(value)
        | TerminatorKind::Suspend { value, .. } => substitute_operand(value, substitutions),
        TerminatorKind::Call {
            function,
            receiver,
            arguments,
            destination,
            error_destination,
            ..
        } => {
            substitute_operand(function, substitutions);
            if let Some(receiver) = receiver {
                substitute_operand(receiver, substitutions);
            }
            for argument in arguments {
                substitute_operand(argument, substitutions);
            }
            if let Some(destination) = destination {
                substitute_place(destination, substitutions);
            }
            if let Some(destination) = error_destination {
                substitute_place(destination, substitutions);
            }
        }
        TerminatorKind::Return(payload) | TerminatorKind::TaggedReturn { payload, .. } => {
            if let Some(payload) = payload {
                substitute_operand(payload, substitutions);
            }
        }
        TerminatorKind::Drop { place, .. } => substitute_place(place, substitutions),
        TerminatorKind::Goto(_) | TerminatorKind::Abort(_) | TerminatorKind::Unreachable => {}
    }
}

fn substitute_function(function: &mut FunctionType, substitutions: &BTreeMap<String, Type>) {
    for parameter in &mut function.parameters {
        substitute_type(parameter, substitutions);
    }
    substitute_type(&mut function.result, substitutions);
    function.generics.clear();
}

fn substitute_type(ty: &mut Type, substitutions: &BTreeMap<String, Type>) {
    if let Type::Generic(name) = ty
        && let Some(replacement) = substitutions.get(name)
    {
        *ty = replacement.clone();
        return;
    }
    match ty {
        Type::Promise { result, error, .. } => {
            substitute_type(result, substitutions);
            substitute_type(error, substitutions);
        }
        Type::Nominal(_, arguments) | Type::DynamicInterface(_, arguments) => {
            for argument in arguments {
                substitute_type(argument, substitutions);
            }
        }
        Type::Optional(inner) | Type::Array(inner, _) | Type::Slice(inner) => {
            substitute_type(inner, substitutions);
        }
        Type::Union(alternatives) => {
            for alternative in alternatives {
                substitute_type(alternative, substitutions);
            }
        }
        Type::Tuple(elements) | Type::Template(elements) => {
            for element in elements {
                substitute_type(element, substitutions);
            }
        }
        Type::Reference { referent, .. } => substitute_type(referent, substitutions),
        Type::RawPointer { pointee, .. } => substitute_type(pointee, substitutions),
        Type::Function(function) => substitute_function(function, substitutions),
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => {}
    }
}
