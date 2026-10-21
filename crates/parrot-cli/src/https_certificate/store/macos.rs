use super::{StoredCertificate, select_development, select_explicit_with};
use crate::https_certificate::{CertificateError, pfx};
use core_foundation::{base::TCFType, data::CFData, string::CFString};
use openssl::{hash::MessageDigest, x509::X509};
use security_framework::{
    certificate::SecCertificate,
    identity::SecIdentity,
    item::{ItemClass, ItemSearchOptions, Limit, Reference, SearchResult},
    os::macos::keychain::SecKeychain,
    policy::SecPolicy,
    trust::SecTrust,
};
use security_framework_sys::{
    base::{errSecItemNotFound, errSecSuccess},
    import_export::{
        SEC_KEY_IMPORT_EXPORT_PARAMS_VERSION, SecItemExport, SecItemImportExportKeyParameters,
        kSecFormatPKCS12,
    },
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    ptr,
};

const NOT_FOUND: CertificateError = CertificateError(
    "no matching HTTPS certificate with a private key in the macOS certificate store",
);
const EXPORT_FAILED: CertificateError = CertificateError(
    "cannot export the selected macOS HTTPS private key; unlock its keychain and allow private-key export/access, or configure an exportable PFX/PEM identity (nonexportable and Secure Enclave keys are not supported)",
);

pub(super) fn explicit(
    subject: &str,
    store: &str,
    location: &str,
    allow_invalid: bool,
    environment: &HashMap<String, String>,
) -> Result<StoredCertificate, CertificateError> {
    let Some(keychain) = open_keychain(store, location, environment)? else {
        // Root and Disallowed are trust stores, not private-key identity stores.
        return Err(NOT_FOUND);
    };
    let mut identities = Vec::new();
    let mut candidates = Vec::new();
    for item in search(&keychain, ItemClass::identity())? {
        if let SearchResult::Ref(Reference::Identity(identity)) = item {
            let der = identity
                .certificate()
                .map_err(|_| CertificateError("cannot read a macOS HTTPS identity certificate"))?
                .to_der();
            let certificate = X509::from_der(&der)
                .map_err(|_| CertificateError("invalid macOS HTTPS identity certificate"))?;
            candidates.push(StoredCertificate {
                certificate,
                key: None,
                chain: Vec::new(),
            });
            identities.push((der, identity));
        }
    }
    let selected = select_explicit_with(candidates, subject, allow_invalid, |candidate| {
        native_trust(candidate).is_some_and(|trust| trust.evaluate_with_error().is_ok())
    })
    .ok_or(NOT_FOUND)?;
    let selected_der = selected
        .certificate
        .to_der()
        .map_err(|_| CertificateError("cannot encode the selected macOS HTTPS certificate"))?;
    let identity = identities
        .iter()
        .find(|(der, _)| *der == selected_der)
        .map(|(_, identity)| identity)
        .ok_or(NOT_FOUND)?;

    // Selection happens before export. An inaccessible/nonexportable selected key must
    // not silently make an older or otherwise lower-priority certificate win.
    export_identity(identity, &selected_der)
}

fn open_keychain(
    store: &str,
    location: &str,
    environment: &HashMap<String, String>,
) -> Result<Option<SecKeychain>, CertificateError> {
    match keychain_location(store, location, environment)? {
        KeychainLocation::Default => SecKeychain::default().map(Some).map_err(|_| {
            CertificateError("cannot open the default macOS HTTPS keychain; check keychain access")
        }),
        KeychainLocation::File(path) => SecKeychain::open(path).map(Some).map_err(|_| {
            CertificateError(
                "cannot open the configured macOS HTTPS keychain; check its path and access",
            )
        }),
        KeychainLocation::TrustOnly => Ok(None),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeychainLocation {
    Default,
    File(PathBuf),
    TrustOnly,
}

fn keychain_location(
    store: &str,
    location: &str,
    environment: &HashMap<String, String>,
) -> Result<KeychainLocation, CertificateError> {
    let current_user = location.eq_ignore_ascii_case("CurrentUser");
    if !current_user && !location.eq_ignore_ascii_case("LocalMachine") {
        return Err(CertificateError(
            "invalid macOS HTTPS certificate store location",
        ));
    }
    if store.eq_ignore_ascii_case("Root") || store.eq_ignore_ascii_case("Disallowed") {
        return Ok(KeychainLocation::TrustOnly);
    }
    if store.eq_ignore_ascii_case("My") {
        return Ok(if current_user {
            // .NET CurrentUser/My uses SecKeychainCopyDefault, not the search list.
            KeychainLocation::Default
        } else {
            KeychainLocation::File(PathBuf::from("/Library/Keychains/System.keychain"))
        });
    }
    if !current_user {
        return Err(CertificateError(
            "custom LocalMachine HTTPS certificate stores are not supported on macOS",
        ));
    }
    if store.trim().is_empty() || store.contains(['/', '\0']) {
        return Err(CertificateError(
            "invalid macOS HTTPS certificate store name",
        ));
    }
    let home = environment
        .get("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(CertificateError(
            "HOME is required for a custom macOS HTTPS certificate store",
        ))?;
    Ok(KeychainLocation::File(
        Path::new(home)
            .join("Library/Keychains")
            .join(format!("{}.keychain", store.to_lowercase())),
    ))
}

fn search(keychain: &SecKeychain, class: ItemClass) -> Result<Vec<SearchResult>, CertificateError> {
    match ItemSearchOptions::new()
        .keychains(std::slice::from_ref(keychain))
        .class(class)
        .load_refs(true)
        .limit(Limit::All)
        .search()
    {
        Ok(items) => Ok(items),
        Err(error) if error.code() == errSecItemNotFound => Ok(Vec::new()),
        Err(_) => Err(CertificateError(
            "cannot enumerate the macOS HTTPS keychain; unlock it and check keychain access",
        )),
    }
}

fn native_trust(candidate: &StoredCertificate) -> Option<SecTrust> {
    let certificates = std::iter::once(&candidate.certificate)
        .chain(&candidate.chain)
        .map(|certificate| SecCertificate::from_der(&certificate.to_der().ok()?).ok())
        .collect::<Option<Vec<_>>>()?;
    // Like .NET's NoCheck chain setup, use Basic X509 without adding a
    // revocation policy. Trust anchors and intermediate discovery remain native.
    SecTrust::create_with_certificates(&certificates, &[SecPolicy::create_x509()]).ok()
}

fn export_identity(
    identity: &SecIdentity,
    selected_der: &[u8],
) -> Result<StoredCertificate, CertificateError> {
    // This passphrase only protects an in-memory handoff to OpenSSL. No key or
    // archive is written to disk. Native-key signing is intentionally unsupported.
    const PASSWORD: &str = "parrot-in-memory-keychain-export";
    let password = CFString::new(PASSWORD);
    let parameters = SecItemImportExportKeyParameters {
        version: SEC_KEY_IMPORT_EXPORT_PARAMS_VERSION,
        flags: 0,
        passphrase: password.as_CFTypeRef(),
        alertTitle: ptr::null(),
        alertPrompt: ptr::null(),
        accessRef: ptr::null_mut(),
        keyUsage: ptr::null(),
        keyAttributes: ptr::null(),
    };
    let mut output = ptr::null();
    // SAFETY: the identity and passphrase remain alive for the synchronous call;
    // SecItemExport returns a retained CFData that is released by its wrapper.
    let status = unsafe {
        SecItemExport(
            identity.as_CFTypeRef(),
            kSecFormatPKCS12,
            0,
            &parameters,
            &mut output,
        )
    };
    let data = if output.is_null() {
        None
    } else {
        // SAFETY: a non-null SecItemExport output is a CFData under the create rule.
        Some(unsafe { CFData::wrap_under_create_rule(output) })
    };
    if status != errSecSuccess {
        return Err(EXPORT_FAILED);
    }
    let data = data.ok_or(EXPORT_FAILED)?;
    let (certificates, key) = pfx(data.bytes(), PASSWORD).map_err(|_| EXPORT_FAILED)?;
    let mut certificates = certificates.into_iter();
    let certificate = certificates.next().ok_or(EXPORT_FAILED)?;
    if certificate.to_der().map_err(|_| EXPORT_FAILED)? != selected_der {
        return Err(CertificateError(
            "macOS HTTPS keychain export did not contain the selected certificate",
        ));
    }
    let mut selected = StoredCertificate {
        certificate,
        key: Some(key),
        chain: certificates.collect(),
    };
    // SecItemExport of one identity may omit intermediates. Ask the native trust
    // builder for them even when AllowInvalid permitted an untrusted leaf.
    if selected.chain.is_empty() {
        if let Some(trust) = native_trust(&selected) {
            let _ = trust.evaluate_with_error();
            #[allow(deprecated)]
            for index in 1..trust.certificate_count() {
                if let Some(certificate) = trust.certificate_at_index(index) {
                    if let Ok(certificate) = X509::from_der(&certificate.to_der()) {
                        selected.chain.push(certificate);
                    }
                }
            }
        }
    }
    Ok(selected)
}

pub(super) fn development(environment: &HashMap<String, String>) -> Option<StoredCertificate> {
    let directory = Path::new(environment.get("HOME")?).join(".aspnet/dev-certs/https");
    let keychain = SecKeychain::default().ok()?;
    // Store enumeration includes certificate-only entries: ListCertificates uses
    // requireExportable:false, and a correlated disk PFX supplies the private key.
    let mut native = Vec::new();
    for item in search(&keychain, ItemClass::certificate()).ok()? {
        if let SearchResult::Ref(Reference::Certificate(certificate)) = item {
            native.push(StoredCertificate {
                certificate: X509::from_der(&certificate.to_der()).ok()?,
                key: None,
                chain: Vec::new(),
            });
        }
    }
    let disk = disk_certificates(&directory).ok()?;
    finish_development(correlate_development(native, disk)?, &directory)
}

fn finish_development(
    candidates: Vec<StoredCertificate>,
    directory: &Path,
) -> Option<StoredCertificate> {
    let selected = select_development(candidates)?;
    // CheckCertificateState runs AFTER selection. A newer keychain-only entry
    // must fail here, not cause fallback to an older disk-backed development cert.
    let path = directory.join(format!(
        "aspnetcore-localhost-{}.pfx",
        thumbprint(&selected.certificate)?
    ));
    if !path.is_file() || selected.key.is_none() {
        return None;
    }
    Some(selected)
}

fn disk_certificates(directory: &Path) -> Result<Vec<StoredCertificate>, CertificateError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(CertificateError(
                "cannot read macOS HTTPS development certificate directory",
            ));
        }
    };
    let mut certificates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| {
            CertificateError("cannot read macOS HTTPS development certificate file")
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("aspnetcore-localhost-") || !name.ends_with(".pfx") {
            continue;
        }
        let bytes = fs::read(entry.path()).map_err(|_| {
            CertificateError("cannot read macOS HTTPS development certificate file")
        })?;
        let (chain, key) = pfx(&bytes, "")?;
        let mut chain = chain.into_iter();
        let certificate = chain
            .next()
            .ok_or(CertificateError("empty macOS HTTPS development PFX"))?;
        certificates.push(StoredCertificate {
            certificate,
            key: Some(key),
            chain: chain.collect(),
        });
    }
    Ok(certificates)
}

fn correlate_development(
    native: Vec<StoredCertificate>,
    disk: Vec<StoredCertificate>,
) -> Option<Vec<StoredCertificate>> {
    let native_hashes = native
        .iter()
        .map(|candidate| thumbprint(&candidate.certificate))
        .collect::<Option<HashSet<_>>>()?;
    let mut seen = HashSet::new();
    let mut correlated = Vec::new();
    for candidate in disk {
        let hash = thumbprint(&candidate.certificate)?;
        if native_hashes.contains(&hash) && seen.insert(hash) {
            correlated.push(candidate);
        }
    }
    let mut candidates = Vec::new();
    for candidate in native {
        if seen.insert(thumbprint(&candidate.certificate)?) {
            candidates.push(candidate);
        }
    }
    // .NET appends disk+keychain matches after keychain-only entries.
    candidates.extend(correlated);
    Some(candidates)
}

fn thumbprint(certificate: &X509) -> Option<String> {
    let hash = certificate.digest(MessageDigest::sha1()).ok()?;
    Some(hash.iter().map(|byte| format!("{byte:02X}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::{
        asn1::{Asn1Object, Asn1OctetString, Asn1Time},
        bn::BigNum,
        pkcs12::Pkcs12,
        pkey::PKey,
        x509::{X509Extension, X509NameBuilder},
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    const CERTIFICATE: &[u8] =
        include_bytes!("../../../../parrot-host/tests/grpc/fixtures/server.pem");
    const KEY: &[u8] = include_bytes!("../../../../parrot-host/tests/grpc/fixtures/server-key.pem");
    const ROOT: &[u8] = include_bytes!("../../../../parrot-host/tests/grpc/fixtures/root.pem");
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "parrot-macos-dev-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn dev_certificate(version: u8) -> X509 {
        let key = PKey::private_key_from_pem(KEY).unwrap();
        let mut subject = X509NameBuilder::new().unwrap();
        subject.append_entry_by_text("CN", "localhost").unwrap();
        let subject = subject.build();
        let mut certificate = X509::builder().unwrap();
        certificate.set_version(2).unwrap();
        certificate.set_subject_name(&subject).unwrap();
        certificate.set_issuer_name(&subject).unwrap();
        certificate
            .set_serial_number(
                &BigNum::from_u32(u32::from(version))
                    .unwrap()
                    .to_asn1_integer()
                    .unwrap(),
            )
            .unwrap();
        certificate.set_pubkey(&key).unwrap();
        certificate
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        certificate
            .set_not_after(&Asn1Time::days_from_now(10).unwrap())
            .unwrap();
        let oid = Asn1Object::from_str("1.3.6.1.4.1.311.84.1.1").unwrap();
        let payload = Asn1OctetString::new_from_bytes(&[version]).unwrap();
        certificate
            .append_extension(X509Extension::new_from_der(&oid, false, &payload).unwrap())
            .unwrap();
        certificate.sign(&key, MessageDigest::sha256()).unwrap();
        certificate.build()
    }

    fn write_dev_pfx(directory: &Path, certificate: &X509) {
        let archive = Pkcs12::builder()
            .pkey(&PKey::private_key_from_pem(KEY).unwrap())
            .cert(certificate)
            .build2("")
            .unwrap()
            .to_der()
            .unwrap();
        fs::write(
            directory.join(format!(
                "aspnetcore-localhost-{}.pfx",
                thumbprint(certificate).unwrap()
            )),
            archive,
        )
        .unwrap();
    }

    fn candidate(pem: &[u8], has_key: bool) -> StoredCertificate {
        StoredCertificate {
            certificate: X509::from_pem(pem).unwrap(),
            key: has_key.then(|| PKey::private_key_from_pem(KEY).unwrap()),
            chain: Vec::new(),
        }
    }

    #[test]
    fn store_mapping_matches_dotnet_without_opening_a_keychain() {
        let environment = HashMap::from([("HOME".into(), "/synthetic/home".into())]);
        assert_eq!(
            keychain_location("my", "currentuser", &environment).unwrap(),
            KeychainLocation::Default
        );
        assert_eq!(
            keychain_location("My", "LocalMachine", &environment).unwrap(),
            KeychainLocation::File("/Library/Keychains/System.keychain".into())
        );
        assert_eq!(
            keychain_location("CustomSTORE", "CurrentUser", &environment).unwrap(),
            KeychainLocation::File("/synthetic/home/Library/Keychains/customstore.keychain".into())
        );
        assert_eq!(
            keychain_location("Root", "CurrentUser", &environment).unwrap(),
            KeychainLocation::TrustOnly
        );
        assert!(keychain_location("Custom", "LocalMachine", &environment).is_err());
        assert!(keychain_location("../bad", "CurrentUser", &environment).is_err());
        assert!(keychain_location("bad\0name", "CurrentUser", &environment).is_err());
        assert!(keychain_location(" ", "CurrentUser", &environment).is_err());
        assert!(keychain_location("My", "bad", &environment).is_err());
    }

    #[test]
    fn disk_only_certificates_are_excluded_and_disk_key_is_preferred() {
        let candidates = correlate_development(
            vec![candidate(CERTIFICATE, false)],
            vec![candidate(ROOT, false), candidate(CERTIFICATE, true)],
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].key.is_some());
        assert_eq!(
            candidates[0].certificate.to_der().unwrap(),
            X509::from_pem(CERTIFICATE).unwrap().to_der().unwrap()
        );
    }

    #[test]
    fn keychain_only_entries_remain_candidates_before_correlated_disk_entries() {
        let candidates = correlate_development(
            vec![candidate(CERTIFICATE, false), candidate(ROOT, false)],
            vec![candidate(CERTIFICATE, true)],
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].certificate.to_der().unwrap(),
            X509::from_pem(ROOT).unwrap().to_der().unwrap()
        );
        assert!(candidates[0].key.is_none());
        assert!(candidates[1].key.is_some());
    }

    #[test]
    fn selected_keychain_only_dev_certificate_does_not_fall_back_to_older_disk_identity() {
        let directory = Directory::new();
        let older = dev_certificate(4);
        let newer = dev_certificate(5);
        write_dev_pfx(&directory.0, &older);
        let native = || {
            vec![
                StoredCertificate {
                    certificate: older.clone(),
                    key: None,
                    chain: Vec::new(),
                },
                StoredCertificate {
                    certificate: newer.clone(),
                    key: None,
                    chain: Vec::new(),
                },
            ]
        };
        let candidates =
            correlate_development(native(), disk_certificates(&directory.0).unwrap()).unwrap();
        assert!(finish_development(candidates, &directory.0).is_none());

        write_dev_pfx(&directory.0, &newer);
        let candidates =
            correlate_development(native(), disk_certificates(&directory.0).unwrap()).unwrap();
        let selected = finish_development(candidates, &directory.0).unwrap();
        assert_eq!(
            selected.certificate.to_der().unwrap(),
            newer.to_der().unwrap()
        );
        assert!(selected.key.is_some());
    }

    #[test]
    fn disk_pfx_is_unpassworded_and_malformed_matching_files_fail_discovery() {
        let directory = Directory::new();
        let archive = Pkcs12::builder()
            .pkey(&PKey::private_key_from_pem(KEY).unwrap())
            .cert(&X509::from_pem(CERTIFICATE).unwrap())
            .build2("")
            .unwrap()
            .to_der()
            .unwrap();
        fs::write(directory.0.join("aspnetcore-localhost-test.pfx"), archive).unwrap();
        fs::write(directory.0.join("unrelated.pfx"), b"invalid").unwrap();
        let result = disk_certificates(&directory.0).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].key.is_some());
        let hash = thumbprint(&result[0].certificate).unwrap();
        assert_eq!(hash.len(), 40);
        assert!(
            hash.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
        );
        fs::write(
            directory.0.join("aspnetcore-localhost-broken.pfx"),
            b"invalid",
        )
        .unwrap();
        assert!(disk_certificates(&directory.0).is_err());
    }
}
