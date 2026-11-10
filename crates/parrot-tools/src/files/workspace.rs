use std::{collections::BTreeMap, path::Path};

/// Materialized agent path variables only; this value performs no resolution or authorization.
#[derive(Clone, Debug, Default)]
pub struct WorkspacePaths {
    variables: BTreeMap<String, String>,
}

impl WorkspacePaths {
    pub fn new(variables: &BTreeMap<String, String>) -> Self {
        Self {
            variables: variables.clone(),
        }
    }

    pub fn expand_path(&self, path: &str) -> String {
        let bytes = path.as_bytes();
        let mut output = String::with_capacity(path.len());
        let mut copied = 0;
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'$' {
                index += 1;
                continue;
            }
            let start = index;
            let braced = bytes.get(index + 1) == Some(&b'{');
            let name_start = index + if braced { 2 } else { 1 };
            let Some(first) = bytes.get(name_start) else {
                break;
            };
            if !first.is_ascii_alphabetic() && *first != b'_' {
                index += 1;
                continue;
            }
            let mut end = name_start + 1;
            while bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                end += 1;
            }
            if braced && bytes.get(end) != Some(&b'}') {
                index += 1;
                continue;
            }
            let matched_end = end + usize::from(braced);
            output.push_str(&path[copied..start]);
            match self.variables.get(&path[name_start..end]) {
                Some(value) => output.push_str(value),
                None => output.push_str(&path[start..matched_end]),
            }
            copied = matched_end;
            index = matched_end;
        }
        output.push_str(&path[copied..]);
        output
    }

    /// The `$NAME` or `$NAME/suffix` shorthand for an expanded path under the
    /// longest matching variable root, or none when no root matches.
    pub fn shorten(&self, path: &str) -> Option<String> {
        let file = Path::new(path);
        let mut matched: Option<(usize, String)> = None;
        for (name, root) in &self.variables {
            let Ok(suffix) = file.strip_prefix(root) else {
                continue;
            };
            if matched
                .as_ref()
                .is_none_or(|(length, _)| root.len() > *length)
            {
                let display = if suffix.as_os_str().is_empty() {
                    format!("${name}")
                } else {
                    format!("${name}/{}", suffix.display())
                };
                matched = Some((root.len(), display));
            }
        }
        matched.map(|(_, display)| display)
    }
}
