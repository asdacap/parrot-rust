use crate::provider_error::Error;
use getrandom::getrandom;

/// Identifier a provider uses to group one agent session's requests.
/// Parrot assigns no meaning to it beyond reuse across a user-session resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSessionId(String);

impl ProviderSessionId {
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0_u8; 16];
        getrandom(&mut bytes)
            .map_err(|_| Error::Provider("provider: cannot generate session ID".into()))?;
        Ok(Self(encode_hex(&bytes)))
    }
    pub fn try_parse(value: &str) -> Option<Self> {
        (value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
        .then(|| Self(value.to_owned()))
    }
    pub fn value(&self) -> &str {
        &self.0
    }
}

/// Stores provider session ids so a resumed user session reuses the id a provider already saw.
pub trait ProviderSessionIds: Send + Sync {
    /// Returns the persisted id for one agent session and provider, or none when absent or invalid.
    fn stored(&self, agent_session_id: &str, provider_id: &str) -> Option<ProviderSessionId>;
    /// Persists the id for one agent session and provider; failures degrade to a memory-only id.
    fn record(&self, agent_session_id: &str, provider_id: &str, session_id: &ProviderSessionId);
}

fn encode_hex(bytes: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push(char::from(HEX[usize::from(byte >> 4)]));
        id.push(char::from(HEX[usize::from(byte & 15)]));
    }
    id
}
