use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const CERTIFICATE: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/server.pem");
const KEY: &[u8] = include_bytes!("../../../parrot-host/tests/grpc/fixtures/server-key.pem");
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
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn explicit_pem_files_load_without_other_configuration() {
    let directory = Directory::new();
    let certificate = directory.0.join("server.pem");
    let key = directory.0.join("key.pem");
    fs::write(&certificate, CERTIFICATE).unwrap();
    fs::write(&key, KEY).unwrap();
    assert!(load(&certificate, &key).is_ok());
}

#[test]
fn unreadable_files_report_the_flag_without_exposing_file_contents() {
    let directory = Directory::new();
    let certificate = directory.0.join("server.pem");
    let key = directory.0.join("key.pem");
    assert_eq!(
        load(&certificate, &key).unwrap_err().to_string(),
        "cannot read HTTPS certificate from --tls-cert-file"
    );
    fs::write(&certificate, CERTIFICATE).unwrap();
    assert_eq!(
        load(&certificate, &key).unwrap_err().to_string(),
        "cannot read HTTPS private key from --tls-key-file"
    );
}
