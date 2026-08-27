fn checksum() -> u64 {
    const PRIME: u64 = 1_000_000_007;
    let mut numeric = 17_u64;
    let mut graph = 29_u64;
    let mut text = 7_u64;
    for index in 0..500_000_u64 {
        numeric = (numeric * 1_664_525 + index * 1_013_904_223 + 12_345) % PRIME;
        graph = (graph + ((index * 31 + 17) % 9_973) * ((index % 13) + 1)) % PRIME;
        text = (text + match index % 4 {
            0 => 84,
            1 => 78,
            2 => 67,
            _ => 72,
        }) % PRIME;
    }
    (numeric + graph * 31 + text * 131) % PRIME
}

fn main() {
    if checksum() == 899_120_682 {
        println!("checksum=899120682");
    } else {
        println!("checksum=invalid");
        std::process::exit(1);
    }
}
