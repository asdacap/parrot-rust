use std::{fmt, fs, path::Path};
use tonic::transport::{Identity, ServerTlsConfig};

#[derive(Debug)]
pub struct CertificateError(&'static str);

impl fmt::Display for CertificateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for CertificateError {}

pub fn load(certificate: &Path, key: &Path) -> Result<ServerTlsConfig, CertificateError> {
    let certificate = fs::read(certificate)
        .map_err(|_| CertificateError("cannot read HTTPS certificate from --tls-cert-file"))?;
    let key = fs::read(key)
        .map_err(|_| CertificateError("cannot read HTTPS private key from --tls-key-file"))?;
    Ok(ServerTlsConfig::new().identity(Identity::from_pem(certificate, key)))
}

#[cfg(test)]
#[path = "https_certificate/tests.rs"]
mod tests;
