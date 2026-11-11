use std::fmt;

use serde_json::Value;

const CAMPAIGN: &str = include_str!("../fixtures/regressions/T77/campaign.json");

pub fn manifest() -> Value {
    serde_json::from_str(CAMPAIGN).expect("committed T77 campaign manifest must parse")
}

pub fn seeds() -> Vec<u64> {
    manifest()["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            u64::from_str_radix(value.as_str().unwrap().strip_prefix("0x").unwrap(), 16).unwrap()
        })
        .collect()
}

pub fn cases(domain: &str) -> usize {
    manifest()["cases_per_seed"][domain]
        .as_u64()
        .unwrap_or_else(|| panic!("missing committed budget for {domain}")) as usize
}

#[derive(Clone, Copy, Debug)]
pub struct Case {
    pub domain: &'static str,
    pub seed: u64,
    pub index: usize,
}

impl fmt::Display for Case {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "T77 domain={} seed=0x{:016x} case={}",
            self.domain, self.seed, self.index
        )
    }
}

#[path = "generator.rs"]
mod generator;
pub use generator::Generator;

impl Generator {
    pub fn text(&mut self, atoms: usize) -> String {
        const ATOMS: [&str; 12] = [
            "a", "Z", "0", " ", "_", "-", "é", "中", "🦜", "\"", "\\", "\n",
        ];
        (0..atoms).map(|_| ATOMS[self.pick(ATOMS.len())]).collect()
    }

    pub fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.next() as u8).collect()
    }
}

pub fn for_cases(domain: &'static str, mut exercise: impl FnMut(Case, &mut Generator)) {
    let count = cases(domain);
    for seed in seeds() {
        println!("T77_BEGIN domain={domain} seed=0x{seed:016x} cases={count}");
        let mut generator = Generator::new(seed);
        for index in 0..count {
            exercise(
                Case {
                    domain,
                    seed,
                    index,
                },
                &mut generator,
            );
        }
        println!("T77_END domain={domain} seed=0x{seed:016x} completed={count}");
    }
}

pub fn hex(input: &str) -> Vec<u8> {
    assert_eq!(input.len() % 2, 0, "committed hex must have even length");
    input
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16)
                .expect("committed corpus contains valid hexadecimal")
        })
        .collect()
}
