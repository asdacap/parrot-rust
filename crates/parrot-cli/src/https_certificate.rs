//! Kestrel certificate-file and platform-store provisioning for `serve`.

mod store;

use std::{collections::HashMap, fmt, fs, io, path::Path};

use foreign_types::ForeignType;
use openssl::{
    asn1::Asn1Object,
    nid::Nid,
    pkcs12::Pkcs12,
    pkey::{PKey, Private},
    x509::X509,
};
use serde_json::Value;
use tonic::transport::{Identity, ServerTlsConfig};

const DEFAULT: &str = "kestrel:certificates:default:";
const DEVELOPMENT: &str = "kestrel:certificates:development:";
const DEVELOPMENT_OID: &str = "1.3.6.1.4.1.311.84.1.1";
const MISSING_IDENTITY: CertificateError = CertificateError(
    "HTTPS requires an explicit certificate or a usable ASP.NET development certificate: configure Kestrel:Certificates:Default:Path (PFX), Path and KeyPath (PEM), or Subject with Store/Location; no usable certificate file or certificate-store identity was found",
);

#[derive(Debug)]
pub struct CertificateError(&'static str);

impl fmt::Display for CertificateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for CertificateError {}

pub fn load() -> Result<ServerTlsConfig, CertificateError> {
    let directory = std::env::current_dir()
        .map_err(|_| CertificateError("cannot resolve HTTPS content root"))?;
    let executable = std::env::current_exe()
        .map_err(|_| CertificateError("cannot resolve HTTPS application directory"))?;
    let environment: HashMap<String, String> = std::env::vars().collect();
    #[cfg(target_os = "linux")]
    let environment = {
        let mut environment = environment;
        if environment.get("HOME").is_none_or(|home| home.is_empty()) {
            // Unix std uses HOME/getpwuid, matching .NET PersistedFiles. Its
            // deprecated Windows behavior is not used by this Linux-only path.
            #[allow(deprecated)]
            if let Some(home) = std::env::home_dir() {
                environment.insert("HOME".into(), home.to_string_lossy().into_owned());
            }
        }
        environment
    };
    let (settings, root, application_name) = configuration(
        &environment,
        &directory,
        executable.parent().unwrap_or(&directory),
    )?;
    select_identity(&settings, &root, &environment, &application_name)
        .map(|identity| ServerTlsConfig::new().identity(identity))
}

// CreateSlimBuilder() has no command-line args in the reference. Host settings
// are frozen before application JSON and unprefixed environment providers run.
fn configuration(
    environment: &HashMap<String, String>,
    directory: &Path,
    application_directory: &Path,
) -> Result<(HashMap<String, String>, std::path::PathBuf, String), CertificateError> {
    let mut settings = HashMap::new();
    add_environment(&mut settings, environment, "ASPNETCORE_");
    settings
        .entry("contentroot".into())
        .or_insert_with(|| directory.to_string_lossy().into_owned());
    add_environment(&mut settings, environment, "DOTNET_");
    let root = application_directory.join(&settings["contentroot"]);
    let application_name = settings
        .get("applicationname")
        .filter(|name| !name.is_empty())
        .map_or("parrot", String::as_str)
        .to_owned();
    let name = settings
        .get("environment")
        .map_or("Production", String::as_str);
    for name in [
        "appsettings.json".to_owned(),
        format!("appsettings.{name}.json"),
    ] {
        let text = match fs::read_to_string(root.join(name)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                return Err(CertificateError(
                    "cannot read HTTPS appsettings configuration",
                ));
            }
        };
        // Appsettings accepts comments and trailing commas, unlike strict JSON.
        let value: Value = json5::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|_| CertificateError("invalid HTTPS appsettings configuration"))?;
        if !value.is_object() {
            return Err(CertificateError(
                "HTTPS appsettings configuration must be an object",
            ));
        }
        add_json(&mut settings, "", &value);
    }
    add_environment(&mut settings, environment, "");
    Ok((settings, root, application_name))
}

fn add_environment(
    settings: &mut HashMap<String, String>,
    environment: &HashMap<String, String>,
    prefix: &str,
) {
    for (name, value) in environment {
        let name = name.replace("__", ":").to_ascii_lowercase();
        if let Some(name) = name.strip_prefix(&prefix.to_ascii_lowercase()) {
            settings.insert(name.into(), value.clone());
        }
    }
}

fn add_json(settings: &mut HashMap<String, String>, prefix: &str, value: &Value) {
    // Section presence matters: an empty Default section disables developer-file
    // fallback, just as a populated Default section does in Kestrel.
    let certificate_section = [DEFAULT, DEVELOPMENT]
        .iter()
        .any(|section| prefix == section.trim_end_matches(':'));
    if certificate_section {
        settings.insert(prefix.into(), String::new());
    }
    if let Value::Object(object) = value {
        for (name, value) in object {
            let name = if prefix.is_empty() {
                name.to_ascii_lowercase()
            } else {
                format!("{prefix}:{}", name.to_ascii_lowercase())
            };
            add_json(settings, &name, value);
        }
    } else if value.is_null() {
        // JSON null shadows lower-provider values but is not an empty password
        // or key path. Keep only the certificate-section presence marker above.
        if !certificate_section {
            settings.remove(prefix);
        }
    } else {
        settings.insert(
            prefix.into(),
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned),
        );
    }
}

fn select_identity(
    settings: &HashMap<String, String>,
    root: &Path,
    environment: &HashMap<String, String>,
    application_name: &str,
) -> Result<Identity, CertificateError> {
    let configured = |name: &str| {
        settings
            .get(&format!("{DEFAULT}{name}"))
            .is_some_and(|value| !value.is_empty())
    };
    if configured("path") {
        return identity(settings, root);
    }
    if configured("subject") {
        return store_identity(store::explicit(settings, environment)?);
    }
    let has_default_section = settings
        .keys()
        .any(|key| key == DEFAULT.trim_end_matches(':') || key.starts_with(DEFAULT));
    if !has_default_section {
        if let Some(identity) = developer_identity(settings, environment, application_name) {
            return Ok(identity);
        }
    }
    // An unusable/empty Default section suppresses developer-file lookup, not
    // KestrelServerOptions' subsequent development-store fallback.
    store_identity(store::development(environment).ok_or(MISSING_IDENTITY)?)
}

fn store_identity(candidate: store::StoredCertificate) -> Result<Identity, CertificateError> {
    let key = candidate.key.ok_or(CertificateError("selected HTTPS store certificate has no accessible private key; provision a usable PEM/PFX identity"))?;
    let mut certificates = vec![candidate.certificate];
    certificates.extend(candidate.chain);
    encode_identity(certificates, key)
}

fn developer_identity(
    settings: &HashMap<String, String>,
    environment: &HashMap<String, String>,
    application_name: &str,
) -> Option<Identity> {
    if settings.contains_key(&format!("{DEVELOPMENT}path")) {
        return None;
    }
    let password = settings.get(&format!("{DEVELOPMENT}password"))?;
    // APPDATA wins even when the file is absent; do not then search HOME.
    let base = match environment.get("APPDATA") {
        Some(appdata) => Path::new(appdata).join("ASP.NET/https"),
        None => Path::new(environment.get("HOME")?).join(".aspnet/https"),
    };
    let bytes = fs::read(base.join(format!("{application_name}.pfx"))).ok()?;
    let (certificates, key) = pfx(&bytes, password).ok()?;
    let certificate = certificates.first()?;
    let mut subject = certificate.subject_name().entries();
    let common_name = subject.next()?;
    if subject.next().is_some()
        || common_name.object().nid() != Nid::COMMONNAME
        || common_name.data().to_string().ok()? != "localhost"
    {
        return None;
    }
    let oid = Asn1Object::from_str(DEVELOPMENT_OID).ok()?;
    // SAFETY: both pointers borrow live OpenSSL objects. This lookup neither
    // mutates nor retains them, and returns only an extension index.
    if unsafe { openssl_sys::X509_get_ext_by_OBJ(certificate.as_ptr(), oid.as_ptr(), -1) } < 0 {
        return None;
    }
    encode_identity(certificates, key).ok()
}

fn identity(settings: &HashMap<String, String>, root: &Path) -> Result<Identity, CertificateError> {
    let get = |name: &str| {
        settings
            .get(&format!("{DEFAULT}{name}"))
            .map(String::as_str)
    };
    let path = get("path").filter(|path| !path.is_empty());
    if get("subject").is_some_and(|subject| !subject.is_empty()) {
        return Err(CertificateError(
            "HTTPS certificate Path and Subject are mutually exclusive",
        ));
    }
    let path = path.ok_or(MISSING_IDENTITY)?;
    let bytes = fs::read(root.join(path)).map_err(|_| {
        CertificateError(
            "cannot read HTTPS certificate configured by Kestrel:Certificates:Default:Path",
        )
    })?;
    let password = get("password").unwrap_or("");
    let (certificates, key) = if let Some(key_path) = get("keypath") {
        let certificates = X509::stack_from_pem(&bytes)
            .map_err(|_| CertificateError("invalid HTTPS PEM certificate chain"))?;
        if certificates.is_empty() {
            return Err(CertificateError(
                "invalid HTTPS PEM certificate chain: no certificates found",
            ));
        }
        let key_bytes = fs::read(root.join(key_path)).map_err(|_| {
            CertificateError(
                "cannot read HTTPS private key configured by Kestrel:Certificates:Default:KeyPath",
            )
        })?;
        // Supplying even an empty passphrase prevents OpenSSL prompting on stdin.
        let key = PKey::private_key_from_pem_passphrase(&key_bytes, password.as_bytes())
            .map_err(|_| CertificateError("invalid HTTPS PEM private key or password"))?;
        (certificates, key)
    } else {
        pfx(&bytes, password)?
    };
    encode_identity(certificates, key)
}

fn pfx(bytes: &[u8], password: &str) -> Result<(Vec<X509>, PKey<Private>), CertificateError> {
    if password.contains('\0') {
        return Err(CertificateError(
            "invalid HTTPS PFX/PKCS12 certificate or password",
        ));
    }
    let archive = Pkcs12::from_der(bytes)
        .and_then(|archive| archive.parse2(password))
        .map_err(|_| CertificateError("invalid HTTPS PFX/PKCS12 certificate or password"))?;
    let certificate = archive
        .cert
        .ok_or(CertificateError("HTTPS PFX/PKCS12 has no certificate"))?;
    let key = archive
        .pkey
        .ok_or(CertificateError("HTTPS PFX/PKCS12 has no private key"))?;
    let mut certificates = vec![certificate];
    if let Some(chain) = archive.ca {
        certificates.extend(chain);
    }
    Ok((certificates, key))
}

fn encode_identity(
    certificates: Vec<X509>,
    key: PKey<Private>,
) -> Result<Identity, CertificateError> {
    let certificate = certificates
        .first()
        .ok_or(CertificateError("HTTPS certificate chain is empty"))?;
    let public_key = certificate
        .public_key()
        .map_err(|_| CertificateError("invalid HTTPS certificate public key"))?;
    if !public_key.public_eq(&key) {
        return Err(CertificateError(
            "HTTPS certificate and private key do not match",
        ));
    }
    let mut chain = Vec::new();
    for certificate in certificates {
        chain.extend(
            certificate
                .to_pem()
                .map_err(|_| CertificateError("cannot encode HTTPS certificate chain"))?,
        );
    }
    let key = key
        .private_key_to_pem_pkcs8()
        .map_err(|_| CertificateError("cannot encode HTTPS private key"))?;
    Ok(Identity::from_pem(chain, key))
}

#[cfg(test)]
#[path = "https_certificate/tests.rs"]
mod tests;
