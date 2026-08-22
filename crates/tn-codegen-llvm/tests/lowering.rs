use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, FunctionType, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, BasicBlockId, BinaryOperator, Body, Constant, Local, LocalId, Operand, Place,
    Rvalue, Statement, StatementKind, Terminator, TerminatorKind, lower_typed_errors, validate,
};

fn span() -> SourceSpan {
    SourceSpan::new("codegen.tn", 0..0, "")
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

fn host_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

#[test]
fn verifies_checked_integer_control_flow_before_emission() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let body = Body {
        declaration: DeclarationId(1),
        member: None,
        locals: vec![
            local("left", integer.clone(), true),
            local("right", integer.clone(), true),
            local("result", integer.clone(), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(2)),
                    Box::new(Rvalue::CheckedBinary {
                        operator: BinaryOperator::Add,
                        left: Operand::Copy(Place::local(LocalId(0))),
                        right: Operand::Copy(Place::local(LocalId(1))),
                        operand_type: integer.clone(),
                        result_type: integer.clone(),
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Copy(Place::local(LocalId(2))))),
                span: span(),
            },
        }],
        return_type: integer,
        effects: Vec::new(),
    };
    validate(&body).expect("checked arithmetic MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "checked",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("verified LLVM module");
    assert!(ir.contains("llvm.sadd.with.overflow.i32"));
    assert!(ir.contains("tn_runtime_abort"));
    assert!(ir.contains("br i1"));
    assert!(!ir.contains("invoke "));
    assert!(!ir.contains("landingpad"));

    let directory = tempfile::tempdir().expect("emission directory");
    for (emission, name) in [
        (tn_codegen_llvm::Emission::LlvmIr, "checked.ll"),
        (tn_codegen_llvm::Emission::Bitcode, "checked.bc"),
        (tn_codegen_llvm::Emission::Assembly, "checked.s"),
        (tn_codegen_llvm::Emission::Object, "checked.o"),
    ] {
        let path = directory.path().join(name);
        tn_codegen_llvm::emit_to_file(
            "checked",
            &[lower_typed_errors(&body)],
            host_triple(),
            tn_codegen_llvm::CodegenProfile::Optimized,
            emission,
            &path,
        )
        .expect("verified backend emission");
        assert!(std::fs::metadata(path).expect("emitted product").len() > 0);
    }
}

#[test]
fn lowers_integer_constants_into_present_optional_values() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let optional = Type::Optional(Box::new(integer));
    let body = Body {
        declaration: DeclarationId(6),
        member: None,
        locals: vec![local("result", optional.clone(), false)],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(0)),
                    Box::new(Rvalue::Use(Operand::Constant(Constant::Integer {
                        value: 13,
                        ty: optional.clone(),
                    }))),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Copy(Place::local(LocalId(0))))),
                span: span(),
            },
        }],
        return_type: optional,
        effects: Vec::new(),
    };
    validate(&body).expect("optional constant MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "optional_constant",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("verified optional constant module");
    assert!(ir.contains("store { i1, i32 } { i1 true, i32 13 }"));
}

#[test]
fn lowers_owned_string_ordering_through_content_comparison() {
    let boolean = Type::Primitive(PrimitiveType::Bool);
    let body = Body {
        declaration: DeclarationId(5),
        member: None,
        locals: vec![
            local("left", Type::String, true),
            local("right", Type::String, true),
            local("result", boolean.clone(), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(2)),
                    Box::new(Rvalue::CheckedBinary {
                        operator: BinaryOperator::Less,
                        left: Operand::Copy(Place::local(LocalId(0))),
                        right: Operand::Copy(Place::local(LocalId(1))),
                        operand_type: Type::String,
                        result_type: boolean.clone(),
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Copy(Place::local(LocalId(2))))),
                span: span(),
            },
        }],
        return_type: boolean,
        effects: Vec::new(),
    };
    validate(&body).expect("string ordering MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "string_order",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("verified string ordering module");
    assert!(ir.contains("call i32 @tn_string_compare"));
    assert!(ir.contains("icmp slt i32"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn lowers_fallible_calls_to_tag_tests_and_explicit_error_successors() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let boolean = Type::Primitive(PrimitiveType::Bool);
    let error = DeclarationId(70);
    let fallible_type = Type::Function(FunctionType {
        parameters: vec![boolean.clone()],
        result: Box::new(integer.clone()),
        effects: vec![error],
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    });
    let producer = Body {
        declaration: DeclarationId(2),
        member: None,
        locals: vec![local("condition", boolean.clone(), true)],
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
                    kind: TerminatorKind::Return(Some(Operand::Constant(Constant::Integer {
                        value: 9,
                        ty: integer.clone(),
                    }))),
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
        return_type: integer.clone(),
        effects: vec![error],
    };
    let propagator = Body {
        declaration: DeclarationId(3),
        member: None,
        locals: vec![
            local("condition", boolean, true),
            local("result", integer.clone(), false),
            local("error", Type::ErrorUnion(vec![error]), false),
        ],
        blocks: vec![
            BasicBlock {
                statements: Vec::new(),
                terminator: Terminator {
                    kind: TerminatorKind::Call {
                        function: Operand::Constant(Constant::Function(
                            DeclarationId(2),
                            fallible_type,
                        )),
                        receiver: None,
                        arguments: vec![Operand::Copy(Place::local(LocalId(0)))],
                        destination: Some(Place::local(LocalId(1))),
                        error_destination: Some(Place::local(LocalId(2))),
                        success: BasicBlockId(1),
                        error: Some(BasicBlockId(2)),
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
        ],
        return_type: integer,
        effects: vec![error],
    };
    validate(&producer).expect("fallible callee MIR");
    validate(&propagator).expect("fallible caller MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "errors",
        &[
            lower_typed_errors(&producer),
            lower_typed_errors(&propagator),
        ],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("verified tagged call module");
    assert!(ir.contains("extractvalue"));
    assert!(ir.contains("call { i8, i32, ptr } @tn_2"));
    assert!(!ir.contains("personality"));
    assert!(!ir.contains("resume "));
}

#[test]
fn lowers_aggregate_array_indexing_with_a_dominating_bounds_check() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let array = Type::Array(Box::new(integer.clone()), 3);
    let body = Body {
        declaration: DeclarationId(4),
        member: None,
        locals: vec![
            local("index", Type::Primitive(PrimitiveType::Usize), true),
            local("values", array.clone(), false),
            local("result", integer.clone(), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(1)),
                        Box::new(Rvalue::Aggregate {
                            ty: array,
                            variant: None,
                            fields: [10, 20, 30]
                                .into_iter()
                                .map(|value| {
                                    Operand::Constant(Constant::Integer {
                                        value,
                                        ty: integer.clone(),
                                    })
                                })
                                .collect(),
                            field_types: vec![integer.clone(); 3],
                        }),
                    ),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(2)),
                        Box::new(Rvalue::CheckedIndex {
                            collection: Place::local(LocalId(1)),
                            index: Operand::Copy(Place::local(LocalId(0))),
                        }),
                    ),
                    span: span(),
                },
            ],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(2))))),
                span: span(),
            },
        }],
        return_type: integer,
        effects: Vec::new(),
    };
    validate(&body).expect("array index MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "index",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("verified array module");
    assert!(ir.contains("icmp ult"));
    assert!(ir.contains("getelementptr [3 x i32]"));
    assert!(!ir.contains("getelementptr inbounds"));
    assert!(ir.contains("tn_runtime_abort"));
}
