use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, FunctionType, HirClosureId, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, BasicBlockId, BinaryOperator, Body, Callable, Constant, Instance, Local, LocalId,
    MonomorphizedBody, Operand, Place, Rvalue, Statement, StatementKind, Terminator,
    TerminatorKind, lower_typed_errors, validate,
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
        "aarch64-apple-darwin"
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
fn emits_configured_external_runtime_calls_as_ordinary_calls() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let signature = FunctionType {
        parameters: vec![integer.clone()],
        result: Box::new(integer.clone()),
        effects: Vec::new(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    };
    let body = Body {
        declaration: DeclarationId(2),
        member: None,
        locals: vec![
            local("value", integer.clone(), true),
            local("result", integer, false),
        ],
        blocks: vec![
            BasicBlock {
                statements: vec![Statement {
                    kind: StatementKind::StorageLive(LocalId(1)),
                    span: span(),
                }],
                terminator: Terminator {
                    kind: TerminatorKind::Call {
                        function: Operand::Constant(Constant::ExternalFunction {
                            symbol: "tn_test_runtime".into(),
                            ty: Type::Function(signature.clone()),
                        }),
                        receiver: None,
                        arguments: vec![Operand::Copy(Place::local(LocalId(0)))],
                        destination: Some(Place::local(LocalId(1))),
                        error_destination: None,
                        success: BasicBlockId(1),
                        error: None,
                    },
                    span: span(),
                },
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator {
                    kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(1))))),
                    span: span(),
                },
            },
        ],
        return_type: signature.result.as_ref().clone(),
        effects: Vec::new(),
    };
    validate(&body).expect("external runtime call MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "external-runtime",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("external runtime call lowers to LLVM");
    assert!(ir.contains("tn_test_runtime"));
    assert!(ir.contains("call i32 @tn_test_runtime"));
}

#[test]
fn lowers_array_binding_rest_to_a_slice_view() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let source = Type::Array(Box::new(integer.clone()), 3);
    let rest = Type::Slice(Box::new(integer.clone()));
    let body = Body {
        declaration: DeclarationId(3),
        member: None,
        locals: vec![
            local("values", source, true),
            local("rest", rest.clone(), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::RawOperation {
                        operation: "binding_rest".into(),
                        operands: vec![
                            Operand::Copy(Place::local(LocalId(0))),
                            Operand::Constant(Constant::Integer {
                                value: 1,
                                ty: Type::Primitive(PrimitiveType::Usize),
                            }),
                        ],
                        ty: rest.clone(),
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(1))))),
                span: span(),
            },
        }],
        return_type: rest,
        effects: Vec::new(),
    };
    validate(&body).expect("array binding rest MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "binding_rest",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("array binding rest emits valid LLVM");
    assert!(ir.contains("binding.rest.array.data"), "{ir}");
    assert!(ir.contains("binding.rest.length"), "{ir}");
}

#[test]
fn carries_inline_attribute_to_direct_llvm_function() {
    let body = Body {
        declaration: DeclarationId(77),
        member: None,
        locals: Vec::new(),
        blocks: vec![BasicBlock {
            statements: Vec::new(),
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    let callable = Callable::function(DeclarationId(77));
    let units = vec![MonomorphizedBody {
        instance: Instance::concrete(callable),
        body: lower_typed_errors(&body),
    }];
    let mut layouts = tn_codegen_llvm::Layouts::default();
    layouts.inlines.insert(callable);
    let ir = tn_codegen_llvm::compile_program_to_llvm_ir(
        "inline_hint",
        &units,
        &layouts,
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("inline function emits valid LLVM");
    assert!(ir.contains("inlinehint"), "{ir}");
}

#[test]
fn lowers_atomic_load_store_rmw_cmpxchg_and_fence_to_llvm() {
    let i32_type = Type::Primitive(PrimitiveType::I32);
    let bool_type = Type::Primitive(PrimitiveType::Bool);
    let pointer_type = Type::RawPointer {
        mutable: true,
        pointee: Box::new(i32_type.clone()),
    };
    let order = |value| {
        Operand::Constant(Constant::Integer {
            value,
            ty: Type::Primitive(PrimitiveType::U8),
        })
    };
    let local_result = |name: &str, ty: Type| local(name, ty, false);
    let body = Body {
        declaration: DeclarationId(78),
        member: None,
        locals: vec![
            local("value", pointer_type.clone(), true),
            local("delta", i32_type.clone(), true),
            local("expected", pointer_type.clone(), true),
            local_result("loaded", i32_type.clone()),
            local_result("added", i32_type.clone()),
            local_result("stored", i32_type.clone()),
            local_result("exchanged", bool_type.clone()),
            local_result("fenced", bool_type.clone()),
        ],
        blocks: vec![BasicBlock {
            statements: vec![
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(3)),
                        Box::new(Rvalue::RawOperation {
                            operation: "atomic_i32_load".into(),
                            operands: vec![Operand::Copy(Place::local(LocalId(0))), order(1)],
                            ty: i32_type.clone(),
                        }),
                    ),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(4)),
                        Box::new(Rvalue::RawOperation {
                            operation: "atomic_i32_fetch_add".into(),
                            operands: vec![
                                Operand::Copy(Place::local(LocalId(0))),
                                Operand::Copy(Place::local(LocalId(1))),
                                order(0),
                            ],
                            ty: i32_type.clone(),
                        }),
                    ),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(5)),
                        Box::new(Rvalue::RawOperation {
                            operation: "atomic_i32_store".into(),
                            operands: vec![
                                Operand::Copy(Place::local(LocalId(0))),
                                Operand::Copy(Place::local(LocalId(1))),
                                order(2),
                            ],
                            ty: i32_type.clone(),
                        }),
                    ),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(6)),
                        Box::new(Rvalue::RawOperation {
                            operation: "atomic_i32_compare_exchange".into(),
                            operands: vec![
                                Operand::Copy(Place::local(LocalId(0))),
                                Operand::Copy(Place::local(LocalId(2))),
                                Operand::Copy(Place::local(LocalId(1))),
                                order(3),
                                order(1),
                            ],
                            ty: bool_type.clone(),
                        }),
                    ),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(7)),
                        Box::new(Rvalue::RawOperation {
                            operation: "atomic_fence".into(),
                            operands: vec![order(4)],
                            ty: bool_type.clone(),
                        }),
                    ),
                    span: span(),
                },
            ],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(6))))),
                span: span(),
            },
        }],
        return_type: bool_type,
        effects: Vec::new(),
    };
    validate(&body).expect("atomic MIR");
    let ir = tn_codegen_llvm::compile_to_llvm_ir(
        "atomics",
        &[lower_typed_errors(&body)],
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("atomic operations emit valid LLVM");
    assert!(ir.contains("load atomic i32"), "{ir}");
    assert!(ir.contains("atomicrmw add"), "{ir}");
    assert!(ir.contains("store atomic i32"), "{ir}");
    assert!(ir.contains("cmpxchg"), "{ir}");
    assert!(ir.contains("fence seq_cst"), "{ir}");
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
fn loads_global_callables_as_borrowed_values() {
    let optional_callback = Type::Optional(Box::new(Type::Function(closure_function())));
    let body = Body {
        declaration: DeclarationId(1000),
        member: None,
        locals: vec![local("callback", optional_callback.clone(), false)],
        blocks: vec![BasicBlock {
            statements: vec![
                Statement {
                    kind: StatementKind::StorageLive(LocalId(0)),
                    span: span(),
                },
                Statement {
                    kind: StatementKind::Assign(
                        Place::local(LocalId(0)),
                        Box::new(Rvalue::RawOperation {
                            operation: "global_load:1001".into(),
                            operands: Vec::new(),
                            ty: optional_callback.clone(),
                        }),
                    ),
                    span: span(),
                },
            ],
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    };
    let mut layouts = tn_codegen_llvm::Layouts::default();
    layouts.globals.insert(
        DeclarationId(1001),
        tn_codegen_llvm::GlobalLayout {
            name: "global_callback".into(),
            ty: optional_callback.clone(),
            mutable_static: true,
        },
    );
    let units = vec![MonomorphizedBody {
        instance: Instance::concrete(Callable::function(DeclarationId(1000))),
        body: lower_typed_errors(&body),
    }];
    let ir = tn_codegen_llvm::compile_program_to_llvm_ir(
        "borrowed_global",
        &units,
        &layouts,
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("global callback load emits valid LLVM");
    assert!(ir.contains("global.borrowed.optional.payload"), "{ir}");
    assert!(ir.contains("global.borrowed.callable.drop"), "{ir}");
    assert!(ir.contains("ptr null"), "{ir}");
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

fn closure_function() -> FunctionType {
    FunctionType {
        parameters: Vec::new(),
        result: Box::new(Type::Primitive(PrimitiveType::Void)),
        effects: Vec::new(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    }
}

fn closure_body(captured: Type) -> Body {
    Body {
        declaration: DeclarationId(901),
        member: None,
        locals: vec![local("captured", captured, true)],
        blocks: vec![BasicBlock {
            statements: Vec::new(),
            terminator: Terminator {
                kind: TerminatorKind::Return(None),
                span: span(),
            },
        }],
        return_type: Type::Primitive(PrimitiveType::Void),
        effects: Vec::new(),
    }
}

fn body_with_closure(captured: Type) -> Body {
    let function = closure_function();
    Body {
        declaration: DeclarationId(900),
        member: None,
        locals: vec![
            local("captured", captured.clone(), true),
            local("callback", Type::Function(function.clone()), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::Closure {
                        id: HirClosureId(0),
                        function,
                        captures: vec![Operand::Copy(Place::local(LocalId(0)))],
                        body: Box::new(closure_body(captured)),
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
    }
}

#[test]
fn specializes_closure_targets_per_monomorphized_instance() {
    let callable = Callable::function(DeclarationId(900));
    let units = vec![
        MonomorphizedBody {
            instance: Instance {
                callable,
                type_arguments: vec![Type::Primitive(PrimitiveType::Bool)],
                effects: Vec::new(),
            },
            body: lower_typed_errors(&body_with_closure(Type::Primitive(PrimitiveType::Bool))),
        },
        MonomorphizedBody {
            instance: Instance {
                callable,
                type_arguments: vec![Type::Primitive(PrimitiveType::I32)],
                effects: Vec::new(),
            },
            body: lower_typed_errors(&body_with_closure(Type::Primitive(PrimitiveType::I32))),
        },
    ];
    let ir = tn_codegen_llvm::compile_program_to_llvm_ir(
        "closure-specialization",
        &units,
        &tn_codegen_llvm::Layouts::default(),
        host_triple(),
        tn_codegen_llvm::CodegenProfile::Debug,
    )
    .expect("specialized closure targets emit valid LLVM");
    let closure_bodies = ir
        .lines()
        .filter(|line| line.starts_with("define internal void @tn_closure_0_body"))
        .count();
    assert_eq!(closure_bodies, 2, "{ir}");
    assert!(!ir.contains("No predecessors!"), "{ir}");
}
