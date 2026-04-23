//! Diagnostic tool: compare steward-list selection distribution under two
//! retry strategies for a small group. Run with:
//!
//! ```sh
//! cargo run --example selection_distribution
//! ```
//!
//! 4 members, sn=2, group="test".
//! Strategy A: keep epoch=0, vary retry_round 0..5 (in-epoch retry).
//! Strategy B: keep retry_round=0, vary epoch 0..5 (one list per epoch).

use de_mls::core::{ProtocolConfig, StewardList};

fn addr(hex: &str) -> Vec<u8> {
    let trimmed = hex.trim_start_matches("0x");
    (0..trimmed.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&trimmed[i..i + 2], 16).unwrap())
        .collect()
}

fn short(a: &[u8]) -> String {
    format!("{:02x}{:02x}{:02x}…", a[0], a[1], a[2])
}

fn main() {
    let alice = addr("0x90f79bf6eb2c4f870365e785982e1f101e93b906");
    let bob = addr("0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc");
    let charlie = addr("0x70997970c51812dc3a010c7d01b50e0d17dc79c8");
    let dave = addr("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");

    let members = vec![alice.clone(), bob.clone(), charlie.clone(), dave.clone()];
    let labels: std::collections::HashMap<Vec<u8>, &str> = [
        (alice.clone(), "alice"),
        (bob.clone(), "bob  "),
        (charlie.clone(), "charl"),
        (dave.clone(), "dave "),
    ]
    .into_iter()
    .collect();

    let config = ProtocolConfig::new(1, 2).unwrap();
    let group = b"test";
    let rounds = 5;

    println!("\n=== Strategy A: epoch=0, retry_round = 0..{rounds} ===");
    let mut counts_a: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for r in 0..rounds {
        let list = StewardList::generate(0, group, &members, 2, config.clone(), r).unwrap();
        let tags: Vec<&str> = list
            .members()
            .iter()
            .map(|m| *labels.get(m).unwrap())
            .collect();
        println!("  retry {r}: [{}]", tags.join(", "));
        for m in list.members() {
            *counts_a.entry(m.clone()).or_insert(0) += 1;
        }
    }
    println!("  ── count per member ──");
    for m in &members {
        println!(
            "    {} ({}): {}",
            labels.get(m).unwrap(),
            short(m),
            counts_a.get(m).copied().unwrap_or(0)
        );
    }

    println!("\n=== Strategy B: retry_round=0, epoch = 0..{rounds} ===");
    let mut counts_b: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    for e in 0..rounds as u64 {
        let list = StewardList::generate(e, group, &members, 2, config.clone(), 0).unwrap();
        let tags: Vec<&str> = list
            .members()
            .iter()
            .map(|m| *labels.get(m).unwrap())
            .collect();
        println!("  epoch {e}: [{}]", tags.join(", "));
        for m in list.members() {
            *counts_b.entry(m.clone()).or_insert(0) += 1;
        }
    }
    println!("  ── count per member ──");
    for m in &members {
        println!(
            "    {} ({}): {}",
            labels.get(m).unwrap(),
            short(m),
            counts_b.get(m).copied().unwrap_or(0)
        );
    }

    let long_rounds = 100u32;
    println!("\n=== Long-run: retry_round=0, epoch = 0..{long_rounds} ===");
    let mut counts_long: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for e in 0..long_rounds as u64 {
        let list = StewardList::generate(e, group, &members, 2, config.clone(), 0).unwrap();
        for m in list.members() {
            *counts_long.entry(m.clone()).or_insert(0) += 1;
        }
    }
    println!("  expected per member: {long_rounds} * 2/4 = 50");
    for m in &members {
        println!(
            "    {} ({}): {}",
            labels.get(m).unwrap(),
            short(m),
            counts_long.get(m).copied().unwrap_or(0)
        );
    }

    println!("\n=== Long-run: epoch=0, retry_round = 0..{long_rounds} ===");
    let mut counts_long_r: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for r in 0..long_rounds {
        let list = StewardList::generate(0, group, &members, 2, config.clone(), r).unwrap();
        for m in list.members() {
            *counts_long_r.entry(m.clone()).or_insert(0) += 1;
        }
    }
    println!("  expected per member: {long_rounds} * 2/4 = 50");
    for m in &members {
        println!(
            "    {} ({}): {}",
            labels.get(m).unwrap(),
            short(m),
            counts_long_r.get(m).copied().unwrap_or(0)
        );
    }
}
