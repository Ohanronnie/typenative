use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, FunctionType, HirClosureId, HirTemplateId, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, BasicBlockId, Body, BorrowKind, Completion, Constant, DropSemantics, Local,
    LocalId, MirValidationError, Operand, Place, RegionId, Rvalue, Statement, StatementKind,
    TemplatePart, Terminator, TerminatorKind, elaborate_drops, lower_typed_errors, validate,
};

fn span() -> SourceSpan {
    SourceSpan::new("mir.tn", 0..0, "")
}

fn valid_body() -> Body {
    let integer = Type::Primitive(PrimitiveType::I32);
    Body {
        declaration: DeclarationId(1),
        member: None,
        locals: vec![Local {
            name: Some("result".into()),
            ty: integer.clone(),
            mutable: false,
            argument: false,
            span: span(),
        }],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(0)),
                    Box::new(Rvalue::Use(Operand::Constant(Constant::Integer {
                        value: 42,
                        ty: integer.clone(),
                    }))),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(0))))),
                span: span(),
            },
        }],
        return_type: integer,
        effects: Vec::new(),
    }
}

#[test]
fn accepts_well_typed_initialized_control_flow_and_renders_deterministically() {
    let body = valid_body();
    validate(&body).expect("valid MIR");
    assert_eq!(body.to_string(), body.to_string());
    assert!(body.to_string().contains("bb0"));
}

#[test]
fn validates_closure_capture_signature_and_nested_body_invariants() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let capture = Type::Reference {
        mutable: false,
        lifetime: "scope".into(),
        referent: Box::new(integer.clone()),
    };
    let function = FunctionType {
        parameters: vec![integer.clone()],
        result: Box::new(integer.clone()),
        effects: Vec::new(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    };
    let closure_body = Body {
        declaration: DeclarationId(5),
        member: None,
        locals: vec![
            Local {
                name: Some("capture".into()),
                ty: capture.clone(),
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("value".into()),
                ty: integer.clone(),
                mutable: false,
                argument: true,
                span: span(),
            },
        ],
        blocks: vec![BasicBlock {
            statements: Vec::new(),
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Copy(Place::local(LocalId(1))))),
                span: span(),
            },
        }],
        return_type: integer.clone(),
        effects: Vec::new(),
    };
    let mut body = Body {
        declaration: DeclarationId(5),
        member: None,
        locals: vec![
            Local {
                name: Some("capture".into()),
                ty: capture,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("closure".into()),
                ty: Type::Function(function.clone()),
                mutable: false,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::Closure {
                        id: HirClosureId(0),
                        function,
                        captures: vec![Operand::Copy(Place::local(LocalId(0)))],
                        body: Box::new(closure_body),
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    validate(&body).expect("valid closure MIR");
    let StatementKind::Assign(_, closure) = &mut body.blocks[0].statements[0].kind else {
        panic!("closure assignment");
    };
    let Rvalue::Closure {
        body: nested_body, ..
    } = closure.as_mut()
    else {
        panic!("closure rvalue");
    };
    nested_body.locals[1].ty = Type::Primitive(PrimitiveType::Bool);
    assert!(
        validate(&body)
            .expect_err("closure signature mismatch")
            .iter()
            .any(|error| matches!(error, MirValidationError::TypeMismatch { .. }))
    );
}

#[test]
fn validates_template_capture_types_and_part_indices() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let template_type = Type::Template(vec![integer.clone()]);
    let mut body = Body {
        declaration: DeclarationId(6),
        member: None,
        locals: vec![
            Local {
                name: Some("value".into()),
                ty: integer.clone(),
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("template".into()),
                ty: template_type.clone(),
                mutable: false,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::Template {
                        id: HirTemplateId(0),
                        parts: vec![
                            TemplatePart::Literal("value=".into()),
                            TemplatePart::Interpolation {
                                capture: 0,
                                value_type: integer,
                            },
                        ],
                        captures: vec![Operand::Copy(Place::local(LocalId(0)))],
                        ty: template_type,
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    validate(&body).expect("valid template MIR");
    let StatementKind::Assign(_, rvalue) = &mut body.blocks[0].statements[0].kind else {
        panic!("template assignment");
    };
    let Rvalue::Template { parts, .. } = rvalue.as_mut() else {
        panic!("template rvalue");
    };
    let TemplatePart::Interpolation { capture, .. } = &mut parts[1] else {
        panic!("interpolation part");
    };
    *capture = 4;
    assert!(
        validate(&body)
            .expect_err("missing template capture")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidTemplateCapture { .. }))
    );
}

#[test]
fn drop_elaboration_tracks_moves_and_cleans_function_exits_in_reverse_order() {
    let mut body = Body {
        declaration: DeclarationId(7),
        member: None,
        locals: vec![
            Local {
                name: Some("source".into()),
                ty: Type::String,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("result".into()),
                ty: Type::String,
                mutable: false,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![BasicBlock {
            statements: vec![
                Statement {
                    kind: StatementKind::StorageLive(LocalId(1)),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(1)),
                        Box::new(Rvalue::Use(Operand::Move(Place::local(LocalId(0))))),
                    ),
                    span: span(),
                },
            ],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(1))))),
                span: span(),
            },
        }],
        return_type: Type::String,
        effects: Vec::new(),
    };
    body = elaborate_drops(&body, &DropSemantics::default());
    validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert_eq!(body.blocks.len(), 4);
    assert!(matches!(
        body.blocks[0].terminator.kind,
        TerminatorKind::Goto(BasicBlockId(3))
    ));
    assert!(matches!(
        body.blocks[3].terminator.kind,
        TerminatorKind::Drop {
            place: Place {
                local: LocalId(1),
                ..
            },
            success: BasicBlockId(2)
        }
    ));
    assert!(matches!(
        body.blocks[2].terminator.kind,
        TerminatorKind::Drop {
            place: Place {
                local: LocalId(0),
                ..
            },
            success: BasicBlockId(1)
        }
    ));
    let flags = body.blocks[0]
        .statements
        .iter()
        .filter(|statement| matches!(statement.kind, StatementKind::SetDropFlag(_, _)))
        .count();
    assert_eq!(flags, 5);
}

#[test]
fn drop_elaboration_cleans_locals_on_branch_scope_exit() {
    let boolean = Type::Primitive(PrimitiveType::Bool);
    let body = Body {
        declaration: DeclarationId(8),
        member: None,
        locals: vec![
            Local {
                name: Some("condition".into()),
                ty: boolean,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("source".into()),
                ty: Type::String,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("inner".into()),
                ty: Type::String,
                mutable: false,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Switch {
                        value: Operand::Copy(Place::local(LocalId(0))),
                        targets: vec![(1, BasicBlockId(1))],
                        otherwise: BasicBlockId(2),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: vec![
                    Statement {
                        kind: StatementKind::StorageLive(LocalId(2)),
                        span: span(),
                    },
                    Statement {
                        kind: StatementKind::Assign(
                            Place::local(LocalId(2)),
                            Box::new(Rvalue::Use(Operand::Move(Place::local(LocalId(1))))),
                        ),
                        span: span(),
                    },
                ],
                terminator: Terminator {
                    kind: TerminatorKind::Goto(BasicBlockId(3)),
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Goto(BasicBlockId(3)),
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
        ],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    let elaborated = elaborate_drops(&body, &DropSemantics::default());
    validate(&elaborated).unwrap_or_else(|errors| panic!("{errors:?}\n{elaborated}"));
    let TerminatorKind::Goto(cleanup) = elaborated.blocks[1].terminator.kind else {
        panic!("branch cleanup edge");
    };
    assert_ne!(cleanup, BasicBlockId(3));
    assert!(matches!(
        elaborated.blocks[cleanup.0 as usize].terminator.kind,
        TerminatorKind::Drop {
            place: Place {
                local: LocalId(2),
                ..
            },
            success: BasicBlockId(3)
        }
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn drop_elaboration_initializes_and_cleans_typed_error_edges() {
    let boolean = Type::Primitive(PrimitiveType::Bool);
    let void = Type::Primitive(PrimitiveType::Void);
    let error = DeclarationId(90);
    let callable = Type::Function(FunctionType {
        parameters: Vec::new(),
        result: Box::new(void.clone()),
        effects: vec![error],
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    });
    let body = Body {
        declaration: DeclarationId(9),
        member: None,
        locals: vec![
            Local {
                name: Some("condition".into()),
                ty: boolean,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("callee".into()),
                ty: callable,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("source".into()),
                ty: Type::String,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("inner".into()),
                ty: Type::String,
                mutable: false,
                argument: false,
                span: span(),
            },
            Local {
                name: Some("caught".into()),
                ty: Type::ErrorUnion(vec![error]),
                mutable: false,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Switch {
                        value: Operand::Copy(Place::local(LocalId(0))),
                        targets: vec![(1, BasicBlockId(1))],
                        otherwise: BasicBlockId(2),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: vec![
                    Statement {
                        kind: StatementKind::StorageLive(LocalId(3)),
                        span: span(),
                    },
                    Statement {
                        kind: StatementKind::Assign(
                            Place::local(LocalId(3)),
                            Box::new(Rvalue::Use(Operand::Move(Place::local(LocalId(2))))),
                        ),
                        span: span(),
                    },
                    Statement {
                        kind: StatementKind::StorageLive(LocalId(4)),
                        span: span(),
                    },
                ],
                terminator: Terminator {
                    kind: TerminatorKind::Call {
                        function: Operand::Copy(Place::local(LocalId(1))),
                        receiver: None,
                        arguments: Vec::new(),
                        destination: None,
                        error_destination: Some(Place::local(LocalId(4))),
                        success: BasicBlockId(3),
                        error: Some(BasicBlockId(4)),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Switch {
                        value: Operand::Copy(Place::local(LocalId(0))),
                        targets: vec![(1, BasicBlockId(3))],
                        otherwise: BasicBlockId(4),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
        ],
        return_type: void,
        effects: Vec::new(),
    };
    validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let semantics = DropSemantics {
        nominal: [error].into_iter().collect(),
    };
    assert!(semantics.needs_drop(&Type::ErrorUnion(vec![error])));
    let elaborated = elaborate_drops(&body, &semantics);
    validate(&elaborated).unwrap_or_else(|errors| panic!("{errors:?}\n{elaborated}"));
    let TerminatorKind::Call { success, error, .. } = elaborated.blocks[1].terminator.kind else {
        panic!("fallible call retained");
    };
    let error_entry = error.expect("typed error edge");
    assert!(matches!(
        elaborated.blocks[error_entry.0 as usize]
            .statements
            .as_slice(),
        [Statement {
            kind: StatementKind::SetDropFlag(
                Place {
                    local: LocalId(4),
                    ..
                },
                true
            ),
            ..
        }]
    ));
    assert_ne!(success, BasicBlockId(3));
    assert_ne!(error_entry, BasicBlockId(4));
}

#[test]
fn drop_elaboration_drops_before_storage_dead() {
    let body = Body {
        declaration: DeclarationId(10),
        member: None,
        locals: vec![Local {
            name: Some("value".into()),
            ty: Type::String,
            mutable: false,
            argument: true,
            span: span(),
        }],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::StorageDead(LocalId(0)),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    let elaborated = elaborate_drops(&body, &DropSemantics::default());
    validate(&elaborated).unwrap_or_else(|errors| panic!("{errors:?}\n{elaborated}"));
    let TerminatorKind::Drop { place, success } = &elaborated.blocks[0].terminator.kind else {
        panic!("drop before storage death");
    };
    assert_eq!(*place, Place::local(LocalId(0)));
    assert!(matches!(
        elaborated.blocks[success.0 as usize].statements.as_slice(),
        [Statement {
            kind: StatementKind::StorageDead(LocalId(0)),
            ..
        }]
    ));
}

#[test]
fn typed_error_lowering_rewrites_returns_without_native_unwinding() {
    let error = DeclarationId(91);
    let body = Body {
        declaration: DeclarationId(11),
        member: None,
        locals: vec![Local {
            name: Some("condition".into()),
            ty: Type::Primitive(PrimitiveType::Bool),
            mutable: false,
            argument: true,
            span: span(),
        }],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Switch {
                        value: Operand::Copy(Place::local(LocalId(0))),
                        targets: vec![(1, BasicBlockId(1))],
                        otherwise: BasicBlockId(2),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Throw(Operand::Constant(Constant::Undefined(
                        Type::Nominal(error, Vec::new()),
                    ))),
                    span: span(),
                },
            },
        ],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: vec![error],
    };
    validate(&body).expect("generic typed-error MIR");
    let lowered = lower_typed_errors(&body);
    validate(&lowered).expect("tagged completion MIR");
    assert!(matches!(
        lowered.blocks[1].terminator.kind,
        TerminatorKind::TaggedReturn {
            completion: Completion::Success,
            payload: None
        }
    ));
    assert!(matches!(
        lowered.blocks[2].terminator.kind,
        TerminatorKind::TaggedReturn {
            completion: Completion::Error,
            payload: Some(_)
        }
    ));
    assert_eq!(lower_typed_errors(&lowered), lowered);
}

#[test]
fn mutation_tests_detect_each_corrupted_invariant() {
    let mut invalid_target = valid_body();
    invalid_target.blocks[0].terminator.kind = TerminatorKind::Goto(BasicBlockId(4));
    assert!(matches!(
        validate(&invalid_target).expect_err("missing block")[0],
        MirValidationError::InvalidTarget { .. }
    ));

    let mut invalid_local = valid_body();
    invalid_local.blocks[0].statements[0].kind = StatementKind::Assign(
        Place::local(LocalId(9)),
        Box::new(Rvalue::Use(Operand::Constant(Constant::Integer {
            value: 1,
            ty: Type::Primitive(PrimitiveType::I32),
        }))),
    );
    assert!(
        validate(&invalid_local)
            .expect_err("missing local")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidLocal { .. }))
    );

    let mut invalid_type = valid_body();
    invalid_type.blocks[0].statements[0].kind = StatementKind::Assign(
        Place::local(LocalId(0)),
        Box::new(Rvalue::Use(Operand::Constant(Constant::Bool(true)))),
    );
    assert!(
        validate(&invalid_type)
            .expect_err("type mismatch")
            .iter()
            .any(|error| matches!(error, MirValidationError::TypeMismatch { .. }))
    );

    let mut uninitialized = valid_body();
    uninitialized.blocks[0].statements.clear();
    assert!(
        validate(&uninitialized)
            .expect_err("uninitialized result")
            .iter()
            .any(|error| matches!(error, MirValidationError::UninitializedUse { .. }))
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn mutation_tests_cover_borrows_effects_switches_calls_and_error_edges() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let mut invalid_borrow = valid_body();
    invalid_borrow.blocks[0].statements.push(Statement {
        kind: StatementKind::Borrow {
            destination: LocalId(0),
            kind: BorrowKind::Shared,
            place: Place::local(LocalId(0)),
            region: RegionId(0),
        },
        span: span(),
    });
    assert!(
        validate(&invalid_borrow)
            .expect_err("borrow destination type")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidBorrowType { .. }))
    );

    let mut duplicate_region = invalid_borrow.clone();
    let repeated_borrow = duplicate_region.blocks[0].statements[1].clone();
    duplicate_region.blocks[0].statements.push(repeated_borrow);
    assert!(
        validate(&duplicate_region)
            .expect_err("duplicate region")
            .iter()
            .any(|error| matches!(error, MirValidationError::DuplicateRegion { .. }))
    );

    let mut invalid_switch = valid_body();
    invalid_switch.blocks[0].terminator.kind = TerminatorKind::Switch {
        value: Operand::Constant(Constant::Float {
            bits: 0,
            ty: Type::Primitive(PrimitiveType::F64),
        }),
        targets: Vec::new(),
        otherwise: BasicBlockId(0),
    };
    assert!(
        validate(&invalid_switch)
            .expect_err("invalid switch type")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidSwitchType { .. }))
    );

    let mut invalid_throw = valid_body();
    invalid_throw.blocks[0].terminator.kind =
        TerminatorKind::Throw(Operand::Constant(Constant::Integer {
            value: 1,
            ty: integer.clone(),
        }));
    assert!(
        validate(&invalid_throw)
            .expect_err("invalid thrown type")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidThrow { .. }))
    );

    let mut invalid_callable = valid_body();
    invalid_callable.blocks.push(valid_body().blocks.remove(0));
    invalid_callable.blocks[0].terminator.kind = TerminatorKind::Call {
        function: Operand::Constant(Constant::Bool(true)),
        receiver: None,
        arguments: Vec::new(),
        destination: None,
        error_destination: None,
        success: BasicBlockId(1),
        error: None,
    };
    assert!(
        validate(&invalid_callable)
            .expect_err("non-function call")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidCallable { .. }))
    );

    let error = DeclarationId(99);
    let function_type = Type::Function(FunctionType {
        parameters: Vec::new(),
        result: Box::new(integer.clone()),
        effects: vec![error],
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    });
    let error_edge = Body {
        declaration: DeclarationId(2),
        member: None,
        locals: vec![
            Local {
                name: Some("callee".into()),
                ty: function_type,
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("result".into()),
                ty: integer.clone(),
                mutable: true,
                argument: false,
                span: span(),
            },
            Local {
                name: Some("sink".into()),
                ty: integer,
                mutable: true,
                argument: false,
                span: span(),
            },
            Local {
                name: Some("error".into()),
                ty: Type::ErrorUnion(vec![error]),
                mutable: true,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Call {
                        function: Operand::Copy(Place::local(LocalId(0))),
                        receiver: None,
                        arguments: Vec::new(),
                        destination: Some(Place::local(LocalId(1))),
                        error_destination: Some(Place::local(LocalId(3))),
                        success: BasicBlockId(1),
                        error: Some(BasicBlockId(2)),
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
            BasicBlock {
                statements: vec![Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(2)),
                        Box::new(Rvalue::Use(Operand::Copy(Place::local(LocalId(1))))),
                    ),
                    span: span(),
                }],
                terminator: Terminator {
                    kind: TerminatorKind::Return(None),
                    span: span(),
                },
            },
        ],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: vec![error],
    };
    assert!(
        validate(&error_edge)
            .expect_err("call result unavailable on error edge")
            .iter()
            .any(|error| matches!(error, MirValidationError::UninitializedUse { local: 1, .. }))
    );
}

#[test]
fn validates_typed_suspension_destinations_and_edges() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let error = DeclarationId(77);
    let mut body = Body {
        declaration: DeclarationId(3),
        member: None,
        locals: vec![
            Local {
                name: Some("promise".into()),
                ty: Type::Promise {
                    result: Box::new(integer.clone()),
                    effects: vec![error],
                },
                mutable: false,
                argument: true,
                span: span(),
            },
            Local {
                name: Some("result".into()),
                ty: integer.clone(),
                mutable: true,
                argument: false,
                span: span(),
            },
            Local {
                name: Some("error".into()),
                ty: Type::ErrorUnion(vec![error]),
                mutable: true,
                argument: false,
                span: span(),
            },
        ],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Suspend {
                        value: Operand::Move(Place::local(LocalId(0))),
                        destination: Some(Place::local(LocalId(1))),
                        error_destination: Some(Place::local(LocalId(2))),
                        resume: BasicBlockId(1),
                        error: Some(BasicBlockId(2)),
                        cancel: BasicBlockId(3),
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
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Throw(Operand::Move(Place::local(LocalId(2)))),
                    span: span(),
                },
            },
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Unreachable,
                    span: span(),
                },
            },
        ],
        return_type: integer,
        effects: vec![error],
    };
    validate(&body).expect("valid typed suspension");

    if let TerminatorKind::Suspend {
        error_destination, ..
    } = &mut body.blocks[0].terminator.kind
    {
        *error_destination = None;
    }
    assert!(
        validate(&body)
            .expect_err("fallible suspension requires an error payload")
            .iter()
            .any(|error| matches!(error, MirValidationError::InvalidErrorEdge { .. }))
    );
}
