//! Certificate selection shared by the platform store readers.

use std::{cmp::Ordering, collections::HashMap};

use foreign_types::{ForeignType, ForeignTypeRef};
use openssl::{
    asn1::{Asn1Object, Asn1OctetStringRef, Asn1Time},
    nid::Nid,
    pkey::{PKey, Private},
    stack::Stack,
    x509::{X509, X509Ref},
};

use super::{CertificateError, DEVELOPMENT_OID};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub(super) struct StoredCertificate {
    pub(super) certificate: X509,
    // Native identities may keep this absent until the selected key is exported.
    pub(super) key: Option<PKey<Private>>,
    pub(super) chain: Vec<X509>,
}

pub(super) fn explicit(
    settings: &HashMap<String, String>,
    environment: &HashMap<String, String>,
) -> Result<StoredCertificate, CertificateError> {
    let get = |name: &str| {
        settings
            .get(&format!("{}{name}", super::DEFAULT))
            .map(String::as_str)
    };
    let subject = get("subject").unwrap_or("");
    let store = get("store")
        .filter(|value| !value.is_empty())
        .unwrap_or("My");
    let location = get("location")
        .filter(|value| !value.is_empty())
        .unwrap_or("CurrentUser");
    let location = match location.trim() {
        "1" => "CurrentUser",
        "2" => "LocalMachine",
        location => location,
    };
    let allow_invalid = match get("allowinvalid") {
        None => false,
        Some(value) if value.trim().eq_ignore_ascii_case("true") => true,
        Some(value) if value.trim().eq_ignore_ascii_case("false") => false,
        Some(_) => {
            return Err(CertificateError(
                "invalid Kestrel certificate AllowInvalid boolean",
            ));
        }
    };
    #[cfg(target_os = "linux")]
    {
        linux::explicit(subject, store, location, allow_invalid, environment)
    }
    #[cfg(target_os = "macos")]
    {
        macos::explicit(subject, store, location, allow_invalid, environment)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (subject, store, location, allow_invalid, environment);
        Err(CertificateError(
            "certificate-store provisioning is unavailable on this platform; provision a PEM or PFX file",
        ))
    }
}

pub(super) fn development(environment: &HashMap<String, String>) -> Option<StoredCertificate> {
    #[cfg(target_os = "linux")]
    {
        linux::development(environment)
    }
    #[cfg(target_os = "macos")]
    {
        macos::development(environment)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = environment;
        None
    }
}

// Native-key accessibility belongs to the platform reader: in particular macOS
// must not export every key (or prompt) merely to find the certificate to select.
pub(super) fn select_explicit_with(
    mut candidates: Vec<StoredCertificate>,
    subject: &str,
    allow_invalid: bool,
    validate: impl Fn(&StoredCertificate) -> bool,
) -> Option<StoredCertificate> {
    let search = subject.to_lowercase();
    candidates.retain(|candidate| {
        formed_subject(&candidate.certificate)
            .to_lowercase()
            .contains(&search)
            && server_auth_allowed(&candidate.certificate)
            && (allow_invalid || validate(candidate))
    });
    // Stable sorting preserves store enumeration order for equal expiry times.
    candidates.sort_by(|left, right| {
        right
            .certificate
            .not_after()
            .compare(left.certificate.not_after())
            .unwrap_or(Ordering::Equal)
    });
    let selected = candidates
        .iter()
        .position(|candidate| simple_name(&candidate.certificate).to_lowercase() == search)
        .unwrap_or(0);
    if candidates.is_empty() {
        None
    } else {
        Some(candidates.remove(selected))
    }
}

pub(super) fn select_development(
    mut candidates: Vec<StoredCertificate>,
) -> Option<StoredCertificate> {
    let now = Asn1Time::days_from_now(0).ok()?;
    candidates.retain(|candidate| {
        candidate.certificate.not_before() <= now
            && candidate.certificate.not_after() >= now
            && development_version(&candidate.certificate).is_some_and(|version| version >= 4)
    });
    // CertificateManager.ListCertificates(requireExportable:false) checks time
    // and version, not chain trust, subject, EKU, or key exportability.
    candidates.sort_by_key(|candidate| {
        std::cmp::Reverse(development_version(&candidate.certificate).unwrap_or(0))
    });
    candidates.into_iter().next()
}

fn development_version(certificate: &X509Ref) -> Option<u8> {
    let data = extension_data(certificate, DEVELOPMENT_OID)?;
    if data.is_empty()
        || (data.len() == b"ASP.NET Core HTTPS development certificate".len() && data[0] == b'A')
    {
        Some(0)
    } else {
        Some(data[0])
    }
}

fn extension_data(certificate: &X509Ref, oid: &str) -> Option<Vec<u8>> {
    let oid = Asn1Object::from_str(oid).ok()?;
    // SAFETY: all pointers are borrowed from live OpenSSL values. The extension
    // and data pointers remain certificate-owned; copy the bytes before return.
    unsafe {
        let index = openssl_sys::X509_get_ext_by_OBJ(certificate.as_ptr(), oid.as_ptr(), -1);
        if index < 0
            || openssl_sys::X509_get_ext_by_OBJ(certificate.as_ptr(), oid.as_ptr(), index) >= 0
        {
            return None;
        }
        let extension = openssl_sys::X509_get_ext(certificate.as_ptr(), index);
        if extension.is_null() {
            return None;
        }
        let data = openssl_sys::X509_EXTENSION_get_data(extension);
        if data.is_null() {
            return None;
        }
        Some(Asn1OctetStringRef::from_ptr(data).as_slice().to_vec())
    }
}

fn server_auth_allowed(certificate: &X509Ref) -> bool {
    // SAFETY: the certificate pointer is borrowed. X509_get_ext_d2i returns a
    // newly allocated ASN1_OBJECT stack, whose ownership is transferred to Stack.
    unsafe {
        if openssl_sys::X509_get_ext_by_NID(certificate.as_ptr(), Nid::EXT_KEY_USAGE.as_raw(), -1)
            < 0
        {
            return true;
        }
        let objects = openssl_sys::X509_get_ext_d2i(
            certificate.as_ptr(),
            Nid::EXT_KEY_USAGE.as_raw(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if objects.is_null() {
            return false;
        }
        let objects = Stack::<Asn1Object>::from_ptr(objects.cast());
        objects
            .iter()
            .any(|object| object.nid() == Nid::SERVER_AUTH)
    }
}

fn formed_subject(certificate: &X509Ref) -> String {
    certificate
        .subject_name()
        .entries()
        .map(|entry| {
            let oid = entry.object();
            let label = if oid.nid() == Nid::PKCS9_EMAILADDRESS {
                "E".to_owned()
            } else {
                oid.nid()
                    .short_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|_| oid.to_string())
            };
            let value = entry.data().to_string().unwrap_or_default();
            let value = if value.starts_with(char::is_whitespace)
                || value.ends_with(char::is_whitespace)
                || value.contains([',', '+', '=', '"', '\n', '<', '>', '#', ';'])
            {
                format!("\"{}\"", value.replace('"', "\"\""))
            } else {
                value
            };
            format!("{label}={value}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn simple_name(certificate: &X509Ref) -> String {
    let name = certificate.subject_name();
    for nid in [
        Nid::COMMONNAME,
        Nid::ORGANIZATIONALUNITNAME,
        Nid::ORGANIZATIONNAME,
        Nid::PKCS9_EMAILADDRESS,
    ] {
        if let Some(entry) = name.entries_by_nid(nid).next() {
            return entry.data().to_string().unwrap_or_default();
        }
    }
    name.entries()
        .next()
        .and_then(|entry| entry.data().to_string().ok())
        .unwrap_or_default()
}

#[cfg(all(test, target_os = "linux"))]
use linux::{Trust, select_explicit};
#[cfg(all(test, target_os = "linux"))]
mod tests;
