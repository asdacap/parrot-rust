//! Read-only .NET OpenSSL persisted certificate stores on Linux.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
};

use openssl::{
    pkcs12::Pkcs12,
    stack::Stack,
    x509::{
        X509, X509StoreContext,
        store::{X509Store, X509StoreBuilder},
    },
};

use super::{CertificateError, StoredCertificate, select_development, select_explicit_with};

pub(super) fn explicit(
    subject: &str,
    store: &str,
    location: &str,
    allow_invalid: bool,
    environment: &HashMap<String, String>,
) -> Result<StoredCertificate, CertificateError> {
    if location.eq_ignore_ascii_case("LocalMachine") || location == "2" {
        return Err(CertificateError(
            if store.eq_ignore_ascii_case("Root") || store.eq_ignore_ascii_case("CA") {
                "Linux LocalMachine Root/CA stores contain public trust certificates, not accessible identity private keys"
            } else {
                "Linux LocalMachine certificate stores support only read-only Root and CA; use CurrentUser or an explicit certificate file"
            },
        ));
    }
    if !location.eq_ignore_ascii_case("CurrentUser") && location != "1" {
        return Err(CertificateError(
            "invalid Kestrel certificate store Location",
        ));
    }
    let home = home(environment)?;
    let candidates = read_store(home, store)?;
    let trust = if allow_invalid {
        // Chain discovery is only best-effort enrichment when validation is off.
        Trust::load(home, environment).ok()
    } else {
        Some(Trust::load(home, environment)?)
    };
    let mut selected = select_explicit(candidates, subject, allow_invalid, trust.as_ref())
        .ok_or(CertificateError("no matching HTTPS certificate with an accessible private key, server-auth usage, and required validity was found in the configured CurrentUser store"))?;
    if let Some((_, chain)) = trust.as_ref().and_then(|trust| trust.chain(&selected)) {
        if chain.len() > selected.chain.len() + 1 {
            selected.chain = chain.into_iter().skip(1).collect();
        }
    }
    Ok(selected)
}

pub(super) fn development(environment: &HashMap<String, String>) -> Option<StoredCertificate> {
    select_development(read_store(home(environment).ok()?, "My").ok()?)
}

fn home(environment: &HashMap<String, String>) -> Result<&Path, CertificateError> {
    environment
        .get("HOME")
        .filter(|home| !home.is_empty())
        .map(Path::new)
        .ok_or(CertificateError(
            "cannot locate the Linux CurrentUser certificate store without HOME",
        ))
}

pub(super) fn read_store(
    home: &Path,
    store: &str,
) -> Result<Vec<StoredCertificate>, CertificateError> {
    if store.is_empty() || Path::new(store).file_name() != Some(std::ffi::OsStr::new(store)) {
        return Err(CertificateError("invalid Kestrel certificate Store name"));
    }
    let directory = home
        .join(".dotnet/corefx/cryptography/x509stores")
        .join(store.to_lowercase());
    let files = match fs::read_dir(directory) {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(CertificateError(
                "cannot read Linux CurrentUser certificate store",
            ));
        }
    };
    if store.eq_ignore_ascii_case("Disallowed") {
        return if files.count() == 0 {
            Ok(Vec::new())
        } else {
            Err(CertificateError(
                "the Linux Disallowed certificate store must be empty",
            ))
        };
    }
    let mut certificates = Vec::new();
    let mut seen = HashSet::new();
    // Preserve Directory.EnumerateFiles order, including equal-version/expiry ties.
    for file in files {
        let file = file.map_err(|_| {
            CertificateError("cannot enumerate Linux CurrentUser certificate store")
        })?;
        let path = file.path();
        if path.extension().is_none_or(|extension| extension != "pfx") || !path.is_file() {
            continue;
        }
        let Some(candidate) = fs::read(path)
            .ok()
            .and_then(|bytes| parse_persisted(&bytes))
        else {
            continue;
        };
        let Ok(der) = candidate.certificate.to_der() else {
            continue;
        };
        if seen.insert(der) {
            certificates.push(candidate);
        }
    }
    Ok(certificates)
}

fn parse_persisted(bytes: &[u8]) -> Option<StoredCertificate> {
    let archive = Pkcs12::from_der(bytes)
        .and_then(|archive| archive.parse2(""))
        .ok()?;
    let mut chain: Vec<X509> = archive.ca.into_iter().flatten().collect();
    let certificate = match archive.cert {
        Some(certificate) => certificate,
        None if !chain.is_empty() => chain.remove(0),
        None => return None,
    };
    Some(StoredCertificate {
        certificate,
        key: archive.pkey,
        chain,
    })
}

pub(super) struct Trust {
    pub(super) store: X509Store,
    pub(super) intermediates: Vec<X509>,
}

impl Trust {
    fn load(home: &Path, environment: &HashMap<String, String>) -> Result<Self, CertificateError> {
        let mut roots = Vec::new();
        let mut intermediates = Vec::new();
        for certificate in system_certificates(environment) {
            if certificate.subject_name().to_der().ok() == certificate.issuer_name().to_der().ok() {
                roots.push(certificate);
            } else {
                intermediates.push(certificate);
            }
        }
        roots.extend(
            read_store(home, "Root")?
                .into_iter()
                .map(|candidate| candidate.certificate),
        );
        for store in ["CA", "My"] {
            intermediates.extend(
                read_store(home, store)?
                    .into_iter()
                    .map(|candidate| candidate.certificate),
            );
        }
        let mut builder = X509StoreBuilder::new().map_err(|_| {
            CertificateError("cannot initialize HTTPS certificate chain validation")
        })?;
        let mut seen = HashSet::new();
        for root in roots {
            let der = root
                .to_der()
                .map_err(|_| CertificateError("invalid HTTPS trust certificate"))?;
            if seen.insert(der) {
                builder
                    .add_cert(root)
                    .map_err(|_| CertificateError("cannot load HTTPS trust certificate"))?;
            }
        }
        Ok(Self {
            store: builder.build(),
            intermediates,
        })
    }

    fn chain(&self, candidate: &StoredCertificate) -> Option<(bool, Vec<X509>)> {
        let mut extra = Stack::new().ok()?;
        for certificate in self.intermediates.iter().chain(&candidate.chain) {
            extra.push(certificate.clone()).ok()?;
        }
        let mut context = X509StoreContext::new().ok()?;
        context
            .init(&self.store, &candidate.certificate, &extra, |context| {
                // No revocation flags/purpose override: match FindBySubjectName's
                // X509Chain.Build with RevocationMode.NoCheck, then enforce leaf EKU
                // separately in CertificateLoader selection.
                let valid = context.verify_cert()?;
                let chain = context
                    .chain()
                    .map(|chain| {
                        chain
                            .iter()
                            .map(|certificate| certificate.to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                Ok((valid, chain))
            })
            .ok()
    }
}

pub(super) fn select_explicit(
    candidates: Vec<StoredCertificate>,
    subject: &str,
    allow_invalid: bool,
    trust: Option<&Trust>,
) -> Option<StoredCertificate> {
    let candidates = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.key.as_ref().is_some_and(|key| {
                candidate
                    .certificate
                    .public_key()
                    .is_ok_and(|public| public.public_eq(key))
            })
        })
        .collect();
    select_explicit_with(candidates, subject, allow_invalid, |candidate| {
        trust
            .and_then(|trust| trust.chain(candidate))
            .is_some_and(|(valid, _)| valid)
    })
}

fn system_certificates(environment: &HashMap<String, String>) -> Vec<X509> {
    let mut files = Vec::new();
    if let Some(file) = environment.get("SSL_CERT_FILE") {
        files.push(std::path::PathBuf::from(file));
    } else {
        // Vendored OpenSSL's build-time defaults need not describe the runtime
        // Linux distribution (notably a Nix-built portable executable).
        for file in [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
        ] {
            if Path::new(file).is_file() {
                files.push(file.into());
                break;
            }
        }
    }
    let directories = environment
        .get("SSL_CERT_DIR")
        .map_or("/etc/ssl/certs", String::as_str);
    for directory in directories
        .split(':')
        .filter(|directory| !directory.is_empty())
    {
        if let Ok(entries) = fs::read_dir(directory) {
            files.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file()),
            );
        }
    }
    files
        .into_iter()
        .filter_map(|file| fs::read(file).ok())
        .flat_map(|bytes| X509::stack_from_pem(&bytes).unwrap_or_default())
        .collect()
}
