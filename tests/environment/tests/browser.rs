#[path = "../../../crates/parrot-host/tests/grpc/support/authoritative.rs"]
pub mod authoritative;
#[path = "../../../crates/parrot-host/tests/web/browser_service.rs"]
mod browser_service;

use authoritative::{Fixture, bounded};
use browser_service::NoWeb;
use parrot_host::{grpc::service::GrpcService, web::server::WebServer};
use std::{sync::atomic::Ordering, time::Duration};

#[tokio::test]
async fn unchanged_generated_node_connect_client_calls_both_services_streams_errors_and_cancels() {
    let fixture = Fixture::new().await;
    let web = NoWeb::default();
    let released = web.released.clone();
    let mut server = bounded(WebServer::start(
        GrpcService::new(fixture.service.clone()),
        web,
        0,
    ))
    .await
    .unwrap();
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Caller supplies an already-installed, approved read-only module tree.
    // Missing inputs fail rather than substituting manual framing or installing.
    let modules = std::path::PathBuf::from(
        std::env::var_os("PARROT_T64_BROWSER_MODULES")
            .expect("PARROT_T64_BROWSER_MODULES must name the approved installed browser modules"),
    );
    let common_git = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(common_git.status.success());
    let common_git = std::path::PathBuf::from(String::from_utf8(common_git.stdout).unwrap().trim());
    let output = common_git
        .parent()
        .unwrap()
        .join(".tmp/t64-native")
        .join(format!(
            "node-client-{}",
            parrot_domain::values::identifiers::Identifier::event_id().unwrap()
        ));
    let script = workspace.join("tests/environment/tests/browser-client.cjs");
    let address = format!("http://{}", server.address());
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("node")
                .arg(script)
                .arg(address)
                .arg(workspace)
                .arg(modules)
                .arg(output)
                .output()
        }),
    )
    .await
    .expect("bounded Node Connect observation")
    .unwrap()
    .unwrap();
    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    // Always join transport and actual composition before asserting client result.
    let cancellation_released = tokio::time::timeout(Duration::from_secs(2), async {
        while !released.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    bounded(server.close()).await.unwrap();
    fixture.close().await;
    assert!(
        result.status.success(),
        "actual Node Connect client failed: {}",
        result.status
    );
    assert!(
        cancellation_released,
        "HTTP1 disconnect must release the web upstream subscription before host stop"
    );
}
