use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_USER_SESSION_ID_LENGTH: usize = 255;
const ADJECTIVES: &[&str] = &[
    "amber",
    "ancient",
    "autumn",
    "bitter",
    "black",
    "blue",
    "bold",
    "brave",
    "bright",
    "broad",
    "calm",
    "cold",
    "cool",
    "crimson",
    "damp",
    "dark",
    "dawn",
    "deep",
    "divine",
    "dry",
    "dusty",
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
    "loud",
    "lucky",
    "misty",
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
    "waterfall",
    "wave",
    "wildflower",
    "wind",
    "wood",
];

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UserSessionId(String);

impl UserSessionId {
    pub fn generate() -> Result<Self, io::Error> {
        let adjective = random_index(ADJECTIVES.len())?;
        let noun = random_index(NOUNS.len())?;
        let mut suffix = [0_u8; 3];
        random_bytes(&mut suffix)?;
        Ok(Self(format!(
            "user-session-{}-{}-{}",
            ADJECTIVES[adjective],
            NOUNS[noun],
            hex(&suffix)
        )))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidUserSessionId> {
        let value = value.into();
        if valid_user_session_id(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidUserSessionId)
        }
    }

    pub fn try_parse(value: &str) -> Option<Self> {
        valid_user_session_id(value).then(|| Self(value.to_owned()))
    }

    pub fn value(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidUserSessionId;

impl fmt::Display for InvalidUserSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Invalid user session id.")
    }
}

impl std::error::Error for InvalidUserSessionId {}

fn valid_user_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_USER_SESSION_ID_LENGTH
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub struct Identifier;

impl Identifier {
    pub fn user_session() -> Result<UserSessionId, io::Error> {
        UserSessionId::generate()
    }

    pub fn agent_session() -> Result<String, io::Error> {
        Self::compose("agent-session")
    }

    pub fn event_id() -> Result<String, io::Error> {
        uuid_v7()
    }

    pub fn message_id() -> Result<String, io::Error> {
        Ok(format!("msg-{}", uuid_v7()?))
    }

    pub fn input_id() -> Result<String, io::Error> {
        Ok(format!("inp-{}", uuid_v7()?))
    }

    pub fn question_request_id() -> Result<String, io::Error> {
        Ok(format!("question-{}", uuid_v7()?))
    }

    pub fn permission_request_id() -> Result<String, io::Error> {
        Ok(format!("permission-{}", uuid_v7()?))
    }

    fn compose(kind: &str) -> Result<String, io::Error> {
        let adjective = random_index(ADJECTIVES.len())?;
        let noun = random_index(NOUNS.len())?;
        let mut suffix = [0_u8; 3];
        random_bytes(&mut suffix)?;
        Ok(format!(
            "{kind}-{}-{}-{}",
            ADJECTIVES[adjective],
            NOUNS[noun],
            hex(&suffix)
        ))
    }
}

fn random_index(length: usize) -> Result<usize, io::Error> {
    let mut bytes = [0_u8; 8];
    random_bytes(&mut bytes)?;
    Ok((u64::from_ne_bytes(bytes) as usize) % length)
}

fn random_bytes(bytes: &mut [u8]) -> Result<(), io::Error> {
    File::open("/dev/urandom")?.read_exact(bytes)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[(byte >> 4) as usize]));
        output.push(char::from(DIGITS[(byte & 15) as usize]));
    }
    output
}

fn uuid_v7() -> Result<String, io::Error> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| io::Error::other(error.to_string()))?
        .as_millis() as u64;
    let mut bytes = [0_u8; 16];
    random_bytes(&mut bytes)?;
    let timestamp = millis.to_be_bytes();
    bytes[..6].copy_from_slice(&timestamp[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(hex(&bytes))
}
