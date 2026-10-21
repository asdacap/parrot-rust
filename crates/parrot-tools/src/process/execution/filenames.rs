//! Haikunator 3.0.1's default vocabulary and tokenLength=0 filename shape.
//!
//! Vocabulary: Haikunator by Atrox, Copyright 2018 Atrox, package 3.0.1.
//! Extracted from the actual pinned Haikunator.dll (SHA256
//! f9ebf37fb71478508252b0924d2472140c1cf122b11a6dc78d1c8960ea9699fa).
//! The ordered lists deliberately retain the package's duplicate sun/water.
//! Native random selection is independent of the CLR PRNG; filenames have no
//! uniqueness guarantee. Exclusive file creation and retries belong to callers.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};

use parrot_domain::contracts::filenames::FilenameGenerator;

const ADJECTIVES: &[&str] = &[
    "aged",
    "ancient",
    "autumn",
    "billowing",
    "bitter",
    "black",
    "blue",
    "bold",
    "broad",
    "broken",
    "calm",
    "cold",
    "cool",
    "crimson",
    "curly",
    "damp",
    "dark",
    "dawn",
    "delicate",
    "divine",
    "dry",
    "empty",
    "falling",
    "fancy",
    "flat",
    "floral",
    "fragrant",
    "frosty",
    "gentle",
    "green",
    "hidden",
    "holy",
    "icy",
    "jolly",
    "late",
    "lingering",
    "little",
    "lively",
    "long",
    "lucky",
    "misty",
    "morning",
    "muddy",
    "mute",
    "nameless",
    "noisy",
    "odd",
    "old",
    "orange",
    "patient",
    "plain",
    "polished",
    "proud",
    "purple",
    "quiet",
    "rapid",
    "raspy",
    "red",
    "restless",
    "rough",
    "round",
    "royal",
    "shiny",
    "shrill",
    "shy",
    "silent",
    "small",
    "snowy",
    "soft",
    "solitary",
    "sparkling",
    "spring",
    "square",
    "steep",
    "still",
    "summer",
    "super",
    "sweet",
    "throbbing",
    "tight",
    "tiny",
    "twilight",
    "wandering",
    "weathered",
    "white",
    "wild",
    "winter",
    "wispy",
    "withered",
    "yellow",
    "young",
];
const NOUNS: &[&str] = &[
    "art",
    "band",
    "bar",
    "base",
    "bird",
    "block",
    "boat",
    "bonus",
    "bread",
    "breeze",
    "brook",
    "bush",
    "butterfly",
    "cake",
    "cell",
    "cherry",
    "cloud",
    "credit",
    "darkness",
    "dawn",
    "dew",
    "disk",
    "dream",
    "dust",
    "feather",
    "field",
    "fire",
    "firefly",
    "flower",
    "fog",
    "forest",
    "frog",
    "frost",
    "glade",
    "glitter",
    "grass",
    "hall",
    "hat",
    "haze",
    "heart",
    "hill",
    "king",
    "lab",
    "lake",
    "leaf",
    "limit",
    "math",
    "meadow",
    "mode",
    "moon",
    "morning",
    "mountain",
    "mouse",
    "mud",
    "night",
    "paper",
    "pine",
    "poetry",
    "pond",
    "queen",
    "rain",
    "recipe",
    "resonance",
    "rice",
    "river",
    "salad",
    "scene",
    "sea",
    "shadow",
    "shape",
    "silence",
    "sky",
    "smoke",
    "snow",
    "snowflake",
    "sound",
    "star",
    "sun",
    "sun",
    "sunset",
    "surf",
    "term",
    "thunder",
    "tooth",
    "tree",
    "truth",
    "union",
    "unit",
    "violet",
    "voice",
    "water",
    "water",
    "waterfall",
    "wave",
    "wildflower",
    "wind",
    "wood",
];

#[derive(Default)]
pub struct HaikunatorNameGenerator {
    random: RandomState,
    sequence: AtomicU64,
}

impl FilenameGenerator for HaikunatorNameGenerator {
    fn next_filename(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let adjective = self.random.hash_one((sequence, 0u8)) % ADJECTIVES.len() as u64;
        let noun = self.random.hash_one((sequence, 1u8)) % NOUNS.len() as u64;
        format!(
            "{}-{}-arse.dat",
            ADJECTIVES[adjective as usize], NOUNS[noun as usize]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn pinned_vocabulary_shape_keeps_order_and_duplicate_weights() {
        assert_eq!(ADJECTIVES.len(), 91);
        assert_eq!(NOUNS.len(), 97);
        assert_eq!((ADJECTIVES[0], ADJECTIVES[90]), ("aged", "young"));
        assert_eq!((NOUNS[0], NOUNS[96]), ("art", "wood"));
        assert_eq!(NOUNS.iter().filter(|word| **word == "sun").count(), 2);
        assert_eq!(NOUNS.iter().filter(|word| **word == "water").count(), 2);
    }

    #[test]
    fn actual_generator_produces_only_two_default_words_and_no_token() {
        let names = HaikunatorNameGenerator::default();
        let mut observed = HashSet::new();
        for _ in 0..1000 {
            let filename = names.next_filename();
            let parts: Vec<_> = filename.split('-').collect();
            assert_eq!(parts.len(), 3);
            assert!(ADJECTIVES.contains(&parts[0]));
            assert!(NOUNS.contains(&parts[1]));
            assert_eq!(parts[2], "arse.dat");
            observed.insert(filename);
        }
        assert!(observed.len() > 1);
    }
}
