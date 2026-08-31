use std::collections::BTreeMap;
use std::path::Path;
use tn_hir::{DefinitionData, GenericBound, Type, lower_program};
use tn_hir::{ImportClause, load_module_graph, load_module_graph_with_jsx_runtime};

fn write(path: &Path, source: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture directory");
    }
    std::fs::write(path, source).expect("fixture source");
}

#[test]
fn loads_the_intrinsic_string_prelude_without_an_import() {
    let directory = tempfile::tempdir().expect("temporary HIR program");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(&root.join("main.tn"), "function main(): void {}\n");
    write(
        &standard_library.join("string.tn"),
        "struct OwnedString {}\n",
    );
    let graph = load_module_graph(root, &root.join("main.tn"), &standard_library)
        .expect("module graph with string prelude");
    assert!(
        graph
            .modules
            .iter()
            .any(|module| module.path.ends_with("std/string.tn"))
    );
    let program = lower_program(graph).expect("lowered string prelude");
    assert!(program.intrinsic_type_declaration(&Type::String).is_some());
}

#[test]
fn loads_tnx_modules_and_retains_the_configured_jsx_runtime() {
    let directory = tempfile::tempdir().expect("temporary JSX module graph");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(
        &root.join("main.tnx"),
        "import { helper } from \"./helper\";\nfunction main(): void { helper(); }\n",
    );
    write(
        &root.join("helper.tnx"),
        "export function helper(): void {}\n",
    );
    write(
        &root.join("tnx-runtime.tn"),
        "export struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
    );

    let graph = load_module_graph_with_jsx_runtime(
        root,
        &root.join("main.tnx"),
        &standard_library,
        Some("./tnx-runtime".into()),
    )
    .expect(".tnx module graph");
    assert_eq!(graph.jsx_runtime.as_deref(), Some("./tnx-runtime"));
    assert!(graph.jsx_runtime_module.is_some());
    assert!(
        graph
            .modules
            .iter()
            .any(|module| module.path.ends_with("main.tnx"))
    );
    assert!(
        graph
            .modules
            .iter()
            .any(|module| module.path.ends_with("helper.tnx"))
    );
}

#[test]
fn resolves_an_installed_jsx_runtime_as_a_normal_package_module() {
    let directory = tempfile::tempdir().expect("temporary package module graph");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(&root.join("main.tnx"), "function main(): void {}\n");
    write(
        &root.join("node_modules/@typenative/ui/tnx-runtime.tn"),
        "export struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
    );

    let graph = load_module_graph_with_jsx_runtime(
        root,
        &root.join("main.tnx"),
        &standard_library,
        Some("@typenative/ui/tnx-runtime".into()),
    )
    .expect("installed JSX runtime resolves");
    assert!(
        graph
            .module(graph.jsx_runtime_module.expect("runtime module"))
            .expect("runtime graph node")
            .path
            .ends_with(Path::new("node_modules/@typenative/ui/tnx-runtime.tn"))
    );
}

#[test]
fn rejects_cycles_reachable_from_the_configured_jsx_runtime() {
    let directory = tempfile::tempdir().expect("temporary runtime cycle graph");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(&root.join("main.tnx"), "function main(): void {}\n");
    write(
        &root.join("tnx-runtime.tn"),
        "import { helper } from \"./helper\";\nexport struct Element {}\nexport function createElement<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createElements<P, E, K>(component: (P) => E, properties: P, key: K): E { return component(properties); }\nexport function createFragment<C>(children: C): Element { return new Element(); }\n",
    );
    write(
        &root.join("helper.tn"),
        "import { createElement } from \"./tnx-runtime\";\nexport function helper(): void {}\n",
    );

    let error = load_module_graph_with_jsx_runtime(
        root,
        &root.join("main.tnx"),
        &standard_library,
        Some("./tnx-runtime".into()),
    )
    .expect_err("runtime import cycle must be rejected");
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.condition.as_str() == "RESOLVE_JSX_RUNTIME_IMPORT_CYCLE")
    );
}

#[test]
fn reports_a_missing_configured_jsx_runtime_module() {
    let directory = tempfile::tempdir().expect("temporary missing runtime graph");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(&root.join("main.tnx"), "function main(): void {}\n");

    let error = load_module_graph_with_jsx_runtime(
        root,
        &root.join("main.tnx"),
        &standard_library,
        Some("./missing-runtime".into()),
    )
    .expect_err("missing runtime module must be rejected");
    assert!(
        error
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.condition.as_str() == "RESOLVE_JSX_RUNTIME_MODULE")
    );
}

#[test]
fn lowers_resolved_nominal_generic_and_compound_signatures() {
    let directory = tempfile::tempdir().expect("temporary HIR program");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(
        &root.join("main.tn"),
        r"export interface Display {
  display(value: i32): void;
}

export struct Point {
  public x: f64;
  public label?: string;
  display(value: i32): void {}
}
struct Box<T extends Display> {
  public value: T;
}
type MaybePoint = Point | undefined;
function identity<T extends Display>(value: T): T {
  return value;
}
class Base {}
class Derived extends Base implements Display {
  public display(value: i32): void {}
}
",
    );
    let graph =
        load_module_graph(root, &root.join("main.tn"), &standard_library).expect("module graph");
    let program = lower_program(graph).expect("resolved HIR");
    assert_eq!(program.definitions.len(), 7);
    let alias = program
        .definitions
        .iter()
        .find_map(|definition| match &definition.data {
            DefinitionData::TypeAlias(ty) => Some(ty),
            _ => None,
        })
        .expect("type alias");
    assert!(matches!(alias, Type::Optional(_)));
    let function = program
        .definitions
        .iter()
        .find_map(|definition| match &definition.data {
            DefinitionData::Function(function) => Some(function),
            _ => None,
        })
        .expect("function");
    assert_eq!(function.parameters[0].ty, Type::Generic("T".into()));
    assert_eq!(function.result, Type::Generic("T".into()));
    assert_eq!(function.generics.len(), 1);
    assert!(matches!(
        function.generics[0].bounds.as_slice(),
        [GenericBound::Interface(_, _)]
    ));
    let generic_struct = program
        .definitions
        .iter()
        .find(|definition| {
            matches!(definition.data, DefinitionData::Struct { .. })
                && definition.generics.len() == 1
        })
        .expect("generic struct definition");
    assert!(matches!(
        generic_struct.generics[0].bounds.as_slice(),
        [GenericBound::Interface(_, _)]
    ));
    assert_eq!(
        program
            .definitions
            .iter()
            .filter(|definition| matches!(definition.data, DefinitionData::Class { .. }))
            .count(),
        2
    );
}

#[test]
fn open_hierarchy_has_no_closed_dispatch_metadata() {
    let directory = tempfile::tempdir().expect("temporary HIR program");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(
        &root.join("main.tn"),
        "class Closed {}\ninterface Marker {}\n",
    );
    let graph = load_module_graph(root, &root.join("main.tn"), &standard_library)
        .expect("open module graph");
    let program = lower_program(graph).expect("open HIR");
    assert!(
        program
            .definitions
            .iter()
            .any(|definition| matches!(definition.data, DefinitionData::Class { .. }))
    );
    assert!(
        program
            .definitions
            .iter()
            .any(|definition| matches!(definition.data, DefinitionData::Interface { .. }))
    );
}

#[test]
fn resolves_exact_relative_and_standard_modules_across_cycles() {
    let directory = tempfile::tempdir().expect("temporary module graph");
    let root = directory.path();
    let standard_library = root.join("std");
    write(
        &root.join("main.tn"),
        "import { helper as run } from \"./helper\";\nimport { Value } from \"std/core\";\nexport function main(): void {}\n",
    );
    write(
        &root.join("helper.tn"),
        "import { main } from \"./main\";\nexport function helper(): void {}\n",
    );
    write(
        &standard_library.join("core.tn"),
        "export struct Value {}\n",
    );

    let graph = load_module_graph(root, &root.join("main.tn"), &standard_library)
        .expect("valid module graph");
    assert_eq!(graph.modules.len(), 3);
    let entry = graph.module(graph.entry).expect("entry module");
    assert_eq!(entry.imports.len(), 2);
    assert!(matches!(entry.imports[0].clause, ImportClause::Named(_)));
    assert_eq!(
        graph,
        load_module_graph(root, &root.join("main.tn"), &standard_library)
            .expect("repeat graph is deterministic")
    );
}

#[test]
fn module_ids_are_stable_across_workspace_roots() {
    let left_directory = tempfile::tempdir().expect("left temporary module graph");
    let right_directory = tempfile::tempdir().expect("right temporary module graph");
    for root in [left_directory.path(), right_directory.path()] {
        let standard_library = root.join("std");
        write(
            &root.join("main.tn"),
            "import { helper } from \"./helper\";\nimport { Value } from \"std/core\";\nfunction main(): void {}\n",
        );
        write(
            &root.join("helper.tn"),
            "export function helper(): void {}\n",
        );
        write(
            &standard_library.join("core.tn"),
            "export struct Value {}\n",
        );
    }

    let left_root = left_directory
        .path()
        .canonicalize()
        .expect("left canonical root");
    let right_root = right_directory
        .path()
        .canonicalize()
        .expect("right canonical root");
    let load = |root: &Path| {
        load_module_graph(root, &root.join("main.tn"), &root.join("std"))
            .expect("valid module graph")
    };
    let left = load(&left_root);
    let right = load(&right_root);

    let identity = |graph: &tn_hir::ModuleGraph, root: &Path| {
        graph
            .modules
            .iter()
            .map(|module| {
                let key = if let Ok(path) = module.path.strip_prefix(root.join("std")) {
                    format!("std/{}", path.display())
                } else {
                    format!(
                        "project/{}",
                        module.path.strip_prefix(root).unwrap().display()
                    )
                };
                (
                    key,
                    (
                        module.id.0,
                        module
                            .imports
                            .iter()
                            .map(|import| import.target.0)
                            .collect::<Vec<_>>(),
                        module
                            .declarations
                            .iter()
                            .map(|declaration| declaration.id.0)
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };

    assert_eq!(left.entry, right.entry);
    assert_eq!(identity(&left, &left_root), identity(&right, &right_root));
}

#[test]
fn rejects_package_specifiers_missing_exports_and_overload_sets() {
    let directory = tempfile::tempdir().expect("temporary module graph");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(
        &root.join("main.tn"),
        "import { hidden } from \"./helper\";\nimport \"package\";\nfunction duplicate(): void {}\nfunction duplicate(value: i32): void {}\n",
    );
    write(&root.join("helper.tn"), "function hidden(): void {}\n");
    let error = load_module_graph(root, &root.join("main.tn"), &standard_library)
        .expect_err("invalid graph must fail");
    let conditions = error
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.condition.as_str())
        .collect::<Vec<_>>();
    assert!(conditions.contains(&"RESOLVE_INVALID_MODULE_SPECIFIER"));
    assert!(conditions.contains(&"RESOLVE_MISSING_EXPORT"));
    assert!(conditions.contains(&"RESOLVE_DUPLICATE_DECLARATION"));
}

#[test]
fn removed_procedural_collection_apis_are_not_importable() {
    let directory = tempfile::tempdir().expect("temporary module graph");
    let root = directory.path();
    let standard_library = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
    write(
        &root.join("main.tn"),
        "import { dequeAllocate, mapAllocate, orderedMapAllocate, orderedSetAllocate, queueAllocate, setAllocate } from \"std/collections\";\nfunction main(): void {}\n",
    );
    let error = load_module_graph(root, &root.join("main.tn"), &standard_library)
        .expect_err("removed procedural collection APIs must not resolve");
    assert_eq!(
        error
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.condition.as_str() == "RESOLVE_MISSING_EXPORT")
            .count(),
        6
    );
}

#[test]
fn retains_interface_and_lifetime_generic_arguments() {
    let directory = tempfile::tempdir().expect("temporary HIR program");
    let root = directory.path();
    let standard_library = root.join("std");
    std::fs::create_dir(&standard_library).expect("standard-library directory");
    write(
        &root.join("main.tn"),
        r"interface Container<Item> {
  item(): Item;
}
struct Bag<T> implements Container<T> { public value: T;
  item(): T { return this.value; }
}
struct Borrowed<lifetime a, T> { public value: &a T; }
type StaticBorrow = Borrowed<static, i32>;
",
    );
    let graph =
        load_module_graph(root, &root.join("main.tn"), &standard_library).expect("module graph");
    let program = lower_program(graph).expect("resolved HIR");

    let bag = program
        .definitions
        .iter()
        .find_map(|definition| match &definition.data {
            DefinitionData::Struct { methods, .. } if definition.generics.len() == 1 => {
                Some((definition, methods))
            }
            _ => None,
        })
        .expect("generic conformance target");
    assert_eq!(bag.1[0].name, "item");
    let DefinitionData::Struct { interfaces, .. } = &bag.0.data else {
        panic!("conformance target must be a struct");
    };
    assert_eq!(interfaces.len(), 1);
    assert!(matches!(interfaces[0], Type::Nominal(_, ref arguments) if arguments.len() == 1));

    let alias = program
        .definitions
        .iter()
        .find_map(|definition| match &definition.data {
            DefinitionData::TypeAlias(ty) => Some(ty),
            _ => None,
        })
        .expect("lifetime-instantiated alias");
    assert!(matches!(
        alias,
        Type::Nominal(_, arguments)
            if arguments == &[Type::Lifetime("static".into()), Type::Primitive(tn_hir::PrimitiveType::I32)]
    ));
}
