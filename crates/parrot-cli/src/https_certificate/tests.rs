use super::*;
use openssl::{rsa::Rsa, symm::Cipher};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const CERTIFICATE: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/server.pem");
const KEY: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/server-key.pem");
const ROOT: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/root.pem");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "parrot-https-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        fs::write(self.0.join(path), bytes).unwrap();
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn pem() -> HashMap<String, String> {
    HashMap::from([
        (format!("{DEFAULT}path"), "server.pem".into()),
        (format!("{DEFAULT}keypath"), "key.pem".into()),
    ])
}

#[test]
fn pem_and_encrypted_pem_load_and_validate_key_match_without_exposing_secrets() {
    let directory = Directory::new();
    directory.write("server.pem", [CERTIFICATE, ROOT].concat());
    directory.write("key.pem", KEY);
    let mut settings = pem();
    assert!(identity(&settings, &directory.0).is_ok());

    let key = PKey::private_key_from_pem(KEY).unwrap();
    directory.write(
        "key.pem",
        key.private_key_to_pem_pkcs8_passphrase(Cipher::aes_256_cbc(), b"synthetic-secret")
            .unwrap(),
    );
    settings.insert(format!("{DEFAULT}password"), "synthetic-secret".into());
    assert!(identity(&settings, &directory.0).is_ok());
    settings.insert(format!("{DEFAULT}password"), "wrong-secret".into());
    let error = identity(&settings, &directory.0).unwrap_err().to_string();
    assert_eq!(error, "invalid HTTPS PEM private key or password");
    assert!(!error.contains("secret"));
    settings.remove(&format!("{DEFAULT}password"));
    assert!(identity(&settings, &directory.0).is_err());

    let wrong = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    directory.write("key.pem", wrong.private_key_to_pem_pkcs8().unwrap());
    assert_eq!(
        identity(&settings, &directory.0).unwrap_err().to_string(),
        "HTTPS certificate and private key do not match"
    );
}

#[test]
fn pfx_with_chain_loads_with_password_and_rejects_bad_input() {
    let directory = Directory::new();
    let key = PKey::private_key_from_pem(KEY).unwrap();
    let certificate = X509::from_pem(CERTIFICATE).unwrap();
    let mut chain = openssl::stack::Stack::new().unwrap();
    chain.push(X509::from_pem(ROOT).unwrap()).unwrap();
    let archive = Pkcs12::builder()
        .name("synthetic")
        .pkey(&key)
        .cert(&certificate)
        .ca(chain)
        .build2("synthetic-secret")
        .unwrap();
    directory.write("server.pfx", archive.to_der().unwrap());
    let mut settings = HashMap::from([
        (format!("{DEFAULT}path"), "server.pfx".into()),
        (format!("{DEFAULT}password"), "synthetic-secret".into()),
    ]);
    assert!(identity(&settings, &directory.0).is_ok());
    settings.insert(format!("{DEFAULT}password"), "wrong-secret".into());
    assert_eq!(
        identity(&settings, &directory.0).unwrap_err().to_string(),
        "invalid HTTPS PFX/PKCS12 certificate or password"
    );
    settings.insert(format!("{DEFAULT}password"), "invalid\0password".into());
    assert!(identity(&settings, &directory.0).is_err());
    directory.write("server.pfx", b"not a PFX");
    settings.remove(&format!("{DEFAULT}password"));
    assert!(identity(&settings, &directory.0).is_err());
}

#[test]
fn missing_malformed_and_conflicting_file_configuration_fail_closed() {
    let directory = Directory::new();
    assert!(
        identity(&HashMap::new(), &directory.0)
            .unwrap_err()
            .to_string()
            .contains("explicit certificate")
    );
    let mut settings = pem();
    assert!(
        identity(&settings, &directory.0)
            .unwrap_err()
            .to_string()
            .contains("cannot read HTTPS certificate")
    );
    directory.write("server.pem", b"not a certificate");
    directory.write("key.pem", KEY);
    assert!(
        identity(&settings, &directory.0)
            .unwrap_err()
            .to_string()
            .contains("invalid HTTPS PEM")
    );
    directory.write("server.pem", CERTIFICATE);
    directory.write("key.pem", b"not a key");
    assert!(
        identity(&settings, &directory.0)
            .unwrap_err()
            .to_string()
            .contains("invalid HTTPS PEM private key")
    );
    settings.insert(format!("{DEFAULT}subject"), "synthetic subject".into());
    assert!(
        identity(&settings, &directory.0)
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive")
    );
}

#[test]
fn slim_configuration_precedence_freezes_host_environment_and_content_root() {
    let directory = Directory::new();
    directory.write(
        "appsettings.json",
        br#"{
        // Standard appsettings comments and trailing commas are accepted.
        "Kestrel": { "Certificates": { "Default": {
            "Path": "base.pfx", "Password": "base-password",
        }}},
    }"#,
    );
    directory.write(
        "appsettings.Staging.json",
        br#"{"kestrel:certificates:default:Path":"staging.pfx"}"#,
    );
    let mut env = HashMap::from([
        ("ASPNETCORE_CONTENTROOT".into(), "/absent-aspnetcore".into()),
        (
            "DOTNET_CONTENTROOT".into(),
            directory.0.to_string_lossy().into_owned(),
        ),
        ("ASPNETCORE_ENVIRONMENT".into(), "Development".into()),
        ("DOTNET_ENVIRONMENT".into(), "Staging".into()),
        (
            "ASPNETCORE_Kestrel__Certificates__Default__Path".into(),
            "aspnetcore.pfx".into(),
        ),
        (
            "DOTNET_Kestrel__Certificates__Default__Path".into(),
            "dotnet.pfx".into(),
        ),
        ("CONTENTROOT".into(), "/too-late-contentroot".into()),
        ("ENVIRONMENT".into(), "TooLate".into()),
    ]);
    let (settings, root, _) =
        configuration(&env, Path::new("/absent-cwd"), Path::new("/application")).unwrap();
    assert_eq!(root, directory.0);
    assert_eq!(settings[&format!("{DEFAULT}path")], "staging.pfx");
    assert_eq!(settings[&format!("{DEFAULT}password")], "base-password");
    env.insert(
        "Kestrel__Certificates__Default__Path".into(),
        "environment.pfx".into(),
    );
    let (settings, _, _) = configuration(&env, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(settings[&format!("{DEFAULT}path")], "environment.pfx");

    fs::remove_file(directory.0.join("appsettings.json")).unwrap();
    fs::remove_file(directory.0.join("appsettings.Staging.json")).unwrap();
    env.remove("Kestrel__Certificates__Default__Path");
    let (settings, _, _) = configuration(&env, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(settings[&format!("{DEFAULT}path")], "dotnet.pfx");
    env.remove("DOTNET_Kestrel__Certificates__Default__Path");
    let (settings, _, _) = configuration(&env, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(settings[&format!("{DEFAULT}path")], "aspnetcore.pfx");
}

#[test]
fn default_production_json_and_relative_content_root_resolve_like_host_builder() {
    let directory = Directory::new();
    directory.write(
        "appsettings.Production.json",
        br#"{"Kestrel:Certificates:Default:Path":"production.pfx"}"#,
    );
    let (settings, root, _) =
        configuration(&HashMap::new(), &directory.0, Path::new("/application")).unwrap();
    assert_eq!(root, directory.0);
    assert_eq!(settings[&format!("{DEFAULT}path")], "production.pfx");
    let env = HashMap::from([("DOTNET_CONTENTROOT".into(), "relative-root".into())]);
    let (_, root, _) = configuration(&env, Path::new("/different-cwd"), &directory.0).unwrap();
    assert_eq!(root, directory.0.join("relative-root"));
    directory.write("appsettings.json", "malformed synthetic-secret");
    assert_eq!(
        configuration(&HashMap::new(), &directory.0, Path::new("/application"))
            .unwrap_err()
            .to_string(),
        "invalid HTTPS appsettings configuration"
    );
}

#[test]
fn certificate_json_null_shadows_lower_values_and_empty_default_remains_present() {
    let directory = Directory::new();
    directory.write(
        "appsettings.json",
        br#"{
        "Kestrel": { "Certificates": {
            "Default": { "KeyPath": null, "Password": null },
            "Development": { "Path": null, "Password": null }
        }}
    }"#,
    );
    let environment = HashMap::from([
        (
            "DOTNET_Kestrel__Certificates__Default__KeyPath".into(),
            "lower-key.pem".into(),
        ),
        (
            "DOTNET_Kestrel__Certificates__Default__Password".into(),
            "lower-password".into(),
        ),
        (
            "DOTNET_Kestrel__Certificates__Development__Path".into(),
            "lower-path.pfx".into(),
        ),
        (
            "DOTNET_Kestrel__Certificates__Development__Password".into(),
            "lower-password".into(),
        ),
    ]);
    let (settings, _, _) =
        configuration(&environment, &directory.0, Path::new("/application")).unwrap();
    for key in [
        format!("{DEFAULT}keypath"),
        format!("{DEFAULT}password"),
        format!("{DEVELOPMENT}path"),
        format!("{DEVELOPMENT}password"),
    ] {
        assert!(!settings.contains_key(&key), "null did not shadow {key}");
    }
    assert!(settings.contains_key(DEFAULT.trim_end_matches(':')));
    directory.write(
        "appsettings.json",
        br#"{"Kestrel:Certificates:Default":{}}"#,
    );
    let (settings, _, _) =
        configuration(&HashMap::new(), &directory.0, Path::new("/application")).unwrap();
    assert!(settings.contains_key(DEFAULT.trim_end_matches(':')));
    assert!(select_identity(&settings, &directory.0, &HashMap::new(), "parrot").is_err());
}

#[test]
fn developer_application_name_is_a_frozen_host_setting() {
    let directory = Directory::new();
    directory.write("appsettings.json", br#"{"ApplicationName":"ignored-json"}"#);
    let mut environment = HashMap::from([
        (
            "ASPNETCORE_APPLICATIONNAME".into(),
            "aspnetcore-name".into(),
        ),
        ("DOTNET_APPLICATIONNAME".into(), "dotnet-name".into()),
        ("APPLICATIONNAME".into(), "ignored-application".into()),
    ]);
    let (_, _, name) =
        configuration(&environment, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(name, "dotnet-name");
    environment.remove("DOTNET_APPLICATIONNAME");
    let (_, _, name) =
        configuration(&environment, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(name, "aspnetcore-name");
    environment.remove("ASPNETCORE_APPLICATIONNAME");
    let (_, _, name) =
        configuration(&environment, &directory.0, Path::new("/application")).unwrap();
    assert_eq!(name, "parrot");
}
