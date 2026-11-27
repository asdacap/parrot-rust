//! Statically typed OpenRouter alpha.decisions request and answer payloads.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const BOOLEAN_QUESTION_TYPE: &str = "noul";
pub const TRUE_CRITERION: &str = "true";
pub const FALSE_CRITERION: &str = "false";
pub const SAFETY_QUESTION_NAME: &str = "is_command_safe";
/// Alpha endpoint path under the provider base URL.
pub const DECISIONS_ENDPOINT: &str = "alpha/decisions";

/// One typed question about the request state. The command safety gate asks a
/// single boolean question, so only the `noul` shape is represented.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionsQuestion {
    #[serde(rename = "type")]
    pub question_type: &'static str,
    pub instructions: String,
    pub criteria: BTreeMap<&'static str, String>,
}

impl DecisionsQuestion {
    /// Builds one boolean question whose `true` criterion describes the safe
    /// outcome and whose `false` criterion describes the unsafe outcome.
    pub fn boolean(instructions: &str, safe_criteria: &str, unsafe_criteria: &str) -> Self {
        Self {
            question_type: BOOLEAN_QUESTION_TYPE,
            instructions: instructions.to_owned(),
            criteria: BTreeMap::from([
                (TRUE_CRITERION, safe_criteria.to_owned()),
                (FALSE_CRITERION, unsafe_criteria.to_owned()),
            ]),
        }
    }
}

/// The decisions request envelope: typed questions evaluated against the state.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionsRequest {
    pub model: String,
    pub questions: BTreeMap<String, DecisionsQuestion>,
    pub state: serde_json::Value,
}

/// One answer per question. A boolean answer carries the probability of the
/// `true` criterion; a refusal carries no value at all. The shape is a plain
/// struct because serde's internally-tagged enums cannot round-trip numeric
/// fields under serde_json's arbitrary-precision feature.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DecisionsAnswer {
    #[serde(rename = "type")]
    pub answer_type: String,
    #[serde(default)]
    pub noul: Option<f64>,
}

impl DecisionsAnswer {
    pub const BOOLEAN_TYPE: &'static str = "noul";
    pub const REFUSAL_TYPE: &'static str = "refusal";
}

/// The decisions response envelope: one answer per asked question.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DecisionsResponse {
    pub id: String,
    pub model: String,
    pub answers: BTreeMap<String, DecisionsAnswer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safety_request() -> DecisionsRequest {
        DecisionsRequest {
            model: "openrouter/perplexity/pplx-decider-v1.2".into(),
            questions: BTreeMap::from([(
                SAFETY_QUESTION_NAME.to_owned(),
                DecisionsQuestion::boolean("Is the command safe?", "reads only", "deletes data"),
            )]),
            state: serde_json::json!({"command": "rm -rf /"}),
        }
    }

    #[test]
    fn request_serializes_the_typed_boolean_question() {
        let encoded = serde_json::to_value(safety_request()).unwrap();
        assert_eq!(encoded["model"], "openrouter/perplexity/pplx-decider-v1.2");
        let question = &encoded["questions"][SAFETY_QUESTION_NAME];
        assert_eq!(question["type"], "noul");
        assert_eq!(question["instructions"], "Is the command safe?");
        assert_eq!(question["criteria"]["true"], "reads only");
        assert_eq!(question["criteria"]["false"], "deletes data");
        assert_eq!(encoded["state"]["command"], "rm -rf /");
    }

    #[test]
    fn noul_answer_decodes_as_a_probability() {
        let decoded: DecisionsResponse = serde_json::from_str(
            r#"{"id":"req_1","model":"openrouter/perplexity/pplx-decider-v1.2","answers":{"is_command_safe":{"type":"noul","noul":0.97,"confidence":0.8}}}"#,
        )
        .unwrap();
        let answer = &decoded.answers[SAFETY_QUESTION_NAME];
        assert_eq!(answer.answer_type, "noul");
        assert!((answer.noul.unwrap() - 0.97).abs() < 1e-9);
    }

    #[test]
    fn refusal_and_unknown_answers_decode_without_a_value() {
        let refusal: DecisionsAnswer = serde_json::from_str(r#"{"type":"refusal"}"#).unwrap();
        assert_eq!(refusal.answer_type, DecisionsAnswer::REFUSAL_TYPE);
        assert_eq!(refusal.noul, None);
        let unknown: DecisionsAnswer =
            serde_json::from_str(r#"{"type":"box","bbox":[1,2,3,4]}"#).unwrap();
        assert_eq!(unknown.answer_type, "box");
        assert_eq!(unknown.noul, None);
    }
}
