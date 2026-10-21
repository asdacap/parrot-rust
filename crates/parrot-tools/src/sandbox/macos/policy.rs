use std::cmp::Ordering;
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeatbeltAccess {
    pub read: bool,
    pub write: bool,
}

#[derive(Clone, Debug)]
struct AccessRule {
    path: String,
    access: SeatbeltAccess,
}

/// Ordered lexical-path rules, initially allowing reads and denying writes.
#[derive(Clone, Debug)]
pub struct SeatbeltPolicy {
    rules: Vec<AccessRule>,
}

/// Captured policy text, independent of subsequent rule or filesystem changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeatbeltPolicySnapshot {
    text: String,
}

impl SeatbeltPolicySnapshot {
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Default for SeatbeltPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl SeatbeltPolicy {
    pub fn new() -> Self {
        Self {
            rules: vec![AccessRule {
                path: "/".to_owned(),
                access: SeatbeltAccess {
                    read: true,
                    write: false,
                },
            }],
        }
    }

    /// Translates already supplied effective rules, without materializing or approving them.
    pub fn compile_rules<P>(
        rules: impl IntoIterator<Item = (P, SeatbeltAccess)>,
    ) -> io::Result<SeatbeltPolicySnapshot>
    where
        P: AsRef<str>,
    {
        let mut policy = Self::new();
        policy.allow_write("/dev/null")?;
        for (path, access) in rules {
            let path = path.as_ref();
            if !access.read {
                policy.deny_read(path)?;
            } else if access.write {
                policy.allow_write(path)?;
            } else {
                policy.allow_read(path)?;
                policy.deny_write(path)?;
            }
        }
        policy.capture()
    }

    pub fn allow_read(&mut self, path: &str) -> io::Result<()> {
        let access = self.evaluate(path)?;
        self.set(path, true, access.write)
    }

    pub fn allow_write(&mut self, path: &str) -> io::Result<()> {
        self.set(path, true, true)
    }

    pub fn deny_read(&mut self, path: &str) -> io::Result<()> {
        self.set(path, false, false)
    }

    pub fn deny_write(&mut self, path: &str) -> io::Result<()> {
        let access = self.evaluate(path)?;
        self.set(path, access.read, false)
    }

    /// The last containing rule wins, not the most specific rule.
    pub fn evaluate(&self, path: &str) -> io::Result<SeatbeltAccess> {
        let path = normalize(path)?;
        Ok(self.evaluate_normalized(&path))
    }

    pub fn capture(&self) -> io::Result<SeatbeltPolicySnapshot> {
        let mut paths: Vec<&str> = self.rules.iter().map(|rule| rule.path.as_str()).collect();
        paths.sort_by(|a, b| utf16_len(a).cmp(&utf16_len(b)).then_with(|| ordinal(a, b)));
        paths.dedup();
        let states: Vec<_> = paths
            .iter()
            .map(|path| self.evaluate_normalized(path))
            .collect();
        let mut text = String::from("(version 1)\n(allow default)\n");
        append_deny_rules(&mut text, "file-read*", &paths, &states, |access| {
            access.read
        })?;
        append_deny_rules(&mut text, "file-write*", &paths, &states, |access| {
            access.write
        })?;
        Ok(SeatbeltPolicySnapshot { text })
    }

    fn evaluate_normalized(&self, path: &str) -> SeatbeltAccess {
        self.rules
            .iter()
            .rev()
            .find(|rule| contains(&rule.path, path))
            .expect("the root rule contains every normalized Unix path")
            .access
    }

    fn set(&mut self, path: &str, read: bool, write: bool) -> io::Result<()> {
        self.rules.push(AccessRule {
            path: normalize(path)?,
            access: SeatbeltAccess { read, write },
        });
        Ok(())
    }
}

pub(super) fn ordinal(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn contains(root: &str, path: &str) -> bool {
    root == "/"
        || root == path
        || path
            .strip_prefix(root)
            .is_some_and(|tail| tail.starts_with('/'))
}

// GetFullPath + TrimEndingDirectorySeparator on Unix: lexical, not realpath.
fn normalize(path: &str) -> io::Result<String> {
    if path.is_empty() || path.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Seatbelt paths must be nonempty and cannot contain NUL.",
        ));
    }
    let full = if path.starts_with('/') {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir()?;
        let cwd = cwd.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "The current directory is not UTF-8.",
            )
        })?;
        format!("{cwd}/{path}")
    };
    let mut components = Vec::new();
    for component in full.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    Ok(format!("/{}", components.join("/")))
}

fn append_deny_rules(
    text: &mut String,
    operation: &str,
    paths: &[&str],
    states: &[SeatbeltAccess],
    select: impl Fn(SeatbeltAccess) -> bool,
) -> io::Result<()> {
    let transitions: Vec<usize> = (0..paths.len())
        .filter(|&index| {
            let parent = (0..paths.len())
                .filter(|&candidate| candidate != index && contains(paths[candidate], paths[index]))
                .max_by_key(|&candidate| utf16_len(paths[candidate]));
            parent.is_none_or(|parent| select(states[parent]) != select(states[index]))
        })
        .collect();
    for &denied in transitions.iter().filter(|&&index| !select(states[index])) {
        let exceptions: Vec<&str> = transitions
            .iter()
            .copied()
            .filter(|&index| select(states[index]) && contains(paths[denied], paths[index]))
            .filter(|&index| {
                !transitions.iter().copied().any(|parent| {
                    parent != denied
                        && parent != index
                        && contains(paths[denied], paths[parent])
                        && contains(paths[parent], paths[index])
                })
            })
            .map(|index| paths[index])
            .collect();
        text.push_str("(deny ");
        text.push_str(operation);
        append_denied_filter(text, paths[denied], &exceptions)?;
        text.push_str(")\n");
    }
    Ok(())
}

fn append_denied_filter(text: &mut String, denied: &str, exceptions: &[&str]) -> io::Result<()> {
    if exceptions.is_empty() {
        if denied != "/" {
            text.push(' ');
            append_path_filter(text, denied)?;
        }
    } else if denied == "/" {
        text.push_str(" (require-not ");
        append_any_filter(text, exceptions)?;
        text.push(')');
    } else {
        text.push_str(" (require-all ");
        append_path_filter(text, denied)?;
        text.push_str(" (require-not ");
        append_any_filter(text, exceptions)?;
        text.push_str("))");
    }
    Ok(())
}

fn append_any_filter(text: &mut String, paths: &[&str]) -> io::Result<()> {
    if paths.len() == 1 {
        append_path_filter(text, paths[0])?;
    } else {
        text.push_str("(require-any");
        for path in paths {
            text.push(' ');
            append_path_filter(text, path)?;
        }
        text.push(')');
    }
    Ok(())
}

fn append_path_filter(text: &mut String, path: &str) -> io::Result<()> {
    if path
        .chars()
        .any(|character| character < ' ' || character == '\u{7f}')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Seatbelt paths cannot contain control characters.",
        ));
    }
    // File.Exists follows links and returns false for directories, unlike is_file.
    let exists = std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir());
    text.push_str(if exists { "(literal \"" } else { "(subpath \"" });
    for character in path.chars() {
        if character == '\\' || character == '"' {
            text.push('\\');
        }
        text.push(character);
    }
    text.push_str("\")");
    Ok(())
}
