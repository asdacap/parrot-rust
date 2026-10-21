//! Input-only observations for native URI authority security regressions.
use super::family_projection::source_error;
use parrot_provider::http_stream::*;
use serde_json::{Value, json};

pub fn observe(input: &Value) -> Value {
    let raw = input["raw_endpoint"].as_str().unwrap();
    let parsed = Endpoint::parse(raw);
    let (success, parse_error, observation) = match parsed {
        Ok(endpoint) => {
            let host_port = endpoint
                .authority()
                .split_once("://")
                .unwrap()
                .1
                .rsplit('@')
                .next()
                .unwrap();
            let host = if endpoint.as_url().port().is_some() {
                host_port.rsplit_once(':').unwrap().0
            } else {
                host_port
            };
            let user_info = match endpoint.as_url().password() {
                Some(password) => format!("{}:{password}", endpoint.as_url().username()),
                None => endpoint.as_url().username().to_owned(),
            };
            let matches: Vec<_> = input["authorities"].as_array().unwrap().iter().map(|authority| json!({"authority":authority,"matches":has_authority(&endpoint,authority.as_str().unwrap())})).collect();
            (
                true,
                Value::Null,
                json!({"host":host,"idn_host":endpoint.as_url().host_str().unwrap(),"authority":endpoint.authority(),"absolute_uri":endpoint.as_str(),"user_info":user_info,"matches":matches}),
            )
        }
        Err(error) => {
            // Source CLR identity adapter, not a native Rust exception claim.
            let message = if error == url::ParseError::RelativeUrlWithoutBase {
                "Invalid URI: The Authority/Host could not be parsed."
            } else {
                "Invalid URI: The hostname could not be parsed."
            };
            (
                false,
                json!({"category":"System.UriFormatException","message":message}),
                Value::Null,
            )
        }
    };
    let (endpoint_url, endpoint_error) = match endpoint_url(
        raw,
        input["endpoint"].as_str().unwrap(),
        input["allow_local"].as_bool().unwrap(),
        input["allow_remote"].as_bool().unwrap(),
    ) {
        Ok(endpoint) => (json!(endpoint.as_str()), Value::Null),
        Err(error) => {
            let error = source_error(&error);
            (
                Value::Null,
                json!({"category":error["category"],"message":error["message"]}),
            )
        }
    };
    json!({"uri_try_create_success":success,"parse_error":parse_error,"parsed":observation,"endpoint_url":endpoint_url,"endpoint_error":endpoint_error})
}

#[cfg(test)]
#[test]
fn independent_native_uri_security_matrix() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/security-v1/expected.json")).unwrap();
    assert_eq!(oracle["cases"].as_array().unwrap().len(), 20);
    for case in oracle["cases"].as_array().unwrap() {
        assert_eq!(
            observe(&case["input"]),
            case["observed"],
            "{}",
            case["case_id"]
        );
    }
}
