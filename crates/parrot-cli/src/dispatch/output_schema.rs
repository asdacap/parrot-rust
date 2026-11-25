//! One-shot output schema capture: the root agent's final response must match
//! the caller-supplied JSON schema. The capture mirrors the renderer's own
//! segmentation so the validated candidate is the last response of the turn,
//! not the concatenation of every streamed fragment.

use parrot_domain::protocol::messages::parrot::{Event, event::Payload};

use crate::dispatch::hierarchy::AgentSessionHierarchy;

pub struct OutputSchemaCapture {
    validator: jsonschema::Validator,
    hierarchy: AgentSessionHierarchy,
    candidate: String,
}

impl OutputSchemaCapture {
    pub fn new(schema: &str) -> Result<Self, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(schema).map_err(|error| error.to_string())?;
        let validator = jsonschema::validator_for(&parsed).map_err(|error| error.to_string())?;
        Ok(Self {
            validator,
            hierarchy: AgentSessionHierarchy::new(),
            candidate: String::new(),
        })
    }

    pub fn observe(&mut self, event: &Event) {
        self.hierarchy.observe(event);
        if !self.hierarchy.is_root(&event.agent_session_id) {
            return;
        }
        match &event.payload {
            Some(Payload::TextChunk(chunk)) => self.candidate.push_str(&chunk.fragment),
            Some(Payload::ToolCallChunk(_))
            | Some(Payload::ToolStarted(_))
            | Some(Payload::ToolFinished(_))
            | Some(Payload::ToolCancelled(_))
            | Some(Payload::ToolError(_))
            | Some(Payload::ToolExecutionStarted(_))
            | Some(Payload::ToolRequestReceived(_))
            // A steer or a repair injection replaces the streamed response so
            // far; the next fragments form the new candidate.
            | Some(Payload::InputPromoted(_))
            | Some(Payload::InputAdmitted(_)) => self.candidate.clear(),
            Some(Payload::AgentNotice(notice)) if notice.kind == "completion_blocker" => {
                self.candidate.clear();
            }
            Some(Payload::RetryNotice(_)) => self.candidate.clear(),
            _ => {}
        }
    }

    /// Errors when the captured final response does not match the schema.
    pub fn validate(&self) -> Result<(), String> {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(self.candidate.trim()) else {
            return Err(format!(
                "the final response is not valid JSON:\n{}",
                self.candidate.trim()
            ));
        };
        let mut errors = self.validator.iter_errors(&parsed);
        match errors.next() {
            None => Ok(()),
            Some(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_domain::protocol::messages::parrot::{
        AgentStarted, TextChunk, ToolStarted, TurnStarted,
    };

    fn event(session: &str, payload: Payload) -> Event {
        let mut event = Event::new();
        event.agent_session_id = session.to_owned();
        event.payload = Some(payload);
        event
    }

    fn root_turn(capture: &mut OutputSchemaCapture) {
        capture.observe(&event("root", Payload::TurnStarted(TurnStarted::new())));
    }

    fn capture(schema: &str) -> OutputSchemaCapture {
        OutputSchemaCapture::new(schema).expect("schema compiles")
    }

    #[test]
    fn object_response_validates() {
        let mut capture = capture("{\"type\":\"object\",\"required\":[\"name\"]}");
        root_turn(&mut capture);
        for fragment in ["{\"na", "me\":\"parrot\"}"] {
            capture.observe(&event(
                "root",
                Payload::TextChunk(TextChunk {
                    fragment: fragment.to_owned(),
                    ..Default::default()
                }),
            ));
        }
        capture.validate().expect("response matches the schema");
    }

    #[test]
    fn missing_required_property_fails_validation() {
        let mut capture = capture("{\"type\":\"object\",\"required\":[\"name\"]}");
        root_turn(&mut capture);
        capture.observe(&event(
            "root",
            Payload::TextChunk(TextChunk {
                fragment: "{}".to_owned(),
                ..Default::default()
            }),
        ));
        assert!(capture.validate().is_err());
    }

    #[test]
    fn non_json_response_fails_validation() {
        let mut capture = capture("{\"type\":\"object\"}");
        root_turn(&mut capture);
        capture.observe(&event(
            "root",
            Payload::TextChunk(TextChunk {
                fragment: "plain prose".to_owned(),
                ..Default::default()
            }),
        ));
        assert!(capture.validate().is_err());
    }

    #[test]
    fn tool_activity_resets_the_candidate() {
        let mut capture = capture("{\"type\":\"object\"}");
        root_turn(&mut capture);
        capture.observe(&event(
            "root",
            Payload::TextChunk(TextChunk {
                fragment: "not json".to_owned(),
                ..Default::default()
            }),
        ));
        capture.observe(&event(
            "root",
            Payload::ToolStarted(ToolStarted {
                tool_call_id: "call".into(),
                tool_name: "read".into(),
                ..Default::default()
            }),
        ));
        capture.observe(&event(
            "root",
            Payload::TextChunk(TextChunk {
                fragment: "{}".to_owned(),
                ..Default::default()
            }),
        ));
        capture.validate().expect("only the post-tool text counts");
    }

    #[test]
    fn child_agent_text_is_ignored() {
        let mut capture = capture("{\"type\":\"object\"}");
        root_turn(&mut capture);
        let mut child = AgentStarted::new();
        child.parent_agent_session_id = "root".into();
        child.name = "worker".into();
        capture.observe(&event("child", Payload::AgentStarted(child)));
        capture.observe(&event(
            "child",
            Payload::TextChunk(TextChunk {
                fragment: "child prose".to_owned(),
                ..Default::default()
            }),
        ));
        let error = capture.validate().expect_err("empty root candidate");
        assert!(error.contains("not valid JSON"));
    }

    #[test]
    fn an_invalid_schema_is_rejected() {
        assert!(OutputSchemaCapture::new("not a schema").is_err());
    }
}
