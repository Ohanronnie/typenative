use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, FunctionType, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, BasicBlockId, Body, Callable, Constant, GenericBody, Instance, Local, LocalId,
    MonomorphizationError, Operand, Place, Terminator, TerminatorKind, monomorphize, validate,
};

fn span() -> SourceSpan {
    SourceSpan::new("instances.tn", 0..0, "")
}

fn local(name: &str, ty: Type, argument: bool) -> Local {
    Local {
        name: Some(name.into()),
        ty,
        mutable: false,
        argument,
        span: span(),
    }
}

fn function_type(parameter: Type, result: Type) -> Type {
    Type::Function(FunctionType {
        parameters: vec![parameter],
        result: Box::new(result),
        effects: Vec::new(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    })
}

fn recursive_identity() -> GenericBody {
    let generic = Type::Generic("T".into());
    GenericBody {
        body: Body {
            declaration: DeclarationId(2),
            member: None,
            locals: vec![
                local("value", generic.clone(), true),
                local("recursive", generic.clone(), false),
            ],
            blocks: vec![
                BasicBlock {
                    statements: Vec::new(),
                    terminator: Terminator {
                        kind: TerminatorKind::Call {
                            function: Operand::Constant(Constant::Function(
                                DeclarationId(2),
                                function_type(generic.clone(), generic.clone()),
                            )),
                            receiver: None,
                            arguments: vec![Operand::Move(Place::local(LocalId(0)))],
                            destination: Some(Place::local(LocalId(1))),
                            error_destination: None,
                            success: BasicBlockId(1),
                            error: None,
                        },
                        span: span(),
                    },
                },
                BasicBlock {
                    statements: Vec::new(),
                    terminator: Terminator {
                        kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(1))))),
                        span: span(),
                    },
                },
            ],
            return_type: generic,
            effects: Vec::new(),
        },
        type_parameters: vec!["T".into()],
    }
}

fn entry_body() -> GenericBody {
    let integer = Type::Primitive(PrimitiveType::I32);
    GenericBody {
        body: Body {
            declaration: DeclarationId(1),
            member: None,
            locals: vec![local("result", integer.clone(), false)],
            blocks: vec![
                BasicBlock {
                    statements: Vec::new(),
                    terminator: Terminator {
                        kind: TerminatorKind::Call {
                            function: Operand::Constant(Constant::Function(
                                DeclarationId(2),
                                function_type(integer.clone(), integer.clone()),
                            )),
                            receiver: None,
                            arguments: vec![Operand::Constant(Constant::Integer {
                                value: 7,
                                ty: integer.clone(),
                            })],
                            destination: Some(Place::local(LocalId(0))),
                            error_destination: None,
                            success: BasicBlockId(1),
                            error: None,
                        },
                        span: span(),
                    },
                },
                BasicBlock {
                    statements: Vec::new(),
                    terminator: Terminator {
                        kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(0))))),
                        span: span(),
                    },
                },
            ],
            return_type: integer,
            effects: Vec::new(),
        },
        type_parameters: Vec::new(),
    }
}

#[test]
fn discovers_specializes_and_terminates_recursive_instances_deterministically() {
    let bodies = vec![recursive_identity(), entry_body()];
    let root = Instance::concrete(Callable::function(DeclarationId(1)));
    let first = monomorphize(&bodies, [root.clone()]).expect("reachable instances");
    let second = monomorphize(&bodies, [root]).expect("deterministic instances");
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    let identity = first
        .iter()
        .find(|instance| instance.instance.callable.declaration == DeclarationId(2))
        .expect("generic identity instance");
    assert_eq!(
        identity.instance.type_arguments,
        vec![Type::Primitive(PrimitiveType::I32)]
    );
    assert!(
        identity
            .body
            .locals
            .iter()
            .all(|local| local.ty == Type::Primitive(PrimitiveType::I32))
    );
    validate(&identity.body).expect("specialized MIR remains valid");
}

#[test]
fn rejects_missing_roots_and_wrong_type_argument_arity() {
    let missing = monomorphize(
        &[entry_body()],
        [Instance::concrete(Callable::function(DeclarationId(40)))],
    )
    .expect_err("missing root");
    assert!(matches!(missing, MonomorphizationError::MissingBody { .. }));

    let arity = monomorphize(
        &[recursive_identity()],
        [Instance::concrete(Callable::function(DeclarationId(2)))],
    )
    .expect_err("missing type argument");
    assert!(matches!(
        arity,
        MonomorphizationError::TypeArgumentArity {
            expected: 1,
            actual: 0,
            ..
        }
    ));
}
