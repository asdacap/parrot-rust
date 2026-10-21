use serde::{Deserialize, Serialize};
use std::fmt;

pub const CURRENT_VERSION: i32 = 1;

#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn value(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
    S3,
}

impl<'de> Deserialize<'de> for CredentialType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Tag {
            Name(String),
            Number(u32),
        }
        match Tag::deserialize(deserializer)? {
            Tag::Name(name) => match name.as_str() {
                "api_key" => Ok(Self::ApiKey),
                "oauth" => Ok(Self::OAuth),
                "s3" => Ok(Self::S3),
                _ => Err(serde::de::Error::custom("unknown credential type")),
            },
            Tag::Number(0) => Ok(Self::ApiKey),
            Tag::Number(1) => Ok(Self::OAuth),
            Tag::Number(2) => Ok(Self::S3),
            Tag::Number(_) => Err(serde::de::Error::custom("unknown credential type")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyCredential {
    pub key: Secret,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthCredential {
    pub access_token: Secret,
    pub refresh_token: Secret,
    #[serde(
        deserialize_with = "deserialize_expiry",
        serialize_with = "serialize_expiry"
    )]
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuthCredential {
    pub fn create(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            access_token: Secret::new(access_token),
            refresh_token: Secret::new(refresh_token),
            expires_at: normalize_expiry(expires_at.into()),
            account_id: Some(account_id.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Credential {
    pub access_key: Secret,
    pub secret_key: Secret,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub version: i32,
    #[serde(rename = "type")]
    pub credential_type: CredentialType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKeyCredential>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthCredential>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Credential>,
}

impl Credential {
    pub fn for_api_key(key: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            credential_type: CredentialType::ApiKey,
            api_key: Some(ApiKeyCredential {
                key: Secret::new(key),
            }),
            oauth: None,
            s3: None,
        }
    }
    pub fn for_oauth(value: OAuthCredential) -> Self {
        Self {
            version: CURRENT_VERSION,
            credential_type: CredentialType::OAuth,
            api_key: None,
            oauth: Some(value),
            s3: None,
        }
    }
    pub fn for_s3(access_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            credential_type: CredentialType::S3,
            api_key: None,
            oauth: None,
            s3: Some(S3Credential {
                access_key: Secret::new(access_key),
                secret_key: Secret::new(secret_key),
            }),
        }
    }
    pub fn matches(&self, other: &Self) -> bool {
        if self.credential_type != other.credential_type {
            return false;
        }
        match self.credential_type {
            CredentialType::ApiKey => {
                self.api_key.as_ref().map(|x| &x.key) == other.api_key.as_ref().map(|x| &x.key)
            }
            CredentialType::OAuth => {
                self.oauth.as_ref().map(|x| &x.account_id)
                    == other.oauth.as_ref().map(|x| &x.account_id)
            }
            CredentialType::S3 => {
                self.s3.as_ref().map(|x| (&x.access_key, &x.secret_key))
                    == other.s3.as_ref().map(|x| (&x.access_key, &x.secret_key))
            }
        }
    }
    pub fn validate(&self) -> Result<(), CredentialError> {
        if self.version != CURRENT_VERSION {
            return Err(CredentialError::UnsupportedVersion(self.version));
        }
        let valid = match self.credential_type {
            CredentialType::ApiKey => {
                self.api_key
                    .as_ref()
                    .is_some_and(|x| !x.key.value().is_empty())
                    && self.oauth.is_none()
                    && self.s3.is_none()
            }
            CredentialType::OAuth => {
                self.api_key.is_none()
                    && self.s3.is_none()
                    && self.oauth.as_ref().is_some_and(|x| {
                        !x.access_token.value().is_empty()
                            && !x.refresh_token.value().is_empty()
                            && valid_datetime_offset(&x.expires_at)
                    })
            }
            CredentialType::S3 => {
                self.api_key.is_none()
                    && self.oauth.is_none()
                    && self.s3.as_ref().is_some_and(|x| {
                        !x.access_key.value().trim().is_empty()
                            && !x.secret_key.value().trim().is_empty()
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(CredentialError::Invalid(self.credential_type))
        }
    }
}

fn deserialize_expiry<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    String::deserialize(deserializer).map(normalize_expiry)
}

fn serialize_expiry<S: serde::Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&normalize_expiry(value.to_owned()))
}

fn normalize_expiry(value: String) -> String {
    // Keep invalid inputs intact so validation cannot turn a rejected date into a valid one.
    if !valid_datetime_offset(&value) {
        return value;
    }
    let bytes = value.as_bytes();
    let mut suffix = 19;
    let mut output = value[..19].to_owned();
    if bytes.get(suffix) == Some(&b'.') {
        suffix += 1;
        let start = suffix;
        while bytes.get(suffix).is_some_and(u8::is_ascii_digit) {
            suffix += 1;
        }
        let fraction = value[start..suffix.min(start + 7)].trim_end_matches('0');
        if !fraction.is_empty() {
            output.push('.');
            output.push_str(fraction);
        }
    }
    match &value[suffix..] {
        "Z" | "+00:00" | "-00:00" => output.push_str("+00:00"),
        offset => output.push_str(offset),
    }
    output
}

fn valid_datetime_offset(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() < 20
        || b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || b.get(10) != Some(&b'T')
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return false;
    }
    let digits = |s: usize, e: usize| {
        b.get(s..e)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    if !(digits(0, 4)
        && digits(5, 7)
        && digits(8, 10)
        && digits(11, 13)
        && digits(14, 16)
        && digits(17, 19))
    {
        return false;
    }
    let pair = |offset: usize| u16::from(b[offset] - b'0') * 10 + u16::from(b[offset + 1] - b'0');
    let month = pair(5);
    let day = pair(8);
    let hour = pair(11);
    let minute = pair(14);
    let second = pair(17);
    let year = u32::from(b[0] - b'0') * 1000
        + u32::from(b[1] - b'0') * 100
        + u32::from(b[2] - b'0') * 10
        + u32::from(b[3] - b'0');
    if year == 0 || month == 0 || month > 12 || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > month_days[(month - 1) as usize] {
        return false;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start || i - start > 16 {
            return false;
        }
    }
    let offset_minutes = if b.get(i) == Some(&b'Z') {
        if i + 1 != b.len() {
            return false;
        }
        0
    } else if matches!(b.get(i), Some(b'+') | Some(b'-')) {
        if i + 6 != b.len()
            || b.get(i + 3) != Some(&b':')
            || !b
                .get(i + 1..i + 3)
                .is_some_and(|v| v.iter().all(u8::is_ascii_digit))
            || !b
                .get(i + 4..i + 6)
                .is_some_and(|v| v.iter().all(u8::is_ascii_digit))
        {
            return false;
        }
        let offset_hour = pair(i + 1);
        let offset_minute = pair(i + 4);
        if offset_minute > 59 || offset_hour > 14 || (offset_hour == 14 && offset_minute != 0) {
            return false;
        }
        let magnitude = i64::from(offset_hour * 60 + offset_minute);
        if b[i] == b'+' { magnitude } else { -magnitude }
    } else {
        return false;
    };
    // DateTimeOffset retains at most seven fractional digits (100ns ticks); later
    // digits are discarded by the source JSON parser and cannot affect the instant.
    let fraction_is_zero = b.get(19) != Some(&b'.')
        || b.get(20..i)
            .is_some_and(|digits| digits.iter().take(7).all(|digit| *digit == b'0'));

    let days_before_year = |year: u32| {
        let year = u64::from(year - 1);
        year * 365 + year / 4 - year / 100 + year / 400
    };
    let days_before_month: u32 = month_days[..(month - 1) as usize]
        .iter()
        .map(|days| u32::from(*days))
        .sum();
    let local_days = days_before_year(year) + u64::from(days_before_month + u32::from(day) - 1);
    let local_seconds = (local_days as i64) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    // DateTimeOffset keeps its UTC instant inside the same year 1 through year 9999 range.
    // Positive local offsets move the UTC instant earlier; negative offsets move it later.
    let utc_seconds = local_seconds - offset_minutes * 60;
    let max_utc_seconds = (days_before_year(10_000) as i64) * 86_400 - 1;
    if !(0..=max_utc_seconds).contains(&utc_seconds) {
        return false;
    }
    !(utc_seconds == 0 && fraction_is_zero)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialError {
    UnsupportedVersion(i32),
    Invalid(CredentialType),
    Malformed,
    MalformedStore,
    StoreTooLarge,
    NotFound,
    InvalidPresetName,
}
impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "auth: unsupported credential version {v}"),
            Self::Invalid(t) => write!(
                f,
                "auth: invalid {} credential",
                match t {
                    CredentialType::ApiKey => "api key",
                    CredentialType::OAuth => "oauth",
                    CredentialType::S3 => "s3",
                }
            ),
            Self::Malformed => f.write_str("auth: malformed credential"),
            Self::MalformedStore => f.write_str("auth: malformed credential store"),
            Self::StoreTooLarge => f.write_str("auth: credential store exceeds byte limit"),
            Self::NotFound => f.write_str("auth: credential not found"),
            Self::InvalidPresetName => f.write_str("auth: invalid credential preset name"),
        }
    }
}
impl std::error::Error for CredentialError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_union_and_secret_redaction() {
        let c = Credential::for_api_key("fake-secret");
        c.validate().unwrap();
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"version":1,"type":"api_key","api_key":{"key":"fake-secret"}}"#
        );
        assert!(!format!("{c:?}").contains("fake-secret"));
        assert!(!format!("{}", c.api_key.as_ref().unwrap().key).contains("fake-secret"));
        assert!(serde_json::from_str::<Credential>(r#"{"version":1,"type":"api_key","api_key":{"key":"k"},"s3":{"access_key":"a","secret_key":"s"}}"#).unwrap().validate().is_err());
    }
    #[test]
    fn matching_ignores_rotating_oauth_fields() {
        let a = Credential::for_oauth(OAuthCredential::create(
            "a",
            "r1",
            "2030-01-01T00:00:00+00:00",
            "acct",
        ));
        let b = Credential::for_oauth(OAuthCredential::create(
            "b",
            "r2",
            "2031-01-01T00:00:00Z",
            "acct",
        ));
        assert!(a.matches(&b));
    }
    #[test]
    fn oauth_datetime_offsets_and_year_bounds_match_datetimeoffset() {
        for value in [
            "0001-01-01T01:00:00+00:00",
            "0001-01-01T00:00:00-01:00",
            "0001-01-01T00:02:00+00:01",
            "0001-01-01T00:00:00.0000001+00:00",
            "0001-01-01T01:00:00.0000001+01:00",
            "2030-01-01T00:00:00.1234567890123456+00:00",
            "9999-12-31T23:59:59+00:00",
            "9999-12-31T23:58:59-00:01",
            "9999-12-31T23:59:59+01:00",
            "9999-12-31T23:59:59.9999999+00:00",
            "2030-01-01T00:00:00+14:00",
            "2030-01-01T00:00:00-14:00",
        ] {
            assert!(
                Credential::for_oauth(OAuthCredential::create("a", "b", value, "x"))
                    .validate()
                    .is_ok(),
                "{value}"
            );
        }
        for value in [
            "0000-12-31T23:59:59+00:00",
            "2030-01-01T00:00:00+14:01",
            "2030-01-01T00:00:00-14:01",
            "2030-01-01T00:00:00+99:99",
            "0001-01-01T00:00:00+00:00",
            "0001-01-01T00:00:00+01:00",
            "9999-12-31T23:59:59-01:00",
            "0001-01-01T01:00:00+01:00",
            "0001-01-01T01:00:00.0000000+01:00",
            "0001-01-01T00:00:00.00000001+00:00",
            "0001-01-01T01:00:00.00000001+01:00",
            "0001-01-01T00:00:00.00000010000000000+00:00",
            "2030-01-01T00:00:00.12345678901234567+00:00",
            "0001-01-01T00:00:00+00:01",
            "9999-12-31T23:59:59.9999999-00:01",
        ] {
            assert!(
                Credential::for_oauth(OAuthCredential::create("a", "b", value, "x"))
                    .validate()
                    .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn s3_empty_keys_and_bad_dates_reject() {
        assert!(Credential::for_s3(" ", "s").validate().is_err());
        assert!(
            Credential::for_oauth(OAuthCredential::create(
                "a",
                "b",
                "0001-01-01T00:00:00Z",
                "x"
            ))
            .validate()
            .is_err()
        );
    }
}
