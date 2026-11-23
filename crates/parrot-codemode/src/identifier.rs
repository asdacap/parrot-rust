//! The identifier a script uses for a tool name.

/// Maps a tool name to a JavaScript identifier: characters that are not valid
/// in an identifier become `_`. `my-tool` becomes `my_tool`,
/// `mcp__docs__search` stays as is.
pub fn to_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character.is_ascii_alphabetic() || character == '_' || character == '$'
        } else {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        };
        if valid {
            identifier.push(character);
        } else {
            identifier.push('_');
        }
    }
    if identifier.is_empty() {
        identifier.push('_');
    }
    identifier
}

#[cfg(test)]
mod tests {
    use super::to_identifier;

    #[test]
    fn keeps_valid_identifiers() {
        assert_eq!(to_identifier("read"), "read");
        assert_eq!(to_identifier("mcp__docs__search"), "mcp__docs__search");
        assert_eq!(to_identifier("_private"), "_private");
        assert_eq!(to_identifier("$dollar"), "$dollar");
    }

    #[test]
    fn replaces_invalid_characters() {
        assert_eq!(to_identifier("my-tool"), "my_tool");
        assert_eq!(to_identifier("tool.name"), "tool_name");
        assert_eq!(to_identifier("read file"), "read_file");
    }

    #[test]
    fn rejects_leading_invalid_characters() {
        assert_eq!(to_identifier("2fa"), "_fa");
        assert_eq!(to_identifier("-tool"), "_tool");
    }

    #[test]
    fn maps_empty_names_to_underscore() {
        assert_eq!(to_identifier(""), "_");
    }
}
