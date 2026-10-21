use parrot_domain::protocol;
use protobuf::Message;
use protobuf::descriptor::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet, ServiceDescriptorProto,
};

#[derive(Debug, Eq, PartialEq)]
struct FieldContract {
    name: String,
    number: i32,
    label: i32,
    field_type: i32,
    type_name: String,
    oneof_name: Option<String>,
    json_name: String,
    proto3_optional: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct EnumContract {
    name: String,
    values: Vec<(String, i32)>,
}

#[derive(Debug, Eq, PartialEq)]
struct MessageContract {
    name: String,
    fields: Vec<FieldContract>,
    oneofs: Vec<String>,
    enums: Vec<EnumContract>,
    nested: Vec<MessageContract>,
}

#[derive(Debug, Eq, PartialEq)]
struct MethodContract {
    name: String,
    input: String,
    output: String,
    client_streaming: bool,
    server_streaming: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ServiceContract {
    name: String,
    methods: Vec<MethodContract>,
}

#[derive(Debug, Eq, PartialEq)]
struct FileContract {
    name: String,
    package: String,
    syntax: String,
    dependencies: Vec<String>,
    messages: Vec<MessageContract>,
    enums: Vec<EnumContract>,
    services: Vec<ServiceContract>,
}

fn field_contract(
    field: &FieldDescriptorProto,
    oneofs: &[protobuf::descriptor::OneofDescriptorProto],
) -> FieldContract {
    FieldContract {
        name: field.name().to_owned(),
        number: field.number.unwrap_or_default(),
        label: field.label.as_ref().map_or(0, |value| value.value()),
        field_type: field.type_.as_ref().map_or(0, |value| value.value()),
        type_name: field.type_name().to_owned(),
        oneof_name: field
            .oneof_index
            .map(|index| oneofs[index as usize].name().to_owned()),
        json_name: field.json_name().to_owned(),
        proto3_optional: field.proto3_optional.unwrap_or_default(),
    }
}

fn enum_contract(value: &EnumDescriptorProto) -> EnumContract {
    EnumContract {
        name: value.name().to_owned(),
        values: value
            .value
            .iter()
            .map(|entry| (entry.name().to_owned(), entry.number.unwrap_or_default()))
            .collect(),
    }
}

fn message_contract(message: &DescriptorProto) -> MessageContract {
    MessageContract {
        name: message.name().to_owned(),
        fields: message
            .field
            .iter()
            .map(|field| field_contract(field, &message.oneof_decl))
            .collect(),
        // Protoc and protobuf-parse may order synthetic proto3-optional
        // oneofs differently. Compare declarations by name and each field's
        // resolved oneof name, not by descriptor-local indexes.
        oneofs: {
            let mut names: Vec<_> = message
                .oneof_decl
                .iter()
                .map(|oneof| oneof.name().to_owned())
                .collect();
            names.sort();
            names
        },
        enums: message.enum_type.iter().map(enum_contract).collect(),
        nested: message.nested_type.iter().map(message_contract).collect(),
    }
}

fn service_contract(service: &ServiceDescriptorProto) -> ServiceContract {
    ServiceContract {
        name: service.name().to_owned(),
        methods: service
            .method
            .iter()
            .map(|method| MethodContract {
                name: method.name().to_owned(),
                input: method.input_type().to_owned(),
                output: method.output_type().to_owned(),
                client_streaming: method.client_streaming.unwrap_or_default(),
                server_streaming: method.server_streaming.unwrap_or_default(),
            })
            .collect(),
    }
}

fn file_contract(file: &FileDescriptorProto) -> FileContract {
    FileContract {
        name: file.name().to_owned(),
        package: file.package().to_owned(),
        syntax: file.syntax().to_owned(),
        dependencies: file.dependency.clone(),
        messages: file.message_type.iter().map(message_contract).collect(),
        enums: file.enum_type.iter().map(enum_contract).collect(),
        services: file.service.iter().map(service_contract).collect(),
    }
}

fn descriptor_contract(bytes: &[u8]) -> Vec<FileContract> {
    let descriptors = FileDescriptorSet::parse_from_bytes(bytes).expect("parse descriptor set");
    let mut files: Vec<_> = descriptors.file.iter().map(file_contract).collect();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    files
}

fn assert_messages_match(expected: &[MessageContract], actual: &[MessageContract], path: &str) {
    assert_eq!(actual.len(), expected.len(), "message count at {path}");
    for (expected, actual) in expected.iter().zip(actual) {
        let current = format!("{path}.{}", expected.name);
        assert_eq!(actual.name, expected.name, "message name at {path}");
        assert_eq!(actual.oneofs, expected.oneofs, "oneof fields at {current}");
        assert_eq!(
            actual.enums, expected.enums,
            "enum names/values at {current}"
        );
        assert_eq!(
            actual.fields.len(),
            expected.fields.len(),
            "field count at {current}"
        );
        for (index, (expected, actual)) in expected.fields.iter().zip(&actual.fields).enumerate() {
            assert_eq!(actual, expected, "field descriptor {index} at {current}");
        }
        assert_messages_match(&expected.nested, &actual.nested, &current);
    }
}

fn assert_file_matches(expected: &FileContract, actual: &FileContract) {
    assert_eq!(actual.name, expected.name, "schema filename");
    assert_eq!(
        actual.package, expected.package,
        "package for {}",
        expected.name
    );
    assert_eq!(
        actual.syntax, expected.syntax,
        "syntax for {}",
        expected.name
    );
    assert_eq!(
        actual.dependencies, expected.dependencies,
        "imports for {}",
        expected.name
    );
    assert_messages_match(&expected.messages, &actual.messages, &expected.name);
    assert_eq!(
        actual.enums, expected.enums,
        "top-level enums for {}",
        expected.name
    );
    assert_eq!(
        actual.services.len(),
        expected.services.len(),
        "service count for {}",
        expected.name
    );
    for (expected_service, actual_service) in expected.services.iter().zip(&actual.services) {
        assert_eq!(
            actual_service.name, expected_service.name,
            "service name in {}",
            expected.name
        );
        assert_eq!(
            actual_service.methods.len(),
            expected_service.methods.len(),
            "method count for {}",
            expected_service.name
        );
        for (expected_method, actual_method) in
            expected_service.methods.iter().zip(&actual_service.methods)
        {
            assert_eq!(
                actual_method, expected_method,
                "method descriptor for {}.{}",
                expected_service.name, expected_method.name
            );
        }
    }
}

#[test]
fn independent_protoc_and_rust_codegen_descriptors_match() {
    let source = include_bytes!("fixtures/source-descriptors.pb");
    let generated = include_bytes!(concat!(env!("OUT_DIR"), "/protocol-descriptors.pb"));
    let expected = descriptor_contract(source);
    let actual = descriptor_contract(generated);
    assert_eq!(actual.len(), expected.len(), "source descriptor file count");
    for expected_file in &expected {
        let actual_file = actual
            .iter()
            .find(|file| file.name == expected_file.name)
            .expect("generated descriptor file");
        assert_file_matches(expected_file, actual_file);
    }
    for (name, generated_file) in [
        (
            "parrot.proto",
            protocol::messages::parrot::file_descriptor().proto(),
        ),
        (
            "web.proto",
            protocol::messages::web::file_descriptor().proto(),
        ),
        (
            "queue.proto",
            protocol::messages::queue::file_descriptor().proto(),
        ),
        (
            "process.proto",
            protocol::messages::process::file_descriptor().proto(),
        ),
    ] {
        let expected_file = expected
            .iter()
            .find(|file| file.name == name)
            .expect("source descriptor file");
        assert_eq!(
            &file_contract(generated_file),
            expected_file,
            "generated messages for {name}"
        );
    }

    let methods: usize = expected
        .iter()
        .flat_map(|file| &file.services)
        .map(|service| service.methods.len())
        .sum();
    assert_eq!(methods, 37);
    assert_eq!(
        include_str!(concat!(
            env!("OUT_DIR"),
            "/protocol-service-method-count.txt"
        )),
        "37"
    );
}
