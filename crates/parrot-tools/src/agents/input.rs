use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputError(pub String);

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for InputError {}

type Properties = Vec<(String, Value)>;

struct OrderedProperties(Properties);
impl<'de> Deserialize<'de> for OrderedProperties {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PropertiesVisitor;
        impl<'de> Visitor<'de> for PropertiesVisitor {
            type Value = OrderedProperties;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a tool arguments object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut properties = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    properties.push((key, map.next_value::<Value>()?));
                }
                Ok(OrderedProperties(properties))
            }
        }
        deserializer.deserialize_map(PropertiesVisitor)
    }
}

pub(super) fn object(arguments: &str) -> Result<Properties, InputError> {
    let value: Value =
        serde_json::from_str(arguments).map_err(|error| InputError(error.to_string()))?;
    if !value.is_object() {
        return Err(InputError("Tool arguments must be an object.".into()));
    }
    serde_json::from_str::<OrderedProperties>(arguments)
        .map(|properties| properties.0)
        .map_err(|error| InputError(error.to_string()))
}

pub(super) fn validate_string_fields(
    properties: &Properties,
    allowed: &[&str],
) -> Result<(), InputError> {
    for (name, value) in properties {
        if !allowed.contains(&name.as_str()) {
            return Err(InputError(format!(
                "Tool arguments contain an unexpected property: {name}."
            )));
        }
        if !matches!(value, Value::Null | Value::String(_)) {
            return Err(InputError(format!(
                "Tool argument '{name}' must be a string."
            )));
        }
    }
    Ok(())
}

pub(super) fn optional_string(
    properties: &Properties,
    name: &str,
) -> Result<Option<String>, InputError> {
    let mut result = None;
    for (_, value) in properties.iter().filter(|(key, _)| key == name) {
        result = match value {
            Value::Null => None,
            Value::String(value) => Some(value.clone()),
            _ => {
                return Err(InputError(format!(
                    "Tool argument '{name}' must be a string."
                )));
            }
        };
    }
    Ok(result)
}

pub(super) fn required_string(properties: &Properties, name: &str) -> Result<String, InputError> {
    optional_string(properties, name)?
        .ok_or_else(|| InputError(format!("Tool arguments require a string '{name}'.")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForkSelection {
    Empty,
    Full,
    Named(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnInput {
    pub prompt: String,
    pub agent: String,
    pub model: String,
    pub name: String,
    pub scope: String,
    pub fork: ForkSelection,
}

impl SpawnInput {
    pub fn parse(arguments: &str) -> Result<Self, InputError> {
        let properties = object(arguments)?;
        validate_string_fields(
            &properties,
            &["prompt", "agent", "model", "name", "scope", "fork"],
        )?;
        let prompt = required_string(&properties, "prompt")?;
        let agent = required_string(&properties, "agent")?;
        let model = optional_string(&properties, "model")?.unwrap_or_default();
        let name = required_string(&properties, "name")?;
        if name.is_empty() {
            return Err(InputError("Tool arguments require a string 'name'.".into()));
        }
        let scope = optional_string(&properties, "scope")?.unwrap_or_default();
        let fork = match optional_string(&properties, "fork")?
            .as_deref()
            .unwrap_or("")
        {
            "" | "empty" => ForkSelection::Empty,
            "full" => ForkSelection::Full,
            title => ForkSelection::Named(title.to_owned()),
        };
        Ok(Self {
            prompt,
            agent,
            model,
            name,
            scope,
            fork,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendInput {
    pub name: String,
    pub message: String,
}
impl SendInput {
    pub const MAXIMUM_MESSAGE_BYTES: usize = 32 * 1024;

    pub fn parse(arguments: &str) -> Result<Self, InputError> {
        let properties = object(arguments)?;
        validate_string_fields(&properties, &["name", "message"])?;
        let name = required_string(&properties, "name")?;
        let message = required_string(&properties, "message")?;
        if message.len() > Self::MAXIMUM_MESSAGE_BYTES {
            return Err(InputError(format!(
                "agent message exceeds {} UTF-8 bytes; split it into smaller messages",
                Self::MAXIMUM_MESSAGE_BYTES
            )));
        }
        Ok(Self { name, message })
    }
}

pub fn parse_interrupt_name(arguments: &str) -> Result<String, InputError> {
    let properties = object(arguments)?;
    validate_string_fields(&properties, &["name"])?;
    let name = required_string(&properties, "name")?.trim().to_owned();
    if name.is_empty() {
        return Err(InputError("no child agent given".into()));
    }
    Ok(name)
}

pub fn parse_status_name(arguments: &str) -> Result<String, InputError> {
    let properties = object(arguments)?;
    validate_string_fields(&properties, &["name"])?;
    let name = required_string(&properties, "name")?;
    if name.trim().is_empty() {
        return Err(InputError("no child agent given".into()));
    }
    Ok(name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitInput {
    pub duration_ms: u64,
}
impl WaitInput {
    pub const MINIMUM_DURATION_MS: u64 = 10_000;
    pub const DEFAULT_DURATION_MS: u64 = 300_000;
    pub const MAXIMUM_DURATION_MS: u64 = u32::MAX as u64 - 1;

    pub fn parse(arguments: &str) -> Result<Self, InputError> {
        let properties = object(arguments)?;
        if properties.iter().any(|(key, _)| key != "duration_ms") {
            return Err(InputError(
                "Tool arguments contain an unexpected property.".into(),
            ));
        }
        let duration_ms = match properties
            .iter()
            .rev()
            .find(|(key, _)| key == "duration_ms")
            .map(|(_, value)| value)
        {
            None => Self::DEFAULT_DURATION_MS,
            Some(value) => value
                .as_u64()
                .filter(|value| {
                    (Self::MINIMUM_DURATION_MS..=Self::MAXIMUM_DURATION_MS).contains(value)
                })
                .ok_or_else(|| {
                    InputError(format!(
                        "Tool argument 'duration_ms' must be an integer from {} through {}.",
                        Self::MINIMUM_DURATION_MS,
                        Self::MAXIMUM_DURATION_MS
                    ))
                })?,
        };
        Ok(Self { duration_ms })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireQuestionOption {
    pub label: String,
    pub description: String,
}
impl<'de> Deserialize<'de> for WireQuestionOption {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OptionVisitor;
        impl<'de> Visitor<'de> for OptionVisitor {
            type Value = WireQuestionOption;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a question option string or object")
            }
            fn visit_str<E: de::Error>(self, label: &str) -> Result<Self::Value, E> {
                if label.trim().is_empty() {
                    return Err(E::custom("question options cannot be empty"));
                }
                Ok(WireQuestionOption {
                    label: label.into(),
                    description: String::new(),
                })
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut label: Option<String> = None;
                let mut description = String::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "label" => {
                            let value = map.next_value::<Value>()?;
                            label = Some(
                                value
                                    .as_str()
                                    .ok_or_else(|| {
                                        de::Error::custom(
                                            "question option 'label' must be a string",
                                        )
                                    })?
                                    .into(),
                            );
                        }
                        "description" => {
                            let value = map.next_value::<Value>()?;
                            description = value
                                .as_str()
                                .ok_or_else(|| {
                                    de::Error::custom(
                                        "question option 'description' must be a string",
                                    )
                                })?
                                .into();
                        }
                        _ => {
                            return Err(de::Error::custom(format!(
                                "unknown question option property: {key}"
                            )));
                        }
                    }
                }
                let label = label
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        de::Error::custom("question option requires a nonblank 'label'")
                    })?;
                Ok(WireQuestionOption { label, description })
            }
        }
        deserializer.deserialize_any(OptionVisitor)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WireQuestion {
    pub header: Option<String>,
    pub prompt: Option<String>,
    pub options: Option<Vec<WireQuestionOption>>,
    pub multiple: bool,
    pub custom: Option<bool>,
}
impl<'de> Deserialize<'de> for WireQuestion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct QuestionVisitor;
        impl<'de> Visitor<'de> for QuestionVisitor {
            type Value = WireQuestion;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a question object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut question = WireQuestion::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "header" => question.header = map.next_value()?,
                        "prompt" => question.prompt = map.next_value()?,
                        "options" => question.options = map.next_value()?,
                        "multiple" => question.multiple = map.next_value()?,
                        "custom" => question.custom = map.next_value()?,
                        _ => {
                            return Err(de::Error::custom(format!(
                                "unknown question property: {key}"
                            )));
                        }
                    }
                }
                Ok(question)
            }
        }
        deserializer.deserialize_map(QuestionVisitor)
    }
}

struct QuestionInput {
    questions: Option<Vec<WireQuestion>>,
}
impl<'de> Deserialize<'de> for QuestionInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct QuestionsVisitor;
        impl<'de> Visitor<'de> for QuestionsVisitor {
            type Value = QuestionInput;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a question request object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut questions = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "questions" => questions = map.next_value()?,
                        _ => {
                            return Err(de::Error::custom(format!(
                                "unknown question request property: {key}"
                            )));
                        }
                    }
                }
                Ok(QuestionInput { questions })
            }
        }
        deserializer.deserialize_map(QuestionsVisitor)
    }
}

pub fn parse_questions(arguments: &str) -> Result<Vec<WireQuestion>, InputError> {
    object(arguments)?;
    let input: QuestionInput =
        serde_json::from_str(arguments).map_err(|error| InputError(error.to_string()))?;
    input
        .questions
        .ok_or_else(|| InputError("Tool arguments require an array 'questions'.".into()))
}

struct AnswerInput {
    agent_name: Option<String>,
    answers: Option<Vec<Option<String>>>,
}
impl<'de> Deserialize<'de> for AnswerInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AnswersVisitor;
        impl<'de> Visitor<'de> for AnswersVisitor {
            type Value = AnswerInput;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an answer request object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut input = AnswerInput {
                    agent_name: None,
                    answers: None,
                };
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "agent_name" => input.agent_name = map.next_value()?,
                        "answers" => input.answers = map.next_value()?,
                        _ => {
                            return Err(de::Error::custom(format!(
                                "unknown answer property: {key}"
                            )));
                        }
                    }
                }
                Ok(input)
            }
        }
        deserializer.deserialize_map(AnswersVisitor)
    }
}

pub fn parse_answers(arguments: &str) -> Result<(String, Vec<String>), InputError> {
    object(arguments)?;
    let input: AnswerInput =
        serde_json::from_str(arguments).map_err(|error| InputError(error.to_string()))?;
    let name = input
        .agent_name
        .ok_or_else(|| InputError("Tool arguments require a string 'agent_name'.".into()))?;
    let answers = input
        .answers
        .ok_or_else(|| InputError("Tool arguments require an array 'answers'.".into()))?
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    Ok((name, answers))
}
