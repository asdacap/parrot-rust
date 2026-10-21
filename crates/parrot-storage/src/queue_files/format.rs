use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::{Error, ErrorKind, MAXIMUM_FILE_BYTES, Metadata, Result};

pub(super) fn read(path: &Path, name: &str) -> Result<(Metadata, Vec<String>)> {
    let info = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(name)
        } else {
            Error::io("queue: could not inspect file", error)
        }
    })?;
    if info.len() > MAXIMUM_FILE_BYTES as u64 {
        return Err(Error::too_large());
    }
    let data = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(name)
        } else {
            Error::io("queue: could not read file", error)
        }
    })?;
    let newline = data
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| Error::queue("queue: could not read metadata line"))?;
    let parsed: Option<ParsedMetadata> =
        serde_json::from_slice(&data[..newline]).map_err(|_| malformed())?;
    let parsed = parsed.ok_or_else(|| Error::queue("queue: metadata is null"))?;
    if parsed.name.as_deref() != Some(name) {
        return Err(Error::queue("queue: metadata name does not match path"));
    }
    let description = parsed.description;
    let closed = parsed.closed;
    let mut items = Vec::new();
    let mut offset = newline + 1;
    while offset < data.len() {
        let end = data[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(data.len(), |length| offset + length);
        match serde_json::from_slice::<Value>(&data[offset..end]).map_err(|_| malformed())? {
            Value::Null => return Err(Error::queue("queue: item is null")),
            Value::String(value) => items.push(value),
            _ => return Err(malformed()),
        }
        offset = end + 1;
    }
    Ok((
        Metadata {
            name: name.to_owned(),
            description,
            closed,
        },
        items,
    ))
}

struct ParsedMetadata {
    name: Option<String>,
    description: Option<String>,
    closed: bool,
}

impl<'de> serde::Deserialize<'de> for ParsedMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct MetadataVisitor;
        impl<'de> serde::de::Visitor<'de> for MetadataVisitor {
            type Value = ParsedMetadata;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("queue metadata object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut name = None;
                let mut seen_name = false;
                let mut description = None;
                let mut closed = false;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            name = map.next_value::<Option<String>>()?;
                            seen_name = true;
                        }
                        "description" => description = map.next_value::<Option<String>>()?,
                        "closed" => closed = map.next_value::<bool>()?,
                        _ => {
                            return Err(serde::de::Error::unknown_field(
                                &key,
                                &["name", "description", "closed"],
                            ));
                        }
                    }
                }
                if !seen_name {
                    return Err(serde::de::Error::missing_field("name"));
                }
                Ok(ParsedMetadata {
                    name,
                    description,
                    closed,
                })
            }
        }
        deserializer.deserialize_map(MetadataVisitor)
    }
}

fn malformed() -> Error {
    Error::queue("queue: malformed JSONL")
}

// System.Text.Json's default JavaScriptEncoder emits ASCII JSON with uppercase
// UTF-16 escapes, including its HTML-sensitive ASCII block list.
fn string(value: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' if !matches!(character, '"' | '\'' | '&' | '+' | '<' | '>' | '`') => {
                out.push(character)
            }
            _ => {
                for unit in character.encode_utf16(&mut [0; 2]).iter() {
                    write!(out, "\\u{unit:04X}").expect("writing a string cannot fail");
                }
            }
        }
    }
    out.push('"');
}

pub(super) fn encode(metadata: &Metadata, items: &[String]) -> Result<Vec<u8>> {
    let mut output = String::from("{\"name\":");
    string(&metadata.name, &mut output);
    if let Some(description) = &metadata.description {
        output.push_str(",\"description\":");
        string(description, &mut output);
    }
    if metadata.closed {
        output.push_str(",\"closed\":true");
    }
    output.push_str("}\n");
    for item in items {
        string(item, &mut output);
        output.push('\n');
    }
    if output.len() > MAXIMUM_FILE_BYTES {
        return Err(Error::too_large());
    }
    Ok(output.into_bytes())
}

pub(super) fn write(
    path: &Path,
    metadata: &Metadata,
    items: &[String],
    create: bool,
) -> Result<()> {
    let bytes = encode(metadata, items)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        ^ u128::from(serial);
    let temporary = path.with_file_name(format!(
        ".{}-{stamp:032x}",
        path.file_name().unwrap().to_string_lossy()
    ));
    let create_failure = |error: std::io::Error| {
        let kind = if error.kind() == std::io::ErrorKind::PermissionDenied {
            ErrorKind::UnauthorizedAccess
        } else {
            ErrorKind::Io
        };
        Error::new(kind, error.to_string())
    };
    let persist = || -> Result<()> {
        let write_temporary = || -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        };
        if create {
            write_temporary().map_err(create_failure)?;
            // Publish atomically without replacing an existing destination.
            #[cfg(all(test, unix))]
            super::publication_protocol_tests::publication_barrier(
                super::publication_protocol_tests::PublicationKind::CreateHardLink,
                super::publication_protocol_tests::PublicationPoint::BeforePublicationAfterTempSync,
                path,
            );
            fs::hard_link(&temporary, path).map_err(|error| {
                if error.kind() != std::io::ErrorKind::PermissionDenied && path.is_file() {
                    Error::new(
                        ErrorKind::AlreadyExists,
                        format!("queue: '{}' already exists", metadata.name),
                    )
                } else {
                    create_failure(error)
                }
            })?;
            #[cfg(all(test, unix))]
            super::publication_protocol_tests::publication_barrier(
                super::publication_protocol_tests::PublicationKind::CreateHardLink,
                super::publication_protocol_tests::PublicationPoint::AfterPublicationBeforeCleanup,
                path,
            );
        } else {
            let overwrite = || -> std::io::Result<()> {
                write_temporary()?;
                #[cfg(all(test, unix))]
                super::publication_protocol_tests::publication_barrier(
                    super::publication_protocol_tests::PublicationKind::OverwriteRename,
                    super::publication_protocol_tests::PublicationPoint::BeforePublicationAfterTempSync,
                    path,
                );
                fs::rename(&temporary, path)?;
                #[cfg(all(test, unix))]
                super::publication_protocol_tests::publication_barrier(
                    super::publication_protocol_tests::PublicationKind::OverwriteRename,
                    super::publication_protocol_tests::PublicationPoint::AfterPublicationBeforeCleanup,
                    path,
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                }
                Ok(())
            };
            overwrite().map_err(|error| Error::io("queue: could not persist file", error))?;
        }
        Ok(())
    };
    let result = persist();
    let _ = fs::remove_file(&temporary);
    result
}
