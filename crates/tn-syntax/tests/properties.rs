use proptest::prelude::*;

fn expression() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("0i32".to_owned()),
        Just("true".to_owned()),
        Just("value".to_owned()),
        "v[a-z0-9]{0,7}".prop_map(|identifier| identifier),
    ];
    leaf.prop_recursive(8, 256, 8, |inner| {
        prop_oneof![
            inner.clone().prop_map(|value| format!("({value})")),
            prop::collection::vec(inner.clone(), 0..5)
                .prop_map(|values| format!("[{}]", values.join(", "))),
            (inner.clone(), inner.clone()).prop_map(|(left, right)| format!("({left} + {right})")),
            prop::collection::vec(inner, 0..5)
                .prop_map(|arguments| format!("call({})", arguments.join(", "))),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn balanced_expression_trees_parse_without_panicking(expression in expression()) {
        let source = format!("function main(): void {{ const result = {expression}; }}\n");
        let parsed = tn_syntax::parse("property.tn", source.as_bytes());
        prop_assert!(parsed.is_success(), "{:#?}\n{source}", parsed.diagnostics());
        prop_assert_eq!(parsed.syntax().to_string(), source);
    }

    #[test]
    fn arbitrary_input_always_produces_a_finite_lossless_tree(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let parsed = tn_syntax::parse("arbitrary.tn", &bytes);
        if let Ok(source) = std::str::from_utf8(&bytes) {
            prop_assert_eq!(parsed.syntax().to_string(), source);
        } else {
            prop_assert!(!parsed.is_success());
        }
    }
}
