use std::{
    fs,
    path::{Path, PathBuf},
};

fn read_sources(directory: &Path, sources: &mut Vec<(PathBuf, String)>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).unwrap();
            sources.push((path, source));
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
        .flat_map(|(_, source)| source.lines())
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
            "SkillProvider",
            "States",
            "Tools",
            "Turn",
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
    for (_, source) in sources {
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

#[test]
fn public_types_are_declared_in_protocol() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    read_sources(&root, &mut sources);
    let mut violations = Vec::new();
    for (path, source) in sources {
        if path == root.join("protocol.rs") || path.starts_with(root.join("protocol")) {
            continue;
        }
        for (line, declaration) in source.lines().enumerate() {
            let declaration = declaration.trim();
            if ["pub struct ", "pub enum ", "pub trait ", "pub type "]
                .iter()
                .any(|prefix| declaration.starts_with(prefix))
            {
                violations.push(format!("{}:{}: {declaration}", path.display(), line + 1));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "public core types must be declared in protocol:\n{}",
        violations.join("\n")
    );
}
