use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, BinaryOperator, Body, Local, LocalId, Operand, Place, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, lower_typed_errors, validate,
};

fn span() -> SourceSpan {
    SourceSpan::new("numeric.tn", 0..0, "")
}

fn host_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

fn numeric_body(declaration: u64, primitive: PrimitiveType, operator: BinaryOperator) -> Body {
    let operand_type = Type::Primitive(primitive);
    let result_type = if matches!(
        operator,
        BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual
    ) {
        Type::Primitive(PrimitiveType::Bool)
    } else {
        operand_type.clone()
    };
    let local = |name: &str, ty: Type, argument| Local {
        name: Some(name.into()),
        ty,
        mutable: false,
        argument,
        span: span(),
    };
    Body {
        declaration: DeclarationId(declaration),
        member: None,
        locals: vec![
            local("left", operand_type.clone(), true),
            local("right", operand_type.clone(), true),
            local("result", result_type.clone(), false),
        ],
        blocks: vec![BasicBlock {
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(2)),
                    Box::new(Rvalue::CheckedBinary {
                        operator,
                        left: Operand::Copy(Place::local(LocalId(0))),
                        right: Operand::Copy(Place::local(LocalId(1))),
                        operand_type,
                        result_type: result_type.clone(),
                    }),
                ),
                span: span(),
            }],
            terminator: Terminator {
                kind: TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(2))))),
                span: span(),
            },
        }],
        return_type: result_type,
        effects: Vec::new(),
    }
}

#[test]
fn verifies_every_integer_width_and_binary_operation_in_both_profiles() {
    let primitives = [
        PrimitiveType::I8,
        PrimitiveType::I16,
        PrimitiveType::I32,
        PrimitiveType::I64,
        PrimitiveType::I128,
        PrimitiveType::Isize,
        PrimitiveType::U8,
        PrimitiveType::U16,
        PrimitiveType::U32,
        PrimitiveType::U64,
        PrimitiveType::U128,
        PrimitiveType::Usize,
    ];
    let operators = [
        BinaryOperator::Add,
        BinaryOperator::Subtract,
        BinaryOperator::Multiply,
        BinaryOperator::Divide,
        BinaryOperator::Remainder,
        BinaryOperator::ShiftLeft,
        BinaryOperator::ShiftRight,
        BinaryOperator::BitAnd,
        BinaryOperator::BitOr,
        BinaryOperator::BitXor,
        BinaryOperator::Equal,
        BinaryOperator::NotEqual,
        BinaryOperator::Less,
        BinaryOperator::LessEqual,
        BinaryOperator::Greater,
        BinaryOperator::GreaterEqual,
    ];
    let mut declaration = 100_u64;
    let mut bodies = Vec::new();
    for primitive in primitives {
        for operator in operators {
            let body = numeric_body(declaration, primitive.clone(), operator);
            validate(&body).expect("numeric MIR");
            bodies.push(lower_typed_errors(&body));
            declaration += 1;
        }
    }
    for profile in [
        tn_codegen_llvm::CodegenProfile::Debug,
        tn_codegen_llvm::CodegenProfile::Optimized,
    ] {
        let ir = tn_codegen_llvm::compile_to_llvm_ir("numeric", &bodies, host_triple(), profile)
            .expect("verified numeric module");
        for width in [8, 16, 32, 64, 128] {
            assert!(ir.contains(&format!("with.overflow.i{width}")));
        }
        assert!(ir.contains("division_overflows"));
        assert!(ir.contains("shift_count_valid"));
        assert!(ir.contains("shift_preserved"));
    }
}
