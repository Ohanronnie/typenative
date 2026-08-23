use tn_diagnostics::SourceSpan;
use tn_hir::{DeclarationId, PrimitiveType, Type};
use tn_mir::{
    BasicBlock, Body, BorrowKind, Constant, Local, LocalId, Operand, Place, Projection, RegionId,
    Rvalue, Statement, StatementKind, Terminator, TerminatorKind,
};
use tn_typecheck::{
    Capture, CaptureKind, OwnershipFacts, check_capture_requirements, check_ownership,
};

fn span(offset: usize) -> SourceSpan {
    SourceSpan::new("ownership.tn", offset..offset + 1, "0123456789")
}

fn local(name: &str, ty: Type, argument: bool) -> Local {
    Local {
        name: Some(name.into()),
        ty,
        mutable: true,
        argument,
        span: span(0),
    }
}

fn body(
    locals: Vec<Local>,
    statements: Vec<Statement>,
    terminator: TerminatorKind,
    result: Type,
) -> Body {
    Body {
        declaration: DeclarationId(1),
        member: None,
        locals,
        blocks: vec![BasicBlock {
            statements,
            terminator: Terminator {
                kind: terminator,
                span: span(9),
            },
        }],
        return_type: result,
        effects: Vec::new(),
    }
}

fn conditions(body: &Body, facts: &OwnershipFacts) -> Vec<String> {
    check_ownership(body, facts)
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

fn source_conditions(source: &str) -> Vec<String> {
    let program = source_program(source);
    tn_typecheck::check_source_rules(&program)
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

fn source_program(source: &str) -> tn_hir::Program {
    let directory = tempfile::tempdir().expect("temporary ownership source fixture");
    let standard_library = directory.path().join("std");
    std::fs::create_dir(&standard_library).expect("create standard library fixture");
    let path = directory.path().join("main.tn");
    std::fs::write(&path, source).expect("write ownership source fixture");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load ownership source graph");
    tn_hir::lower_program(graph).expect("lower ownership source fixture")
}

fn source_program_with_workspace_standard_library(source: &str) -> tn_hir::Program {
    let directory = tempfile::tempdir().expect("temporary ownership source fixture");
    let path = directory.path().join("main.tn");
    std::fs::write(&path, source).expect("write ownership source fixture");
    let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load ownership source graph with workspace standard library");
    tn_hir::lower_program(graph).expect("lower ownership source fixture")
}

fn ownership_conditions_with_workspace_standard_library(source: &str) -> Vec<String> {
    let program = source_program_with_workspace_standard_library(source);
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let checked = tn_typecheck::check_bodies_with_ownership(&program, &facts);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    tn_typecheck::lower_mir_with_ownership(&program, &checked.bodies, &facts)
        .iter()
        .flat_map(|body| tn_typecheck::check_ownership(body, &facts).diagnostics)
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

#[test]
fn lowers_intrinsic_string_instance_calls_as_direct_methods() {
    let program = source_program_with_workspace_standard_library(
        r"
function main(value: string): u8 {
  const upper = value.toAsciiUppercase();
  const copy = value.clone();
  const view: &str = value.asStr();
  const raw: &[u8] = value.bytes();
  return raw[0usize];
}
",
    );
    let declaration = program
        .intrinsic_type_declaration(&Type::String)
        .expect("declared string intrinsic");
    let tn_hir::DefinitionData::Struct { methods, .. } = &program
        .definition(declaration)
        .expect("string definition")
        .data
    else {
        panic!("string intrinsic must be a struct declaration");
    };
    let expected = methods
        .iter()
        .filter(|method| {
            ["toAsciiUppercase", "clone", "asStr", "bytes"].contains(&method.name.as_str())
        })
        .map(|method| method.id)
        .collect::<std::collections::BTreeSet<_>>();
    let bodies = lower_mir(&program);
    let body = bodies
        .iter()
        .find(|body| {
            program
                .graph
                .declaration(body.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("main")
        })
        .expect("main MIR");
    let lowered = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .filter_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::DirectMethod { member, .. } => Some(*member),
                _ => None,
            },
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(expected.len(), 4);
    assert!(expected.is_subset(&lowered));
}

#[test]
fn lowers_atomic_class_methods_to_direct_intrinsic_operations() {
    let program = source_program_with_workspace_standard_library(
        r#"
import { AtomicI32, MemoryOrder } from "std/core";
function main(): i32 {
  let counter = new AtomicI32(0i32);
  counter.fetchAdd(1i32, MemoryOrder.Relaxed);
  return counter.load(MemoryOrder.Acquire);
}
"#,
    );
    let mir = lower_mir(&program);
    let operations = mir
        .iter()
        .flat_map(|body| body.blocks.iter())
        .flat_map(|block| block.statements.iter())
        .filter_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::RawOperation { operation, .. } => Some(operation.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        operations.contains(&"atomic_i32_fetch_add".into()),
        "{operations:?}"
    );
    assert!(
        operations.contains(&"atomic_i32_load".into()),
        "{operations:?}"
    );
}

#[test]
fn preserves_copy_queries_until_generic_specialization() {
    let program = source_program_with_workspace_standard_library(
        "import { Array } from \"std/collections\";\nfunction main(): void {}\n",
    );
    let (array, push) = program
        .definitions
        .iter()
        .find_map(|definition| {
            let declaration = program.graph.declaration(definition.declaration)?;
            let tn_hir::DefinitionData::Class { methods, .. } = &definition.data else {
                return None;
            };
            (declaration.name.as_deref() == Some("Array")).then(|| {
                (
                    definition.declaration,
                    methods
                        .iter()
                        .find(|method| method.name == "push")
                        .expect("Array.push")
                        .id,
                )
            })
        })
        .expect("Array declaration");
    let bodies = lower_mir(&program);
    let push_body = bodies
        .iter()
        .find(|body| body.declaration == array && body.member == Some(push))
        .expect("Array.push MIR");
    assert!(push_body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(
                        value.as_ref(),
                        Rvalue::RawOperation { operation, operands, .. }
                            if operation == "is_copy"
                                && matches!(
                                    operands.as_slice(),
                                    [Operand::Constant(Constant::Undefined(Type::Generic(name)))]
                                        if name == "T"
                                )
                    )
            )
        })
    }));
}

#[test]
fn rejects_user_defined_intrinsic_type_bindings() {
    assert_eq!(
        source_conditions("@Intrinsic(\"string\") struct FakeString {}"),
        ["TYPE_INVALID_ATTRIBUTE_TARGET"]
    );
}

#[test]
fn rejects_user_defined_primitive_intrinsic_bindings() {
    assert_eq!(
        source_conditions("@Intrinsic(\"usize\") struct FakeUsize {}"),
        ["TYPE_INVALID_ATTRIBUTE_TARGET"]
    );
}

#[test]
fn rejects_user_defined_intrinsic_operations() {
    assert_eq!(
        source_conditions("@Intrinsic(\"size_of\") function forged<T>(): usize { return 0usize; }"),
        ["TYPE_INVALID_ATTRIBUTE_TARGET"]
    );
    assert_eq!(
        source_conditions("@Intrinsic function unnamed<T>(): usize { return 0usize; }"),
        ["TYPE_INVALID_ATTRIBUTE_TARGET"]
    );
}

fn lower_mir(program: &tn_hir::Program) -> Vec<Body> {
    let checked = tn_typecheck::check_bodies(program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    tn_typecheck::lower_mir(program, &checked.bodies)
}

#[test]
fn dereference_uses_the_pointee_type_inside_a_boolean_expression() {
    let program = source_program(
        r"
function less(left: &i32, right: &i32): bool {
  unsafe { return *left < *right; }
}
",
    );
    let body = lower_mir(&program).pop().expect("dereference MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let dereference_types = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .filter_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::RawOperation { operation, ty, .. } if operation == "dereference" => {
                    Some(ty.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dereference_types,
        [
            Type::Primitive(PrimitiveType::I32),
            Type::Primitive(PrimitiveType::I32)
        ]
    );
}

#[test]
fn lowers_direct_dereference_of_a_method_call() {
    let program = source_program(
        r"
class Counter {
  private value: i32;
  public constructor(value: i32) { this.value = value; }
  public get(): &i32 { unsafe { return & this.value; } }
}
function read(counter: Counter): i32 { unsafe { return * counter.get(); } }
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.member.is_none()
                && body
                    .locals
                    .iter()
                    .any(|local| local.name.as_deref() == Some("counter"))
        })
        .expect("method dereference MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::VtableLookup { .. })
            )
        })
    }));
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::RawOperation { operation, .. } if operation == "dereference")
            )
        })
    }));
}

#[test]
fn lowers_arc_get_as_a_class_method_call() {
    let program = source_program_with_workspace_standard_library(
        r#"
import { Arc } from "std/alloc";
function main(): i32 {
  let owner = new Arc(42i32);
  let value: i32 = 0i32;
  unsafe { value = * owner.get(); }
  return value;
}
"#,
    );
    let arc = program
        .definitions
        .iter()
        .find(|definition| {
            program
                .graph
                .declaration(definition.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("Arc")
        })
        .expect("Arc declaration");
    let tn_hir::DefinitionData::Class { methods, .. } = &arc.data else {
        panic!("Arc must be a class");
    };
    assert!(methods.iter().any(|method| method.name == "get"));
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.member.is_none()
                && program
                    .graph
                    .declaration(body.declaration)
                    .and_then(|declaration| declaration.name.as_deref())
                    == Some("main")
        })
        .expect("Arc get MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::VtableLookup { .. })
            )
        })
    }));
}

#[test]
fn lowers_generic_async_run_body_after_specialization() {
    let program = source_program_with_workspace_standard_library(
        r#"
import { run } from "std/async";
async function answer(): Promise<i32, never> { return 42i32; }
function main(): i32 { return run(answer()); }
"#,
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.member.is_none()
                && program
                    .graph
                    .declaration(body.declaration)
                    .and_then(|declaration| declaration.name.as_deref())
                    == Some("run")
        })
        .expect("generic run MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::RawOperation { operation, .. } if operation == "dereference")
            )
        })
    }));
    assert!(
        body.blocks
            .iter()
            .any(|block| { matches!(block.terminator.kind, TerminatorKind::Return(Some(_))) }),
        "{body:#?}"
    );
}

#[test]
fn lowers_generic_mutex_guard_get_mut_body() {
    let program = source_program_with_workspace_standard_library(
        r#"
import { MutexGuard } from "std/sync";
function main(): void {}
"#,
    );
    let guard = program
        .definitions
        .iter()
        .find(|definition| {
            program
                .graph
                .declaration(definition.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("MutexGuard")
        })
        .expect("MutexGuard declaration");
    let tn_hir::DefinitionData::Struct { methods, .. } = &guard.data else {
        panic!("MutexGuard must be a struct");
    };
    let get_mut = methods
        .iter()
        .find(|method| method.name == "getMut")
        .expect("MutexGuard.getMut method");
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| body.declaration == guard.declaration && body.member == Some(get_mut.id))
        .expect("MutexGuard.getMut MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks
            .iter()
            .any(|block| { matches!(block.terminator.kind, TerminatorKind::Return(Some(_))) })
    );
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Borrow {
                    kind: BorrowKind::Mutable,
                    ..
                }
            )
        })
    }));
}

#[test]
fn lowers_pointer_assignment_to_a_dereference_place() {
    let program = source_program_with_workspace_standard_library(
        r#"
import { rawAlloc, rawFree } from "std/alloc";
function write(): void {
  unsafe {
    const pointer = rawAlloc(4usize);
    *(pointer as *mut i32) = 19i32;
    rawFree(pointer);
  }
}
function main(): void { write(); }
"#,
    );
    let bodies = lower_mir(&program);
    assert!(bodies.iter().any(|body| {
        body.blocks.iter().any(|block| {
            block.statements.iter().any(|statement| {
                matches!(
                    &statement.kind,
                    StatementKind::Assign(place, _)
                        if place.projection == vec![Projection::Dereference]
                )
            })
        })
    }));
}

#[test]
fn lowers_unary_short_circuit_generic_calls_without_function_pointer_operands() {
    let program = source_program(
        r"
function identity<T>(value: T): T { return value; }
function evaluate(): bool {
  return !identity<bool>(true) || !identity<bool>(false);
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    assert!(!bodies.is_empty());
    assert!(bodies.iter().any(|body| {
        body.blocks.iter().any(|block| {
            block.statements.iter().any(|statement| {
                matches!(
                    statement.kind,
                    StatementKind::Assign(_, ref value)
                        if matches!(
                            value.as_ref(),
                            Rvalue::Unary {
                                operator: tn_mir::UnaryOperator::LogicalNot,
                                ..
                            }
                        )
                )
            })
        })
    }));
    for body in bodies {
        tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
}

#[test]
fn preserves_unary_not_on_short_circuit_if_operands() {
    let program = source_program(
        r"
function predicate(value: bool): bool { return value; }
function evaluate(): i32 {
  if (!predicate(true) || !predicate(true)) {
    return 1;
  }
  return 42;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    let evaluate = bodies
        .iter()
        .find(|body| body.return_type == Type::Primitive(PrimitiveType::I32))
        .expect("evaluate MIR");
    let first_switch = evaluate
        .blocks
        .iter()
        .find(|block| matches!(block.terminator.kind, TerminatorKind::Switch { .. }))
        .expect("short-circuit switch");
    let switch_local = match &first_switch.terminator.kind {
        TerminatorKind::Switch {
            value: Operand::Move(place),
            ..
        } if place.projection.is_empty() => place.local,
        _ => panic!("short-circuit switch should consume a local"),
    };
    assert!(first_switch.statements.iter().any(|statement| {
        matches!(
            &statement.kind,
            StatementKind::Assign(place, value)
                if place.local == switch_local
                    && place.projection.is_empty()
                    && matches!(
                        value.as_ref(),
                        Rvalue::Unary {
                            operator: tn_mir::UnaryOperator::LogicalNot,
                            ..
                        }
                    )
        )
    }));
}

#[test]
fn rejects_move_after_use_at_the_causal_access() {
    let program = body(
        vec![
            local("value", Type::String, true),
            local("sink", Type::String, false),
        ],
        vec![
            Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::Use(Operand::Move(Place::local(LocalId(0))))),
                ),
                span: span(1),
            },
            Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(1)),
                    Box::new(Rvalue::Use(Operand::Copy(Place::local(LocalId(0))))),
                ),
                span: span(2),
            },
        ],
        TerminatorKind::Return(None),
        Type::Primitive(PrimitiveType::Void),
    );
    assert!(
        conditions(&program, &OwnershipFacts::default())
            .contains(&"OWNERSHIP_USE_AFTER_MOVE".into())
    );
}

#[test]
fn ownership_state_does_not_flow_from_a_returning_branch_into_its_join() {
    let program = source_program(
        r"
function choose(early: bool, value: string): string {
  if (early) { return value; }
  return value;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("branch MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let result =
        tn_typecheck::check_ownership(&body, &tn_typecheck::derive_ownership_facts(&program));
    assert!(
        result.diagnostics.is_empty(),
        "{:?}\n{body}",
        result.diagnostics
    );
}

#[test]
fn non_lexical_liveness_rejects_only_overlapping_live_loans() {
    let reference = Type::Reference {
        mutable: true,
        lifetime: "scope".into(),
        referent: Box::new(Type::String),
    };
    let program = body(
        vec![
            local("value", Type::String, true),
            local("first", reference.clone(), false),
            local("second", reference.clone(), false),
            local("useFirst", reference, false),
        ],
        vec![
            Statement {
                kind: StatementKind::Borrow {
                    destination: LocalId(1),
                    kind: BorrowKind::Mutable,
                    place: Place::local(LocalId(0)),
                    region: RegionId(0),
                },
                span: span(1),
            },
            Statement {
                kind: StatementKind::Borrow {
                    destination: LocalId(2),
                    kind: BorrowKind::Mutable,
                    place: Place::local(LocalId(0)),
                    region: RegionId(1),
                },
                span: span(2),
            },
            Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(3)),
                    Box::new(Rvalue::Use(Operand::Copy(Place::local(LocalId(1))))),
                ),
                span: span(3),
            },
        ],
        TerminatorKind::Return(None),
        Type::Primitive(PrimitiveType::Void),
    );
    assert!(
        conditions(&program, &OwnershipFacts::default())
            .contains(&"OWNERSHIP_CONFLICTING_BORROW".into())
    );

    let mut ended = program;
    ended.blocks[0].statements.pop();
    assert!(
        !conditions(&ended, &OwnershipFacts::default())
            .contains(&"OWNERSHIP_CONFLICTING_BORROW".into())
    );
}

#[test]
fn rejects_returned_local_references_and_partial_moves_from_drop_types() {
    let integer = Type::Primitive(PrimitiveType::I32);
    let reference = Type::Reference {
        mutable: false,
        lifetime: "scope".into(),
        referent: Box::new(integer.clone()),
    };
    let returned = body(
        vec![
            local("local", integer.clone(), false),
            local("reference", reference.clone(), false),
        ],
        vec![
            Statement {
                kind: StatementKind::Assign(
                    Place::local(LocalId(0)),
                    Box::new(Rvalue::Use(Operand::Constant(Constant::Integer {
                        value: 1,
                        ty: integer,
                    }))),
                ),
                span: span(1),
            },
            Statement {
                kind: StatementKind::Borrow {
                    destination: LocalId(1),
                    kind: BorrowKind::Shared,
                    place: Place::local(LocalId(0)),
                    region: RegionId(0),
                },
                span: span(2),
            },
        ],
        TerminatorKind::Return(Some(Operand::Move(Place::local(LocalId(1))))),
        reference,
    );
    assert!(
        conditions(&returned, &OwnershipFacts::default())
            .contains(&"OWNERSHIP_RETURNED_LOCAL_REFERENCE".into())
    );

    let nominal = DeclarationId(77);
    let mut facts = OwnershipFacts::default();
    facts.drop.insert(nominal);
    let partial = body(
        vec![
            local("owner", Type::Nominal(nominal, Vec::new()), true),
            local("field", Type::Error, false),
        ],
        vec![Statement {
            kind: StatementKind::Assign(
                Place::local(LocalId(1)),
                Box::new(Rvalue::Use(Operand::Move(Place {
                    local: LocalId(0),
                    projection: vec![Projection::Field {
                        index: 0,
                        ty: Type::Error,
                    }],
                }))),
            ),
            span: span(4),
        }],
        TerminatorKind::Return(None),
        Type::Primitive(PrimitiveType::Void),
    );
    assert!(conditions(&partial, &facts).contains(&"OWNERSHIP_PARTIAL_MOVE_FROM_DROP_TYPE".into()));
}

#[test]
fn source_pipeline_rejects_move_from_borrow_returned_local_and_live_await_loan() {
    let diagnostics = source_conditions(
        r"
function moveBorrow(value: &string): void { move value; }
function returned(): &number {
  const value = 1;
  const borrowed = &value;
  return borrowed;
}
async function suspended(pending: Promise<void, never>): Promise<void, never> {
  const value = 1;
  const borrowed = &value;
  await pending;
  borrowed;
  return;
}
",
    );
    assert!(diagnostics.contains(&"OWNERSHIP_MOVE_FROM_BORROW".into()));
    assert!(diagnostics.contains(&"OWNERSHIP_RETURNED_LOCAL_REFERENCE".into()));
    assert!(diagnostics.contains(&"OWNERSHIP_BORROW_ACROSS_SUSPEND".into()));
}

#[test]
fn returned_lifetime_aggregate_keeps_the_source_loan_live() {
    let diagnostics = ownership_conditions_with_workspace_standard_library(
        r"
struct View<lifetime a> { public value: &a i32; }
function retain<lifetime a>(value: &a i32): View<a> {
  return { value: value };
}
function consume<lifetime a>(view: &View<a>): void {
  view.value;
}
function rejected(): void {
  let value = 1i32;
  const view = retain(&value);
  value = 2i32;
  consume(&view);
}
function accepted(): void {
  let value = 1i32;
  const view = retain(&value);
  consume(&view);
  value = 2i32;
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "OWNERSHIP_WRITE_DURING_BORROW")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn byte_views_prevent_compaction_until_the_last_use() {
    let diagnostics = ownership_conditions_with_workspace_standard_library(
        r#"
import { borrow, ByteView, BytesMut } from "std/bytes";
function consume<lifetime a>(view: &ByteView<a>): void {
  view.get(0usize);
}
function rejected(): void {
  let buffer = new BytesMut({ capacity: 8usize });
  const view = borrow(&buffer);
  buffer.discardPrefix(1usize);
  consume(&view);
}
function accepted(): void {
  let buffer = new BytesMut({ capacity: 8usize });
  const view = borrow(&buffer);
  consume(&view);
  buffer.discardPrefix(1usize);
}
function escaped(): ByteView<scope> {
  let buffer = new BytesMut({ capacity: 8usize });
  return borrow(&buffer);
}
"#,
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "OWNERSHIP_WRITE_DURING_BORROW")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "OWNERSHIP_RETURNED_LOCAL_REFERENCE")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn enforces_send_sync_and_static_constraints_for_thread_captures() {
    let nominal = DeclarationId(42);
    let captures = vec![
        Capture {
            name: "raw".into(),
            ty: Type::RawPointer {
                mutable: false,
                pointee: Box::new(Type::Primitive(PrimitiveType::I32)),
            },
            kind: CaptureKind::Move,
            span: span(1),
        },
        Capture {
            name: "borrowed".into(),
            ty: Type::Reference {
                mutable: false,
                lifetime: "scope".into(),
                referent: Box::new(Type::Nominal(nominal, Vec::new())),
            },
            kind: CaptureKind::SharedBorrow,
            span: span(2),
        },
    ];
    let diagnostics = check_capture_requirements(&captures, true, &OwnershipFacts::default())
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(diagnostics.contains(&"OWNERSHIP_CAPTURE_NOT_THREAD_SAFE".into()));
    assert!(diagnostics.contains(&"OWNERSHIP_DETACHED_CAPTURE_NOT_STATIC".into()));
}

#[test]
fn lowers_source_ownership_events_to_valid_deterministic_mir() {
    let program = source_program(
        r"
function ownership(value: string): void {
  const number = 1;
  const borrowed = &number;
  const moved = move value;
  borrowed;
}
",
    );
    let bodies = lower_mir(&program);
    assert_eq!(bodies.len(), 1);
    let validation = tn_mir::validate(&bodies[0]);
    assert!(validation.is_ok(), "{validation:?}\n{}", bodies[0]);
    assert_eq!(bodies[0].to_string(), bodies[0].to_string());
    let invalid = source_program(
        r"
function ownership(value: string): void {
  const moved = move value;
  value;
}
",
    );
    let invalid_bodies =
        tn_typecheck::lower_mir(&invalid, &tn_typecheck::check_bodies(&invalid).bodies);
    let diagnostics = check_ownership(&invalid_bodies[0], &OwnershipFacts::default())
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(diagnostics.contains(&"OWNERSHIP_USE_AFTER_MOVE".into()));
}

#[test]
fn implicitly_moves_noncopy_bindings_arguments_and_fields() {
    let program = source_program(
        r"
interface Drop { mut drop(): void; }
@Conform(Drop)
struct Pair {
  public first: string;
  public second: string;
  mut drop(): void {}
}
function consume(value: string): void {}
function assignment(value: string): void {
  const taken = value;
  value;
  taken;
}
function argument(value: string): void {
  consume(value);
  value;
}
function partial(pair: Pair): void {
  const first = pair.first;
  pair;
  first;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let diagnostics = tn_typecheck::lower_mir(&program, &checked.bodies)
        .iter()
        .flat_map(|body| tn_typecheck::check_ownership(body, &facts).diagnostics)
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(diagnostics.contains(&"OWNERSHIP_USE_AFTER_MOVE".into()));
    assert!(diagnostics.contains(&"OWNERSHIP_PARTIAL_MOVE_FROM_DROP_TYPE".into()));
}

#[test]
fn derives_send_and_sync_structurally_for_aggregate_fields() {
    let program = source_program(
        r"
struct Safe { public value: i32; }
struct Unsafe { public pointer: *const i32; }
struct Wrapper<T> { public value: T; }
class Recursive { public next?: Recursive; }
enum Choice { Number(i32), Pointer(*const i32) }
",
    );
    let declaration = |name: &str| {
        program
            .graph
            .modules
            .iter()
            .flat_map(|module| &module.declarations)
            .find(|declaration| declaration.name.as_deref() == Some(name))
            .expect("named aggregate")
            .id
    };
    let facts = tn_typecheck::derive_ownership_facts(&program);
    assert!(facts.is_send(&Type::Nominal(declaration("Safe"), Vec::new())));
    assert!(facts.is_sync(&Type::Nominal(declaration("Safe"), Vec::new())));
    assert!(facts.is_send(&Type::Nominal(declaration("Recursive"), Vec::new())));
    assert!(!facts.is_send(&Type::Nominal(declaration("Unsafe"), Vec::new())));
    assert!(!facts.is_sync(&Type::Nominal(declaration("Choice"), Vec::new())));
    assert!(facts.is_send(&Type::Nominal(
        declaration("Wrapper"),
        vec![Type::Primitive(PrimitiveType::I32)]
    )));
    assert!(!facts.is_send(&Type::Nominal(
        declaration("Wrapper"),
        vec![Type::RawPointer {
            mutable: false,
            pointee: Box::new(Type::Primitive(PrimitiveType::I32)),
        }]
    )));
}

#[test]
fn promise_error_alternatives_participate_in_thread_safety() {
    let program = source_program(
        r"
class UnsafeError { public pointer: *const i32; }
function ready(): Promise<i32, never> { return undefined; }
function unsafePromise(): Promise<i32, UnsafeError> { return undefined; }
",
    );
    let declaration = |name: &str| {
        program
            .graph
            .modules
            .iter()
            .flat_map(|module| &module.declarations)
            .find(|declaration| declaration.name.as_deref() == Some(name))
            .expect("named function")
            .id
    };
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let promise = |name: &str| {
        let definition = program
            .definition(declaration(name))
            .expect("named function definition");
        let tn_hir::DefinitionData::Function(function) = &definition.data else {
            panic!("expected function");
        };
        function.result.clone()
    };
    assert!(facts.is_send(&promise("ready")));
    assert!(facts.is_sync(&promise("ready")));
    assert!(!facts.is_send(&promise("unsafePromise")));
    assert!(!facts.is_sync(&promise("unsafePromise")));
}

#[test]
fn derives_copy_from_the_canonical_attribute() {
    let program = source_program(
        r"
@Copy
struct Scalar { public value: i32; }
",
    );
    let scalar = program
        .definitions
        .iter()
        .find(|definition| {
            program
                .graph
                .declaration(definition.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("Scalar")
        })
        .expect("derived scalar")
        .declaration;
    let facts = tn_typecheck::derive_ownership_facts(&program);
    assert!(facts.is_copy(&Type::Nominal(scalar, Vec::new())));
}

#[test]
fn derives_structural_drop_glue_and_elaborates_source_scope_exits() {
    let program = source_program(
        r"
struct Resource { public text: string; }
function release(condition: bool, resource: Resource): void {
  if (condition) {
    const inner = resource;
    inner;
  }
}
",
    );
    let resource = program
        .definitions
        .iter()
        .find(|definition| {
            program
                .graph
                .declaration(definition.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("Resource")
        })
        .expect("resource definition")
        .declaration;
    let semantics = tn_typecheck::derive_drop_semantics(&program);
    assert!(semantics.nominal.contains(&resource));
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("resource MIR");
    let elaborated = tn_typecheck::elaborate_drops(&program, &body);
    tn_mir::validate(&elaborated).unwrap_or_else(|errors| panic!("{errors:?}\n{elaborated}"));
    assert!(
        elaborated
            .blocks
            .iter()
            .any(|block| matches!(block.terminator.kind, TerminatorKind::Drop { .. }))
    );
}

#[test]
fn lowers_annotated_and_literal_returns_with_unique_method_identities() {
    let program = source_program(
        r"
function annotated(): i32 { const value: i32 = 42; return value; }
function literal(): i32 { return 7; }
class Methods {
  public first(): void {}
  public second(): void {}
}
",
    );
    let bodies = lower_mir(&program);
    assert_eq!(bodies.len(), 4);
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
    let methods = bodies
        .iter()
        .filter_map(|body| body.member)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(methods.len(), 2);
    assert!(bodies.iter().any(|body| {
        body.return_type == Type::Primitive(PrimitiveType::I32)
            && body.locals.iter().any(|local| {
                local.name.as_deref() == Some("value")
                    && local.ty == Type::Primitive(PrimitiveType::I32)
            })
    }));
}

#[test]
fn lowers_branching_loops_and_typed_binary_expressions_to_valid_cfg() {
    let program = source_program(
        r"
function control(value: i32): i32 {
  const incremented: i32 = value + 1;
  while (value > 0) {
    if (value === 1) { break; }
    continue;
  }
  if (value < 0) { return 0; } else { return incremented; }
}
",
    );
    let bodies = lower_mir(&program);
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks
            .iter()
            .filter(|block| matches!(block.terminator.kind, TerminatorKind::Switch { .. }))
            .count()
            >= 3
    );
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(statement.kind, StatementKind::Assign(_, ref value)
            if matches!(value.as_ref(), Rvalue::CheckedBinary { .. }))
    )));
}

#[test]
fn lowers_calls_with_typed_arguments_results_and_error_edges() {
    let program = source_program(
        r"
class Failure {}
function add(left: i32, right: i32): i32 { return left + right; }
function fail(): i32 throws Failure { return 1; }
function caller(value: i32): i32 throws Failure {
  const sum: i32 = add(value, 2);
  return sum + try fail();
}
",
    );
    let bodies = lower_mir(&program);
    let caller = bodies
        .iter()
        .find(|body| {
            body.effects.len() == 1
                && body
                    .locals
                    .iter()
                    .any(|local| local.name.as_deref() == Some("sum"))
        })
        .expect("caller MIR");
    tn_mir::validate(caller).unwrap_or_else(|errors| panic!("{errors:?}\n{caller}"));
    let calls = caller
        .blocks
        .iter()
        .filter_map(|block| match &block.terminator.kind {
            TerminatorKind::Call {
                arguments,
                destination,
                error,
                ..
            } => Some((arguments.len(), destination.is_some(), error.is_some())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, vec![(2, true, false), (0, true, true)]);
}

#[test]
fn lowers_tuple_array_aggregates_and_checked_indexes() {
    let program = source_program(
        r"
function aggregate(value: i32): i32 {
  const values: [i32; 2] = [value, 2];
  const pair: (i32, bool) = (value, true);
  pair;
  return values[0];
}
",
    );
    let bodies = lower_mir(&program);
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let mut aggregates = 0;
    let mut indexes = 0;
    for statement in body.blocks.iter().flat_map(|block| &block.statements) {
        if let StatementKind::Assign(_, value) = &statement.kind {
            match value.as_ref() {
                Rvalue::Aggregate { .. } => aggregates += 1,
                Rvalue::CheckedIndex { .. } => indexes += 1,
                _ => {}
            }
        }
    }
    assert_eq!((aggregates, indexes), (2, 1));
}

#[test]
fn lowers_resolved_nominal_fields_to_typed_projections() {
    let program = source_program(
        r"
struct Point { public x: i32; public y: i32; }
function field(point: Point): i32 { return point.y; }
",
    );
    let body = lower_mir(&program).pop().expect("field MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(matches!(
        &body.blocks[0].terminator.kind,
        TerminatorKind::Return(Some(Operand::Copy(Place { projection, .. })))
            if matches!(projection.as_slice(), [Projection::Field { index: 1, ty }]
                if *ty == Type::Primitive(PrimitiveType::I32))
    ));
}

#[test]
fn lowers_builtin_for_of_to_checked_iterator_cfg() {
    let program = source_program(
        r"
function sum(values: [i32; 3]): i32 {
  let result: i32 = 0;
  for (const item of values) {
    result = result + item;
  }
  return result;
}
",
    );
    let body = lower_mir(&program).pop().expect("for-of MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(statement.kind, StatementKind::Assign(_, ref value)
            if matches!(value.as_ref(), Rvalue::Length(_)))
    )));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(statement.kind, StatementKind::Assign(_, ref value)
            if matches!(value.as_ref(), Rvalue::CheckedIndex { .. }))
    )));
}

#[test]
fn lowers_user_iterators_through_recorded_implementation_witnesses() {
    let program = source_program(
        r"
interface Iterator<Item> { mut next(): Item | undefined; }
interface IntoIterator<Item, Iter extends Iterator<Item> > {
  move intoIterator(): Iter;
}
@Conform(Iterator)
struct BagIterator<T> { public done: bool;
  mut next(): T | undefined { return undefined; }
}
@Conform(IntoIterator)
struct Bag<T> {
  move intoIterator(): BagIterator<T> { return { done: false }; }
}
function consume(values: Bag<i32>): void {
  for (const value of values) { value; }
}
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("values"))
        })
        .expect("user iterator MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let methods = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .filter_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::DirectMethod {
                    implementation,
                    member,
                    receiver,
                    ..
                } => Some((*implementation, *member, *receiver)),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 2, "{body}");
    assert!(
        methods
            .iter()
            .any(|(_, _, receiver)| *receiver == tn_hir::ReceiverMode::Move)
    );
    assert!(
        methods
            .iter()
            .any(|(_, _, receiver)| *receiver == tn_hir::ReceiverMode::Mutable)
    );
    assert_ne!(methods[0].0, methods[1].0);
    assert!(body.blocks.iter().any(|block| matches!(
        block.terminator.kind,
        TerminatorKind::Switch { ref targets, .. }
            if targets.len() == 1 && targets[0].0 == 1
    )));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::Use(Operand::Move(Place { projection, .. }))
                if projection.as_slice() == [Projection::Downcast(1)]))
    )));
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let ownership = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        ownership.diagnostics.is_empty(),
        "{:?}",
        ownership.diagnostics
    );
}

#[test]
fn iterator_calls_reinitialize_noncopy_optional_destinations() {
    let program = source_program(
        r"
interface Iterator<Item> { mut next(): Item | undefined; }
interface IntoIterator<Item, Iter extends Iterator<Item> > {
  move intoIterator(): Iter;
}
struct Payload { public text: string; }
@Conform(Iterator)
struct PayloadIterator {
  mut next(): Payload | undefined { return undefined; }
}
@Conform(IntoIterator)
struct Payloads {
  move intoIterator(): PayloadIterator { return {}; }
}
function consume(values: Payloads): void {
  for (const value of values) { value.text; }
}
",
    );
    let facts = tn_typecheck::derive_ownership_facts(&program);
    for body in lower_mir(&program) {
        tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
        let checked = tn_typecheck::check_ownership(&body, &facts);
        assert!(
            checked.diagnostics.is_empty(),
            "{:?}\n{body}",
            checked.diagnostics
        );
    }
}

#[test]
fn lowers_closed_enum_match_with_payloads_to_switch_cfg() {
    let program = source_program(
        r"
enum Choice { Number(i32), Empty }
function choose(value: Choice): i32 {
  return switch (value) {
    case Choice.Number(number) if number > 0: number,
    case Choice.Number(other): 0,
    case Choice.Empty: 0,
  };
}
",
    );
    let body = lower_mir(&program).pop().expect("match MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(matches!(
        &body.blocks[0].terminator.kind,
        TerminatorKind::Switch { targets, .. } if targets.len() == 2
    ));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(statement.kind, StatementKind::Assign(_, ref value)
            if matches!(value.as_ref(), Rvalue::Use(Operand::Copy(Place { projection, .. }))
                if projection.iter().any(|projection| matches!(projection, Projection::Downcast(0)))))
    )));
}

#[test]
fn lowers_explicit_enum_discriminants_as_switch_values() {
    let program = source_program(
        r"
enum Status { Ready = 41, Done = 73 }
function code(value: Status): i32 {
  return switch (value) { case Status.Ready: 1, case Status.Done: 2, };
}
",
    );
    let body = lower_mir(&program).pop().expect("discriminant MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(matches!(
        &body.blocks[0].terminator.kind,
        TerminatorKind::Switch { targets, .. }
            if targets.iter().map(|(value, _)| *value).collect::<Vec<_>>() == vec![41, 73]
    ));
}

#[test]
fn retains_pattern_binding_field_projections_independent_of_name_order() {
    let program = source_program(
        r"
enum Pair { Both(i32, bool) }
function second(value: Pair): bool {
  return switch (value) { case Pair.Both(zeta, alpha): alpha, };
}
",
    );
    let body = lower_mir(&program).pop().expect("pattern projection MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let alpha = body
        .locals
        .iter()
        .position(|local| local.name.as_deref() == Some("alpha"))
        .expect("alpha binding");
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| {
                matches!(&statement.kind,
            StatementKind::Assign(Place { local, .. }, value)
                if local.0 as usize == alpha
                    && matches!(value.as_ref(), Rvalue::Use(Operand::Copy(Place { projection, .. }))
                        if projection.iter().any(|projection| matches!(projection,
                            Projection::Field { index: 1, .. }))))
            }))
    );
}

#[test]
fn lowers_optional_narrowing_to_payload_downcast() {
    let program = source_program(
        r"
function unwrap(value: i32 | undefined): i32 {
  return switch (value) { case undefined: 0, case present: present, };
}
",
    );
    let body = lower_mir(&program).pop().expect("optional match MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| {
                matches!(&statement.kind,
            StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::Use(Operand::Copy(Place { projection, .. }))
                    if projection.as_slice() == [Projection::Downcast(1)]))
            }))
    );
}

#[test]
fn lowers_typed_catches_with_explicit_error_payload_dispatch() {
    let program = source_program(
        r"
class Failure {}
class SpecificFailure extends Failure {}
class OtherFailure {}
function fail(): void throws SpecificFailure {}
function failOther(): void throws OtherFailure {}
function handled(): void {
  try {
    try fail();
    try failOther();
  } catch (error: Failure) {
    error;
  } catch (other: OtherFailure) {
    other;
  }
}
",
    );
    let bodies = lower_mir(&program);
    let body = bodies
        .iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("other"))
        })
        .expect("catch MIR");
    tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| matches!(
        block.terminator.kind,
        TerminatorKind::Call {
            error_destination: Some(_),
            error: Some(_),
            ..
        }
    )));
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| {
                matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::Cast { kind: tn_mir::CastKind::ErrorUnion, .. }))
            }))
    );
    assert!(
        body.blocks.iter().any(|block| matches!(
            &block.terminator.kind,
            TerminatorKind::Switch { targets, .. } if targets.len() == 2
        )),
        "{body}"
    );
}

#[test]
fn lowers_unary_cast_conditional_and_short_circuit_expressions() {
    let program = source_program(
        r"
class Base {}
class Derived extends Base {}
function expressions(flag: bool, value: i32, base: Base): i32 {
  const negative: i32 = -value;
  const inverted: i32 = ~value;
  const opposite: bool = !flag;
  const chosen: i32 = flag ? negative : inverted;
  const both: bool = flag && opposite;
  const downcast: &Derived | undefined = base as? Derived;
  both;
  downcast;
  return chosen;
}
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("chosen"))
        })
        .expect("expression MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| {
                matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::Unary { .. }))
            }))
    );
    assert!(
        body.blocks
            .iter()
            .filter(|block| matches!(block.terminator.kind, TerminatorKind::Switch { .. }))
            .count()
            >= 2
    );
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| {
                matches!(&statement.kind, StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::Cast {
                    kind: tn_mir::CastKind::CheckedDowncast,
                    ..
                }))
            }))
    );
}

#[test]
fn checked_downcasts_reborrow_owners_and_keep_the_source_loan_live() {
    let program = source_program(
        r"
class Base {}
class Derived extends Base {}
function invalid(base: &mut Base): void {
  const derived = base as? Derived;
  move base;
  derived;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("checked downcast MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| matches!(
                statement.kind,
                StatementKind::Borrow {
                    kind: BorrowKind::Mutable,
                    ..
                }
            )))
    );
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let ownership = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        ownership
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.condition.as_str() == "OWNERSHIP_WRITE_DURING_BORROW" })
    );
}

#[test]
fn lowers_nullish_coalescing_with_lazy_fallback_cfg() {
    let program = source_program(
        r"
function fallback(): i32 { return 9; }
function unwrap(value: i32 | undefined): i32 { return value ?? fallback(); }
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("value"))
        })
        .expect("coalesce MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(matches!(
        body.blocks[0].terminator.kind,
        TerminatorKind::Switch { .. }
    ));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::Use(Operand::Copy(Place { projection, .. }))
                if projection.as_slice() == [Projection::Downcast(1)]))
    )));
    assert!(
        body.blocks
            .iter()
            .any(|block| matches!(block.terminator.kind, TerminatorKind::Call { .. }))
    );
}

#[test]
fn lowers_complete_optional_chains_to_lazy_present_path_cfg() {
    let program = source_program(
        r"
struct Meta { public code: i32; }
struct Details {
  public meta: Meta;
  public maybeMeta?: Meta;
  public values: [i32; 2];
  public codeValue(): i32 { return this.meta.code; }
}
struct Config { public details?: Details; }
function field(config: Config): i32 | undefined {
  return config.details?.meta.code;
}
function method(config: Config): i32 | undefined {
  return config.details?.codeValue();
}
function index(config: Config): i32 | undefined {
  return config.details?.values[0usize];
}
function nested(config: Config): i32 | undefined {
  return config.details?.maybeMeta?.code;
}
",
    );
    let bodies = lower_mir(&program)
        .into_iter()
        .filter(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("config"))
        })
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 4);
    let facts = tn_typecheck::derive_ownership_facts(&program);
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
        let ownership = tn_typecheck::check_ownership(body, &facts);
        assert!(
            ownership.diagnostics.is_empty(),
            "{:?}\n{body}",
            ownership.diagnostics
        );
        assert!(body.blocks.iter().any(|block| matches!(
            block.terminator.kind,
            TerminatorKind::Switch { ref targets, .. }
                if targets.len() == 1 && targets[0].0 == 1
        )));
        assert!(body.blocks.iter().any(|block| block.statements.iter().any(
            |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::Use(Operand::Constant(
                    tn_mir::Constant::Undefined(Type::Optional(_))))))
        )));
        assert!(body.blocks.iter().any(|block| {
            block
                .statements
                .iter()
                .any(|statement| matches!(statement.kind, StatementKind::SetDiscriminant(_, 1)))
        }));
    }
    assert!(bodies.iter().any(|body| {
        body.blocks
            .iter()
            .any(|block| matches!(block.terminator.kind, TerminatorKind::Call { .. }))
    }));
    assert!(bodies.iter().any(
        |body| body.blocks.iter().any(|block| block.statements.iter().any(
            |statement| matches!(&statement.kind,
            StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::CheckedIndex { .. }))
        ))
    ));
    assert!(bodies.iter().any(|body| {
        body.blocks
            .iter()
            .filter(|block| matches!(block.terminator.kind, TerminatorKind::Switch { .. }))
            .count()
            == 2
    }));
}

#[test]
fn lowers_optional_field_reborrows_without_moving_borrowed_payloads() {
    let program = source_program(
        r"
struct Payload { public text: string; }
struct Holder { public payload?: Payload; }
function shared(holder: &Holder): void {
  const selected = holder.payload?.text;
  selected;
}
function mutable(holder: &mut Holder): void {
  const selected = holder.payload?.text;
  selected;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    assert_eq!(bodies.len(), 2);
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let mut shared = 0;
    let mut mutable = 0;
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
        let ownership = tn_typecheck::check_ownership(body, &facts);
        assert!(
            ownership.diagnostics.is_empty(),
            "{:?}\n{body}",
            ownership.diagnostics
        );
        for kind in body
            .blocks
            .iter()
            .flat_map(|block| &block.statements)
            .filter_map(|statement| match statement.kind {
                StatementKind::Borrow { kind, .. } => Some(kind),
                _ => None,
            })
        {
            match kind {
                BorrowKind::Shared => shared += 1,
                BorrowKind::Mutable => mutable += 1,
            }
        }
        assert!(
            body.blocks
                .iter()
                .any(|block| matches!(block.terminator.kind, TerminatorKind::Switch { .. }))
        );
    }
    assert!(shared >= 2, "shared payload and field reborrows");
    assert!(mutable >= 2, "mutable payload and field reborrows");
}

#[test]
fn lowers_closure_environments_and_bodies_with_typed_capture_modes() {
    let program = source_program(
        r"
function shared(base: i32): i32 {
  const add: (i32) => i32 = (value: i32): i32 => value + base;
  return add(2);
}
function mutable(base: i32): i32 {
  let total: i32 = base;
  const bump: () => void = (): void => { total = total + 1; };
  bump();
  return total;
}
function moved(base: i32): i32 {
  const add: (i32) => i32 = move (value: i32): i32 => value + base;
  return add(2);
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    assert_eq!(
        checked
            .bodies
            .iter()
            .map(|body| body.closures.len())
            .sum::<usize>(),
        3
    );
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let mut borrowed = 0;
    let mut moved = 0;
    let mut mutable = 0;
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
        let ownership = tn_typecheck::check_ownership(body, &facts);
        assert!(
            ownership.diagnostics.is_empty(),
            "{:?}\n{body}",
            ownership.diagnostics
        );
        for closure in body
            .blocks
            .iter()
            .flat_map(|block| &block.statements)
            .filter_map(|statement| match &statement.kind {
                StatementKind::Assign(_, value) => match value.as_ref() {
                    Rvalue::Closure { captures, body, .. } => Some((captures, body)),
                    _ => None,
                },
                _ => None,
            })
        {
            assert_eq!(closure.0.len(), 1);
            assert!(
                closure
                    .1
                    .blocks
                    .iter()
                    .any(|block| block.statements.iter().any(
                        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::CheckedBinary { .. }))
                    ))
            );
            match &closure.1.locals[0].ty {
                Type::Reference { mutable: true, .. } => mutable += 1,
                Type::Reference { mutable: false, .. } => borrowed += 1,
                Type::Primitive(PrimitiveType::I32) => moved += 1,
                ty => panic!("unexpected capture type {ty:?}"),
            }
        }
    }
    assert_eq!((borrowed, mutable, moved), (1, 1, 1));
}

#[test]
fn lowers_templates_with_ordered_owned_values_and_shared_borrows() {
    let program = source_program(
        r"
interface Display {}
@Conform(Display)
struct Shown {}
function side(value: i32): i32 { return value + 1; }
function format(value: i32, shown: Shown): void {
  const formatted = `start\n${side(value)} shown=${shown} value=${value} literal=\${x} tick=\``;
  formatted;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("formatted"))
        })
        .expect("template MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let ownership = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        ownership.diagnostics.is_empty(),
        "{:?}\n{body}",
        ownership.diagnostics
    );
    let call_block = body
        .blocks
        .iter()
        .position(|block| matches!(block.terminator.kind, TerminatorKind::Call { .. }))
        .expect("interpolation call");
    let (template_block, parts, captures, template_type) = body
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(block, body)| {
            body.statements.iter().filter_map(move |statement| {
                let StatementKind::Assign(_, value) = &statement.kind else {
                    return None;
                };
                let Rvalue::Template {
                    parts,
                    captures,
                    ty,
                    ..
                } = value.as_ref()
                else {
                    return None;
                };
                Some((block, parts, captures, ty))
            })
        })
        .next()
        .expect("template rvalue");
    assert!(call_block < template_block, "{body}");
    assert_eq!(captures.len(), 3);
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, tn_mir::TemplatePart::Interpolation { .. }))
            .count(),
        3
    );
    assert_eq!(
        parts
            .iter()
            .filter_map(|part| match part {
                tn_mir::TemplatePart::Literal(value) => Some(value.as_str()),
                tn_mir::TemplatePart::Interpolation { .. } => None,
            })
            .collect::<String>(),
        "start\n shown= value= literal=${x} tick=`"
    );
    let Type::Template(capture_types) = template_type else {
        panic!("typed template value");
    };
    assert!(matches!(
        capture_types.as_slice(),
        [
            Type::Primitive(PrimitiveType::I32),
            Type::Reference { mutable: false, .. },
            Type::Reference { mutable: false, .. }
        ]
    ));
}

#[test]
fn lowers_nested_closures_with_outer_capture_and_parameter_reborrows() {
    let program = source_program(
        r"
type Unary = (i32) => i32;
function nested(base: i32): i32 {
  const make: (i32) => Unary =
    (left: i32): Unary => (right: i32): i32 => left + right + base;
  const add: Unary = make(1);
  return add(2);
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("nested closure MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let outer = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .find_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::Closure { body, .. } => Some(body),
                _ => None,
            },
            _ => None,
        })
        .expect("outer closure");
    let inner_captures = outer
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .find_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::Closure { captures, .. } => Some(captures.len()),
                _ => None,
            },
            _ => None,
        });
    assert_eq!(inner_captures, Some(2));
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let ownership = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        ownership.diagnostics.is_empty(),
        "{:?}",
        ownership.diagnostics
    );
}

#[test]
fn keeps_captured_loans_live_until_the_closure_last_use() {
    let program = source_program(
        r"
function invalid(): void {
  let value: i32 = 0;
  const read: () => void = (): void => { value; };
  value = 1;
  read();
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("captured loan MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let ownership = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        ownership
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.condition.as_str() == "OWNERSHIP_WRITE_DURING_BORROW" }),
        "{:?}\n{body}",
        ownership.diagnostics
    );
}

#[test]
fn lowers_struct_literals_in_declaration_layout_order() {
    let program = source_program(
        r"
struct Point { public x: i32; public y: bool; }
function make(): Point { return { y: true, x: 7 }; }
",
    );
    let body = lower_mir(&program).pop().expect("struct aggregate MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let aggregate = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .find_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::Aggregate {
                    fields,
                    field_types,
                    ..
                } => Some((fields, field_types)),
                _ => None,
            },
            _ => None,
        })
        .expect("aggregate rvalue");
    assert!(matches!(
        aggregate.0.as_slice(),
        [
            Operand::Constant(tn_mir::Constant::Integer { value: 7, .. }),
            Operand::Constant(tn_mir::Constant::Bool(true)),
        ]
    ));
    assert_eq!(
        aggregate.1,
        &vec![
            Type::Primitive(PrimitiveType::I32),
            Type::Primitive(PrimitiveType::Bool),
        ]
    );
}

#[test]
fn lowers_decoded_string_and_character_literals() {
    let program = source_program(
        r#"
function text(): &str { return "line\n\u{03bb}"; }
function character(): char { return '\u{03bb}'; }
"#,
    );
    let bodies = lower_mir(&program);
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
    assert!(bodies.iter().any(|body| body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(),
                        Rvalue::Use(Operand::Constant(tn_mir::Constant::String(value)))
                        | Rvalue::Cast {
                            operand: Operand::Constant(tn_mir::Constant::String(value)), ..
                        } if value == "line\nλ")
            )
        }) || matches!(
            &block.terminator.kind,
            TerminatorKind::Return(Some(Operand::Constant(tn_mir::Constant::String(value))))
                if value == "line\nλ"
        )
    })));
    assert!(
        bodies
            .iter()
            .any(|body| body.blocks.iter().any(|block| matches!(
                block.terminator.kind,
                TerminatorKind::Return(Some(Operand::Constant(tn_mir::Constant::Character('λ'))))
            )))
    );
}

#[test]
fn lowers_contextual_owned_string_conversion_explicitly() {
    let program = source_program(
        r#"
function consume(value: string): string { return value; }
function equals(value: string): bool { return value === "guest"; }
function make(): string {
  const local: string = "ronnie";
  consume("guest");
  return "done";
}
"#,
    );
    let bodies = lower_mir(&program);
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
    let conversions = bodies
        .iter()
        .flat_map(|body| &body.blocks)
        .flat_map(|block| &block.statements)
        .filter(|statement| {
            matches!(
                &statement.kind,
                StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), Rvalue::RawOperation { operation, ty: Type::String, .. } if operation == "string_from_static")
            )
        })
        .count();
    assert_eq!(conversions, 3);
}

#[test]
fn lowers_compound_assignments_as_checked_read_modify_write() {
    let program = source_program(
        r"
function update(): i32 {
  let value: i32 = 4;
  value += 2;
  value <<= 1;
  return value;
}
",
    );
    let body = lower_mir(&program).pop().expect("compound assignment MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let operators = body
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .filter_map(|statement| match &statement.kind {
            StatementKind::Assign(_, value) => match value.as_ref() {
                Rvalue::CheckedBinary { operator, .. } => Some(*operator),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(operators.contains(&tn_mir::BinaryOperator::Add));
    assert!(operators.contains(&tn_mir::BinaryOperator::ShiftLeft));
}

#[test]
fn lowers_class_construction_to_typed_constructor_call() {
    let program = source_program(
        r"
class Boxed {
  private value: i32;
  public constructor(value: i32) { this.value = value; }
}
function make(): Boxed { return new Boxed(4); }
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| body.member.is_none())
        .expect("constructor call MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| matches!(
        &block.terminator.kind,
        TerminatorKind::Call {
            function: Operand::Constant(tn_mir::Constant::Constructor { member: Some(_), .. }),
            arguments,
            destination: Some(_),
            ..
        } if matches!(arguments.as_slice(), [Operand::Constant(tn_mir::Constant::Integer { value: 4, .. })])
    )));
}

#[test]
fn lowers_class_method_calls_through_stable_vtable_slots() {
    let program = source_program(
        r"
class Counter {
  private value: i32;
  public constructor(value: i32) { this.value = value; }
  public read(): i32 { return this.value; }
}
function read(counter: Counter): i32 { return counter.read(); }
",
    );
    let body = lower_mir(&program)
        .into_iter()
        .find(|body| {
            body.member.is_none()
                && body
                    .locals
                    .iter()
                    .any(|local| local.name.as_deref() == Some("counter"))
        })
        .expect("method call MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(
        body.blocks.iter().any(|block| block.statements.iter().any(
            |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::VtableLookup { slot: 1, .. }))
        )),
        "{body}"
    );
    assert!(body.blocks.iter().any(|block| matches!(
        &block.terminator.kind,
        TerminatorKind::Call { arguments, .. } if arguments.is_empty()
    )));
}

#[test]
fn lowers_dynamic_interface_calls_through_witness_slots() {
    let program = source_program(
        r"
interface Reader {
  first(): i32;
  second(value: i32): i32;
}
function call(reader: Reader): i32 { return reader.second(3); }
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| body.member.is_none())
        .expect("witness call MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::WitnessLookup { slot: 1, .. }))
    )));
}

#[test]
fn lowers_instanceof_to_typed_runtime_type_test() {
    let program = source_program(
        r"
class Base {}
class Derived extends Base {}
function isDerived(value: Base): bool { return value instanceof Derived; }
",
    );
    let body = lower_mir(&program).pop().expect("type-test MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::TypeTest { target: Type::Nominal(_, _), .. }))
    )));
}

#[test]
fn lowers_empty_and_payload_enum_construction_to_tagged_aggregates() {
    let program = source_program(
        r"
enum Message { Empty, Value(i32) }
function empty(): Message { return Message.Empty; }
function value(): Message { return Message.Value(8); }
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    for body in &bodies {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
    assert!(bodies.iter().any(|body| body.blocks.iter().any(|block| {
        block
            .statements
            .iter()
            .any(|statement| matches!(statement.kind, StatementKind::SetDiscriminant(_, 0)))
    })));
    assert!(bodies.iter().any(|body| body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| matches!(
            statement.kind,
            StatementKind::SetDiscriminant(_, 1)
        )) && block.statements.iter().any(|statement| matches!(
            &statement.kind,
            StatementKind::Assign(_, value)
                if matches!(value.as_ref(), Rvalue::Aggregate { fields, .. } if fields.len() == 1)
        ))
    })));
}

#[test]
fn specializes_generic_enum_variant_payloads_from_context() {
    let program = source_program(
        r"
enum Boxed<T> { Value(T) }
function boxed(): Boxed<i32> { return Boxed.Value(12); }
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .pop()
        .expect("generic variant MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::Aggregate { field_types, .. }
                if field_types == &vec![Type::Primitive(PrimitiveType::I32)]))
    )));
}

#[test]
fn restores_shadowed_locals_and_ends_nested_block_storage() {
    let program = source_program(
        r"
function shadow(): i32 {
  const value: i32 = 4;
  { const value: bool = true; value; }
  return value;
}
",
    );
    let body = lower_mir(&program).pop().expect("shadowing MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let outer = body
        .locals
        .iter()
        .position(|local| {
            local.name.as_deref() == Some("value")
                && local.ty == Type::Primitive(PrimitiveType::I32)
        })
        .expect("outer value");
    let inner = body
        .locals
        .iter()
        .position(|local| {
            local.name.as_deref() == Some("value")
                && local.ty == Type::Primitive(PrimitiveType::Bool)
        })
        .expect("inner value");
    assert!(body.blocks.iter().any(|block| matches!(
        block.terminator.kind,
        TerminatorKind::Return(Some(Operand::Copy(Place { local, .. })))
            if local.0 as usize == outer
    )));
    assert!(
        body.blocks
            .iter()
            .any(|block| block.statements.iter().any(|statement| matches!(
                statement.kind,
                StatementKind::StorageDead(local) if local.0 as usize == inner
            )))
    );
}

#[test]
fn lowers_async_creation_and_typed_completion_as_distinct_edges() {
    let program = source_program(
        r"
class Failure {}
async function ready(): Promise<i32, never> { return 1; }
async function fail(): Promise<i32, Failure> { return 2; }
async function consume(): Promise<i32, Failure> {
  const first: i32 = await ready();
  const pending = fail();
  const second: i32 = try await pending;
  return first + second;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("pending"))
        })
        .expect("async completion MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| matches!(
        &block.terminator.kind,
        TerminatorKind::Call {
            function: Operand::Constant(tn_mir::Constant::Function(_, Type::Function(signature))),
            error: None,
            error_destination: None,
            ..
        } if signature.is_async && !signature.effects.is_empty()
    )));
    assert_eq!(
        body.blocks
            .iter()
            .filter(|block| matches!(block.terminator.kind, TerminatorKind::Suspend { .. }))
            .count(),
        2
    );
    assert!(body.blocks.iter().any(|block| matches!(
        &block.terminator.kind,
        TerminatorKind::Suspend {
            destination: Some(_),
            error_destination: Some(_),
            error: Some(_),
            ..
        }
    )));
}

#[test]
fn lowers_generic_concrete_methods_as_bound_direct_calls() {
    let program = source_program(
        r"
struct Boxed<T> {
  public value: T;
  public get(): T { return this.value; }
}
function read(boxed: Boxed<i32>): i32 { return boxed.get(); }
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("boxed"))
        })
        .expect("direct method MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| block.statements.iter().any(
        |statement| matches!(&statement.kind, StatementKind::Assign(_, value)
            if matches!(value.as_ref(), Rvalue::DirectMethod {
                receiver: tn_hir::ReceiverMode::Shared,
                ty: Type::Function(signature),
                ..
            } if signature.result.as_ref() == &Type::Primitive(PrimitiveType::I32)))
    )));
}

#[test]
fn move_receiver_consumes_the_concrete_method_owner() {
    let program = source_program(
        r"
struct Ticket { public move consume(): void {} }
function invalid(ticket: Ticket): void {
  ticket.consume();
  ticket;
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| {
            body.locals
                .iter()
                .any(|local| local.name.as_deref() == Some("ticket"))
        })
        .expect("move receiver MIR");
    let facts = tn_typecheck::derive_ownership_facts(&program);
    let result = tn_typecheck::check_ownership(&body, &facts);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.condition.as_str() == "OWNERSHIP_USE_AFTER_MOVE" })
    );
}

#[test]
fn method_receiver_reborrows_remain_live_through_argument_evaluation() {
    let program = source_program(
        r"
struct Item { public mut touch(other: &mut Item): void {} }
function invalid(item: &mut Item): void {
  item.touch(item);
}
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| body.member.is_none())
        .expect("receiver reborrow MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    let result =
        tn_typecheck::check_ownership(&body, &tn_typecheck::derive_ownership_facts(&program));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.condition.as_str() == "OWNERSHIP_WRITE_DURING_BORROW" })
    );
}

#[test]
fn lowers_static_methods_without_a_runtime_receiver() {
    let program = source_program(
        r"
class Math {
  public static twice(value: i32): i32 { return value + value; }
}
function calculate(): i32 { return Math.twice(6); }
",
    );
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let body = tn_typecheck::lower_mir(&program, &checked.bodies)
        .into_iter()
        .find(|body| body.member.is_none())
        .expect("static method MIR");
    tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    assert!(body.blocks.iter().any(|block| matches!(
        &block.terminator.kind,
        TerminatorKind::Call {
            function: Operand::Constant(tn_mir::Constant::Method { .. }),
            arguments,
            ..
        } if matches!(arguments.as_slice(), [Operand::Constant(tn_mir::Constant::Integer { value: 6, .. })])
    )));
}
