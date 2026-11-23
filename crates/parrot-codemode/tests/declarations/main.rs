//! Declaration rendering tests, checked against pi's observable behavior.

use parrot_codemode::{
    CHARS_PER_TOKEN, CatalogGroup, DeclaredTool, build_groups, render_declaration,
    render_tool_sample, render_tool_section, render_tool_signature, resolve_schema_ref,
    schema_to_type, select_catalog,
};
use serde_json::json;

fn tool(name: &str, description: &str, parameters: serde_json::Value) -> DeclaredTool {
    DeclaredTool {
        name: name.into(),
        description: description.into(),
        parameters_json: parameters,
        output_schema: None,
    }
}

#[test]
fn renders_flat_object_schema() {
    let declaration = render_declaration(
        &tool(
            "read",
            "Read a file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The file path"},
                    "depth": {"type": "integer", "minimum": 1, "default": 20},
                    "mode": {"enum": ["fast", "slow"]},
                },
                "required": ["path"]
            }),
        ),
        16_000,
    );
    assert_eq!(declaration.js_name, "read");
    let signature = render_tool_signature(&declaration);
    assert!(signature.starts_with("read(args: {"), "{signature}");
    // Required and optional markers.
    assert!(signature.contains("path: string;"), "{signature}");
    assert!(signature.contains("depth?: number;"), "{signature}");
    assert!(
        signature.contains("mode?: \"fast\" | \"slow\";"),
        "{signature}"
    );
    assert!(signature.ends_with("): Promise<unknown>;"), "{signature}");
}

#[test]
fn multiline_comments_show_descriptions_and_tags() {
    let declaration = render_declaration(
        &tool(
            "query",
            "",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Rows to return", "minimum": 1, "default": 20},
                },
            }),
        ),
        16_000,
    );
    let rendered = render_tool_signature(&declaration);
    assert!(rendered.contains("// Rows to return"), "{rendered}");
    assert!(
        rendered.contains("// @integer @minimum 1 @default 20"),
        "{rendered}"
    );
    assert!(rendered.contains('\n'), "multiline form: {rendered}");
    // Property sorted by name; optional marker present.
    assert!(rendered.contains("limit?: number;"), "{rendered}");
}

#[test]
fn constraints_drop_out_past_the_size_limit() {
    let big = json!({
        "type": "object",
        "properties": (0..60)
            .map(|index| {
                (format!("p{index:03}"), json!({
                    "type": "integer",
                    "description": "A long description so each member renders multiline comments",
                    "minimum": 1, "maximum": 100, "default": 20,
                }))
            })
            .collect::<serde_json::Map<String, serde_json::Value>>(),
    });
    let big_type = schema_to_type(&big, None);
    assert!(big_type.len() > 4_000);
    // Past the limit the tags drop first; still past, the type becomes unknown.
    let limited = schema_to_type(&big, Some(3_000));
    assert_eq!(limited, "unknown");
}

#[test]
fn expands_local_refs_and_stops_recursion() {
    let with_defs = json!({
        "type": "object",
        "properties": {"item": {"$ref": "#/$defs/Item"}},
        "$defs": {"Item": {"type": "object", "properties": {"label": {"type": "string"}}}},
    });
    assert_eq!(
        schema_to_type(&with_defs, None),
        "{ item?: { label?: string; }; }"
    );
    let recursive = json!({
        "type": "object",
        "properties": {"child": {"$ref": "#/$defs/Node"}},
        "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}},
    });
    assert_eq!(
        schema_to_type(&recursive, None),
        "{ child?: { next?: unknown; }; }"
    );
    assert_eq!(
        resolve_schema_ref(
            "#/definitions/Legacy",
            &json!({"definitions": {"Legacy": {"type": "string"}}})
        )
        .expect("legacy refs resolve"),
        json!({"type": "string"})
    );
    assert!(resolve_schema_ref("https://remote/schema", &with_defs).is_none());
}

#[test]
fn unions_all_of_and_type_arrays() {
    assert_eq!(
        schema_to_type(
            &json!({"anyOf": [{"type": "string"}, {"type": "number"}]}),
            None
        ),
        "string | number"
    );
    assert_eq!(
        schema_to_type(&json!({"allOf": [{"type": "object", "properties": {"a": {"type": "string"}}}, {"type": "object", "properties": {"b": {"type": "number"}}}]}), None)
            .replace('\n', " "),
        "{ a?: string; } & { b?: number; }"
    );
    assert_eq!(
        schema_to_type(&json!({"type": ["string", "null"]}), None),
        "string | null"
    );
    assert_eq!(
        schema_to_type(&json!({"const": "start"}), None),
        "\"start\""
    );
    assert_eq!(
        schema_to_type(&json!({"type": "array", "items": {"type": "string"}}), None),
        "Array<string>"
    );
    assert_eq!(
        schema_to_type(
            &json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]}),
            None
        ),
        "[string, number]"
    );
    // pi gives a bare object schema an index signature, not `{}`.
    assert_eq!(
        schema_to_type(&json!({"type": "object"}), None),
        "{ [key: string]: unknown; }"
    );
    assert_eq!(
        schema_to_type(
            &json!({"type": "object", "additionalProperties": {"type": "number"}}),
            None
        ),
        "{ [key: string]: number; }"
    );
}

#[test]
fn quoted_property_keys_and_draft04_exclusive_bounds() {
    let rendered = schema_to_type(
        &json!({"type": "object", "properties": {"my-key": {"type": "string"}}}),
        None,
    );
    assert!(rendered.contains("\"my-key\"?: string;"), "{rendered}");
    let tags = schema_to_type(
        &json!({"type": "object", "properties": {"level": {"type": "number", "minimum": 5, "exclusiveMinimum": true}}}),
        None,
    );
    assert!(tags.contains("@exclusiveMinimum 5"), "{tags}");
    assert!(!tags.contains("@minimum 5"), "{tags}");
}

#[test]
fn identifier_collisions_first_tool_wins() {
    // pi's mapping is pure: each name maps on its own, and the tool table
    // keeps the first tool when two names normalize to the same identifier.
    assert_eq!(parrot_codemode::to_identifier("my-tool"), "my_tool");
    assert_eq!(parrot_codemode::to_identifier("my_tool"), "my_tool");
    // The sandbox prelude implements first-wins; the declaration layer renders
    // each tool with its own mapped name.
    let first = render_declaration(&tool("my-tool", "first", json!({})), 16_000);
    let second = render_declaration(&tool("my_tool", "second", json!({})), 16_000);
    assert_eq!(first.js_name, second.js_name);
    assert_eq!(first.description, "first");
}

#[test]
fn budget_selection_round_robins_the_cheapest() {
    let section = |text: &str| text.to_string();
    let groups = vec![CatalogGroup {
        namespace: None,
        entries: vec![
            (
                "big".into(),
                section(&"x".repeat(400 * CHARS_PER_TOKEN)),
                400,
            ),
            (
                "small".into(),
                section(&"y".repeat(10 * CHARS_PER_TOKEN)),
                10,
            ),
            (
                "medium".into(),
                section(&"z".repeat(30 * CHARS_PER_TOKEN)),
                30,
            ),
        ],
    }];
    // No budget lists everything.
    assert_eq!(select_catalog(&groups, None).len(), 3);
    // With a budget of 50 the round-robin picks the cheapest first; big never fits after.
    let shown = select_catalog(&groups, Some(50));
    assert!(shown.contains("small"), "{shown:?}");
    assert!(shown.contains("medium"), "{shown:?}");
    assert!(!shown.contains("big"), "{shown:?}");
    // A group whose next tool does not fit drops out while others continue.
    let groups = vec![
        CatalogGroup {
            namespace: None,
            entries: vec![("huge".into(), "h".repeat(100 * CHARS_PER_TOKEN), 100)],
        },
        CatalogGroup {
            namespace: Some("ns".into()),
            entries: vec![("tiny".into(), "t".repeat(10 * CHARS_PER_TOKEN), 10)],
        },
    ];
    let shown = select_catalog(&groups, Some(20));
    assert!(shown.contains("tiny"), "{shown:?}");
    assert!(!shown.contains("huge"), "{shown:?}");
}

#[test]
fn groups_order_unnamed_first_then_names() {
    let declarations = vec![
        render_declaration(&tool("zed", "", json!({})), 16_000),
        render_declaration(&tool("plain", "", json!({})), 16_000),
    ];
    let mut namespaces = std::collections::BTreeMap::new();
    namespaces.insert("zed".to_string(), Some("beta".to_string()));
    let sections = [
        ("zed".to_string(), "zed section".to_string()),
        ("plain".to_string(), "plain section".to_string()),
    ]
    .into_iter()
    .collect();
    let groups = build_groups(&declarations, sections, &namespaces);
    assert_eq!(groups[0].namespace, None);
    assert_eq!(groups[0].entries[0].0, "plain");
    assert_eq!(groups[1].namespace, Some("beta".into()));
}

#[test]
fn section_shape_matches_pi() {
    let declaration = render_declaration(
        &tool(
            "read",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ),
        16_000,
    );
    let section = render_tool_section(&declaration);
    assert!(
        section.starts_with("### `read`\nRead a file\n\ncodemode tool declaration:\n```ts"),
        "{section}"
    );
    assert!(
        section.contains(
            "declare const tools: { read(args: { path?: string; }): Promise<unknown>; };"
        ),
        "{section}"
    );
    assert!(section.ends_with("```"), "{section}");
    let renamed = render_declaration(&tool("my-tool", "d", json!({})), 16_000);
    let section = render_tool_section(&renamed);
    assert!(
        section.starts_with("### `my_tool` (`my-tool`)\n"),
        "{section}"
    );
    let sample = render_tool_sample(&declaration);
    assert!(
        sample.starts_with("Read a file\n\ncodemode tool declaration:\n```ts\n"),
        "{sample}"
    );
}

#[test]
fn doc_comment_escapes_and_splits() {
    assert_eq!(
        parrot_codemode::doc_comment(Some("  hi  "), ""),
        "/** hi */\n"
    );
    let multi = parrot_codemode::doc_comment(Some("one\ntwo"), "  ");
    assert!(
        multi.starts_with("  /**\n   * one\n   * two\n   */\n"),
        "{multi}"
    );
    assert_eq!(
        parrot_codemode::doc_comment(Some("a */ b"), ""),
        "/** a *\\/ b */\n"
    );
    assert_eq!(parrot_codemode::doc_comment(None, ""), "");
    assert_eq!(parrot_codemode::doc_comment(Some("   "), ""), "");
}
