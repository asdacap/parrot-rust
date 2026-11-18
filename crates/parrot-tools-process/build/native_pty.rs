//! Builds the retained Linux PTY bridge, not a sandbox or an emulated helper.

use std::env;
use std::path::PathBuf;
use std::process::Command;

pub fn build() {
    println!("cargo:rerun-if-changed=native/parrot-pty-attach.c");
    println!("cargo:rerun-if-changed=build/native_pty.rs");
    println!("cargo:rerun-if-env-changed=CC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let target = env::var("TARGET").expect("Cargo TARGET");
    let host = env::var("HOST").expect("Cargo HOST");
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    assert!(
        target == host || env::var_os("CC").is_some(),
        "cross-compiling the Linux PTY helper requires an explicit target CC"
    );
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join("parrot-pty-attach");
    let status = Command::new(compiler)
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg("native/parrot-pty-attach.c")
        .arg("-o")
        .arg(&output)
        .status()
        .expect("start the native PTY helper compiler");
    assert!(
        status.success(),
        "native PTY helper compilation failed: {status}"
    );
    println!("cargo:rustc-env=PARROT_PTY_HELPER={}", output.display());
}
