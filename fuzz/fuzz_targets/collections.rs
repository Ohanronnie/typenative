#![no_main]

use std::collections::{HashMap, HashSet};

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let mut values = HashMap::<u8, u32>::new();
    let mut order = Vec::<u8>::new();
    let mut seen = HashSet::<u8>::new();
    for operation in bytes.chunks_exact(3) {
        let key = operation[1] % 16;
        let value = u32::from(operation[2]);
        match operation[0] % 5 {
            0 => {
                if values.insert(key, value).is_none() {
                    order.push(key);
                    assert!(seen.insert(key));
                }
            }
            1 => {
                let removed = values.remove(&key);
                if removed.is_some() {
                    order.retain(|candidate| *candidate != key);
                    assert!(seen.remove(&key));
                }
            }
            2 => {
                let present = seen.contains(&key);
                assert_eq!(values.contains_key(&key), present);
                if let Some(value) = values.get(&key) {
                    assert!(*value <= u32::from(u8::MAX));
                }
            }
            3 => {
                values.clear();
                order.clear();
                seen.clear();
            }
            _ => assert_eq!(values.contains_key(&key), seen.contains(&key)),
        }
        assert_eq!(values.len(), order.len());
        assert_eq!(values.len(), seen.len());
        assert!(order.windows(2).all(|pair| pair[0] != pair[1]));
        assert!(order.iter().all(|key| values.contains_key(key)));
    }
});
