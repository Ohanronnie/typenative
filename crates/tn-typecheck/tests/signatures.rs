fn signature_conditions(source: &str) -> Vec<String> {
    let directory = tempfile::tempdir().expect("temporary signature fixture");
    let standard_library = directory.path().join("std");
    std::fs::create_dir(&standard_library).expect("create standard library fixture");
    let path = directory.path().join("main.tn");
    std::fs::write(&path, source).expect("write signature fixture");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load signature graph");
    let program = tn_hir::lower_program(graph).expect("lower signature fixture");
    tn_typecheck::check_signatures(&program)
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

#[test]
fn enforces_class_method_and_override_substitutability_rules() {
    let diagnostics = signature_conditions(
        r"
class Animal {}
class Dog extends Animal {}
class Parent {
  public final closed(): Animal { return new Animal(); }
  public safe(): Animal { return new Animal(); }
}
class Child extends Parent {
  public override closed(): Dog { return new Dog(); }
  public override unsafe safe(): Dog { return new Dog(); }
}
class Concrete {
  public abstract absent(): void;
  public bodyless(): void;
  public generic<T>(): T { return undefined; }
}
abstract class AbstractOwner {
  public abstract implemented(): void {}
}
",
    );
    assert!(diagnostics.contains(&"TYPE_OVERRIDE_FINAL_METHOD".into()));
    assert!(diagnostics.contains(&"TYPE_INVALID_OVERRIDE_SIGNATURE".into()));
    assert!(diagnostics.contains(&"TYPE_ABSTRACT_METHOD_IN_CONCRETE_CLASS".into()));
    assert!(diagnostics.contains(&"TYPE_CONCRETE_METHOD_MISSING_BODY".into()));
    assert!(diagnostics.contains(&"TYPE_GENERIC_VIRTUAL_METHOD_EXCLUDED".into()));
    assert!(diagnostics.contains(&"TYPE_ABSTRACT_METHOD_HAS_BODY".into()));
}

#[test]
fn validates_constructor_synthesis_super_order_and_field_initialization() {
    let diagnostics = signature_conditions(
        r"
class NeedsConstructor { private value: i32; }
class Built {
  private value: i32;
  public constructor(value: i32) { this.value = value; }
}

class Base { protected constructor() {} }
class BadDerived extends Base {
  private value: i32;
  public constructor() { this.value = 1; super(); }
}
class Incomplete {
  private value: i32;
  public constructor() {}
}
",
    );
    assert!(diagnostics.contains(&"TYPE_CONSTRUCTOR_REQUIRED".into()));
    assert!(diagnostics.contains(&"TYPE_INVALID_SUPER_CONSTRUCTOR_CALL".into()));
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_UNINITIALIZED_CLASS_FIELD")
            .count(),
        1
    );
}

#[test]
fn validates_generic_arity_namespaces_and_interface_kinds() {
    let diagnostics = signature_conditions(
        r"
interface Pair<T, lifetime a> {}
struct Plain {}
class NotAnInterface {}
class WrongConformance implements NotAnInterface {}
type TooFew = Pair<i32>;
type WrongOrder = Pair<static, i32>;
",
    );
    assert!(diagnostics.contains(&"TYPE_CONFORMANCE_TARGET_NOT_INTERFACE".into()));
    assert!(diagnostics.contains(&"TYPE_GENERIC_ARGUMENT_ARITY".into()));
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_GENERIC_ARGUMENT_NAMESPACE")
            .count(),
        2
    );
}

#[test]
fn validates_public_reference_elision_and_lifetime_relations() {
    let diagnostics = signature_conditions(
        r"
interface Marker {}
export function single(value: &i32): &i32 { return value; }
export function explicit<lifetime a>(left: &a i32, right: &i32): &a i32 { return left; }
export function ambiguous(left: &i32, right: &i32): &i32 { return left; }
export function withoutInput(): &i32 { return undefined; }
export function unrelated<lifetime a>(value: &i32): &a i32 { return value; }
export class Owner {
  public receiverBorrow(): &i32 { return undefined; }
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_AMBIGUOUS_ELIDED_OUTPUT_LIFETIME")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(diagnostics.contains(&"TYPE_RETURN_REFERENCE_WITHOUT_INPUT".into()));
    assert!(diagnostics.contains(&"TYPE_UNRELATED_OUTPUT_LIFETIME".into()));
}

#[test]
fn rejects_invalid_enum_discriminant_layouts() {
    let diagnostics = signature_conditions(
        r"
enum Mixed { Value(i32), Empty = 1 }
enum Duplicate { First = 2, Second = 2 }
enum ImplicitCollision { Zero, One = 0 }
",
    );
    assert!(diagnostics.contains(&"TYPE_ENUM_PAYLOAD_DISCRIMINANT_MIX".into()));
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_DUPLICATE_ENUM_DISCRIMINANT")
            .count(),
        2,
        "{diagnostics:?}"
    );
}

#[test]
fn checks_constructor_initialization_on_every_control_flow_path() {
    let diagnostics = signature_conditions(
        r"
class Complete {
  private value: i32;
  public constructor(flag: bool) {
    if (flag) { this.value = 1; } else { this.value = 2; }
  }
}

class MissingBranch {
  private value: i32;
  public constructor(flag: bool) {
    if (flag) { this.value = 1; }
  }
}
class EarlyReturn {
  private value: i32;
  public constructor(flag: bool) {
    if (flag) { return; }
    this.value = 1;
  }
}
class EscapesEarly {
  private value: i32;
  public constructor() {
    this.observe();
    this.value = 1;
  }
  private observe(): void {}
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_UNINITIALIZED_CLASS_FIELD")
            .count(),
        2,
        "{diagnostics:?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_SELF_USE_BEFORE_INITIALIZATION")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn validates_consistent_generic_interface_method_substitution() {
    let diagnostics = signature_conditions(
        r#"
interface Source<Item> {
  first(): Item;
  second(): Item;
}
@Conform(Source)
class Good<T> {
  first(): T { return undefined; }
  second(): T { return undefined; }
}
@Conform(Source)
class Inconsistent {
  first(): i32 { return 1i32; }
  second(): string { return "wrong"; }
}
"#,
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_INTERFACE_METHOD_MISMATCH")
            .count(),
        1,
        "{diagnostics:?}"
    );
}

#[test]
fn copy_implementations_require_copy_fields_and_exclude_drop_and_classes() {
    let diagnostics = signature_conditions(
        r"
interface Copy {}
interface Drop { mut drop(): void; }
@Conform(Copy)
struct Scalar { value: i32; }
@Conform(Copy)
struct Text { value: string; }
@Copy
struct DerivedText { value: string; }
@Conform(Copy)
@Conform(Drop)
struct Destroyed { mut drop(): void {} }
@Conform(Copy)
class Owner {}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_INVALID_COPY_IMPLEMENTATION")
            .count(),
        4,
        "{diagnostics:?}"
    );
}

#[test]
fn send_and_sync_implementations_require_an_explicit_unsafe_boundary() {
    let diagnostics = signature_conditions(
        r"
interface Send {}
interface Sync {}
@Conform(Send)
struct Manual {}
@Conform(Sync)
struct SyncManual {}
@Conform(Send, unsafe)
struct Reviewed { public pointer: *mut i32; }
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| {
                condition.as_str() == "TYPE_UNSAFE_MARKER_IMPLEMENTATION_REQUIRES_UNSAFE"
            })
            .count(),
        2,
        "{diagnostics:?}"
    );
}
