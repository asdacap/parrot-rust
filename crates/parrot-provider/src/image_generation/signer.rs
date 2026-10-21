use super::transport::ImageHttpRequest;
use crate::sessions::Error;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write};

pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Single-chunk S3 AWS Signature Version 4; content headers are not signed.
#[derive(Clone)]
pub struct AwsV4Signer {
    access_key: String,
    secret_key: String,
    region: String,
}
impl AwsV4Signer {
    pub fn new(access_key: String, secret_key: String, region: String) -> Self {
        Self {
            access_key,
            secret_key,
            region,
        }
    }
    pub fn sign(
        &self,
        request: &mut ImageHttpRequest,
        payload_hash: &str,
        amz_date: &str,
    ) -> Result<(), Error> {
        request.endpoint = super::uri::normalize_endpoint(&request.endpoint)?;
        let date = amz_date
            .get(..8)
            .ok_or_else(|| Error::Provider("invalid AWS timestamp".into()))?;
        append(&mut request.headers, "x-amz-date", amz_date);
        append(&mut request.headers, "x-amz-content-sha256", payload_hash);
        let uri = request.endpoint.as_url();
        let mut host = uri.host_str().unwrap_or_default().to_owned();
        if let Some(port) = uri.port() {
            host.push_str(&format!(":{port}"));
        }
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in &request.headers {
            let name = name.to_ascii_lowercase();
            let value = value.trim().to_owned();
            headers
                .entry(name)
                .and_modify(|previous| {
                    previous.push(',');
                    previous.push_str(&value);
                })
                .or_insert(value);
        }
        headers.entry("host".into()).or_insert(host);
        let signed = headers.keys().cloned().collect::<Vec<_>>().join(";");
        let mut canonical_headers = String::new();
        for (name, value) in &headers {
            writeln!(&mut canonical_headers, "{name}:{value}").unwrap();
        }
        let path = uri
            .path()
            .split('/')
            .map(|part| encode(&unescape(part)))
            .collect::<Vec<_>>()
            .join("/");
        let mut query = uri
            .query()
            .unwrap_or_default()
            .split('&')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                (encode(&unescape(key)), encode(&unescape(value)))
            })
            .collect::<Vec<_>>();
        query.sort();
        let query = query
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let canonical = format!(
            "{}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload_hash}",
            request.method
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256(canonical.as_bytes())
        );
        let key = hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let key = hmac(
            &hmac(&hmac(&key, self.region.as_bytes()), b"s3"),
            b"aws4_request",
        );
        let signature = hex(&hmac(&key, to_sign.as_bytes()));
        append(
            &mut request.headers,
            "Authorization",
            &format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
                self.access_key
            ),
        );
        Ok(())
    }
}
fn append(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.push((name.into(), value.into()));
}
pub fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}
fn hex(data: &[u8]) -> String {
    let mut output = String::with_capacity(data.len() * 2);
    for byte in data {
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = if key.len() > 64 {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    let mut inner = [0x36; 64];
    let mut outer = [0x5c; 64];
    for (i, b) in key.iter().enumerate() {
        inner[i] ^= b;
        outer[i] ^= b;
    }
    let mut hash = Sha256::new();
    hash.update(inner);
    hash.update(data);
    let result = hash.finalize();
    let mut hash = Sha256::new();
    hash.update(outer);
    hash.update(result);
    hash.finalize().to_vec()
}
fn encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
fn unescape(value: &str) -> String {
    let mut output = String::new();
    let mut offset = 0;
    while offset < value.len() {
        let start = offset;
        let mut octets = Vec::new();
        while value.as_bytes().get(offset) == Some(&b'%') && offset + 2 < value.len() {
            let pair = &value.as_bytes()[offset + 1..offset + 3];
            let (Some(a), Some(b)) = (
                (pair[0] as char).to_digit(16),
                (pair[1] as char).to_digit(16),
            ) else {
                break;
            };
            octets.push((a * 16 + b) as u8);
            offset += 3;
        }
        if octets.is_empty() {
            let character = value[offset..].chars().next().unwrap();
            output.push(character);
            offset += character.len_utf8();
            continue;
        }
        // Uri.UnescapeDataString keeps invalid UTF-8 percent triplets verbatim;
        // replacement characters would change the AWS canonical request hash.
        let mut consumed = 0;
        while consumed < octets.len() {
            match std::str::from_utf8(&octets[consumed..]) {
                Ok(text) => {
                    output.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output.push_str(
                        std::str::from_utf8(&octets[consumed..consumed + valid]).unwrap(),
                    );
                    consumed += valid;
                    let invalid = error.error_len().unwrap_or(octets.len() - consumed);
                    output.push_str(&value[start + consumed * 3..start + (consumed + invalid) * 3]);
                    consumed += invalid;
                }
            }
        }
    }
    output
}
