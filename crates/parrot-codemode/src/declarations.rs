//! Model-facing TypeScript declarations, ported from pi's `declarations.ts`
//! (~/repo/pi/packages/codemode, MIT licensed). JSON Schema in, TypeScript
//! type expressions out, with pi's shapes, limits, and tag rules.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Characters per token when estimating the cost of a tool section.
pub const CHARS_PER_TOKEN: usize = 4;
/// Default inline budget for tool sections in estimated tokens.
pub const DEFAULT_INLINE_BUDGET: usize = 12_000;
/// Largest rendered input type, in characters, before it becomes `unknown`.
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema, so shared definitions cannot blow up the output.
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords with a number or string value shown as `@keyword value` property tags.
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
/// Tag keywords whose values all hold at once, so only the largest (lower) or smallest (upper) matters.
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest property tag; longer ones (large defaults, long patterns) are left out.
const MAX_TAG_CHARS: usize = 80;
const INDENT: &str = "  ";

/// One tool the script may call, as declarations need it.
#[derive(Clone, Debug)]
pub struct DeclaredTool {
    pub name: String,
    pub description: String,
    /// The tool's JSON Schema parameters.
    pub parameters_json: Value,
    /// The tool's optional JSON Schema output shape.
    pub output_schema: Option<Value>,
}

/// One tool as the script sees it: `tools.<js_name>` with its rendered declaration.
#[derive(Clone, Debug)]
pub struct ToolDeclaration {
    /// The identifier-safe name the script calls.
    pub js_name: String,
    /// The tool's raw name.
    pub name: String,
    pub description: String,
    /// The rendered input type expression.
    pub input_type: String,
    /// The rendered output type expression, `unknown` without a schema.
    pub output_type: String,
}

/// Renders a tool's declaration. Input types longer than `input_max_chars`
/// render as `unknown`.
pub fn render_declaration(tool: &DeclaredTool, input_max_chars: usize) -> ToolDeclaration {
    let input_type = schema_to_type(&tool.parameters_json, Some(input_max_chars));
    let output_type = match &tool.output_schema {
        Some(schema) => schema_to_type(schema, None),
        None => "unknown".to_string(),
    };
    ToolDeclaration {
        js_name: crate::identifier::to_identifier(&tool.name),
        name: tool.name.clone(),
        description: tool.description.trim().to_string(),
        input_type,
        output_type,
    }
}

/// One tool as a member of the `tools` object: `name(args: T): Promise<R>;`.
pub fn render_tool_signature(declaration: &ToolDeclaration) -> String {
    format!(
        "{}(args: {}): Promise<{}>;",
        declaration.js_name, declaration.input_type, declaration.output_type
    )
}

/// A tool's sample: the description followed by the tool's declaration. Used
/// for tool listings.
pub fn render_tool_sample(declaration: &ToolDeclaration) -> String {
    let signature = render_tool_signature(declaration);
    let description = declaration.description.trim();
    format!(
        "{description}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {signature} }};\n```"
    )
}

/// `### \`id\` (\`raw name\`)` followed by the tool's sample, pi's section shape.
pub fn render_tool_section(declaration: &ToolDeclaration) -> String {
    let js_name = declaration.js_name.clone();
    let name = declaration.name.clone();
    let heading = if js_name == name {
        format!("### `{js_name}`")
    } else {
        format!("### `{js_name}` (`{name}`)")
    };
    format!("{heading}\n{}", render_tool_sample(declaration).trim())
}

/// One group of tools for budget selection, ordered in the description.
pub struct CatalogGroup {
    /// Tools of one namespace share a group; the unnamed group is `None`.
    pub namespace: Option<String>,
    /// Sections in insertion order: (raw tool name, rendered section, cost in tokens).
    pub entries: Vec<(String, String, usize)>,
}

/// Picks the tool sections that fit the budget, like pi's `selectCatalog`: in
/// each round every group places its cheapest remaining tool; a group whose
/// next tool does not fit drops out while the others continue. Returns the raw
/// tool names shown.
pub fn select_catalog(groups: &[CatalogGroup], budget: Option<usize>) -> BTreeSet<String> {
    let Some(budget) = budget else {
        return groups
            .iter()
            .flat_map(|group| group.entries.iter().map(|(name, _, _)| name.clone()))
            .collect();
    };
    let mut queues: Vec<Vec<(String, String, usize)>> = groups
        .iter()
        .map(|group| {
            let mut entries = group.entries.clone();
            entries.sort_by_key(|(_, _, cost)| *cost);
            entries
        })
        .collect();
    let mut shown = BTreeSet::new();
    let mut remaining = budget;
    loop {
        queues.retain(|queue| !queue.is_empty());
        if queues.is_empty() {
            break;
        }
        let mut active = false;
        for queue in &mut queues {
            let Some((name, _, cost)) = queue.first() else {
                continue;
            };
            if *cost > remaining {
                continue;
            }
            remaining -= cost;
            shown.insert(name.clone());
            queue.remove(0);
            active = true;
        }
        if !active {
            break;
        }
    }
    shown
}

/// Orders groups for the description: the unnamed group first, then by name.
pub fn ordered_groups(groups: Vec<CatalogGroup>) -> Vec<CatalogGroup> {
    let mut groups = groups;
    groups.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
        (Some(a), Some(b)) => a.cmp(b),
    });
    groups
}

/// Builds the catalog groups for a set of rendered declarations: one unnamed
/// group for tools without a namespace, one group per namespace in the map.
pub fn build_groups(
    declarations: &[ToolDeclaration],
    sections: BTreeMap<String, String>,
    namespaces: &BTreeMap<String, Option<String>>,
) -> Vec<CatalogGroup> {
    let mut unnamed: Vec<(String, String, usize)> = Vec::new();
    let mut named: BTreeMap<String, Vec<(String, String, usize)>> = BTreeMap::new();
    for declaration in declarations {
        let Some(section) = sections.get(&declaration.name) else {
            continue;
        };
        let entry = (
            declaration.name.clone(),
            section.clone(),
            section.len().div_ceil(CHARS_PER_TOKEN),
        );
        match namespaces.get(&declaration.name) {
            Some(Some(namespace)) => named.entry(namespace.clone()).or_default().push(entry),
            _ => unnamed.push(entry),
        }
    }
    let mut groups = Vec::new();
    if !unnamed.is_empty() {
        groups.push(CatalogGroup {
            namespace: None,
            entries: unnamed,
        });
    }
    for (namespace, entries) in named {
        groups.push(CatalogGroup {
            namespace: Some(namespace),
            entries,
        });
    }
    groups
}

/// The JSDoc comment for a description: single line when it fits, block form
/// otherwise, `*/` escaped.
pub fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let Some(text) = description.map(str::trim).filter(|text| !text.is_empty()) else {
        return String::new();
    };
    let escaped = text.replace("*/", "*\\/");
    let lines: Vec<&str> = escaped
        .split(['\r', '\n'])
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            }
        })
        .collect();
    format!("{indent}/**\n{}\n{indent} */\n", body.join("\n"))
}

/// The subschema a local `$ref` (`#`, `#/$defs/Item`) names in `root`, or
/// `None` for remote references and references that name nothing. Pointer
/// segments are URI-decoded and JSON Pointer unescaped (`~1` is `/`, `~0` is `~`).
pub fn resolve_schema_ref(reference: &str, root: &Value) -> Option<Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference
        .strip_prefix("#/")
        .unwrap_or_default()
        .split('/')
        .filter(|s| !s.is_empty())
    {
        let decoded = percent_decode(segment)?;
        let key = decoded.replace("~1", "/").replace("~0", "~");
        let object = current.as_object()?;
        current = object.get(&key)?;
    }
    Some(current.clone())
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Converts a JSON Schema to a TypeScript type expression: objects on one line
/// with properties sorted by name, or one property per line with `//` comments
/// when a property has a description or constraint tags. A result longer than
/// `max_chars` is rendered again without the constraint tags, and renders as
/// `unknown` if it is still too long. `None` max_chars means no limit.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let render = |tags: bool| to_type(schema, schema, &mut BTreeSet::new(), 0, tags);
    let rendered = render(true);
    if max_chars.is_none_or(|max| rendered.chars().count() <= max) {
        return rendered;
    }
    let untagged = render(false);
    if max_chars.is_none_or(|max| untagged.chars().count() <= max) {
        return untagged;
    }
    "unknown".to_string()
}

fn to_type(
    schema: &Value,
    root: &Value,
    resolving: &mut BTreeSet<String>,
    expansions: usize,
    tags: bool,
) -> String {
    let Some(object) = schema.as_object() else {
        // Boolean schemas: true accepts everything, false rejects everything.
        return match schema {
            Value::Bool(true) => "unknown".to_string(),
            Value::Bool(false) => "never".to_string(),
            _ => "unknown".to_string(),
        };
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if resolving.contains(reference) || expansions >= MAX_REF_EXPANSIONS {
            return "unknown".to_string();
        }
        let Some(target) = resolve_schema_ref(reference, root) else {
            return "unknown".to_string();
        };
        resolving.insert(reference.to_string());
        let expanded = to_type(&target, root, resolving, expansions + 1, tags);
        resolving.remove(reference);
        return expanded;
    }
    if let Some(constant) = object.get("const") {
        return constant.to_string();
    }
    if let Some(enum_values) = object.get("enum").and_then(Value::as_array) {
        return union(enum_values.iter().map(Value::to_string).collect());
    }
    let variants = object
        .get("anyOf")
        .or_else(|| object.get("oneOf"))
        .and_then(Value::as_array);
    if let Some(variants) = variants {
        return union(
            variants
                .iter()
                .map(|variant| to_type(variant, root, resolving, expansions, tags))
                .collect(),
        );
    }
    if let Some(all_of) = object.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = all_of
            .iter()
            .map(|part| to_type(part, root, resolving, expansions, tags))
            .filter(|part| part != "unknown")
            .collect();
        if parts.is_empty() {
            return "unknown".to_string();
        }
        return parts
            .iter()
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }
    match object.get("type") {
        Some(Value::Array(types)) => union(
            types
                .iter()
                .map(|entry| {
                    let mut single = object.clone();
                    single.insert("type".into(), entry.clone());
                    to_type(&Value::Object(single), root, resolving, expansions, tags)
                })
                .collect(),
        ),
        Some(Value::String(type_name)) => match type_name.as_str() {
            "string" => "string".to_string(),
            "number" | "integer" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => array_type(object, root, resolving, expansions, tags),
            "object" => object_type(object, root, resolving, expansions, tags),
            _ => "unknown".to_string(),
        },
        Some(_) => "unknown".to_string(),
        None => {
            if object.contains_key("properties")
                || object.contains_key("additionalProperties")
                || object.contains_key("required")
            {
                object_type(object, root, resolving, expansions, tags)
            } else if object.contains_key("items") || object.contains_key("prefixItems") {
                array_type(object, root, resolving, expansions, tags)
            } else {
                "unknown".to_string()
            }
        }
    }
}

fn array_type(
    schema: &serde_json::Map<String, Value>,
    root: &Value,
    resolving: &mut BTreeSet<String>,
    expansions: usize,
    tags: bool,
) -> String {
    match schema.get("items") {
        Some(items) if !items.is_array() => {
            return format!(
                "Array<{}>",
                to_type(items, root, resolving, expansions, tags)
            );
        }
        _ => {}
    }
    let tuple = schema
        .get("prefixItems")
        .and_then(Value::as_array)
        .or_else(|| schema.get("items").and_then(Value::as_array));
    if let Some(tuple) = tuple.filter(|tuple| !tuple.is_empty()) {
        let rendered: Vec<String> = tuple
            .iter()
            .map(|item| to_type(item, root, resolving, expansions, tags))
            .collect();
        return format!("[{}]", rendered.join(", "));
    }
    "unknown[]".to_string()
}

fn description_of(property: Option<&Value>) -> String {
    property
        .and_then(|property| property.get("description"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// Constraints of a property that its TypeScript type cannot express, as
/// JSDoc-style tags: `@integer @minimum 1 @maximum 100 @default 20`. Empty
/// when there are none.
fn constraint_tags(schema: Option<&Value>) -> String {
    let Some(property) = schema.filter(|property| property.is_object()) else {
        return String::new();
    };
    let object = property.as_object().expect("object above");

    // Optional fields such as Pydantic's `int | None` keep their constraints
    // in the non-null branch. The branch's constraints apply together with the
    // property's, so both are read.
    let variants = object
        .get("anyOf")
        .or_else(|| object.get("oneOf"))
        .and_then(Value::as_array);
    let branch = variants.map(|variants| {
        let non_null: Vec<&Value> = variants
            .iter()
            .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 {
            non_null[0].as_object().cloned()
        } else {
            None
        }
    });

    let mut schemas: Vec<serde_json::Map<String, Value>> = vec![object.clone()];
    if let Some(Some(branch)) = branch.as_ref() {
        schemas.push(branch.clone());
    }
    // Draft-04 and OpenAPI 3.0 make `minimum`/`maximum` exclusive with a
    // boolean `exclusiveMinimum`/`exclusiveMaximum`.
    for schema in &mut schemas {
        if schema.get("exclusiveMinimum") == Some(&Value::Bool(true)) {
            let minimum = schema.get("minimum").cloned();
            schema.insert("exclusiveMinimum".into(), minimum.unwrap_or(Value::Null));
            schema.insert("minimum".into(), Value::Null);
        }
        if schema.get("exclusiveMaximum") == Some(&Value::Bool(true)) {
            let maximum = schema.get("maximum").cloned();
            schema.insert("exclusiveMaximum".into(), maximum.unwrap_or(Value::Null));
            schema.insert("maximum".into(), Value::Null);
        }
    }

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |schema: &serde_json::Map<String, Value>| {
        matches!(
            schema.get("type"),
            Some(Value::String(type_name)) if type_name == "integer"
        ) || matches!(
            schema.get("type"),
            Some(Value::Array(types)) if types.iter().any(|entry| entry == "integer")
        )
    };
    if schemas.iter().any(&is_integer) {
        tags.push("@integer".to_string());
    }
    for keyword in TAG_KEYWORDS {
        let lower = LOWER_BOUNDS.contains(&keyword);
        let upper = UPPER_BOUNDS.contains(&keyword);
        let values: Vec<&Value> = schemas
            .iter()
            .filter_map(|schema| schema.get(keyword))
            .collect();
        if lower || upper {
            let numbers: Vec<i64> = values.iter().filter_map(|value| value.as_i64()).collect();
            if numbers.is_empty() {
                continue;
            }
            let bound = if lower {
                numbers.iter().max().copied()
            } else {
                numbers.iter().min().copied()
            };
            tags.push(format!("@{keyword} {}", bound.unwrap_or_default()));
            continue;
        }
        let mut seen = BTreeSet::new();
        for value in values {
            if !seen.insert(value.to_string()) {
                continue;
            }
            if value.is_number() || value.is_string() {
                tags.push(format!("@{keyword} {value}"));
            }
        }
    }
    if schemas
        .iter()
        .any(|schema| schema.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".to_string());
    }
    // Annotations, unlike constraints, come from the property itself.
    if let Some(
        value @ (Value::Number(_)
        | Value::String(_)
        | Value::Bool(_)
        | Value::Array(_)
        | Value::Object(_)),
    ) = object.get("default")
    {
        tags.push(format!("@default {value}"));
    }
    if object.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".to_string());
    }
    tags.retain(|tag| tag.chars().count() <= MAX_TAG_CHARS);
    tags.into_iter().collect::<Vec<_>>().join(" ")
}

fn property_key(name: &str) -> String {
    let first_valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$');
    let all_valid = name.chars().enumerate().all(|(index, c)| {
        if index == 0 {
            first_valid
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        }
    });
    if first_valid && all_valid {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "unknown".to_string())
    }
}

fn union(types: Vec<String>) -> String {
    // Deduplicated in first-seen order, like pi's `[...new Set(types)]`.
    let mut unique: Vec<String> = Vec::new();
    for kind in types {
        if !unique.contains(&kind) {
            unique.push(kind);
        }
    }
    if unique.iter().any(|kind| kind == "unknown") {
        return "unknown".to_string();
    }
    if unique.is_empty() {
        return "never".to_string();
    }
    unique.join(" | ")
}

fn object_type(
    schema: &serde_json::Map<String, Value>,
    root: &Value,
    resolving: &mut BTreeSet<String>,
    expansions: usize,
    tags: bool,
) -> String {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();
    let members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let property = properties.get(*name);
            let rendered = to_type(
                property.unwrap_or(&Value::Null),
                root,
                resolving,
                expansions,
                tags,
            );
            format!("{}{}: {};", property_key(name), optional, rendered)
        })
        .collect();
    let additional = schema.get("additionalProperties");
    let mut extra_members: Vec<String> = Vec::new();
    match additional {
        Some(value @ (Value::Bool(true) | Value::Object(_))) => {
            let rendered = if value == &Value::Bool(true) {
                "unknown".to_string()
            } else {
                to_type(value, root, resolving, expansions, tags)
            };
            extra_members.push(format!("[key: string]: {rendered};"));
        }
        None if names.is_empty() => extra_members.push("[key: string]: unknown;".to_string()),
        _ => {}
    }
    if members.is_empty() && extra_members.is_empty() {
        return "{}".to_string();
    }
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let property = properties.get(*name);
            let lines: Vec<String> = description_of(property)
                .split(['\r', '\n', '\u{2028}', '\u{2029}'])
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect();
            if tags {
                let constraint = constraint_tags(property);
                if !constraint.is_empty() {
                    let mut lines = lines;
                    lines.push(constraint);
                    return lines;
                }
            }
            lines
        })
        .collect();
    if !comments.iter().any(|comment| !comment.is_empty()) {
        let mut all = members.clone();
        all.extend(extra_members);
        return format!("{{ {} }}", all.join(" "));
    }
    let mut lines = vec!["{".to_string()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        lines.push(format!(
            "{INDENT}{}",
            members[index].replace('\n', &format!("\n{INDENT}"))
        ));
    }
    for member in &extra_members {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".to_string());
    lines.join("\n")
}
