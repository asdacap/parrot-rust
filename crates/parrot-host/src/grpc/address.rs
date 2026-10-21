use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
};
use url::{Host, Position, Url};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportAddress {
    Unix(PathBuf),
    Tcp(Url),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressError(&'static str);

impl fmt::Display for AddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for AddressError {}

impl TransportAddress {
    pub fn parse(value: &str) -> Result<Self, AddressError> {
        if value.trim().is_empty() {
            return Err(AddressError("a transport address is required"));
        }
        if let Some(value) = value.strip_prefix("unix:") {
            let path = Path::new(value);
            if !path.is_absolute() {
                return Err(AddressError(
                    "a unix transport needs an absolute path: unix:/path",
                ));
            }
            let mut normalized = PathBuf::new();
            for component in path.components() {
                match component {
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    Component::CurDir => {}
                    component => normalized.push(component.as_os_str()),
                }
            }
            return Ok(Self::Unix(normalized));
        }
        let uri = value.trim();
        if !uri
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            && !uri
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        {
            return Err(invalid_address());
        }
        let mut url = Url::parse(uri).map_err(|_| invalid_address())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid_address());
        }
        if !matches!(url.path(), "" | "/") {
            return Err(AddressError(
                "a TCP transport address cannot contain a path",
            ));
        }
        url.set_path("");
        Ok(Self::Tcp(url))
    }

    pub fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }

    pub fn is_external(&self) -> bool {
        match self {
            Self::Unix(_) => false,
            Self::Tcp(url) => match url.host() {
                Some(Host::Domain(host)) => !host.eq_ignore_ascii_case("localhost"),
                Some(Host::Ipv4(address)) => !address.is_loopback(),
                Some(Host::Ipv6(address)) => !address.is_loopback(),
                None => true,
            },
        }
    }

    pub fn unix_path(&self) -> Option<&Path> {
        match self {
            Self::Unix(path) => Some(path),
            Self::Tcp(_) => None,
        }
    }

    pub fn tcp_url(&self) -> Option<&Url> {
        match self {
            Self::Tcp(url) => Some(url),
            Self::Unix(_) => None,
        }
    }

    pub fn socket_addr(&self) -> Result<SocketAddr, AddressError> {
        let url = self
            .tcp_url()
            .ok_or(AddressError("the transport is not TCP"))?;
        let address = match url.host() {
            Some(Host::Domain(host)) if host.eq_ignore_ascii_case("localhost") => {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            }
            Some(Host::Ipv4(address)) => IpAddr::V4(address),
            Some(Host::Ipv6(address)) => IpAddr::V6(address),
            _ => {
                return Err(AddressError(
                    "a server TCP address must use localhost or a numeric IP address",
                ));
            }
        };
        let port = url.port_or_known_default().ok_or_else(invalid_address)?;
        Ok(SocketAddr::new(address, port))
    }

    pub fn validate_external_listen(&self, unsafe_allow: bool) -> Result<(), AddressError> {
        if self.is_external()
            && self.tcp_url().is_some_and(|url| url.scheme() == "http")
            && !unsafe_allow
        {
            return Err(AddressError(
                "external plaintext HTTP requires --unsafe-allow-external",
            ));
        }
        Ok(())
    }
}

impl fmt::Display for TransportAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(formatter, "unix:{}", path.display()),
            Self::Tcp(url) => formatter.write_str(&url[..Position::BeforePath]),
        }
    }
}

fn invalid_address() -> AddressError {
    AddressError("transport must be unix:/path, http://host:port, or https://host:port")
}
