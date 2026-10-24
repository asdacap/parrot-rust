//! Typed process-tool arguments. JSON diagnostics use Rust wording rather than
//! reproducing System.Text.Json's CLR type names and exception paths.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(String);

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

impl From<serde_json::Error> for InputError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecInput {
    pub command: String,
    pub environment: BTreeMap<String, String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub yield_after_ms: Option<u64>,
    pub tty: bool,
}

impl ExecInput {
    pub fn parse(json: &str) -> Result<Self, InputError> {
        let mut command = None;
        let mut cmd = None;
        let mut environment = BTreeMap::new();
        let mut name = None;
        let mut description = None;
        let mut milliseconds = None;
        let mut tty = false;
        for (property, value) in object(json, "command")?.0 {
            match property.as_str() {
                "command" => command = optional_string(&value, &property)?,
                "cmd" => cmd = optional_string(&value, &property)?,
                "env" => environment = environment_value(&value)?,
                "name" => name = optional_string(&value, &property)?,
                "description" => description = optional_string(&value, &property)?,
                "yield_after_ms" => milliseconds = optional_integer(&value, &property)?,
                "tty" => {
                    tty = serde_json::from_str::<bool>(value.get())
                        .map_err(|_| argument_type("tty", "a boolean"))?;
                }
                _ => return Err(unknown(&property)),
            }
        }
        let command = command.or(cmd).ok_or_else(|| required("command"))?;
        validate_environment(&environment)?;
        Ok(Self {
            command,
            environment,
            name,
            description,
            yield_after_ms: match milliseconds {
                None => None,
                Some(value) if (0..=10_000).contains(&value) => Some(value as u64),
                Some(_) => {
                    return Err(InputError(
                        "Tool argument 'yield_after_ms' must be a non-negative integer no greater than 10000."
                            .into(),
                    ));
                }
            },
            tty,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStdinInput {
    pub name: String,
    pub input: String,
    pub yield_after_ms: u64,
}

impl WriteStdinInput {
    pub fn parse(json: &str) -> Result<Self, InputError> {
        let mut name = None;
        let mut input = None;
        let mut milliseconds = None;
        for (property, value) in object(json, "name")?.0 {
            match property.as_str() {
                "name" => name = optional_string(&value, &property)?,
                "input" => input = optional_string(&value, &property)?,
                "yield_after_ms" => milliseconds = optional_integer(&value, &property)?,
                _ => {}
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| required("name"))?,
            input: input.ok_or_else(|| required("input"))?,
            yield_after_ms: delay(milliseconds)?.unwrap_or(250),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptInput {
    pub name: String,
    pub signal: i32,
}

impl InterruptInput {
    pub fn parse(json: &str) -> Result<Self, InputError> {
        let object = object(json, "name")?;
        // The source checks the final signal's raw spelling before deserializing
        // any members, then deserializes every occurrence in encounter order.
        if let Some((_, value)) = object.0.iter().rev().find(|(name, _)| name == "signal") {
            let _ = optional_signal(value)?;
        }
        let mut name = None;
        let mut signal = None;
        for (property, value) in object.0 {
            match property.as_str() {
                "name" => name = optional_string(&value, &property)?,
                "signal" => signal = optional_signal(&value)?,
                _ => {}
            }
        }
        let name = name.ok_or_else(|| required("name"))?;
        let signal = signal.unwrap_or(2);
        if !(1..=64).contains(&signal) {
            return Err(InputError(
                "Tool argument 'signal' must be between 1 and 64.".into(),
            ));
        }
        Ok(Self { name, signal })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInput {
    pub command: String,
    pub description: Option<String>,
    pub name: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub timeout_ms: i64,
}

impl MonitorInput {
    pub fn parse(json: &str) -> Result<Self, InputError> {
        let mut command = None;
        let mut description = None;
        let mut name = None;
        let mut environment = BTreeMap::new();
        let mut timeout_ms = None;
        for (property, value) in object(json, "command")?.0 {
            match property.as_str() {
                "command" => command = optional_string(&value, &property)?,
                "description" => description = optional_string(&value, &property)?,
                "name" => name = optional_string(&value, &property)?,
                "env" => environment = environment_value(&value)?,
                "timeout_ms" => timeout_ms = optional_integer(&value, &property)?,
                _ => return Err(unknown(&property)),
            }
        }
        let command = command.ok_or_else(|| required("command"))?;
        Ok(Self {
            command,
            description,
            name,
            environment,
            timeout_ms: timeout_ms.unwrap_or(300_000),
        })
    }
}

// Preserve every occurrence so invalid earlier values cannot be hidden by a
// later duplicate. Only successfully validated assignments use last-wins.
struct Object(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Object {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = Object;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Object(entries))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn object(json: &str, required_property: &str) -> Result<Object, InputError> {
    let value: Box<RawValue> = serde_json::from_str(json)?;
    if !value.get().trim_start().starts_with('{') {
        return Err(required(required_property));
    }
    Ok(serde_json::from_str(value.get())?)
}

fn unknown(name: &str) -> InputError {
    InputError(format!("Tool argument '{name}' is not supported."))
}

fn required(name: &str) -> InputError {
    InputError(format!("Tool arguments require a string '{name}'."))
}

fn argument_type(name: &str, expected: &str) -> InputError {
    InputError(format!("Tool argument '{name}' must be {expected}."))
}

fn optional_string(value: &RawValue, name: &str) -> Result<Option<String>, InputError> {
    serde_json::from_str(value.get()).map_err(|_| argument_type(name, "a string or null"))
}

// Keep the raw number spelling: System.Text.Json's integer conversions reject
// decimal/exponent forms, but accept integer -0. A floating-point Value loses
// that distinction.
fn integer(value: &RawValue) -> Option<i64> {
    value.get().trim().parse().ok()
}

fn optional_integer(value: &RawValue, name: &str) -> Result<Option<i64>, InputError> {
    if value.get().trim() == "null" {
        return Ok(None);
    }
    integer(value)
        .map(Some)
        .ok_or_else(|| argument_type(name, "a 64-bit integer"))
}

fn optional_signal(value: &RawValue) -> Result<Option<i32>, InputError> {
    if value.get().trim() == "null" {
        return Ok(None);
    }
    integer(value)
        .and_then(|number| i32::try_from(number).ok())
        .map(Some)
        .ok_or_else(|| argument_type("signal", "an integer"))
}

fn delay(milliseconds: Option<i64>) -> Result<Option<u64>, InputError> {
    match milliseconds {
        None => Ok(None),
        Some(value) if (0..=4_294_967_294).contains(&value) => Ok(Some(value as u64)),
        Some(_) => Err(InputError(
            "Tool argument 'yield_after_ms' must be a non-negative integer no greater than 4294967294."
                .into(),
        )),
    }
}

fn environment_value(value: &RawValue) -> Result<BTreeMap<String, String>, InputError> {
    if value.get().trim() == "null" {
        return Ok(BTreeMap::new());
    }
    if !value.get().trim_start().starts_with('{') {
        return Err(InputError(
            "Tool argument 'env' must be an object containing string values.".into(),
        ));
    }
    let entries: Object = serde_json::from_str(value.get())?;
    let mut environment = BTreeMap::new();
    for (name, value) in entries.0 {
        let value: String = serde_json::from_str(value.get()).map_err(|_| {
            InputError("Tool argument 'env' must contain only string values.".into())
        })?;
        environment.insert(name, value);
    }
    Ok(environment)
}

pub(super) fn validate_environment(
    environment: &BTreeMap<String, String>,
) -> Result<(), InputError> {
    for (name, value) in environment {
        if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0') {
            return Err(InputError(
                "Tool argument 'env' contains an invalid environment value.".into(),
            ));
        }
    }
    Ok(())
}
