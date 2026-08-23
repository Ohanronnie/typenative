#![no_main]

#[path = "../support.rs"]
mod support;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Some(program) = support::fixed_node_program(bytes) else {
        return;
    };
    let plan = tn_node_api::build_bridge_plan(&program).expect("fixed Node source is valid");
    assert_eq!(plan.functions.len(), 1);
    assert_eq!(plan.functions[0].export_name, "fuzzBridge");
    assert!(
        plan.classes
            .windows(2)
            .all(|pair| pair[0].export_name <= pair[1].export_name)
    );
});
