use std::{fs, path::Path};

fn read_sources(directory: &Path, sources: &mut Vec<String>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(fs::read_to_string(path).unwrap());
        }
    }
}

#[test]
fn public_traits_are_limited_to_services_and_cancellation() {
    let mut sources = Vec::new();
    read_sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut sources,
    );
    let mut traits = sources
        .iter()
        .flat_map(|source| source.lines())
        .filter_map(|line| line.trim().strip_prefix("pub trait "))
        .map(|declaration| declaration.split([':', '<', ' ']).next().unwrap())
        .collect::<Vec<_>>();
    traits.sort_unstable();
    assert_eq!(
        traits,
        [
            "Cancellation",
            "EventPublisher",
            "Provider",
            "States",
            "ToolResultMetadata",
            "Tools",
            "TurnHooks",
            "TurnLifecycle"
        ]
    );
}

#[test]
fn public_functions_do_not_accept_callback_contracts() {
    let mut sources = Vec::new();
    read_sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut sources,
    );
    for source in sources {
        let mut lines = source.lines();
        while let Some(line) = lines.next() {
            let line = line.trim();
            if !line.starts_with("pub fn ") && !line.starts_with("pub async fn ") {
                continue;
            }
            let mut signature = line.to_owned();
            while !signature.contains('{') {
                signature.push_str(lines.next().expect("public function has a body"));
            }
            let callback = signature
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .any(|token| matches!(token, "Fn" | "FnMut" | "FnOnce"));
            assert!(
                !callback,
                "public callback contract is not permitted: {signature}"
            );
        }
    }
}
