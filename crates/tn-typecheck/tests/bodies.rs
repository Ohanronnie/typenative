#![allow(clippy::cast_possible_truncation, clippy::needless_raw_string_hashes)]

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

fn checked_tnx(source: &str) -> (tn_hir::Program, tn_typecheck::BodyCheckResult) {
    let directory = tempfile::tempdir().expect("temporary JSX fixture");
    let path = directory.path().join("main.tnx");
    let standard_library = directory.path().join("std");
    std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
    std::fs::write(&path, source).expect("write JSX fixture");
    std::fs::write(
        directory.path().join("tnx-runtime.tn"),
        "export struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
    )
    .expect("write JSX runtime fixture");
    let graph = tn_hir::load_module_graph_with_jsx_runtime(
        directory.path(),
        &path,
        &standard_library,
        Some("./tnx-runtime".into()),
    )
    .expect("load JSX fixture graph");
    let program = tn_hir::lower_program(graph).expect("lower JSX fixture declarations");
    let checked = tn_typecheck::check_bodies(&program);
    (program, checked)
}

fn conditions(source: &str) -> Vec<String> {
    checked(source)
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

fn conditions_tnx(source: &str) -> Vec<String> {
    checked_tnx(source)
        .1
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.condition.as_str().to_owned())
        .collect()
}

#[test]
fn supports_general_destructured_parameter_bindings() {
    let source = r#"
struct ProfileProps {
  public name: &str;
  public enabled: bool;
}
function profile({ name, enabled = true, ...rest }: ProfileProps): void {
  name;
  enabled;
  rest;
}
function nested([first, [second, third]]: (i32, (i32, i32))): void {
  first;
  second;
  third;
}
"#;
    let result = checked(source);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let profile = result
        .bodies
        .iter()
        .find(|body| body.locals.iter().any(|local| local.name == "name"))
        .expect("profile body");
    assert!(profile.locals.iter().any(|local| local.name == "enabled"));
    assert!(profile.locals.iter().any(|local| local.name == "rest"));
    assert_eq!(profile.binding_patterns.len(), 1);
    assert!(profile.binding_patterns[0].bindings.iter().any(|binding| {
        binding
            .projection
            .iter()
            .any(|projection| matches!(projection, tn_hir::HirPatternProjection::Rest { .. }))
    }));
    assert!(
        profile.binding_patterns[0]
            .bindings
            .iter()
            .any(|binding| binding.default.is_some())
    );
    let default_start = source.find("true").expect("parameter default");
    assert!(profile.expressions.iter().any(|expression| {
        expression.origin.byte_start == default_start as u32
            && expression.origin.byte_end == (default_start + 4) as u32
    }));
    let nested = result
        .bodies
        .iter()
        .find(|body| body.locals.iter().any(|local| local.name == "second"))
        .expect("nested body");
    assert_eq!(nested.binding_patterns.len(), 1);
    assert!(
        nested.binding_patterns[0]
            .bindings
            .iter()
            .any(|binding| binding.projection.len() == 2)
    );
}

#[test]
fn supports_destructured_local_bindings() {
    let result = checked(
        r#"
struct Pair {
  public first: i32;
  public second: i32;
}

function object_local(input: Pair): i32 {
  const { first, second: alias } = input;
  return first + alias;
}
function array_local(input: (i32, i32)): i32 {
  let [left, right] = input;
  return left + right;
}
function mutable_array_local(input: (i32, i32)): i32 {
  const mut [left, right] = input;
  left = right;
  return left;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert!(result.bodies.iter().any(|body| {
        body.binding_patterns.iter().any(|pattern| {
            pattern.bindings.iter().any(|binding| {
                binding
                    .projection
                    .contains(&tn_hir::HirPatternProjection::Field(1))
            })
        })
    }));
    assert!(result.bodies.iter().any(|body| {
        body.binding_patterns.iter().any(|pattern| {
            pattern.bindings.iter().any(|binding| {
                binding
                    .projection
                    .contains(&tn_hir::HirPatternProjection::Index(1))
            })
        })
    }));
}

#[test]
fn checks_destructuring_defaults_and_tracks_their_spans() {
    let result = checked(
        r#"
function defaults(input: (i32, bool)): i32 {
  const [number = 1, flag = 2] = input;
  return number;
}
"#,
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.condition.as_str() == "TYPE_MISMATCH")
    );
    let body = result
        .bodies
        .iter()
        .find(|body| body.locals.iter().any(|local| local.name == "number"))
        .expect("destructuring default body");
    assert!(
        body.binding_patterns[0]
            .bindings
            .iter()
            .any(|binding| binding.default.is_some())
    );
}

#[test]
fn lowers_optional_destructuring_defaults_through_control_flow() {
    let (program, result) = {
        let directory = tempfile::tempdir().expect("temporary optional default fixture");
        let path = directory.path().join("main.tn");
        let standard_library = directory.path().join("std");
        std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
        std::fs::write(
            &path,
            r#"
struct Config {
  public enabled?: bool;
}
function main(input: Config): bool {
  const { enabled = true } = input;
  return enabled;
}
"#,
        )
        .expect("write optional default fixture");
        let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
            .expect("load optional default graph");
        let program = tn_hir::lower_program(graph).expect("lower optional default declarations");
        let result = tn_typecheck::check_bodies(&program);
        (program, result)
    };
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &result.bodies);
    let body = bodies
        .iter()
        .find(|body| body.return_type == tn_hir::Type::Primitive(tn_hir::PrimitiveType::Bool))
        .expect("optional default body");
    assert!(
        body.blocks.iter().any(|block| {
            matches!(block.terminator.kind, tn_mir::TerminatorKind::Switch { .. })
        })
    );
    tn_mir::validate(body).expect("optional default MIR");
}

#[test]
fn lowers_parameter_destructuring_defaults_from_the_function_header() {
    let (program, result) = {
        let directory = tempfile::tempdir().expect("temporary parameter default fixture");
        let path = directory.path().join("main.tn");
        let standard_library = directory.path().join("std");
        std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
        std::fs::write(
            &path,
            r#"
struct Config {
  public enabled?: bool;
}
function main({ enabled = true }: Config): bool {
  return enabled;
}
"#,
        )
        .expect("write parameter default fixture");
        let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
            .expect("load parameter default graph");
        let program = tn_hir::lower_program(graph).expect("lower parameter default declarations");
        let result = tn_typecheck::check_bodies(&program);
        (program, result)
    };
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let main = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("main"))
        .expect("main declaration")
        .id;
    let body = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == main)
        .expect("parameter default MIR");
    assert!(
        body.blocks
            .iter()
            .any(|block| matches!(block.terminator.kind, tn_mir::TerminatorKind::Switch { .. }))
    );
    assert!(body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                tn_mir::StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), tn_mir::Rvalue::Use(tn_mir::Operand::Constant(tn_mir::Constant::Bool(true))))
            )
        })
    }));
    tn_mir::validate(&body).expect("parameter default MIR validates");
}

#[test]
fn preserves_parameter_roots_when_patterns_are_mixed_with_plain_parameters() {
    let (program, result) = {
        let directory = tempfile::tempdir().expect("temporary parameter fixture");
        let path = directory.path().join("main.tn");
        let standard_library = directory.path().join("std");
        std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
        std::fs::write(
            &path,
            r#"
struct Pair {
  public first: i32;
}
function mixed({ first }: Pair, value: i32): i32 {
  return first + value;
}
"#,
        )
        .expect("write parameter fixture");
        let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
            .expect("load parameter fixture graph");
        let program = tn_hir::lower_program(graph).expect("lower parameter fixture declarations");
        let result = tn_typecheck::check_bodies(&program);
        (program, result)
    };
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let body = result
        .bodies
        .iter()
        .find(|body| body.parameter_roots.len() == 2)
        .expect("mixed parameter body");
    let mixed = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("mixed"))
        .expect("mixed declaration")
        .id;
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies);
    let lowered = lowered
        .iter()
        .find(|body| body.declaration == mixed)
        .expect("mixed parameter MIR");
    let arguments = lowered.locals.iter().filter(|local| local.argument).count();
    assert_eq!(arguments, body.parameter_roots.len());
}

#[test]
fn supports_destructured_for_bindings_in_hir_and_mir() {
    let (program, result) = {
        let directory = tempfile::tempdir().expect("temporary loop fixture");
        let path = directory.path().join("main.tn");
        let standard_library = directory.path().join("std");
        std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
        std::fs::write(
            &path,
            r#"
struct Pair {
  public first: i32;
  public second: i32;
}
function main(values: [Pair; 1usize]): i32 {
  for (const { first, second: alias } of values) {
    first;
    alias;
  }
  return 0;
}
"#,
        )
        .expect("write loop fixture");
        let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
            .expect("load loop fixture graph");
        let program = tn_hir::lower_program(graph).expect("lower loop fixture declarations");
        let result = tn_typecheck::check_bodies(&program);
        (program, result)
    };
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert!(result.bodies.iter().any(|body| {
        body.binding_patterns.iter().any(|pattern| {
            pattern.bindings.iter().any(|binding| {
                binding
                    .projection
                    .contains(&tn_hir::HirPatternProjection::Field(1))
            })
        })
    }));
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies);
    for body in lowered {
        tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
}

#[test]
fn typechecks_typed_jsx_and_retains_dedicated_hir() {
    let (program, result) = checked_tnx(
        r#"
struct TextProps {
  public value: &str;
}
struct Element {}
function Text(props: TextProps): Element { return new Element(); }
function App(): Element {
  return <Text value="Hello" key="greeting" />;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let body = result
        .bodies
        .iter()
        .find(|body| body.owner == tn_hir::BodyOwner::Declaration(app))
        .expect("App body");
    assert_eq!(body.jsx_elements.len(), 1);
    let element = &body.jsx_elements[0];
    assert!(!element.fragment);
    assert!(element.component.is_some());
    assert!(element.runtime.is_some());
    assert!(element.runtime_signature.as_ref().is_some_and(|signature| {
        signature.generics.is_empty()
            && signature.parameters.len() == 3
            && signature.result.as_ref() == &element.element_type
    }));
    assert!(element.key.is_some());
    assert!(
        element
            .properties
            .iter()
            .any(|property| property.name.as_deref() == Some("value"))
    );
    assert!(
        body.expressions
            .iter()
            .any(|expression| matches!(expression.kind, tn_hir::HirExpressionKind::Jsx(_)))
    );
    let value = element
        .properties
        .iter()
        .find(|property| property.name.as_deref() == Some("value"))
        .expect("value property");
    assert!(matches!(
        &value.value,
        tn_hir::HirJsxValue::Expression(expression)
            if body
                .expressions
                .iter()
                .find(|candidate| candidate.id == *expression)
                .is_some_and(|candidate| candidate.origin.byte_end > candidate.origin.byte_start)
    ));
}

#[test]
fn typechecks_jsx_comments_as_trivia_children() {
    let (program, result) = checked_tnx(
        r#"
struct TextProps {
  public value: &str;
}
struct ViewProps {
  public children: Element;
}
struct Element {}
function Text(props: TextProps): Element { return new Element(); }
function View(props: ViewProps): Element { return new Element(); }
function App(): Element {
  return <View>{/* keep this child boundary */}<Text value="Hello" />{/* keep this closing boundary */}</View>;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let body = result
        .bodies
        .iter()
        .find(|body| body.owner == tn_hir::BodyOwner::Declaration(app))
        .expect("App body");
    assert_eq!(body.jsx_elements.len(), 2);
    assert!(
        body.jsx_elements
            .iter()
            .any(|element| element.children.len() == 1)
    );
}

#[test]
fn validates_jsx_ref_targets_against_the_produced_element_type() {
    let valid = conditions_tnx(
        r#"
struct Element {}
struct Ref<T> { public current: T; }
struct Props {}
function Text(props: Props): Element { return new Element(); }
function valid(reference: Ref<Element>): Element {
  return <Text ref={reference} />;
}
"#,
    );
    assert!(valid.is_empty(), "{valid:?}");

    let invalid = conditions_tnx(
        r#"
struct Element {}
struct Props {}
function Text(props: Props): Element { return new Element(); }
function invalid(): Element {
  return <Text ref={1} />;
}
"#,
    );
    assert!(
        invalid.contains(&"TYPE_JSX_INVALID_REF".into()),
        "{invalid:?}"
    );
}

#[test]
fn resolves_component_member_expressions_for_jsx() {
    let (program, result) = checked_tnx(
        r#"
struct TextProps {
  public value: &str;
}
struct Element {}
class Components {
  public static Text(props: TextProps): Element { return new Element(); }
}
function App(): Element {
  return <Components.Text value="Hello" />;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let body = result
        .bodies
        .iter()
        .find(|body| body.owner == tn_hir::BodyOwner::Declaration(app))
        .expect("App body");
    let component_id = body.jsx_elements[0]
        .component
        .expect("component expression");
    assert!(body.expressions.iter().any(|expression| {
        expression.id == component_id
            && matches!(
                expression.resolution,
                Some(tn_hir::ResolvedValue::Member(_))
            )
    }));
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies);
    let lowered = lowered
        .iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    tn_mir::validate(lowered).expect("member JSX lowers to valid MIR");
}

#[test]
fn lowers_typed_jsx_spreads_in_source_order() {
    let (program, result) = checked_tnx(
        r#"
struct Props {
  public first: i32;
  public second: i32;
}
struct Element {}
function Text(props: Props): Element { return new Element(); }
function App(base: Props): Element {
  return <Text {...base} first={1} />;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    tn_mir::validate(&lowered).expect("spread JSX lowers to valid MIR");
    assert!(lowered.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                tn_mir::StatementKind::Assign(
                    _,
                    value,
                ) if matches!(value.as_ref(), tn_mir::Rvalue::Aggregate { fields, .. } if fields.len() == 2)
            )
        })
    }));
}

#[test]
fn lowers_typed_jsx_children_into_array_props() {
    let (program, result) = checked_tnx(
        r#"
struct Element { public marker: i32; }
struct TextProps {}
struct Props {
  public first: Element;
  public second: Element;
  public children: [Element; 2usize];
}

function Text(props: TextProps): Element { return new Element({ marker: 7 }); }
function View(props: Props): Element {
  return new Element({ marker: props.children[0].marker });
}
function App(): Element {
  const first = <Text />;
  const second = <Text />;
  return <View first={first} second={second}><Text /><Text /></View>;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    let view = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("View"))
        .expect("View declaration")
        .id;
    let view_body = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == view)
        .expect("lowered View body");
    assert!(view_body.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                tn_mir::StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), tn_mir::Rvalue::CheckedIndex { .. })
            )
        })
    }));
    tn_mir::validate(&lowered).expect("JSX children array lowers to valid MIR");
    assert!(lowered.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                tn_mir::StatementKind::Assign(_, value)
                    if matches!(value.as_ref(), tn_mir::Rvalue::Aggregate { ty: tn_hir::Type::Nominal(_, _), fields, .. } if fields.len() == 3)
            )
        })
    }));
}

#[test]
fn lowers_multiple_element_children_into_the_configured_fragment_runtime() {
    let (program, result) = checked_tnx(
        r#"
struct Element { public marker: i32; }
struct TextProps {}
struct ViewProps {
  public children: Element;
}
function Text(props: TextProps): Element { return new Element({ marker: 7 }); }
function View(props: ViewProps): Element { return props.children; }
function App(): Element {
  return <View><Text /><Text /></View>;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    tn_mir::validate(&lowered).expect("multiple JSX children lower to valid MIR");
}

#[test]
fn diagnoses_jsx_property_type_at_the_attribute_value() {
    let source = r#"
struct TextProps {
  public enabled: bool;
}
struct Element {}
function Text(props: TextProps): Element { return new Element(); }
function App(): Element {
  return <Text enabled="yes" />;
}
"#;
    let (_, result) = checked_tnx(source);
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.condition.as_str() == "TYPE_MISMATCH")
        .expect("JSX property type diagnostic");
    let value_start = source.find("\"yes\"").expect("attribute value");
    assert_eq!(diagnostic.primary.span.byte_start, value_start as u32);
    assert_eq!(diagnostic.primary.span.byte_end, (value_start + 5) as u32);
}

#[test]
fn lowers_present_jsx_optional_properties_with_a_present_discriminant() {
    let (program, result) = checked_tnx(
        r#"
struct TextProps {
  public value?: i32;
}
struct Element {}
function Text(props: TextProps): Element { return new Element(); }
function App(): Element {
  return <Text value={1} />;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    assert!(lowered.blocks.iter().any(|block| {
        block.statements.iter().any(|statement| {
            matches!(
                &statement.kind,
                tn_mir::StatementKind::Assign(_, value)
                    if matches!(
                        value.as_ref(),
                        tn_mir::Rvalue::Aggregate {
                            ty: tn_hir::Type::Optional(_),
                            variant: Some(1),
                            ..
                        }
                    )
            )
        })
    }));
    tn_mir::validate(&lowered).expect("present JSX optional property MIR validates");
}

#[test]
fn lowers_jsx_to_an_ordinary_configured_runtime_call() {
    let (program, result) = checked_tnx(
        r#"
struct TextProps {
  public value: &str;
}
struct Element {}
function Text(props: TextProps): Element { return new Element(); }
function App(): Element {
  return <Text value="Hello" />;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let bodies = tn_typecheck::lower_mir(&program, &result.bodies);
    let body = bodies
        .iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    assert!(body.blocks.iter().any(|block| {
        matches!(
            &block.terminator.kind,
            tn_mir::TerminatorKind::Call {
                function: tn_mir::Operand::Constant(tn_mir::Constant::Function(_, _)),
                ..
            }
        )
    }));
    assert!(!body.blocks.iter().flat_map(|block| &block.statements).any(|statement| {
        matches!(
            &statement.kind,
            tn_mir::StatementKind::Assign(_, value)
                if matches!(value.as_ref(), tn_mir::Rvalue::RawOperation { operation, .. } if operation.contains("createElement"))
        )
    }));
    tn_mir::validate(body).expect("JSX lowers to valid ordinary MIR");
}

#[test]
fn specializes_generic_components_for_each_jsx_property_type() {
    let (program, result) = checked_tnx(
        r#"
struct NumberProps {
  public value: i32;
}
struct TextProps {
  public value: &str;
}
struct Element {}
function Generic<P>(props: P): Element { return new Element(); }
function App(): Element {
  const number: NumberProps = { value: 1 };
  const text: TextProps = { value: "hello" };
  const first = <Generic {...number} />;
  const second = <Generic {...text} />;
  first;
  return second;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let generic = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("Generic"))
        .expect("generic component declaration")
        .id;
    let mut specializations = result
        .monomorphizations
        .iter()
        .filter_map(|instance| match instance {
            tn_typecheck::MonomorphizationInstance {
                callable: tn_typecheck::CallableIdentity::Function(declaration),
                arguments,
            } if *declaration == generic => Some(arguments.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    specializations.sort();
    assert_eq!(specializations.len(), 2, "{specializations:?}");
    assert_ne!(specializations[0], specializations[1]);

    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let jsx = program
        .jsx_runtime_declaration("createElement")
        .expect("configured jsx runtime declaration");
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    let runtime_calls = lowered
        .blocks
        .iter()
        .filter_map(|block| match &block.terminator.kind {
            tn_mir::TerminatorKind::Call {
                function:
                    tn_mir::Operand::Constant(tn_mir::Constant::Function(
                        declaration,
                        tn_hir::Type::Function(signature),
                    )),
                ..
            } if *declaration == jsx => Some(signature.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runtime_calls.len(), 2, "{runtime_calls:?}");
    assert!(
        runtime_calls
            .iter()
            .all(|signature| { signature.generics.is_empty() && signature.parameters.len() == 3 })
    );
    assert_ne!(
        runtime_calls[0].parameters[1],
        runtime_calls[1].parameters[1]
    );
    tn_mir::validate(&lowered).expect("generic JSX MIR validates");
}

#[test]
fn lowers_distinct_jsx_property_shapes_to_ordinary_runtime_calls() {
    let (program, result) = checked_tnx(
        r#"
import { Element } from "./tnx-runtime";
struct TextProps {
  public value: &str;
}
struct ViewProps {
  public enabled: bool;
  public children: Element;
}
function Text(props: TextProps): Element { return new Element(); }
function View(props: ViewProps): Element { return props.children; }
function App(): Element {
  return <View enabled={true}><Text value="Hello" /></View>;
}
"#,
    );
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let app = program
        .graph
        .modules
        .iter()
        .flat_map(|module| &module.declarations)
        .find(|declaration| declaration.name.as_deref() == Some("App"))
        .expect("App declaration")
        .id;
    let jsx = program
        .jsx_runtime_declaration("createElement")
        .expect("configured jsx runtime declaration");
    let lowered = tn_typecheck::lower_mir(&program, &result.bodies)
        .into_iter()
        .find(|body| body.declaration == app)
        .expect("lowered App body");
    let runtime_calls = lowered
        .blocks
        .iter()
        .filter_map(|block| match &block.terminator.kind {
            tn_mir::TerminatorKind::Call {
                function:
                    tn_mir::Operand::Constant(tn_mir::Constant::Function(
                        declaration,
                        tn_hir::Type::Function(signature),
                    )),
                ..
            } if *declaration == jsx => Some(signature.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runtime_calls.len(), 2, "{runtime_calls:?}");
    assert!(
        runtime_calls
            .iter()
            .all(|signature| { signature.generics.is_empty() && signature.parameters.len() == 3 })
    );
    assert_ne!(
        runtime_calls[0].parameters[1], runtime_calls[1].parameters[1],
        "the two component property aggregates must retain distinct MIR types"
    );
    tn_mir::validate(&lowered).expect("collision JSX MIR validates");
}

#[test]
fn lowers_destructured_bindings_to_ordinary_mir_projections() {
    let directory = tempfile::tempdir().expect("temporary MIR fixture");
    let path = directory.path().join("main.tn");
    let standard_library = directory.path().join("std");
    std::fs::create_dir(&standard_library).expect("create empty standard library fixture");
    std::fs::write(
        &path,
        r#"
struct Pair {
  public first: i32;
  public second: i32;
}
function main(input: Pair): i32 {
  const { first, second: alias } = input;
  return first + alias;
}
"#,
    )
    .expect("write MIR fixture");
    let graph = tn_hir::load_module_graph(directory.path(), &path, &standard_library)
        .expect("load MIR fixture graph");
    let program = tn_hir::lower_program(graph).expect("lower MIR fixture declarations");
    let checked = tn_typecheck::check_bodies(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let bodies = tn_typecheck::lower_mir(&program, &checked.bodies);
    let body = bodies.first().expect("lowered main body");
    assert!(
        body.blocks
            .iter()
            .flat_map(|block| &block.statements)
            .any(|statement| {
                matches!(
                    statement.kind,
                    tn_mir::StatementKind::Assign(_, ref rvalue)
                        if matches!(**rvalue, tn_mir::Rvalue::Use(tn_mir::Operand::Move(ref place))
                            if place.projection.iter().any(|projection| matches!(
                                projection,
                                tn_mir::Projection::Field { .. }
                            )))
                )
            })
    );
    tn_mir::validate(body).expect("destructuring MIR validates");
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
fn foreign_calls_require_an_unsafe_block() {
    let diagnostics = conditions(
        r#"
declare extern "C" {
  function puts(text: * const u8): void;
}
function main(): void {
  puts("hello" as * const u8);
}
"#,
    );
    assert!(diagnostics.contains(&"TYPE_UNSAFE_CALL_REQUIRES_UNSAFE".into()));
}

#[test]
fn resolves_canonical_string_operations_as_declared_members() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { fromStatic } from "std/bytes";
import { ParseIntegerError } from "std/core";
import { Utf8Error } from "std/string";
function canonical(value: string): bool throws Utf8Error | ParseIntegerError {
  const made = String("value");
  const decoded = try string.fromUtf8(fromStatic("value"));
  const parsed = try usize.parseAscii(fromStatic("42"));
  const upper = value.toAsciiUppercase();
  const canonicalUpper = value.toUpperCase();
  const starts = value.startsWith("val");
  const contains = value.includes("alu");
  const sliced = try value.slice(0usize, 3usize);
  const copy = value.clone();
  const view: &str = value.asStr();
  const raw: &[u8] = value.bytes();
  return made === copy || decoded === upper || canonicalUpper === sliced || starts || contains || upper === view || raw[0usize] === 0u8 || parsed === 42usize;
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
                "fromUtf8",
                "toAsciiUppercase",
                "toUpperCase",
                "startsWith",
                "includes",
                "slice",
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
    assert_eq!(expected.len(), 9);
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
fn managed_bindings_require_the_matching_disposal_protocol() {
    let (_, valid) = checked_with_workspace_standard_library(
        r#"
import { Disposable } from "std/core";
class Managed implements Disposable {
  public [Symbol.dispose](): void {}
}

function valid(): void {
  using resource = new Managed();
}
"#,
    );
    assert!(valid.diagnostics.is_empty(), "{:?}", valid.diagnostics);

    let (_, invalid) = checked_with_workspace_standard_library(
        r"
class Unmanaged {}
function invalid(): void {
  using resource = new Unmanaged();
}
",
    );
    assert!(
        invalid
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.condition.as_str() == "TYPE_USING_REQUIRES_DISPOSABLE"),
        "{:?}",
        invalid.diagnostics
    );
}

#[test]
fn task_values_are_awaitable_and_async_managed_groups_close_on_return() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { TaskGroup } from "std/async";
async function child(): Promise<i32, never> { return 23; }
async function parent(): Promise<i32, never> {
  await using tasks = new TaskGroup();
  const task = tasks.spawn(child());
  return await task;
}
"#,
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let lowered = tn_typecheck::lower_mir(&program, &checked.bodies);
    for body in &lowered {
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
    assert!(lowered.iter().any(|body| {
        body.blocks.iter().any(|block| {
            matches!(
                block.terminator.kind,
                tn_mir::TerminatorKind::Suspend { .. }
            )
        })
    }));
}

#[test]
fn async_managed_bindings_cleanup_on_loop_exit_error_and_cancellation() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { AsyncDisposable } from "std/core";
class Failure {}
class Resource implements AsyncDisposable {
  public async [Symbol.asyncDispose](): Promise<void, never> { return; }
}

async function child(): Promise<void, never> { return; }
async function failing(): Promise<void, Failure> { throw new Failure(); }
async function flow(flag: bool): Promise<void, never> {
  await using resource = new Resource();
  while (flag) { break; }
  await child();
  return;
}

async function errorFlow(): Promise<void, Failure> {
  await using resource = new Resource();
  try { try await failing(); } catch (error: Failure) { return; }
  return;
}

"#,
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let dispose_member = program
        .definitions
        .iter()
        .find_map(|definition| {
            let declaration = program.graph.declaration(definition.declaration)?;
            (declaration.name.as_deref() == Some("Resource")).then(|| match &definition.data {
                tn_hir::DefinitionData::Class { methods, .. } => methods
                    .iter()
                    .find(|method| method.name == "[Symbol.asyncDispose]")
                    .map(|method| method.id),
                _ => None,
            })?
        })
        .expect("resource async disposal member");
    let lowered = tn_typecheck::lower_mir(&program, &checked.bodies);
    for function_name in ["flow", "errorFlow"] {
        let body = lowered
            .iter()
            .find(|body| {
                program
                    .graph
                    .declaration(body.declaration)
                    .and_then(|declaration| declaration.name.as_deref())
                    == Some(function_name)
            })
            .expect("async managed function MIR");
        tn_mir::validate(body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
        let disposal_calls = body
            .blocks
            .iter()
            .flat_map(|block| &block.statements)
            .filter(|statement| {
                matches!(
                    &statement.kind,
                    tn_mir::StatementKind::Assign(_, value)
                        if matches!(value.as_ref(), tn_mir::Rvalue::DirectMethod { member, .. } if *member == dispose_member)
                )
            })
            .count();
        assert!(
            disposal_calls >= 2,
            "{function_name} has no cleanup on every async edge: {disposal_calls}\n{body}"
        );
        assert!(
            body.blocks.iter().any(|block| matches!(
                block.terminator.kind,
                tn_mir::TerminatorKind::Suspend { .. }
            )),
            "{function_name} should retain an await suspension edge"
        );
    }
}

#[test]
fn class_decorated_method_lowers_as_a_direct_callable() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { ClassMethodDecoratorContext } from "std/core";
function logged(method: () => i32, context: ClassMethodDecoratorContext): () => i32 {
  return move() => method() + 1;
}
class Worker {
  @logged
  public run(): i32 { return 41; }
}
function main(): i32 {
  const worker = new Worker();
  return worker.run();
}
"#,
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let lowered = tn_typecheck::lower_mir(&program, &checked.bodies);
    let body = lowered
        .iter()
        .find(|body| {
            program
                .graph
                .declaration(body.declaration)
                .and_then(|declaration| declaration.name.as_deref())
                == Some("main")
        })
        .expect("class decorator main MIR");
    assert!(
        body.blocks
            .iter()
            .flat_map(|block| &block.statements)
            .any(|statement| {
                matches!(
                    &statement.kind,
                    tn_mir::StatementKind::Assign(_, value)
                        if matches!(value.as_ref(), tn_mir::Rvalue::DirectMethod { .. })
                )
            }),
        "class decorated calls should bypass the raw vtable slot\n{body}"
    );
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
fn allows_explicit_float_width_conversions() {
    let diagnostics = conditions(
        r"
function widen(value: f32): f64 { return value as f64; }
function narrow(value: f64): f32 { return value as f32; }
",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn contextually_types_literals_after_explicit_generic_constructor_substitution() {
    let (program, checked) = checked_with_workspace_standard_library(
        r"
class Box<T> {
  private value: T;
  public constructor(value: T) { this.value = value; }
}
function make(): Box<i32> { return new Box<i32>(1); }
",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    for body in tn_typecheck::lower_mir(&program, &checked.bodies) {
        tn_mir::validate(&body).unwrap_or_else(|errors| panic!("{errors:?}\n{body}"));
    }
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
struct Record {
  private secret: i32;
}
class Derived extends Base {
  public inherited(): i32 { return this.shared; }
  public forbidden(): i32 { return this.secret; }
}
function classes(value: Base): void {
  const upcast: Base = new Derived();
  const forbidden = value.secret;
  const item: Record = { secret: 7i32 };
  const itemSecret = item.secret;
  const abstractValue = new AbstractThing();
}
",
    );
    let inaccessible = diagnostics
        .iter()
        .filter(|condition| condition.as_str() == "TYPE_INACCESSIBLE_MEMBER")
        .count();
    assert_eq!(inaccessible, 3, "{diagnostics:?}");
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
  public increment(): void { this.value = this.value + 1; }
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
struct Good implements Marker {}
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
fn propagates_owner_generic_bounds_to_nested_construction() {
    let diagnostics = conditions(
        r"
interface Marker {}
class Boxed<T extends Marker> {
  public constructor() {}
}
class Factory<T extends Marker> {
  make(): Boxed<T> { return new Boxed<T>(); }
}
",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn contextualizes_object_literals_inside_optional_results() {
    let diagnostics = conditions(
        r"
struct Pair { public left: i32; public right: i32; }
function selected(): Pair | undefined {
  return { left: 1i32, right: 2i32 };
}
function contextless(): void {
  const value = { left: 1i32, right: 2i32 };
}
",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| condition.as_str() == "TYPE_OBJECT_LITERAL_REQUIRES_CONTEXT")
            .count(),
        1,
        "{diagnostics:?}"
    );
    assert!(!diagnostics.contains(&"TYPE_MISMATCH".into()));
}

#[test]
fn types_generator_yields_against_declared_iterable_items() {
    let (program, checked) = checked_with_workspace_standard_library(
        r#"
import { AsyncIterable, Iterable } from "std/core";
function* numbers(): Iterable<i32> {
  yield 1i32;
  yield 2i32;
}
async function* events(): AsyncIterable<i32> {
  yield 3i32;
}
function invalid(): void {
  yield 4i32;
}
"#,
    );
    let diagnostics = checked
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics
            .iter()
            .filter(|condition| **condition == "TYPE_YIELD_OUTSIDE_GENERATOR")
            .count(),
        1,
        "{diagnostics:?}"
    );
    let generators = program
        .definitions
        .iter()
        .filter_map(|definition| match &definition.data {
            tn_hir::DefinitionData::Function(function) if function.is_generator => Some(function),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !diagnostics.contains(&"TYPE_INVALID_GENERATOR_RESULT"),
        "{diagnostics:?} {generators:?}"
    );
    assert_eq!(generators.len(), 2);
    assert!(generators.iter().any(|function| function.is_async));
}

#[test]
fn selects_builtin_and_explicit_operator_interfaces() {
    let diagnostics = conditions(
        r"
interface Add {}
struct Count implements Add {}
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
  next(): Item | undefined;
}
interface IntoIterator<Item, Iter extends Iterator<Item> > {
  move intoIterator(): Iter;
}
struct BagIterator<T> implements Iterator<T> { public done: bool;
  next(): T | undefined { return undefined; }
}
struct Bag<T> implements IntoIterator<T, BagIterator<T> > {
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
interface Iterator<Item> { next(): Item | undefined; }
interface IntoIterator<Item, Iter extends Iterator<Item> > {
  move intoIterator(): Iter;
}
struct Broken implements IntoIterator<i32, Broken> {
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
struct Shown implements Display { display(): void {} }
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
