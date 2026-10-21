use parrot_host::grpc::token::TransportToken;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn bearer_requires_exact_lowercase_token_and_ordinal_prefix() {
    let token = TransportToken::from_hex(&HEX.to_uppercase()).unwrap();
    assert_eq!(token.bearer(), HEX);
    assert!(token.matches(HEX));
    assert!(token.authenticate(Some(&format!("Bearer {HEX}"))));
    for value in [
        None,
        Some(""),
        Some("Bearer"),
        Some("Bearer "),
        Some("Basic ignored"),
    ] {
        assert!(!token.authenticate(value));
    }
    for value in [
        format!("bearer {HEX}"),
        format!("BEARER {HEX}"),
        format!(" Bearer {HEX}"),
        format!("Bearer {HEX} "),
        format!("Bearer {}", HEX.to_uppercase()),
    ] {
        assert!(!token.authenticate(Some(&value)));
    }
    assert!(!token.matches(&"0".repeat(64)));
    assert!(!token.matches(&"é".repeat(64)));
    assert_eq!(format!("{token:?}"), "TransportToken([REDACTED])");
}

#[test]
fn token_generation_is_32_random_bytes_rendered_as_64_lower_hex() {
    let first = TransportToken::generate().unwrap();
    let second = TransportToken::generate().unwrap();
    for value in [first.bearer(), second.bearer()] {
        assert_eq!(value.len(), 64);
        assert!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert!(TransportToken::from_hex(&value).unwrap().matches(&value));
    }
    assert_ne!(first.bearer(), second.bearer());
}

#[test]
fn invalid_token_encodings_are_rejected() {
    for value in [
        "".to_owned(),
        "0".repeat(63),
        "0".repeat(65),
        "g".repeat(64),
        format!(" {HEX}"),
    ] {
        assert!(TransportToken::from_hex(&value).is_err());
    }
}

#[test]
fn token_load_trims_content_and_rejects_non_owner_permissions() {
    let file = TokenFile::new();
    fs::write(&file.0, format!("\n {} \n", HEX.to_uppercase())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o600, 0o400] {
            fs::set_permissions(&file.0, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(TransportToken::load(&file.0).unwrap().bearer(), HEX);
        }
        for mode in [0o644, 0o660, 0o700, 0o1600] {
            fs::set_permissions(&file.0, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                TransportToken::load(&file.0).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        fs::set_permissions(&file.0, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(&file.0, format!("\u{feff}{HEX}\n")).unwrap();
    assert_eq!(TransportToken::load(&file.0).unwrap().bearer(), HEX);
    fs::write(&file.0, "not a token").unwrap();
    assert!(TransportToken::load(&file.0).is_err());
}

struct TokenFile(PathBuf);
impl TokenFile {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "parrot-t63-token-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
