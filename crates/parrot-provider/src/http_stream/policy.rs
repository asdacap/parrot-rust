use super::HttpError;
use std::net::IpAddr;
use url::Url;

/// Keeps source URI spelling for authority decisions before HTTP transports
/// convert its host to IDNA. Unicode and punycode spellings must not broaden
/// the source's authority-scoped certificate exception.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    url: Url,
    display: String,
}
impl Endpoint {
    pub fn parse(value: &str) -> Result<Self, url::ParseError> {
        let value = value.trim();
        let url = Url::parse(value)?;
        if matches!(url.scheme(), "http" | "https") {
            let rest = value
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or_default();
            let authority = rest
                .strip_prefix("//")
                .ok_or(if rest.starts_with('\\') {
                    url::ParseError::InvalidDomainCharacter
                } else {
                    url::ParseError::RelativeUrlWithoutBase
                })?
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default();
            let host_port = authority.rsplit('@').next().unwrap_or_default();
            if host_port.is_empty()
                || authority.contains('\\')
                || authority.chars().any(char::is_control)
                || (!host_port.starts_with('[') && host_port.contains('%'))
            {
                return Err(url::ParseError::InvalidDomainCharacter);
            }
        }
        Ok(Self::from_source_url(url, value))
    }
    fn from_source_url(url: Url, original: &str) -> Self {
        let display = original
            .trim()
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', '\\', '?', '#']).next())
            .map(|authority| authority.rsplit('@').next().unwrap_or_default())
            .filter(|authority| !authority.is_ascii())
            .map(|authority| {
                let host = authority
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                format!(
                    "{}{}{}",
                    &url[..url::Position::BeforeHost],
                    host,
                    &url[url::Position::AfterHost..]
                )
            })
            // Source spelling is only an identity label for this exact parsed
            // transport URL. Never let raw delimiter/userinfo interpretation
            // attach another host's certificate exception to the request.
            .filter(|display| Url::parse(display).is_ok_and(|parsed| parsed == url))
            .unwrap_or_else(|| url.as_str().to_owned());
        Self { url, display }
    }
    pub fn authority(&self) -> &str {
        let start = self.display.find("://").map_or(0, |index| index + 3);
        let end = self.display[start..]
            .find(['/', '?', '#'])
            .map_or(self.display.len(), |index| start + index);
        &self.display[..end]
    }
    pub fn as_str(&self) -> &str {
        &self.display
    }
    pub fn as_url(&self) -> &Url {
        &self.url
    }
    pub fn into_url(self) -> Url {
        self.url
    }
}
impl From<Url> for Endpoint {
    fn from(url: Url) -> Self {
        Self {
            display: url.as_str().to_owned(),
            url,
        }
    }
}
impl From<&Url> for Endpoint {
    fn from(url: &Url) -> Self {
        Self::from(url.clone())
    }
}
impl From<&Endpoint> for Endpoint {
    fn from(endpoint: &Endpoint) -> Self {
        endpoint.clone()
    }
}
impl From<Endpoint> for Url {
    fn from(endpoint: Endpoint) -> Self {
        endpoint.url
    }
}
impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

pub fn endpoint_url(
    base_url: &str,
    endpoint: &str,
    allow_insecure_localhost: bool,
    allow_insecure_remote: bool,
) -> Result<Endpoint, HttpError> {
    let mut parsed = Endpoint::parse(base_url)
        .map_err(|_| HttpError::ProviderHttp(format!("provider: invalid base URL: {base_url}")))?
        .into_url();
    if parsed.host_str().is_none_or(str::is_empty)
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(HttpError::ProviderHttp(
            "provider: base URL must contain only a scheme, host, and path".into(),
        ));
    }
    match parsed.scheme() {
        "https" => {}
        "http" if allow_insecure_remote
            || (allow_insecure_localhost && is_loopback(parsed.host_str().unwrap_or_default())) => {}
        "http" => return Err(HttpError::ProviderHttp(
            "provider: HTTP requires allow_insecure_remote, or allow_insecure_localhost for a loopback host".into(),
        )),
        _ => return Err(HttpError::ProviderHttp("provider: base URL must use HTTPS".into())),
    }
    let path = format!("{}/{}", parsed.path().trim_end_matches('/'), endpoint);
    parsed.set_path(&path);
    Ok(Endpoint::from_source_url(parsed, base_url))
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub fn has_authority(endpoint: impl Into<Endpoint>, authority: &str) -> bool {
    let endpoint = endpoint.into();
    fn ordinal_fold(character: char) -> char {
        let mut upper = character.to_uppercase();
        let first = upper.next().unwrap_or(character);
        if upper.next().is_none() {
            first
        } else {
            character
        }
    }
    endpoint.as_url().scheme() == "https"
        && endpoint
            .authority()
            .chars()
            .map(ordinal_fold)
            .eq(authority.chars().map(ordinal_fold))
}

/// Case-insensitive configured headers, preserving the first key's spelling.
pub fn validate_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, HttpError> {
    let mut result: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        if name.trim() != name
            || name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
        {
            return Err(HttpError::ProviderHttp(format!(
                "provider: invalid configured header name \"{name}\""
            )));
        }
        if ["authorization", "cookie", "host", "proxy-authorization"]
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            return Err(HttpError::ProviderHttp(format!(
                "provider: configured header \"{name}\" is not allowed"
            )));
        }
        if value.contains(['\r', '\n']) {
            return Err(HttpError::ProviderHttp(format!(
                "provider: configured header \"{name}\" has an invalid value"
            )));
        }
        if let Some((_, prior)) = result
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            prior.clone_from(value);
        } else {
            result.push((name.clone(), value.clone()));
        }
    }
    Ok(result)
}

/// The caller resolves its injected API-key source before constructing headers.
pub fn bearer_headers(
    provider_id: &str,
    api_key: &str,
) -> Result<Vec<(String, String)>, HttpError> {
    if api_key.is_empty() {
        return Err(HttpError::Provider(format!(
            "provider: \"{provider_id}\" has no API key"
        )));
    }
    Ok(vec![("Authorization".into(), format!("Bearer {api_key}"))])
}
