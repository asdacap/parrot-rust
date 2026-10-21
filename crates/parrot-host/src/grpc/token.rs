use std::{fmt, fs, io, path::Path};
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct TransportToken([u8; 32]);

impl TransportToken {
    pub fn generate() -> io::Result<Self> {
        let mut value = [0; 32];
        getrandom::fill(&mut value).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self(value))
    }

    pub fn from_hex(encoded: &str) -> io::Result<Self> {
        if encoded.len() != 64 {
            return Err(invalid_token());
        }
        let mut value = [0; 32];
        for (byte, pair) in value.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            let high = hex_digit(pair[0]).ok_or_else(invalid_token)?;
            let low = hex_digit(pair[1]).ok_or_else(invalid_token)?;
            *byte = (high << 4) | low;
        }
        Ok(Self(value))
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(path)?.permissions().mode() & 0o7777 & !0o600 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "token file must be owner-only (0600)",
                ));
            }
        }
        let encoded = fs::read_to_string(path)?;
        Self::from_hex(encoded.strip_prefix('\u{feff}').unwrap_or(&encoded).trim())
    }

    pub fn bearer(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 15)]));
        }
        value
    }

    pub fn matches(&self, presented: &str) -> bool {
        self.bearer().as_bytes().ct_eq(presented.as_bytes()).into()
    }

    pub fn authenticate(&self, authorization: Option<&str>) -> bool {
        authorization
            .and_then(|header| header.strip_prefix("Bearer "))
            .is_some_and(|presented| self.matches(presented))
    }
}

impl fmt::Debug for TransportToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransportToken([REDACTED])")
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_token() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "transport token must contain 64 hexadecimal characters",
    )
}
