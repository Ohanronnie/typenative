#![allow(dead_code)]

use std::fs;

fn fuzz_project(source: &[u8]) -> Option<tn_hir::Program> {
    let directory = std::env::temp_dir().join(format!("typenative-fuzz-{}", std::process::id()));
    fs::create_dir_all(&directory).ok()?;
    let entry = directory.join("main.tn");
    fs::write(&entry, source).ok()?;
    let standard_library = directory.join("std");
    fs::create_dir_all(&standard_library).ok()?;
    let graph = tn_hir::load_module_graph(&directory, &entry, &standard_library).ok()?;
    tn_hir::lower_program(graph).ok()
}

pub fn validate_hir_and_mir(source: &[u8]) {
    let Some(program) = fuzz_project(source) else {
        return;
    };
    let ownership = tn_typecheck::derive_ownership_facts(&program);
    let _ = tn_typecheck::check_signatures_with_ownership(&program, &ownership);
    let _ = tn_typecheck::check_source_rules(&program);
    let bodies = tn_typecheck::check_bodies_with_ownership(&program, &ownership);
    if bodies.diagnostics.is_empty() {
        for body in tn_typecheck::lower_mir_with_ownership(&program, &bodies.bodies, &ownership) {
            let _ = tn_mir::validate(&body);
            let _ = tn_typecheck::check_ownership(&body, &ownership);
        }
    }
}

pub fn fixed_node_program(input: &[u8]) -> Option<tn_hir::Program> {
    let marker = input
        .iter()
        .take(256)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let source = format!(
        "@Export(\"fuzzBridge\")\nfunction fuzzBridge(value: i32): i32 {{\n  return value;\n}}\n// {marker}\n"
    );
    fuzz_project(source.as_bytes())
}
