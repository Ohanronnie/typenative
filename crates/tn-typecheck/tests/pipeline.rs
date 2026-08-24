#[test]
fn whole_program_facts_and_attributes_are_reused_by_the_pipeline() {
    let bodies = include_str!("../src/bodies.rs");
    let mir_lowering = include_str!("../src/mir_lower.rs");
    let ownership = include_str!("../src/ownership.rs");

    let body_pipeline = bodies
        .split("pub fn check_bodies_with_ownership")
        .nth(1)
        .expect("ownership-aware body pipeline");
    assert!(
        !body_pipeline.contains("derive_ownership_facts("),
        "body checking must consume the caller's whole-program ownership facts"
    );
    let mir_pipeline = mir_lowering
        .split("pub fn lower_mir_with_ownership")
        .nth(1)
        .expect("ownership-aware MIR pipeline");
    assert!(
        !mir_pipeline.contains("derive_ownership_facts("),
        "MIR lowering must consume the caller's whole-program ownership facts"
    );
    let conformance_query = ownership
        .split("pub(crate) fn declared_conformances")
        .nth(1)
        .expect("HIR conformance query");
    assert!(
        !conformance_query.contains("lex("),
        "ownership queries must read HIR declarations instead of re-lexing source modules"
    );
}
