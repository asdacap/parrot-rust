#[path = "../../build/web_assets.rs"]
mod web_assets;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("parrot-web-assets-{nonce}"));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}

fn write(root: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("test file has a parent")).expect("create parent");
    fs::write(&path, bytes).expect("write test file");
    path
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    let entries = fs::read_dir(directory).expect("read web asset directory");
    for entry in entries {
        let path = entry.expect("read web asset entry").path();
        let metadata = fs::symlink_metadata(&path).expect("stat web asset");
        assert!(!metadata.file_type().is_symlink(), "web dist has a symlink");
        if metadata.is_dir() {
            collect_files(root, &path, files);
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).expect("asset stays under dist");
            let relative = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, fs::read(path).expect("read web asset bytes")));
        }
    }
}

#[test]
fn embeds_all_files_in_sorted_relative_paths_with_content_types_and_original_bytes() {
    let temp = TempDir::new();
    let dist = temp.0.join("dist");
    let index = write(&dist, "index.html", b"<main>retained UI</main>");
    let script = write(&dist, "assets/app.js", b"globalThis.ready=true");
    let style = write(&dist, "assets/site.css", b"body{margin:0}");
    let nested = write(&dist, "assets/fonts/ui.woff2", b"font bytes");
    let output = temp.0.join("out/web_assets.rs");

    web_assets::generate_embedded_assets(&dist, &output).expect("generate embedded table");
    let generated = fs::read_to_string(output).expect("read generated module");

    let expected = [
        ("assets/app.js", "text/javascript; charset=utf-8", script),
        ("assets/fonts/ui.woff2", "font/woff2", nested),
        ("assets/site.css", "text/css; charset=utf-8", style),
        ("index.html", "text/html; charset=utf-8", index),
    ];
    let mut previous = None;
    for (path, content_type, source) in expected {
        let entry = format!(
            "EmbeddedWebAsset {{ path: {path:?}, content_type: {content_type:?}, bytes: include_bytes!({:?}) }},",
            source.to_string_lossy(),
        );
        assert!(
            generated.contains(&entry),
            "missing generated entry {entry}"
        );
        assert_eq!(
            fs::read(source).expect("read source bytes"),
            match path {
                "assets/app.js" => b"globalThis.ready=true".as_slice(),
                "assets/fonts/ui.woff2" => b"font bytes".as_slice(),
                "assets/site.css" => b"body{margin:0}".as_slice(),
                "index.html" => b"<main>retained UI</main>".as_slice(),
                _ => unreachable!(),
            }
        );
        if let Some(previous) = previous {
            assert!(previous < path, "asset entries are not sorted");
        }
        previous = Some(path);
    }
}

#[test]
fn registered_host_exposes_the_generated_vite_assets() {
    let assets = parrot_host::web_assets::EMBEDDED_WEB_ASSETS;
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dist = crate_dir.join("../../web/dist");
    let mut expected = Vec::new();
    collect_files(&dist, &dist, &mut expected);
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(assets.len(), expected.len());
    for (asset, (path, bytes)) in assets.iter().zip(expected) {
        assert_eq!(asset.path, path);
        assert_eq!(asset.bytes, bytes);
    }
    assert!(assets.len() >= 3);
    let index = assets
        .iter()
        .find(|asset| asset.path == "index.html")
        .expect("host embeds the SPA entry point");
    assert_eq!(index.content_type, "text/html; charset=utf-8");
    assert!(
        std::str::from_utf8(index.bytes)
            .expect("embedded index is UTF-8")
            .contains("<div id=\"root\"></div>")
    );
    assert!(
        assets
            .iter()
            .any(|asset| asset.path.starts_with("assets/") && asset.path.ends_with(".js"))
    );
    assert!(
        assets
            .iter()
            .any(|asset| asset.path.starts_with("assets/") && asset.path.ends_with(".css"))
    );
    assert!(assets.iter().all(|asset| !asset.bytes.is_empty()));
}

#[test]
fn missing_build_output_fails_instead_of_emitting_a_placeholder() {
    let temp = TempDir::new();
    let error =
        web_assets::generate_embedded_assets(&temp.0.join("missing"), &temp.0.join("out.rs"))
            .expect_err("missing dist must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(error.to_string().contains("retained web build is missing"));
    assert!(!temp.0.join("out.rs").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_build_assets_fail_instead_of_silently_disappearing() {
    let temp = TempDir::new();
    let dist = temp.0.join("dist");
    write(&dist, "index.html", b"index");
    std::os::unix::fs::symlink("index.html", dist.join("alias.html")).expect("make symlink");

    let error = web_assets::generate_embedded_assets(&dist, &temp.0.join("out.rs"))
        .expect_err("symlink must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("web build contains a symlink"));
}
