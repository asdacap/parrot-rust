//! Ordered YAML representation and source layering rules.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ANCHOR_IDENTITY: AtomicU64 = AtomicU64::new(1);

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag};
use yaml_rust2::scanner::{Marker, Scanner, TScalarStyle, Token, TokenType};

use super::node::{Kind, Node, Style};
use super::{ConfigError, Result};

struct Events {
    events: Vec<Event>,
    flow: BTreeSet<usize>,
    flow_markers: BTreeSet<usize>,
    anchor_names: BTreeMap<usize, String>,
}

impl MarkedEventReceiver for Events {
    fn on_event(&mut self, event: Event, mark: Marker) {
        if matches!(event, Event::SequenceStart(..) | Event::MappingStart(..))
            && self.flow_markers.contains(&mark.index())
        {
            self.flow.insert(self.events.len());
        }
        self.events.push(event);
    }
}

fn tag_text(tag: Option<Tag>) -> String {
    tag.map(|tag| format!("{}{}", tag.handle, tag.suffix))
        .unwrap_or_default()
}

fn equivalent(left: &Node, right: &Node) -> bool {
    match (&left.kind, &right.kind) {
        (Kind::Scalar(left), Kind::Scalar(right)) => left == right,
        (Kind::Sequence(left), Kind::Sequence(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| equivalent(l, r))
        }
        (Kind::Mapping(left), Kind::Mapping(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .iter()
                        .any(|(k, v)| equivalent(key, k) && equivalent(value, v))
                })
        }
        _ => false,
    }
}

fn read_node(
    events: &Events,
    index: &mut usize,
    anchors: &mut BTreeMap<usize, Node>,
) -> Result<Node> {
    let is_flow = events.flow.contains(index);
    let event = events
        .events
        .get(*index)
        .cloned()
        .ok_or_else(|| ConfigError::invalid_data("unexpected end of YAML".to_owned()))?;
    *index += 1;
    let (mut node, anchor) = match event {
        Event::Scalar(value, style, anchor, tag) => (
            Node {
                kind: Kind::Scalar(Some(value)),
                tag: tag_text(tag),
                anchor: None,
                alias: None,
                anchor_identity: None,
                anchor_revision: 0,
                style: match style {
                    TScalarStyle::Plain => Style::Plain,
                    TScalarStyle::SingleQuoted => Style::SingleQuoted,
                    TScalarStyle::DoubleQuoted => Style::DoubleQuoted,
                    TScalarStyle::Literal => Style::Literal,
                    TScalarStyle::Folded => Style::Folded,
                },
            },
            anchor,
        ),
        Event::Alias(anchor) => {
            let mut node = anchors
                .get(&anchor)
                .cloned()
                .ok_or_else(|| ConfigError::invalid_data("unresolved YAML alias".to_owned()))?;
            node.anchor = None;
            node.alias = events.anchor_names.get(&anchor).cloned();
            return Ok(node);
        }
        Event::SequenceStart(anchor, tag) => {
            let mut items = Vec::new();
            while !matches!(events.events.get(*index), Some(Event::SequenceEnd)) {
                items.push(read_node(events, index, anchors)?);
            }
            *index += 1;
            let mut node = Node::sequence(items);
            node.tag = tag_text(tag);
            if is_flow {
                node.style = Style::Flow;
            }
            (node, anchor)
        }
        Event::MappingStart(anchor, tag) => {
            let mut entries: Vec<(Node, Node)> = Vec::new();
            while !matches!(events.events.get(*index), Some(Event::MappingEnd)) {
                let key = read_node(events, index, anchors)?;
                let value = read_node(events, index, anchors)?;
                if entries
                    .iter()
                    .any(|(existing, _)| equivalent(existing, &key))
                {
                    return Err(ConfigError::invalid_data("Duplicate key".to_owned()));
                }
                entries.push((key, value));
            }
            *index += 1;
            let mut node = Node::mapping(entries);
            node.tag = tag_text(tag);
            if is_flow {
                node.style = Style::Flow;
            }
            (node, anchor)
        }
        _ => {
            return Err(ConfigError::invalid_data(
                "unexpected YAML event".to_owned(),
            ));
        }
    };
    if anchor != 0 {
        node.anchor = events.anchor_names.get(&anchor).cloned();
        node.anchor_identity = Some(NEXT_ANCHOR_IDENTITY.fetch_add(1, Ordering::Relaxed));
        anchors.insert(anchor, node.clone());
    }
    Ok(node)
}

/// Parse without inferring scalar types: YamlDotNet's representation model stores
/// scalar text, quoting style and tags independently.
pub fn parse_documents(content: &str) -> Result<Vec<Node>> {
    let tokens: Vec<_> = Scanner::new(content.chars()).collect();
    let mut events = Events {
        events: Vec::new(),
        flow: BTreeSet::new(),
        flow_markers: tokens
            .iter()
            .filter_map(|Token(mark, kind)| {
                matches!(
                    kind,
                    TokenType::FlowSequenceStart | TokenType::FlowMappingStart
                )
                .then_some(mark.index())
            })
            .collect(),
        anchor_names: tokens
            .into_iter()
            .filter_map(|token| match token {
                Token(_, TokenType::Anchor(name)) => Some(name),
                _ => None,
            })
            .enumerate()
            .map(|(index, name)| (index + 1, name))
            .collect(),
    };
    Parser::new_from_str(content)
        .load(&mut events, true)
        .map_err(|error| ConfigError::invalid_data(error.to_string()))?;
    let mut documents = Vec::new();
    let mut index = 0;
    while index < events.events.len() {
        if matches!(events.events[index], Event::DocumentStart) {
            index += 1;
            documents.push(read_node(&events, &mut index, &mut BTreeMap::new())?);
        } else {
            index += 1;
        }
    }
    Ok(documents)
}

/// The source loader uses the first document only, and treats a non-mapping or
/// absent first document as an empty user mapping.
pub fn parse_root(content: &str) -> Result<Node> {
    Ok(parse_documents(content)?
        .into_iter()
        .next()
        .filter(|node| matches!(node.kind, Kind::Mapping(_)))
        .unwrap_or_else(|| Node::mapping(Vec::new())))
}

fn appends_sequence(path: &[String]) -> bool {
    match path {
        [key] => key == "sandbox_rules" || key == "read_only_exec_command_prefixes",
        [parent, key] => parent == "cli_utilities" && (key == "expected" || key == "optional"),
        [parent, _, key] => parent == "profiles" && key == "sandbox_rules",
        _ => false,
    }
}

fn validate_tag(node: &Node, path: &[String]) -> Result<()> {
    if node.tag == "!replace"
        && (!matches!(node.kind, Kind::Sequence(_)) || !appends_sequence(path))
    {
        return Err(ConfigError::invalid_data(format!(
            "{} may use !replace only on an append-enabled sequence",
            path.join(".")
        )));
    }
    Ok(())
}

fn validate_tags(node: &Node, path: &mut Vec<String>) -> Result<()> {
    validate_tag(node, path)?;
    match &node.kind {
        Kind::Mapping(entries) => {
            for (key, value) in entries {
                let key = key.as_str().ok_or_else(|| {
                    ConfigError::invalid_data(
                        "configuration mapping keys must be strings".to_owned(),
                    )
                })?;
                path.push(key.to_owned());
                validate_tags(value, path)?;
                path.pop();
            }
        }
        Kind::Sequence(items) => {
            for item in items {
                validate_tags(item, path)?;
            }
        }
        Kind::Scalar(_) => {}
    }
    Ok(())
}

pub fn validate_replace_tags(node: &Node) -> Result<()> {
    validate_tags(node, &mut Vec::new())
}

// Source CloneMapping deliberately constructs a fresh mapping without copying
// its tag; scalar/sequence clones retain theirs.
fn clone_node(node: &Node) -> Node {
    match &node.kind {
        Kind::Mapping(entries) => Node::mapping(
            entries
                .iter()
                .map(|(key, value)| (clone_node(key), clone_node(value)))
                .collect(),
        ),
        Kind::Sequence(items) => {
            let mut result = Node::sequence(items.iter().map(clone_node).collect());
            result.tag.clone_from(&node.tag);
            result
        }
        Kind::Scalar(_) => {
            let mut result = node.clone();
            result.anchor = None;
            result.alias = None;
            result.anchor_identity = None;
            result.anchor_revision = 0;
            result
        }
    }
}

fn merge_at(defaults: &mut Node, overrides: &Node, path: &mut Vec<String>) -> Result<()> {
    let Kind::Mapping(default_entries) = &mut defaults.kind else {
        return Err(ConfigError::invalid_data(
            "configuration must be a mapping".to_owned(),
        ));
    };
    let Kind::Mapping(override_entries) = &overrides.kind else {
        return Err(ConfigError::invalid_data(
            "configuration must be a mapping".to_owned(),
        ));
    };
    for (key, value) in override_entries {
        let name = key.as_str().ok_or_else(|| {
            ConfigError::invalid_data("configuration mapping keys must be strings".to_owned())
        })?;
        path.push(name.to_owned());
        validate_tag(value, path)?;
        let existing = default_entries
            .iter_mut()
            .find(|(existing, _)| equivalent(existing, key));
        if let Some((_, current)) = existing {
            if matches!(current.kind, Kind::Mapping(_)) && matches!(value.kind, Kind::Mapping(_)) {
                merge_at(current, value, path)?;
            } else if let Kind::Sequence(items) = &value.kind {
                if appends_sequence(path) {
                    let mut result = if value.tag != "!replace" {
                        match &current.kind {
                            Kind::Sequence(base) => {
                                let mut result =
                                    Node::sequence(base.iter().map(clone_node).collect());
                                result.tag.clone_from(&current.tag);
                                result
                            }
                            _ => Node::sequence(Vec::new()),
                        }
                    } else {
                        Node::sequence(Vec::new())
                    };
                    if let Kind::Sequence(result_items) = &mut result.kind {
                        result_items.extend(items.iter().map(clone_node));
                    }
                    *current = result;
                } else {
                    *current = clone_node(value);
                }
            } else {
                *current = clone_node(value);
            }
        } else {
            let mut copied = clone_node(value);
            if matches!(copied.kind, Kind::Sequence(_)) && appends_sequence(path) {
                copied.tag.clear();
            }
            default_entries.push((clone_node(key), copied));
        }
        path.pop();
    }
    Ok(())
}

pub fn merge(defaults: &mut Node, overrides: &Node) -> Result<()> {
    validate_replace_tags(overrides)?;
    merge_at(defaults, overrides, &mut Vec::new())
}

fn validate_composer_keys(node: &Node, name: &str, path: &str) -> Result<()> {
    match &node.kind {
        Kind::Mapping(entries) => {
            for (key, value) in entries {
                let key = key.as_str().ok_or_else(|| {
                    ConfigError::invalid_data(format!(
                        "{name}: configuration mapping keys at {path} must be strings"
                    ))
                })?;
                let child_path = if path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{path}.{key}")
                };
                validate_composer_keys(value, name, &child_path)?;
            }
        }
        Kind::Sequence(items) => {
            for item in items {
                validate_composer_keys(item, name, path)?;
            }
        }
        Kind::Scalar(_) => {}
    }
    Ok(())
}

fn add_owner(
    owners: &mut BTreeMap<String, String>,
    key: &str,
    name: &str,
    path: &str,
) -> Result<()> {
    if let Some(previous) = owners.get(key) {
        return Err(ConfigError::invalid_data(format!(
            "duplicate predefined configuration at {path} in \"{previous}\" and \"{name}\""
        )));
    }
    owners.insert(key.to_owned(), name.to_owned());
    Ok(())
}

pub fn compose(sources: &[(&str, Option<&str>)]) -> Result<String> {
    let mut combined = Node::mapping(Vec::new());
    let mut owners = BTreeMap::new();
    let mut mapping_owners: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (name, content) in sources {
        let content = content.ok_or_else(|| {
            ConfigError::invalid_data(format!(
                "predefined configuration resource \"{name}\" was not embedded in this build"
            ))
        })?;
        let documents = parse_documents(content).map_err(|_| {
            ConfigError::invalid_data(format!("{name}: invalid predefined configuration YAML"))
        })?;
        if documents.len() != 1 || !matches!(documents[0].kind, Kind::Mapping(_)) {
            return Err(ConfigError::invalid_data(format!(
                "{name}: predefined configuration must contain exactly one mapping document"
            )));
        }
        let root = &documents[0];
        validate_composer_keys(root, name, "")?;
        for (key, value) in root.as_mapping().expect("mapping checked") {
            let key_text = key.as_str().expect("keys checked");
            if matches!(key_text, "prompt_templates" | "system_prompts" | "tools") {
                let entries = value.as_mapping().ok_or_else(|| {
                    ConfigError::invalid_data(format!("{name}: {key_text} must be a mapping"))
                })?;
                if combined.get(key_text).is_none() {
                    combined.set(key_text, Node::mapping(Vec::new()));
                }
                let item_owners = mapping_owners.entry(key_text.to_owned()).or_default();
                for (id, item) in entries {
                    let id_text = id.as_str().expect("keys checked");
                    add_owner(item_owners, id_text, name, &format!("{key_text}.{id_text}"))?;
                    if let Some(Node {
                        kind: Kind::Mapping(target),
                        ..
                    }) = combined.get_mut(key_text)
                    {
                        target.push((id.clone(), item.clone()));
                    }
                }
            } else {
                add_owner(&mut owners, key_text, name, key_text)?;
                if let Kind::Mapping(target) = &mut combined.kind {
                    target.push((key.clone(), value.clone()));
                }
            }
        }
    }
    Ok(compose_with_end(&combined))
}

fn spaces(indent: usize) -> String {
    " ".repeat(indent)
}

fn tag_prefix(node: &Node) -> String {
    let mut prefix = node
        .anchor
        .as_ref()
        .map(|name| format!("&{name} "))
        .unwrap_or_default();
    if let Some(suffix) = node.tag.strip_prefix("tag:yaml.org,2002:") {
        prefix.push_str(&format!("!!{suffix} "));
    } else if node.tag.starts_with('!') {
        prefix.push_str(&format!("{} ", node.tag));
    } else if !node.tag.is_empty() {
        prefix.push_str(&format!("!<{}> ", node.tag));
    }
    prefix
}

fn double_quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\0' => output.push_str("\\0"),
            character if u32::from(character) > 0xFFFF => {
                output.push_str(&format!("\\U{:08X}", u32::from(character)))
            }
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04X}", u32::from(character)))
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

fn safe_plain(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value.starts_with([
            '!', '&', '*', '{', '}', '[', ']', ',', '#', '|', '>', '@', '`', '"', '\'',
        ])
        && !value.starts_with("- ")
        && !value.starts_with("? ")
        && !value.starts_with(": ")
        && !value.contains(": ")
        && !value.ends_with(':')
        && !value.contains(" #")
        && !value.chars().any(char::is_control)
        && !matches!(value, "---" | "...")
}

fn flow_inline(node: &Node) -> String {
    if let Some(name) = &node.alias {
        return format!("*{name}");
    }
    let value = match &node.kind {
        Kind::Mapping(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{}: {}", flow_inline(key), flow_inline(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Kind::Sequence(items) => format!(
            "[{}]",
            items.iter().map(flow_inline).collect::<Vec<_>>().join(", ")
        ),
        Kind::Scalar(None) => String::new(),
        Kind::Scalar(Some(value)) => match node.style {
            Style::DoubleQuoted | Style::Literal | Style::Folded => double_quote(value),
            Style::SingleQuoted if !value.contains('\n') => {
                format!("'{}'", value.replace('\'', "''"))
            }
            _ if safe_plain(value) && !value.contains([',', '[', ']', '{', '}']) => value.clone(),
            _ => double_quote(value),
        },
    };
    format!("{}{value}", tag_prefix(node))
}

fn single_quoted_allowed(value: &str) -> bool {
    let chars: Vec<_> = value.chars().collect();
    !chars
        .iter()
        .any(|c| (c.is_control() && !is_break(*c)) || u32::from(*c) > 0xFFFF)
        && !chars.windows(2).any(|pair| {
            (is_break(pair[0]) && matches!(pair[1], ' ' | '\t'))
                || (matches!(pair[0], ' ' | '\t') && is_break(pair[1]))
        })
}

fn inline(node: &Node) -> Option<String> {
    if let Some(name) = &node.alias {
        return Some(format!("*{name}"));
    }
    if node.style == Style::Flow {
        return Some(flow_inline(node));
    }
    let value = match &node.kind {
        Kind::Mapping(entries) if entries.is_empty() => "{}".to_owned(),
        Kind::Sequence(items) if items.is_empty() => "[]".to_owned(),
        Kind::Scalar(None) => String::new(),
        Kind::Scalar(Some(value)) => {
            if matches!(node.style, Style::Literal | Style::Folded) {
                return None;
            }
            match node.style {
                Style::SingleQuoted if !single_quoted_allowed(value) => double_quote(value),
                Style::SingleQuoted if value.contains('\n') => return None,
                Style::SingleQuoted => format!("'{}'", value.replace('\'', "''")),
                Style::Plain
                    if value.contains('\n')
                        && (!single_quoted_allowed(value) || value.contains('\'')) =>
                {
                    double_quote(value)
                }
                Style::DoubleQuoted => double_quote(value),
                Style::Plain if value.is_empty() => String::new(),
                _ if value
                    .chars()
                    .any(|c| u32::from(c) > 0xFFFF || (c.is_control() && c != '\n')) =>
                {
                    double_quote(value)
                }
                _ if safe_plain(value) => value.clone(),
                _ if value.contains('\n') => return None,
                _ => format!("'{}'", value.replace('\'', "''")),
            }
        }
        _ => return None,
    };
    Some(format!("{}{value}", tag_prefix(node)))
}

fn is_break(value: char) -> bool {
    matches!(value, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn write_scalar_block(node: &Node, indent: usize, output: &mut String) {
    if matches!(node.style, Style::Plain | Style::SingleQuoted) {
        output.push_str(&tag_prefix(node));
        output.push('\'');
        let mut previous_break = false;
        for character in node.as_str().unwrap_or("").chars() {
            if is_break(character) {
                if !previous_break && character == '\n' {
                    output.push('\n');
                }
                output.push(if matches!(character, '\r' | '\u{85}') {
                    '\n'
                } else {
                    character
                });
                previous_break = true;
            } else {
                if previous_break {
                    output.push_str(&spaces(indent));
                }
                if character == '\'' {
                    output.push('\'');
                }
                output.push(character);
                previous_break = false;
            }
        }
        output.push('\'');
        output.push('\n');
        return;
    }
    let value: Vec<char> = node.as_str().unwrap_or("").chars().collect();
    let folded = matches!(node.style, Style::Any | Style::Folded);
    output.push_str(&tag_prefix(node));
    output.push(if folded { '>' } else { '|' });
    if value.first().is_some_and(|c| *c == ' ' || is_break(*c)) {
        output.push('2');
    }
    if !value.last().is_some_and(|c| is_break(*c)) {
        output.push('-');
    } else if value.len() >= 2 && is_break(value[value.len() - 2]) {
        output.push('+');
    }
    output.push('\n');
    let mut at_line_start = true;
    let mut indented_line = true;
    for (index, character) in value.iter().copied().enumerate() {
        if character == '\r' && value.get(index + 1) == Some(&'\n') {
            continue;
        }
        if is_break(character) {
            let line_break = match character {
                '\r' | '\u{85}' => '\n',
                other => other,
            };
            if folded && !at_line_start && !indented_line && line_break == '\n' {
                let next = value[index..].iter().copied().find(|c| !is_break(*c));
                if next.is_some_and(|c| !matches!(c, ' ' | '\t')) {
                    output.push('\n');
                }
            }
            output.push(line_break);
            at_line_start = true;
        } else {
            if at_line_start {
                output.push_str(&spaces(indent));
                indented_line = matches!(character, ' ' | '\t');
            }
            output.push(character);
            at_line_start = false;
        }
    }
    if !at_line_start {
        output.push('\n');
    }
}

fn write_mapping(entries: &[(Node, Node)], indent: usize, output: &mut String) {
    for (key, value) in entries {
        output.push_str(&spaces(indent));
        output.push_str(&inline(key).unwrap_or_else(|| double_quote(key.as_str().unwrap_or(""))));
        output.push(':');
        if let Some(text) = inline(value) {
            output.push(' ');
            output.push_str(&text);
            output.push('\n');
        } else {
            match &value.kind {
                Kind::Mapping(children) => {
                    let properties = tag_prefix(value);
                    if !properties.is_empty() {
                        output.push(' ');
                        output.push_str(properties.trim_end());
                    }
                    output.push('\n');
                    write_mapping(children, indent + 2, output);
                }
                Kind::Sequence(children) => {
                    let properties = tag_prefix(value);
                    if !properties.is_empty() {
                        output.push(' ');
                        output.push_str(properties.trim_end());
                    }
                    output.push('\n');
                    write_sequence(children, indent, output);
                }
                Kind::Scalar(_) => {
                    output.push(' ');
                    write_scalar_block(value, indent + 2, output);
                }
            }
        }
    }
}

fn write_sequence(items: &[Node], indent: usize, output: &mut String) {
    for item in items {
        output.push_str(&spaces(indent));
        output.push('-');
        if let Some(text) = inline(item) {
            if !text.is_empty() {
                output.push(' ');
                output.push_str(&text);
            }
            output.push('\n');
        } else {
            match &item.kind {
                Kind::Scalar(_) => {
                    output.push(' ');
                    write_scalar_block(item, indent + 2, output);
                }
                _ => {
                    let mut body = String::new();
                    match &item.kind {
                        Kind::Mapping(entries) => write_mapping(entries, indent + 2, &mut body),
                        Kind::Sequence(children) => write_sequence(children, indent + 2, &mut body),
                        Kind::Scalar(_) => unreachable!(),
                    }
                    if item.tag.is_empty() && item.anchor.is_none() {
                        output.push(' ');
                        output.push_str(body.strip_prefix(&spaces(indent + 2)).unwrap_or(&body));
                    } else {
                        output.push(' ');
                        output.push_str(tag_prefix(item).trim_end());
                        output.push('\n');
                        output.push_str(&body);
                    }
                }
            }
        }
    }
}

// Source aliases reference the anchored node, not its original mapping slot.
// If a write replaces that slot, the retained node must be defined at its first
// surviving occurrence before later references can use the alias name.
pub(crate) fn latest_alias_occurrence<'a>(root: &'a Node, value: &'a Node) -> &'a Node {
    let Some(identity) = value.anchor_identity else {
        return value;
    };
    fn visit<'a>(node: &'a Node, identity: u64, current: &mut &'a Node) {
        if node.anchor_identity == Some(identity) && node.anchor_revision > current.anchor_revision
        {
            *current = node;
        }
        match &node.kind {
            Kind::Mapping(entries) => {
                for (key, value) in entries {
                    visit(key, identity, current);
                    visit(value, identity, current);
                }
            }
            Kind::Sequence(items) => {
                for item in items {
                    visit(item, identity, current);
                }
            }
            Kind::Scalar(_) => {}
        }
    }
    let mut current = value;
    visit(root, identity, &mut current);
    current
}

fn collect_anchor_payloads(node: &Node, payloads: &mut BTreeMap<u64, Node>) {
    if let Some(identity) = node.anchor_identity {
        if payloads
            .get(&identity)
            .is_none_or(|existing| node.anchor_revision > existing.anchor_revision)
        {
            payloads.insert(identity, node.clone());
        }
    }
    match &node.kind {
        Kind::Mapping(entries) => {
            for (key, value) in entries {
                collect_anchor_payloads(key, payloads);
                collect_anchor_payloads(value, payloads);
            }
        }
        Kind::Sequence(items) => {
            for item in items {
                collect_anchor_payloads(item, payloads);
            }
        }
        Kind::Scalar(_) => {}
    }
}

fn apply_anchor_payloads(node: &mut Node, payloads: &BTreeMap<u64, Node>) {
    if let Some(current) = node
        .anchor_identity
        .and_then(|identity| payloads.get(&identity))
    {
        node.kind.clone_from(&current.kind);
        node.tag.clone_from(&current.tag);
        node.style = current.style;
        node.anchor_revision = current.anchor_revision;
    }
    match &mut node.kind {
        Kind::Mapping(entries) => {
            for (key, value) in entries {
                apply_anchor_payloads(key, payloads);
                apply_anchor_payloads(value, payloads);
            }
        }
        Kind::Sequence(items) => {
            for item in items {
                apply_anchor_payloads(item, payloads);
            }
        }
        Kind::Scalar(_) => {}
    }
}

// Synchronize before yielding a mutable occurrence, not just while emitting.
// A subsequent edit through another alias therefore starts from all preceding
// edits, including distinct fields changed through different occurrences.
pub(crate) fn synchronize_aliases(node: &mut Node) {
    let mut payloads = BTreeMap::new();
    collect_anchor_payloads(node, &mut payloads);
    apply_anchor_payloads(node, &payloads);
}

fn retain_anchor_definitions(
    node: &mut Node,
    emitted: &mut BTreeSet<(Option<u64>, String)>,
    payloads: &BTreeMap<u64, Node>,
) {
    if let Some(current) = node
        .anchor_identity
        .and_then(|identity| payloads.get(&identity))
    {
        node.kind.clone_from(&current.kind);
        node.tag.clone_from(&current.tag);
        node.style = current.style;
        node.anchor_revision = current.anchor_revision;
    }
    if let Some(name) = node.anchor.as_ref().or(node.alias.as_ref()).cloned() {
        let key = (
            node.anchor_identity,
            if node.anchor_identity.is_none() {
                name.clone()
            } else {
                String::new()
            },
        );
        if !emitted.insert(key) {
            node.anchor = None;
            node.alias = Some(name);
            return;
        }
        node.anchor = Some(name);
        node.alias = None;
    }
    match &mut node.kind {
        Kind::Mapping(entries) => {
            for (key, value) in entries {
                retain_anchor_definitions(key, emitted, payloads);
                retain_anchor_definitions(value, emitted, payloads);
            }
        }
        Kind::Sequence(items) => {
            for item in items {
                retain_anchor_definitions(item, emitted, payloads);
            }
        }
        Kind::Scalar(_) => {}
    }
}

/// Deterministic block YAML without the explicit document-end marker used by
/// the source stream emitter. User writes always end in one newline.
pub fn serialize(root: &Node) -> String {
    let mut retained = root.clone();
    let mut payloads = BTreeMap::new();
    collect_anchor_payloads(root, &mut payloads);
    retain_anchor_definitions(&mut retained, &mut BTreeSet::new(), &payloads);
    let root = &retained;
    let mut output = String::new();
    if let Some(value) = inline(root) {
        output.push_str(&value);
        output.push('\n');
    } else {
        match &root.kind {
            Kind::Mapping(entries) => write_mapping(entries, 0, &mut output),
            Kind::Sequence(items) => write_sequence(items, 0, &mut output),
            Kind::Scalar(_) => write_scalar_block(root, 2, &mut output),
        }
    }
    output.truncate(output.trim_end().len());
    output.push('\n');
    output
}

pub fn compose_with_end(root: &Node) -> String {
    format!("{}...\n", serialize(root))
}
