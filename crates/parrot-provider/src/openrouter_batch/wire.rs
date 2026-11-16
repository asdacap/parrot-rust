//! Statically typed OpenRouter Batch submit and status payloads.
//! OpenRouter reads the submit body as a stream, so the envelope serializes
//! `requests` last.
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Serialize, Debug)]
pub struct BatchSubmit {
    pub endpoint: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_window: Option<String>,
    pub requests: Vec<BatchRequestEntry>,
}
impl BatchSubmit {
    /// Encodes the envelope with `requests` serialized last.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}
#[derive(Serialize, Debug)]
pub struct BatchRequestEntry {
    pub custom_id: String,
    pub body: Box<RawValue>,
}
#[derive(Deserialize, Debug)]
pub struct BatchStatus {
    pub id: String,
    pub status: String,
    pub request_counts: Option<RequestCounts>,
    pub usage: Option<BatchUsage>,
    pub results: Option<Vec<BatchResult>>,
    pub error: Option<BatchError>,
}
impl BatchStatus {
    /// Decodes one `GET /batches/{id}` response body.
    pub fn decode(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}
#[derive(Deserialize, Debug, Eq, PartialEq)]
pub struct RequestCounts {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub completed: i64,
    #[serde(default)]
    pub failed: i64,
}
#[derive(Deserialize, Debug, Eq, PartialEq)]
pub struct BatchUsage {
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
}
#[derive(Deserialize, Debug)]
pub struct BatchResult {
    pub custom_id: String,
    pub response: Option<BatchResponse>,
    pub error: Option<BatchError>,
}
#[derive(Deserialize, Debug)]
pub struct BatchResponse {
    pub status_code: i64,
    pub body: Box<RawValue>,
}
#[derive(Deserialize, Debug, Eq, PartialEq)]
pub struct BatchError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_serializes_requests_last_in_declaration_order() {
        let submit = BatchSubmit {
            endpoint: "/v1/chat/completions".into(),
            model: "vendor/model".into(),
            provider: None,
            callback_url: None,
            completion_window: None,
            requests: vec![BatchRequestEntry {
                custom_id: "entry".into(),
                body: serde_json::from_str(r#"{"messages":[]}"#).unwrap(),
            }],
        };
        let encoded = String::from_utf8(submit.encode().unwrap()).unwrap();
        let request_position = encoded.find(r#""requests":"#).unwrap();
        let endpoint_position = encoded.find(r#""endpoint":"#).unwrap();
        let model_position = encoded.find(r#""model":"#).unwrap();
        assert_eq!(
            encoded,
            r#"{"endpoint":"/v1/chat/completions","model":"vendor/model","requests":[{"custom_id":"entry","body":{"messages":[]}}]}"#
        );
        assert!(endpoint_position < model_position);
        assert!(model_position < request_position);
    }

    #[test]
    fn status_decodes_counts_results_and_errors() {
        let status = BatchStatus::decode(
            r#"{"id":"batch_123","object":"batch","status":"in_progress","request_counts":{"total":2,"completed":1,"failed":0},"usage":null,"results":null,"error":{"message":"rejected"}}"#,
        )
        .unwrap();
        assert_eq!(status.id, "batch_123");
        assert_eq!(status.status, "in_progress");
        assert_eq!(
            status.request_counts,
            Some(RequestCounts {
                total: 2,
                completed: 1,
                failed: 0
            })
        );
        assert_eq!(
            status.error.map(|error| error.message),
            Some("rejected".into())
        );
    }

    #[test]
    fn result_decodes_response_body_and_request_error() {
        let status = BatchStatus::decode(
            r#"{"id":"b","status":"completed","results":[
                {"id":"r1","custom_id":"one","response":{"status_code":200,"request_id":"q","body":{"id":"gen","choices":[{"message":{"role":"assistant","content":"ok"}}]}},"error":null},
                {"id":"r2","custom_id":"two","response":null,"error":{"type":"invalid_request_error","message":"bad","param":"max_tokens"}}
            ]}"#,
        )
        .unwrap();
        let results = status.results.unwrap();
        let first = &results[0];
        assert_eq!(first.custom_id, "one");
        assert_eq!(first.response.as_ref().unwrap().status_code, 200);
        assert_eq!(
            first.response.as_ref().unwrap().body.get(),
            r#"{"id":"gen","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#
        );
        assert_eq!(results[1].error.as_ref().unwrap().message, "bad");
    }
}
