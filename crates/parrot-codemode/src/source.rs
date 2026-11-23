//! The source format: an optional options line, then the script body.

use serde::Deserialize;

/// What a script may set on its options line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct ScriptOptions {
    /// Hard deadline in milliseconds; the host aborts the script past it.
    #[serde(default, rename = "timeout_ms")]
    pub timeout_ms: Option<u64>,
    /// Token budget for the script's output; the sandbox reports it and the
    /// caller decides what to do.
    #[serde(default, rename = "max_output_tokens")]
    pub max_output_tokens: Option<u64>,
}

/// Why a source failed to parse. `execute()` reports it as a `script` error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceError {
    /// The source is empty or only whitespace.
    Empty,
    /// The options line is not valid JSON.
    InvalidOptions(String),
    /// The options line set a field the format does not know.
    UnknownOptionsField(String),
    /// The source has an options line but no code after it.
    OptionsWithoutCode,
    /// An options line appears on a line other than the first.
    OptionsAfterFirstLine,
}

/// Splits a script source into its body and its options. The first line may be
/// `// @options: {...}`; it is replaced by an empty line so stack trace line
/// numbers still match the input. Any options line on a later line is an error.
pub fn parse_source(source: &str) -> Result<(String, ScriptOptions), SourceError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(SourceError::Empty);
    }
    let mut lines = source.split('\n');
    let first = lines.next().unwrap_or_default();
    let (options, options_line) = if let Some(json) = first.strip_prefix("// @options:") {
        let options = parse_options(json)?;
        if trimmed == first.trim() {
            return Err(SourceError::OptionsWithoutCode);
        }
        (options, true)
    } else {
        (ScriptOptions::default(), false)
    };
    for line in lines {
        if line.starts_with("// @options:") {
            return Err(SourceError::OptionsAfterFirstLine);
        }
    }
    let body = if options_line {
        let mut blanked = String::with_capacity(source.len());
        blanked.push_str(&" ".repeat(first.len()));
        match source.strip_prefix(first) {
            Some(rest) => blanked.push_str(rest),
            None => return Err(SourceError::Empty),
        }
        blanked
    } else {
        source.to_string()
    };
    Ok((body, options))
}

fn parse_options(json: &str) -> Result<ScriptOptions, SourceError> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default, rename = "timeout_ms")]
        timeout_ms: Option<u64>,
        #[serde(default, rename = "max_output_tokens")]
        max_output_tokens: Option<u64>,
    }
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| SourceError::InvalidOptions(error.to_string()))?;
    let Some(object) = value.as_object() else {
        return Err(SourceError::InvalidOptions(
            "the options line must be a JSON object".into(),
        ));
    };
    for key in object.keys() {
        if key != "timeout_ms" && key != "max_output_tokens" {
            return Err(SourceError::UnknownOptionsField(key.clone()));
        }
    }
    let raw: Raw = serde_json::from_value(value)
        .map_err(|error| SourceError::InvalidOptions(error.to_string()))?;
    Ok(ScriptOptions {
        timeout_ms: raw.timeout_ms,
        max_output_tokens: raw.max_output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_source() {
        let (body, options) = parse_source("return 1;\n").unwrap();
        assert_eq!(body, "return 1;\n");
        assert_eq!(options, ScriptOptions::default());
    }

    #[test]
    fn parses_options_line_and_blanks_it() {
        let first_line = "// @options: {\"timeout_ms\": 30000}";
        let source = format!("{first_line}\nreturn 1;\n");
        let expected_blank = " ".repeat(first_line.chars().count());
        let (body, options) = parse_source(&source).unwrap();
        assert_eq!(body, format!("{expected_blank}\nreturn 1;\n"));
        assert_eq!(options.timeout_ms, Some(30000));
        assert_eq!(options.max_output_tokens, None);
    }

    #[test]
    fn rejects_empty_source() {
        assert_eq!(parse_source("   \n"), Err(SourceError::Empty));
    }

    #[test]
    fn rejects_options_without_code() {
        let error = parse_source("// @options: {}\n   ").unwrap_err();
        assert!(matches!(error, SourceError::OptionsWithoutCode));
    }

    #[test]
    fn rejects_invalid_options_json() {
        let error = parse_source("// @options: nope\nreturn 1;").unwrap_err();
        assert!(matches!(error, SourceError::InvalidOptions(_)));
    }

    #[test]
    fn rejects_unknown_options_field() {
        let error = parse_source("// @options: {\"mode\": 1}\nreturn 1;").unwrap_err();
        assert_eq!(error, SourceError::UnknownOptionsField("mode".into()));
    }

    #[test]
    fn rejects_options_on_later_line() {
        let error = parse_source("return 1;\n// @options: {}\n").unwrap_err();
        assert_eq!(error, SourceError::OptionsAfterFirstLine);
    }

    #[test]
    fn rejects_non_object_options() {
        let error = parse_source("// @options: 5\nreturn 1;").unwrap_err();
        assert!(matches!(error, SourceError::InvalidOptions(_)));
    }
}
