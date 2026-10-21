#![cfg(target_os = "linux")]

use super::linux::read_store;
use super::*;
use openssl::{
    asn1::{Asn1Integer, Asn1Object, Asn1OctetString, Asn1Time},
    bn::BigNum,
    hash::MessageDigest,
    pkcs12::Pkcs12,
    pkey::PKey,
    rsa::Rsa,
    x509::{
        X509, X509Extension, X509NameBuilder,
        extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage},
        store::X509StoreBuilder,
    },
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DAY: i64 = 24 * 60 * 60;
const DEV_OID: &str = "1.3.6.1.4.1.311.84.1.1";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "parrot-linux-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn store(&self, name: &str) -> PathBuf {
        let path = self
            .0
            .join(".dotnet/corefx/cryptography/x509stores")
            .join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn certificate(
    serial: u32,
    common_name: &str,
    validity: (i64, i64),
    issuer: Option<&StoredCertificate>,
    extensions: Vec<X509Extension>,
) -> StoredCertificate {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("O", "Synthetic Store Organization")
        .unwrap();
    name.append_entry_by_text("CN", common_name).unwrap();
    let name = name.build();
    let mut builder = X509::builder().unwrap();
    builder.set_version(2).unwrap();
    let serial = Asn1Integer::from_bn(&BigNum::from_u32(serial).unwrap()).unwrap();
    builder.set_serial_number(&serial).unwrap();
    builder.set_subject_name(&name).unwrap();
    builder
        .set_issuer_name(issuer.map_or(name.as_ref(), |issuer| issuer.certificate.subject_name()))
        .unwrap();
    builder.set_pubkey(&key).unwrap();
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    builder
        .set_not_before(&Asn1Time::from_unix(now + validity.0).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::from_unix(now + validity.1).unwrap())
        .unwrap();
    for extension in extensions {
        builder.append_extension(extension).unwrap();
    }
    builder
        .sign(
            issuer.map_or(&key, |issuer| issuer.key.as_ref().unwrap()),
            MessageDigest::sha256(),
        )
        .unwrap();
    StoredCertificate {
        certificate: builder.build(),
        key: Some(key),
        chain: Vec::new(),
    }
}

fn authority(serial: u32, name: &str, issuer: Option<&StoredCertificate>) -> StoredCertificate {
    certificate(
        serial,
        name,
        (-DAY, 30 * DAY),
        issuer,
        vec![
            BasicConstraints::new().critical().ca().build().unwrap(),
            KeyUsage::new()
                .critical()
                .key_cert_sign()
                .crl_sign()
                .build()
                .unwrap(),
        ],
    )
}

fn leaf(
    serial: u32,
    name: &str,
    validity: (i64, i64),
    issuer: &StoredCertificate,
    eku: Option<X509Extension>,
) -> StoredCertificate {
    let mut extensions = vec![
        BasicConstraints::new().critical().build().unwrap(),
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()
            .unwrap(),
    ];
    extensions.extend(eku);
    certificate(serial, name, validity, Some(issuer), extensions)
}

fn server_auth() -> X509Extension {
    ExtendedKeyUsage::new().server_auth().build().unwrap()
}

fn copy(stored: &StoredCertificate) -> StoredCertificate {
    StoredCertificate {
        certificate: stored.certificate.clone(),
        key: stored.key.clone(),
        chain: stored.chain.clone(),
    }
}

fn trust(root: &StoredCertificate, intermediates: &[&StoredCertificate]) -> Trust {
    let mut store = X509StoreBuilder::new().unwrap();
    store.add_cert(root.certificate.clone()).unwrap();
    Trust {
        store: store.build(),
        intermediates: intermediates
            .iter()
            .map(|cert| cert.certificate.clone())
            .collect(),
    }
}

fn assert_selected(selected: Option<StoredCertificate>, expected: &StoredCertificate) {
    let selected = selected.expect("expected a usable synthetic certificate");
    assert_eq!(
        selected.certificate.to_der().unwrap(),
        expected.certificate.to_der().unwrap()
    );
}

fn developer(
    serial: u32,
    name: &str,
    validity: (i64, i64),
    oid: &str,
    version: &[u8],
) -> StoredCertificate {
    let oid = Asn1Object::from_str(oid).unwrap();
    let payload = Asn1OctetString::new_from_bytes(version).unwrap();
    certificate(
        serial,
        name,
        validity,
        None,
        vec![
            X509Extension::new_from_der(&oid, false, &payload).unwrap(),
            server_auth(),
        ],
    )
}

fn archive(stored: &StoredCertificate, password: &str) -> Vec<u8> {
    let mut archive = Pkcs12::builder();
    archive.cert(&stored.certificate);
    if let Some(key) = &stored.key {
        archive.pkey(key);
    }
    archive.build2(password).unwrap().to_der().unwrap()
}

#[test]
fn explicit_selection_prefers_exact_simple_name_then_latest_expiry_and_matches_organization() {
    let root = authority(1, "Synthetic root", None);
    let exact = leaf(2, "wanted", (-60, 2 * DAY), &root, Some(server_auth()));
    let substring = leaf(
        3,
        "prefix-wanted-suffix",
        (-60, 8 * DAY),
        &root,
        Some(server_auth()),
    );
    assert_selected(
        select_explicit(vec![copy(&substring), copy(&exact)], "WaNtEd", true, None),
        &exact,
    );

    let older = leaf(4, "wanted-old", (-60, DAY), &root, None);
    assert_selected(
        select_explicit(vec![copy(&older), copy(&substring)], "wanted-", true, None),
        &substring,
    );
    assert_selected(
        select_explicit(vec![copy(&substring), older], "wanted-", true, None),
        &substring,
    );
    assert_selected(
        select_explicit(vec![copy(&exact)], "STORE ORGANIZATION", true, None),
        &exact,
    );
    assert!(select_explicit(vec![exact], "not in the subject", true, None).is_none());
}

#[test]
fn allow_invalid_never_bypasses_private_key_or_server_auth_eku_requirements() {
    let root = authority(10, "Synthetic root", None);
    let no_eku = leaf(11, "target-no-eku", (-60, DAY), &root, None);
    let server = leaf(
        12,
        "target-server",
        (-60, 2 * DAY),
        &root,
        Some(server_auth()),
    );
    let client_only = leaf(
        13,
        "target-client",
        (-60, 5 * DAY),
        &root,
        Some(ExtendedKeyUsage::new().client_auth().build().unwrap()),
    );
    let any_only = leaf(
        14,
        "target-any",
        (-60, 6 * DAY),
        &root,
        Some(
            ExtendedKeyUsage::new()
                .other("2.5.29.37.0")
                .build()
                .unwrap(),
        ),
    );
    let mut keyless = leaf(15, "target-keyless", (-60, 7 * DAY), &root, None);
    keyless.key = None;
    assert!(
        select_explicit(
            vec![copy(&client_only), copy(&any_only), copy(&keyless)],
            "target",
            true,
            None,
        )
        .is_none()
    );
    assert_selected(
        select_explicit(
            vec![client_only, any_only, keyless, copy(&no_eku)],
            "target",
            true,
            None,
        ),
        &no_eku,
    );
    assert_selected(
        select_explicit(vec![copy(&server)], "target", true, None),
        &server,
    );
}

#[test]
fn allow_invalid_false_requires_trusted_chain_intermediate_time_and_valid_signature() {
    let root = authority(20, "Trusted synthetic root", None);
    let intermediate = authority(21, "Synthetic intermediate", Some(&root));
    let other_root = authority(22, "Untrusted synthetic root", None);
    let valid = leaf(
        23,
        "target-valid",
        (-60, 2 * DAY),
        &intermediate,
        Some(server_auth()),
    );
    let untrusted = leaf(
        24,
        "target-untrusted",
        (-60, 8 * DAY),
        &other_root,
        Some(server_auth()),
    );
    let expired = leaf(
        25,
        "target-expired",
        (-2 * DAY, -DAY),
        &intermediate,
        Some(server_auth()),
    );
    let future = leaf(
        26,
        "target-future",
        (DAY, 10 * DAY),
        &intermediate,
        Some(server_auth()),
    );
    let bad_issuer = StoredCertificate {
        certificate: intermediate.certificate.clone(),
        key: other_root.key.clone(),
        chain: Vec::new(),
    };
    let invalid_signature = leaf(
        27,
        "target-bad-signature",
        (-60, 9 * DAY),
        &bad_issuer,
        Some(server_auth()),
    );
    let missing_ca = trust(&root, &[]);
    assert!(select_explicit(vec![copy(&valid)], "target", false, Some(&missing_ca)).is_none());
    let trusted = trust(&root, &[&intermediate]);
    for invalid in [&untrusted, &expired, &future, &invalid_signature] {
        assert!(select_explicit(vec![copy(invalid)], "target", false, Some(&trusted)).is_none());
    }
    assert_selected(
        select_explicit(
            vec![
                copy(&untrusted),
                expired,
                future,
                invalid_signature,
                copy(&valid),
            ],
            "target",
            false,
            Some(&trusted),
        ),
        &valid,
    );
    assert_selected(
        select_explicit(
            vec![valid, copy(&untrusted)],
            "target",
            true,
            Some(&trusted),
        ),
        &untrusted,
    );
}

#[test]
fn development_selection_prefers_version_not_subject_or_chain_trust() {
    let version_four = developer(30, "localhost", (-60, 9 * DAY), DEV_OID, &[4]);
    let version_five = developer(31, "not-localhost", (-60, DAY), DEV_OID, &[5]);
    assert_selected(
        select_development(vec![version_four, copy(&version_five)]),
        &version_five,
    );
}

#[test]
fn development_selection_rejects_absent_wrong_old_and_invalid_time_extensions() {
    let no_oid = certificate(40, "localhost", (-60, DAY), None, vec![]);
    let wrong_oid = developer(41, "localhost", (-60, DAY), "1.3.6.1.4.1.311.84.1.2", &[9]);
    let old_version = developer(42, "localhost", (-60, DAY), DEV_OID, &[3]);
    let empty_version = developer(43, "localhost", (-60, DAY), DEV_OID, &[]);
    let legacy_version = developer(
        44,
        "localhost",
        (-60, DAY),
        DEV_OID,
        b"ASP.NET Core HTTPS development certificate",
    );
    let expired = developer(45, "localhost", (-2 * DAY, -DAY), DEV_OID, &[9]);
    let future = developer(46, "localhost", (DAY, 2 * DAY), DEV_OID, &[9]);
    assert!(
        select_development(vec![
            no_oid,
            wrong_oid,
            old_version,
            empty_version,
            legacy_version,
            expired,
            future,
        ])
        .is_none()
    );
}

#[test]
fn development_equal_versions_preserve_store_order_instead_of_sorting_by_expiry() {
    let first = developer(50, "first", (-60, DAY), DEV_OID, &[4]);
    let later_expiry = developer(51, "second", (-60, 9 * DAY), DEV_OID, &[4]);
    assert_selected(
        select_development(vec![copy(&first), copy(&later_expiry)]),
        &first,
    );
    assert_selected(
        select_development(vec![copy(&later_expiry), first]),
        &later_expiry,
    );
}

#[test]
fn read_store_uses_lowercase_persisted_layout_passwordless_pfx_and_first_der_duplicate() {
    let directory = Directory::new();
    let path = directory.store("my");
    let private = certificate(60, "stored-private", (-60, DAY), None, vec![]);
    let mut public_duplicate = copy(&private);
    public_duplicate.key = None;
    let mut public_only = certificate(61, "stored-public", (-60, DAY), None, vec![]);
    public_only.key = None;
    fs::write(
        path.join("duplicate-public.pfx"),
        archive(&public_duplicate, ""),
    )
    .unwrap();
    fs::write(path.join("duplicate-private.pfx"), archive(&private, "")).unwrap();
    fs::write(path.join("public.pfx"), archive(&public_only, "")).unwrap();
    fs::write(path.join("malformed.pfx"), b"not a PFX").unwrap();
    fs::write(path.join("ignored.pem"), archive(&private, "")).unwrap();
    fs::write(
        path.join("password.pfx"),
        archive(&private, "synthetic-password"),
    )
    .unwrap();
    fs::create_dir(path.join("directory.pfx")).unwrap();
    let first_duplicate_has_key = fs::read_dir(&path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .find(|name| name == "duplicate-public.pfx" || name == "duplicate-private.pfx")
        .unwrap()
        == "duplicate-private.pfx";
    let certificates = read_store(&directory.0, "My").unwrap();
    assert_eq!(certificates.len(), 2);
    let private_der = private.certificate.to_der().unwrap();
    let selected = certificates
        .iter()
        .find(|stored| stored.certificate.to_der().unwrap() == private_der)
        .unwrap();
    assert_eq!(selected.key.is_some(), first_duplicate_has_key);
    let public_der = public_only.certificate.to_der().unwrap();
    let public = certificates
        .iter()
        .find(|stored| stored.certificate.to_der().unwrap() == public_der)
        .unwrap();
    assert!(public.key.is_none());
    assert!(read_store(&directory.0, "missing").unwrap().is_empty());
}
