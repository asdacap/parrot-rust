//! Build-script entry point for the pinned A001.2 protocol generator.
//!
//! P01 registers this helper from `crates/parrot-domain/build.rs` after adding
//! the approved build dependencies and crate patches. Keep production inputs to
//! the four unchanged schemas in the repository's `proto/` directory.

use protobuf::Message;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMAS: [&str; 4] = ["parrot.proto", "web.proto", "queue.proto", "process.proto"];
const EXPECTED_SERVICE_METHODS: usize = 37;

fn snake_case(value: &str) -> String {
    let mut result = String::new();
    for (index, ch) in value.chars().enumerate() {
        if ch.is_uppercase() && index != 0 {
            result.push('_');
        }
        result.extend(ch.to_lowercase());
    }
    result
}

fn schema_path(proto_dir: &Path, name: &str) -> PathBuf {
    proto_dir.join(name)
}

/// Generate A001.2 messages, all source RPC bindings, and descriptor evidence.
///
/// The containing crate build script must declare the exact approved A001.2
/// `protobuf`/`protobuf-codegen` path patches and pinned Tonic dependencies.
pub fn generate() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let proto_dir = manifest_dir.join("../../proto");
    let input_paths: Vec<_> = SCHEMAS
        .iter()
        .map(|name| schema_path(&proto_dir, name))
        .collect();
    for path in &input_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    protobuf_codegen::Codegen::new()
        .pure()
        .cargo_out_dir("messages")
        .include(&proto_dir)
        .inputs(&input_paths)
        .run_from_script();

    let parsed = protobuf_parse::Parser::new()
        .pure()
        .include(&proto_dir)
        .inputs(&input_paths)
        .parse_and_typecheck()
        .expect("the four pinned Parrot schemas must parse and typecheck");

    let mut symbols = HashMap::new();
    for file in &parsed.file_descriptors {
        let module = file
            .name()
            .strip_suffix(".proto")
            .expect("schema descriptor names end in .proto");
        for message in &file.message_type {
            let full_name = format!(".{}.{}", file.package(), message.name());
            let rust_path = format!("crate::protocol::messages::{module}::{}", message.name());
            assert!(
                symbols.insert(full_name.clone(), rust_path).is_none(),
                "duplicate protobuf message {full_name}"
            );
        }
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let binding_dir = out_dir.join("bindings");
    fs::create_dir_all(&binding_dir).expect("create protocol binding output directory");
    let mut method_count = 0;
    for file in &parsed.file_descriptors {
        for descriptor in &file.service {
            let mut service = tonic_build::manual::Service::builder()
                .name(descriptor.name())
                .package(file.package());
            for method in &descriptor.method {
                let input = symbols
                    .get(method.input_type())
                    .unwrap_or_else(|| panic!("unmapped RPC input type {}", method.input_type()));
                let output = symbols
                    .get(method.output_type())
                    .unwrap_or_else(|| panic!("unmapped RPC output type {}", method.output_type()));
                let mut builder = tonic_build::manual::Method::builder()
                    .name(snake_case(method.name()))
                    .route_name(method.name())
                    .input_type(input)
                    .output_type(output)
                    .codec_path("crate::protocol::codec::RustProtobufCodec");
                if method.client_streaming() {
                    builder = builder.client_streaming();
                }
                if method.server_streaming() {
                    builder = builder.server_streaming();
                }
                service = service.method(builder.build());
                method_count += 1;
            }
            tonic_build::manual::Builder::new()
                .out_dir(&binding_dir)
                .compile(&[service.build()]);
        }
    }
    assert_eq!(
        method_count, EXPECTED_SERVICE_METHODS,
        "all source RPC methods must be generated"
    );

    let mut descriptors = protobuf::descriptor::FileDescriptorSet::new();
    descriptors.file = parsed.file_descriptors;
    fs::write(
        out_dir.join("protocol-descriptors.pb"),
        descriptors
            .write_to_bytes()
            .expect("encode source descriptor set"),
    )
    .expect("write generated source descriptors");
    fs::write(
        out_dir.join("protocol-service-method-count.txt"),
        method_count.to_string(),
    )
    .expect("write generated RPC method count");
}
