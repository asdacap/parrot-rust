use std::collections::BTreeMap;

use super::{ConfigError, Result};

/// Expand the source configuration's `${NAME}` / `${NAME:-fallback}` syntax.
/// Environment values themselves are deliberately never reparsed.
pub fn resolve(
    template: &str,
    field: &str,
    environment: &BTreeMap<String, String>,
) -> Result<String> {
    let mut resolver = Resolver {
        template,
        field,
        environment,
        index: 0,
    };
    resolver.segment(0, false, true)
}

struct Resolver<'a> {
    template: &'a str,
    field: &'a str,
    environment: &'a BTreeMap<String, String>,
    index: usize,
}

impl Resolver<'_> {
    fn error(&self, message: &str) -> ConfigError {
        ConfigError::invalid_data(format!("{} {message}", self.field))
    }

    fn segment(&mut self, depth: usize, nested: bool, evaluate: bool) -> Result<String> {
        if depth > 32 {
            return Err(self.error("template nesting is too deep"));
        }
        let mut result = String::new();
        while self.index < self.template.len() {
            let rest = &self.template[self.index..];
            if rest.starts_with('}') {
                if !nested {
                    return Err(self.error("contains an unmatched '}'"));
                }
                return Ok(result);
            }
            if rest.starts_with("${") {
                result.push_str(&self.expression(depth + 1, evaluate)?);
                continue;
            }
            let character = rest.chars().next().expect("nonempty suffix");
            result.push(character);
            self.index += character.len_utf8();
        }
        if nested {
            return Err(self.error("contains an unclosed environment template"));
        }
        Ok(result)
    }

    fn expression(&mut self, depth: usize, evaluate: bool) -> Result<String> {
        self.index += 2;
        let start = self.index;
        let bytes = self.template.as_bytes();
        if self.index >= bytes.len() || !name_start(bytes[self.index]) {
            return Err(self.error("contains an invalid environment variable name"));
        }
        self.index += 1;
        while self.index < bytes.len()
            && (name_start(bytes[self.index]) || bytes[self.index].is_ascii_digit())
        {
            self.index += 1;
        }
        let name = &self.template[start..self.index];
        if self.index >= bytes.len() {
            return Err(self.error("contains an unclosed environment template"));
        }
        if bytes[self.index] == b'}' {
            self.index += 1;
            return if evaluate {
                self.environment
                    .get(name)
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .ok_or_else(|| {
                        self.error(&format!("requires nonempty environment variable {name}"))
                    })
            } else {
                Ok(String::new())
            };
        }
        if !self.template[self.index..].starts_with(":-") {
            return Err(self.error("contains an unsupported environment template operator"));
        }
        self.index += 2;
        let value = if evaluate {
            self.environment
                .get(name)
                .filter(|value| !value.is_empty())
                .cloned()
        } else {
            None
        };
        let fallback = self.segment(depth, true, evaluate && value.is_none())?;
        if self.template.as_bytes().get(self.index) != Some(&b'}') {
            return Err(self.error("contains an unclosed environment template"));
        }
        self.index += 1;
        Ok(value.unwrap_or(fallback))
    }
}

fn name_start(character: u8) -> bool {
    character == b'_' || character.is_ascii_alphabetic()
}
