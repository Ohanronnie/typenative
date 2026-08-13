fn checked(source: &str) -> tn_typecheck::BodyCheckResult {
    let directory = tempfile::tempdir().expect("temporary semantic fixture");
    let path = directory.path().join("main.tn");
    let standard_library = directory.path().join("std");
    std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
    std::fs::write(&path, source).expect("write semantic fixture");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load fixture graph");
    let program = tn_hir::lower_program(graph).expect("lower fixture declarations");
    tn_typecheck::check_bodies(&program)
}

fn conditions(source: &str) -> Vec<String> {
    checked(source)
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

fn checked_with_workspace_standard_library(
    source: &str,
) -> (tn_hir::Program, tn_typecheck::BodyCheckResult) {
    let directory = tempfile::tempdir().expect("temporary semantic fixture");
    let path = directory.path().join("main.tn");
    std::fs::write(&path, source).expect("write semantic fixture");
    let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load fixture graph with workspace standard library");
    let program = tn_hir::lower_program(graph).expect("lower fixture declarations");
    let checked = tn_typecheck::check_bodies(&program);
    (program, checked)
}

#[test]
fn resolves_canonical_string_operations_as_declared_members() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { fromStatic } from "std/bytes";
import { ParseIntegerError } from "std/core";
import { Utf8Error } from "std/string";
function canonical(value: string): bool throws Utf8Error | ParseIntegerError {
  const made = string.from("value");
  const decoded = try string.fromUtf8(fromStatic("value"));
  const parsed = try usize.parseAscii(fromStatic("42"));
  const upper = value.toAsciiUppercase();
  const copy = value.clone();
  const view: &str = value.asStr();
  const raw: &[u8] = value.bytes();
  return made === copy || decoded === upper || upper === view || raw[0usize] === 0u8 || parsed === 42usize;
}
"#,
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let declaration = program
        .intrinsic_type_declaration(&tn_hir::Type::String)
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
            [
                "from",
                "fromUtf8",
                "toAsciiUppercase",
                "clone",
                "asStr",
                "bytes",
            ]
            .contains(&method.name.as_str())
        })
        .map(|method| method.id)
        .collect::<std::collections::BTreeSet<_>>();
    let resolved = checked
        .bodies
        .iter()
        .flat_map(|body| &body.expressions)
        .filter_map(|expression| match expression.resolution {
            Some(tn_hir::ResolvedValue::Member(member)) => Some(member),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(expected.len(), 6);
    assert!(expected.is_subset(&resolved));
    let usize_declaration = program
        .intrinsic_type_declaration(&tn_hir::Type::Primitive(tn_hir::PrimitiveType::Usize))
        .expect("declared usize intrinsic");
    let tn_hir::DefinitionData::Struct { methods, .. } = &program
        .definition(usize_declaration)
        .expect("usize definition")
        .data
    else {
        panic!("usize intrinsic must be a struct declaration");
    };
    let parse_ascii = methods
        .iter()
        .find(|method| method.name == "parseAscii")
        .expect("declared parseAscii method");
    assert!(resolved.contains(&parse_ascii.id));
}

#[test]
fn infers_literals_bidirectionally_without_numeric_widening_or_truthiness() {
    let diagnostics = conditions(
        r"
function valid(): i32 {
  const contextual: i32 = 1;
  const inferred = 2;
  return contextual;
}
function invalid(): void {
  const narrow: i32 = 1i64;
  if (1) {}
}
",
    );
    assert_eq!(diagnostics, ["TYPE_MISMATCH", "TYPE_CONDITION_NOT_BOOL"]);
}

#[test]
fn records_contextual_owned_string_conversion_in_hir() {
    let result = checked(
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
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let conversions = result
        .bodies
        .iter()
        .flat_map(|body| &body.expressions)
        .filter(|expression| {
            matches!(
                expression.kind,
                tn_hir::HirExpressionKind::Conversion(
                    tn_hir::HirConversionKind::StringLiteralToOwned
                )
            ) && expression.ty == tn_hir::Type::String
        })
        .count();
    assert_eq!(conversions, 3);
}

#[test]
fn contextually_checks_struct_literals_and_calls() {
    let diagnostics = conditions(
        r"
struct Point {
  public x: i32;
  public y?: i32;
}
struct Box<T> { public value: T; }
function consume(point: Point): i32 { return point.x; }
function valid(): i32 {
  const point: Point = { x: 1 };
  const boxed: Box<i32> = { value: 2 };
  boxed;
  return consume(point);
}
function invalid(): void {
  const missing: Point = {};
  const extraField: Point = { x: 1, z: 2 };
  const wrongGeneric: Box<i32> = { value: true };
  consume(1);
}
",
    );
    assert!(diagnostics.contains(&"TYPE_MISSING_OBJECT_FIELD".into()));
    assert!(diagnostics.contains(&"TYPE_UNKNOWN_OBJECT_FIELD".into()));
    assert!(diagnostics.contains(&"TYPE_CALL_ARGUMENT_MISMATCH".into()));
    assert!(diagnostics.contains(&"TYPE_MISMATCH".into()));
}

#[test]
fn checks_exhaustiveness_raw_pointer_safety_and_await_context() {
    let diagnostics = conditions(
        r"
enum Choice { Left, Right }
function choose(value: Choice): number {
  return switch (value) { case Choice.Left: 1, };
}
function pointer(value: *const i32): i32 { return *value; }
function suspension(pending: Promise<i32, never>): i32 { return await pending; }
",
    );
    assert!(diagnostics.contains(&"TYPE_NON_EXHAUSTIVE_SWITCH".into()));
    assert!(diagnostics.contains(&"TYPE_RAW_POINTER_REQUIRES_UNSAFE".into()));
    assert!(diagnostics.contains(&"TYPE_AWAIT_OUTSIDE_ASYNC".into()));
}

#[test]
fn enforces_closed_sync_and_async_error_effects() {
    let diagnostics = conditions(
        r"
struct Failure {}
struct Other {}
function fail(): i32 throws Failure { return 1; }
function missingTry(): i32 throws Failure { return fail(); }
function undeclared(): i32 { return try fail(); }
async function asyncFail(): Promise<i32, Failure> { return 1; }
async function missingTryAwait(): Promise<i32, Failure> {
  return await asyncFail();
}
async function storedMissingTryAwait(): Promise<i32, Failure> {
  const pending = asyncFail();
  return await pending;
}
function missingCatch(): void {
  try { const value = try fail(); } catch (error: Other) {}
}
",
    );
    assert!(diagnostics.contains(&"TYPE_MISSING_TRY".into()));
    assert!(diagnostics.contains(&"TYPE_UNDECLARED_ERROR_EFFECT".into()));
    assert!(diagnostics.contains(&"TYPE_MISSING_TRY_AWAIT".into()));
    assert!(diagnostics.contains(&"TYPE_MISSING_CATCH".into()));
}

#[test]
fn checks_class_upcasts_abstract_construction_and_member_visibility() {
    let diagnostics = conditions(
        r"
abstract class AbstractThing {}
class Base {
  private secret: i32;
  protected shared: i32;
  public own(): i32 { return this.secret; }
}
class Derived extends Base {
  public inherited(): i32 { return this.shared; }
  public forbidden(): i32 { return this.secret; }
}
function classes(value: Base): void {
  const upcast: Base = new Derived();
  const forbidden = value.secret;
  const abstractValue = new AbstractThing();
}
",
    );
    let inaccessible = diagnostics
        .iter()
        .filter(|condition| condition.as_str() == "TYPE_INACCESSIBLE_MEMBER")
        .count();
    assert_eq!(inaccessible, 2, "{diagnostics:?}");
    assert!(diagnostics.contains(&"TYPE_CONSTRUCTS_ABSTRACT_CLASS".into()));
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
}

#[test]
fn readonly_fields_are_mutable_only_through_their_declaring_type() {
    let diagnostics = conditions(
        r"
class Counter {
  public readonly value: i32;
  public constructor() { this.value = 0; }
  public mut increment(): void { this.value = this.value + 1; }
}
function invalid(counter: Counter): void {
  counter.value = 2;
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_READONLY_FIELD_ASSIGNMENT")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn canonical_set_requires_explicit_equality_and_hash_support() {
    let (_, checked) = checked_with_workspace_standard_library(
        r#"
import { Set } from "std/collections";
struct MissingKeyProtocols {}
function invalid(): void {
  const values = new Set<MissingKeyProtocols>({ capacity: 4usize });
}
"#,
    );
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.condition.as_str() == "TYPE_UNSATISFIED_GENERIC_BOUND"),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn checks_closed_patterns_bindings_guards_and_unreachable_arms() {
    let diagnostics = conditions(
        r"
enum Payload { Value(i32), Empty }
function payload(value: Payload): i32 {
  return switch (value) {
    case Payload.Value(inner): inner,
    case Payload.Empty: 0,
  };
}
function missingBool(value: bool): i32 {
  return switch (value) { case true: 1, };
}
function duplicate(value: bool): i32 {
  return switch (value) { case true: 1, case true: 2, case false: 0, };
}
function infinite(value: i32): i32 {
  return switch (value) { case 1: 1, };
}
function optional(value: i32 | undefined): i32 {
  return switch (value) { case undefined: 0, case present: 1, };
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_NON_EXHAUSTIVE_SWITCH")
            .count(),
        2,
        "{diagnostics:?}"
    );
    assert!(diagnostics.contains(&"TYPE_UNREACHABLE_PATTERN".into()));
    assert!(!diagnostics.contains(&"RESOLVE_UNRESOLVED_VALUE".into()));
}

#[test]
fn types_upcasts_checked_downcasts_instanceof_and_raw_pointer_casts() {
    let diagnostics = conditions(
        r"
interface Marker {}
class Base {}
class Derived extends Base implements Marker {}
function casts(value: Derived, pointer: *const i32): void {
  const upcast: Base = value as Base;
  const downcast: &Derived | undefined = upcast as? Derived;
  const witness: Marker = value as Marker;
  const raw = pointer as *mut i32;
  const classTest = value instanceof Base;
  const invalidTest = 1 instanceof Base;
  const invalidNumeric = 1 as i64;
}
",
    );
    assert!(diagnostics.contains(&"TYPE_RAW_POINTER_CAST_REQUIRES_UNSAFE".into()));
    assert!(diagnostics.contains(&"TYPE_INVALID_INSTANCEOF".into()));
    assert!(diagnostics.contains(&"TYPE_INVALID_CAST".into()));
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
}

#[test]
fn infers_generic_calls_and_checks_declared_interface_bounds() {
    let checked = checked(
        r"
interface Marker {}
@Conform(Marker)
struct Good {}
struct Bad {}
function identity<T>(value: T): T { return value; }
function bounded<T extends Marker>(value: T): T { return value; }
function generics(good: Good, bad: Bad): void {
  const number: i32 = identity(1);
  const accepted: Good = bounded(good);
  const rejected = bounded(bad);
}
",
    );
    let diagnostics = checked
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_UNSATISFIED_GENERIC_BOUND")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(!diagnostics.contains(&"TYPE_CALL_ARGUMENT_MISMATCH".into()));
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
    assert_eq!(checked.monomorphizations.len(), 3);
}

#[test]
fn selects_builtin_and_explicit_operator_interfaces() {
    let diagnostics = conditions(
        r"
interface Add {}
@Conform(Add)
struct Count {}
function constrained<T extends Add>(left: T, right: T): T {
  return left + right;
}
function unconstrained<T>(left: T, right: T): T {
  return left + right;
}
function nominal(left: Count, right: Count): Count {
  return left + right;
}
function invalid(left: bool, right: bool): bool {
  const same = left === right;
  return left + right;
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_OPERATOR_NOT_SUPPORTED")
            .count(),
        2,
        "{diagnostics:?}"
    );
}

#[test]
fn contextually_types_lambda_parameters_and_results() {
    let diagnostics = conditions(
        r"
function apply(transform: (i32) => i32, value: i32): i32 {
  return transform(value);
}
function lambdas(): i32 {
  const contextual = apply((value) => value + 1, 2);
  const annotated = (value: i32): i32 => value;
  const unconstrained = (value) => value;
  return annotated(contextual);
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| {
                condition.as_str() == "TYPE_LAMBDA_PARAMETER_ANNOTATION_REQUIRED"
            })
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(!diagnostics.contains(&"TYPE_CALL_ARGUMENT_MISMATCH".into()));
}

#[test]
fn catch_subtyping_covers_effects_and_detects_shadowed_handlers() {
    let diagnostics = conditions(
        r"
class Failure {}
class SpecificFailure extends Failure {}
function fail(): void throws SpecificFailure {}
function handled(): void {
  try { try fail(); }
  catch (error: Failure) { error; }
  catch (specific: SpecificFailure) { specific; }
}
",
    );
    assert!(diagnostics.contains(&"TYPE_REDUNDANT_CATCH".into()));
    assert!(!diagnostics.contains(&"TYPE_MISSING_CATCH".into()));
    assert!(!diagnostics.contains(&"RESOLVE_UNRESOLVED_VALUE".into()));
}

#[test]
fn requires_nonvoid_results_on_every_control_flow_path() {
    let diagnostics = conditions(
        r"
function complete(flag: bool): i32 {
  if (flag) { return 1; } else { return 2; }
}
function missing(flag: bool): i32 {
  if (flag) { return 1; }
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_MISSING_RETURN")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn infers_for_of_items_from_arrays_slices_and_into_iterator_arguments() {
    let diagnostics = conditions(
        r"
interface Iterator<Item> {
  mut next(): Item | undefined;
}
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
function arrays(values: [i32]): void {
  for (const value of values) { const item: i32 = value; }
}
function bags(values: Bag<i32>): void {
  for (const value of values) { const item: i32 = value; }
}
function invalid(value: i32): void {
  for (const item of value) { item; }
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_NOT_ITERABLE")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
}

#[test]
fn rejects_malformed_iterator_protocol_implementations() {
    let diagnostics = conditions(
        r"
interface Iterator<Item> { mut next(): Item | undefined; }
interface IntoIterator<Item, Iter extends Iterator<Item> > {
  move intoIterator(): Iter;
}
@Conform(IntoIterator)
struct Broken {
  move intoIterator(): Broken { return {}; }
}
function invalid(value: Broken): void {
  for (const item of value) { item; }
}
",
    );
    assert!(
        diagnostics.contains(&"TYPE_INVALID_ITERATOR_PROTOCOL".into()),
        "{diagnostics:?}"
    );
}

#[test]
fn types_complete_optional_postfix_chains_and_rejects_non_optional_receivers() {
    let diagnostics = conditions(
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
function invalid(config: Config): Details | undefined {
  return config?.details;
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_OPTIONAL_CHAIN_NON_OPTIONAL")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(!diagnostics.contains(&"TYPE_NOT_CALLABLE".into()));
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
    assert!(!diagnostics.contains(&"TYPE_UNKNOWN_MEMBER".into()));
}

#[test]
fn reborrows_noncopy_optional_fields_through_borrowed_owners() {
    let result = checked(
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
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let optional_references = result
        .bodies
        .iter()
        .flat_map(|body| &body.expressions)
        .filter_map(|expression| match &expression.ty {
            tn_hir::Type::Optional(inner) => match inner.as_ref() {
                tn_hir::Type::Reference { mutable, .. } => Some(*mutable),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(optional_references.contains(&false));
    assert!(optional_references.contains(&true));
}

#[test]
fn types_template_interpolations_with_display_and_storage_modes() {
    let result = checked(
        r"
interface Display {}
@Conform(Display)
struct Shown { display(): void {} }
struct Hidden {}
function templates(value: i32, shown: Shown, hidden: Hidden): void {
  const first = `value=${value + 1} shown=${shown}`;
  const nested = `outer=${`inner=${value}`}`;
  const invalid = `hidden=${hidden}`;
  first;
  nested;
  invalid;
}
",
    );
    let conditions = result
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        conditions
            .iter()
            .filter(|condition| **condition == "TYPE_TEMPLATE_VALUE_NOT_DISPLAY")
            .count(),
        1,
        "{:?}",
        result.diagnostics
    );
    let body = result
        .bodies
        .iter()
        .find(|body| !body.templates.is_empty())
        .expect("template HIR");
    assert_eq!(body.templates.len(), 4);
    assert!(
        body.templates
            .iter()
            .any(|template| template.parts.iter().any(|part| matches!(
                part,
                tn_hir::HirTemplatePart::Interpolation {
                    storage: tn_hir::HirTemplateStorage::SharedBorrow,
                    ..
                }
            )))
    );
    assert!(
        body.templates
            .iter()
            .any(|template| template.parts.iter().any(|part| matches!(
                part,
                tn_hir::HirTemplatePart::Interpolation {
                    storage: tn_hir::HirTemplateStorage::Owned,
                    ..
                }
            )))
    );
}

#[test]
fn checks_struct_and_record_enum_patterns_with_typed_bindings() {
    let valid = conditions(
        r"
enum Message<T> {
  Record { value: T; code: i32; },
  Pair(T, i32),
  Empty,
}
struct Point { public x: i32; public y: i32; }
function fromMessage(message: Message<i32>): i32 {
  return switch (message) {
    case Message.Record { value, code: _ }: value,
    case Message.Pair(value, _): value,
    case Message.Empty: 0,
  };
}
function fromPoint(point: Point): i32 {
  return switch (point) { case Point { x: value }: value, };
}
",
    );
    assert!(valid.is_empty(), "{valid:?}");

    let invalid = conditions(
        r"
enum Message { Record { value: i32; }, Pair(i32, i32) }
function invalid(message: Message): void {
  switch (message) {
    case Message.Record { missing }: {}
    case Message.Record { value, value }: {}
    case Message.Pair(value): {}
    case _: {}
  };
}
",
    );
    assert!(invalid.contains(&"TYPE_UNKNOWN_PATTERN_FIELD".into()));
    assert!(invalid.contains(&"TYPE_DUPLICATE_PATTERN_FIELD".into()));
    assert!(invalid.contains(&"TYPE_PATTERN_ARITY_MISMATCH".into()));
}

#[test]
fn infers_closure_captures_and_enforces_spawn_and_detach_constraints() {
    let result = checked(
        r"
function spawn(task: () => void): void {}
function detach(task: () => void): void {}
function captures(value: i32, raw: *const i32): void {
  const read = (): void => { value; };
  const mutate = (): void => { value = 1; };
  const moved = move (): void => { value; };
  spawn(move (): void => { value; });
  detach((): void => { value; });
  spawn(move (): void => { raw; });
}
",
    );
    let conditions = result
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str())
        .collect::<Vec<_>>();
    assert!(conditions.contains(&"OWNERSHIP_DETACHED_CAPTURE_NOT_STATIC"));
    assert!(conditions.contains(&"OWNERSHIP_CAPTURE_NOT_THREAD_SAFE"));
    assert!(result.closures.iter().any(|closure| {
        closure
            .captures
            .iter()
            .any(|capture| capture.kind == tn_typecheck::CaptureKind::SharedBorrow)
    }));
    assert!(result.closures.iter().any(|closure| {
        closure
            .captures
            .iter()
            .any(|capture| capture.kind == tn_typecheck::CaptureKind::MutableBorrow)
    }));
    assert!(result.closures.iter().any(|closure| {
        closure.moved
            && closure
                .captures
                .iter()
                .any(|capture| capture.kind == tn_typecheck::CaptureKind::Move)
    }));
}

#[test]
fn materializes_resolved_typed_body_hir_with_stable_origins() {
    let result = checked(
        r"
enum Message<T> { Value(T), Empty }
function compute(input: Message<i32>): i32 {
  const offset: i32 = 1;
  return switch (input) {
    case Message.Value(value): value + offset,
    case Message.Empty: 0,
  };
}
class Owner {
  public first(): void {}
  public second(): void {}
}
",
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(result.bodies.len(), 3);
    let body = result
        .bodies
        .iter()
        .find(|body| body.locals.iter().any(|local| local.name == "offset"))
        .expect("function body HIR");
    assert!(body.locals.iter().any(|local| local.name == "input"));
    assert!(body.locals.iter().any(|local| local.name == "value"));
    assert!(body.expressions.iter().all(|expression| {
        expression.ty != tn_hir::Type::Error && expression.origin.file.ends_with("main.tn")
    }));
    assert!(
        body.expressions.iter().any(|expression| matches!(
            expression.resolution,
            Some(tn_hir::ResolvedValue::Local(_))
        ))
    );
    assert_eq!(body.patterns.len(), 2);
    assert!(body.patterns[0].constructor.is_some());
    assert_eq!(body.patterns[0].bindings.len(), 1);
    assert_eq!(body.roots.len(), 2);
    assert!(
        body.statements
            .iter()
            .enumerate()
            .all(|(index, statement)| {
                statement.id.0 as usize == index
                    && statement
                        .children
                        .iter()
                        .all(|child| child.0 < statement.id.0)
            })
    );
    assert!(
        body.expressions
            .iter()
            .enumerate()
            .all(|(index, expression)| {
                expression.id.0 as usize == index
                    && expression
                        .children
                        .iter()
                        .all(|child| child.0 < expression.id.0)
            })
    );
    assert!(body.expressions.iter().any(|expression| {
        matches!(expression.kind, tn_hir::HirExpressionKind::Binary)
            && expression.children.len() == 2
    }));
    assert!(body.statements.iter().any(|statement| {
        matches!(statement.kind, tn_hir::HirStatementKind::Return)
            && statement.expressions.len() == 1
    }));
    let owners = result
        .bodies
        .iter()
        .filter_map(|body| match body.owner {
            tn_hir::BodyOwner::Member { member, .. } => Some(member),
            tn_hir::BodyOwner::Declaration(_) => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(owners.len(), 2);
}

#[test]
fn distinguishes_static_and_instance_method_qualifiers() {
    let diagnostics = conditions(
        r"
class Dispatch {
  public static create(): Dispatch { return new Dispatch(); }
  public execute(): void {}
}
function invalid(value: Dispatch): void {
  value.create();
  Dispatch.execute();
}
",
    );
    assert!(diagnostics.contains(&"TYPE_STATIC_METHOD_REQUIRES_TYPE".into()));
    assert!(diagnostics.contains(&"TYPE_INSTANCE_METHOD_REQUIRES_VALUE".into()));
}
