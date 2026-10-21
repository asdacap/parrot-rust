use super::*;
use crate::{
    responses::prepare,
    responses_websocket::{ReceivedFrame, WebSocketRequest},
    sessions::{Message, Request},
};
use parrot_domain::contracts::async_ops::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ScriptedFrame {
    bytes: Vec<u8>,
    end_of_message: bool,
}

struct ScriptedSocket {
    frames: VecDeque<ScriptedFrame>,
    sent: Arc<Mutex<Vec<u8>>>,
    received: Arc<Mutex<Vec<ScriptedFrame>>>,
    aborted: bool,
}

impl Socket for ScriptedSocket {
    fn state(&self) -> SocketState {
        if self.aborted {
            SocketState::Aborted
        } else {
            SocketState::Open
        }
    }

    fn send<'a>(
        &'a mut self,
        bytes: &'a [u8],
        end_of_message: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        assert!(end_of_message);
        self.sent.lock().unwrap().extend_from_slice(bytes);
        Box::pin(async { Ok(()) })
    }

    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        let frame = self.frames.pop_front().expect("unexpected socket receive");
        buffer[..frame.bytes.len()].copy_from_slice(&frame.bytes);
        self.received.lock().unwrap().push(frame.clone());
        Box::pin(async move {
            Ok(ReceivedFrame {
                count: frame.bytes.len(),
                kind: FrameKind::Text,
                end_of_message: frame.end_of_message,
                close_status: None,
                close_description: None,
            })
        })
    }

    fn abort(&mut self) {
        self.aborted = true;
    }
}

#[tokio::test]
async fn original_text_frames_reassemble_and_share_responses_event_semantics() {
    // ResponsesWebSocketTests.cs:140-146, including the original byte boundary.
    let delta = r#"{"type":"response.output_text.delta","delta":"hello"}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"resp-1","usage":{"input_tokens":7,"output_tokens":2},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}}"#;
    let split = delta.as_bytes();
    let frames = vec![
        ScriptedFrame {
            bytes: split[..17].to_vec(),
            end_of_message: false,
        },
        ScriptedFrame {
            bytes: split[17..].to_vec(),
            end_of_message: true,
        },
        ScriptedFrame {
            bytes: completed.as_bytes().to_vec(),
            end_of_message: true,
        },
    ];
    let sent = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(Mutex::new(Vec::new()));
    let socket = ScriptedSocket {
        frames: frames.clone().into(),
        sent: sent.clone(),
        received: received.clone(),
        aborted: false,
    };
    let mut connection = ResponsesWebSocket::new(
        Box::new(socket),
        Vec::new(),
        Some(Duration::from_secs(1)),
        64 << 20,
    );
    let idle_timeout_ms = connection.idle_timeout.unwrap().as_millis();
    let maximum_request_bytes = connection.maximum_request_bytes;
    let request = Request {
        model: "model".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let prepared = prepare(&request, "test").unwrap();
    let encoded = WebSocketRequest::new(&prepared).unwrap().encode("", 0, "");
    let mut exchange = connection.send(encoded, "test").unwrap();
    let mut events = Vec::new();
    while let Some(event) = exchange.next(&Never).await.unwrap() {
        events.push(event);
    }

    // Read the parser actually owned and advanced by this transport exchange.
    let (response_id, output) = exchange.state.parser.original_text_frame_state();
    let sent_body = String::from_utf8(sent.lock().unwrap().clone()).unwrap();
    let sent_json: Value = serde_json::from_str(&sent_body).unwrap();
    let kinds: Vec<_> = events
        .iter()
        .map(|event| match event {
            ResponsesEvent::TextDelta(_) => "TextDelta",
            ResponsesEvent::ReasoningDelta { .. } => "ReasoningDelta",
            ResponsesEvent::EncryptedReasoningReceived => "EncryptedReasoningReceived",
            ResponsesEvent::ToolCallDelta(_) => "ToolCallDelta",
            ResponsesEvent::Completed(_) => "Completed",
        })
        .collect();
    let ResponsesEvent::Completed(last) = events.last().unwrap() else {
        panic!("the actual final event is not Completed");
    };
    let event_observations: Vec<Value> = events
        .iter()
        .map(|event| match event {
            ResponsesEvent::TextDelta(text) => json!({"kind":"TextDelta","text":text}),
            ResponsesEvent::Completed(completion) => json!({
                "kind":"Completed",
                "assistant_text":completion.assistant_text,
                "input_tokens":completion.input_tokens,
            }),
            other => json!({"unexpected_native_event":format!("{other:?}")}),
        })
        .collect();
    let received = received.lock().unwrap().clone();
    let received_observations: Vec<Value> = received
        .iter()
        .map(|frame| json!({"bytes":frame.bytes,"end_of_message":frame.end_of_message}))
        .collect();
    let private_output: Vec<Value> = output
        .iter()
        .map(|item| serde_json::from_str(item).unwrap())
        .collect();
    println!(
        "T27_ORIGINAL_TEXTFRAME_OBSERVATION={}",
        json!({
            "sent_utf8":sent_body,
            "event_kinds":kinds,
            "assistant_text":last.assistant_text,
            "input_tokens":last.input_tokens,
            "parser_response_id":response_id,
            "parser_output":private_output,
            "sent_json":sent_json,
            "events":event_observations,
            "parser_output_raw":output,
            "consumed_frame_count":received.len(),
            "received_frames":received_observations,
            "idle_timeout_ms":idle_timeout_ms,
            "maximum_request_bytes":maximum_request_bytes,
        })
    );

    // The seven original predicates at source lines171-178.
    assert_eq!(sent_json["type"].as_str(), Some("response.create"));
    assert_eq!(sent_json["stream"].as_bool(), Some(true));
    assert_eq!(kinds, ["TextDelta", "Completed"]);
    assert_eq!(last.assistant_text, "hello");
    assert_eq!(last.input_tokens, 7);
    assert_eq!(response_id, "resp-1");
    assert_eq!(output.len(), 1);

    assert_eq!(received, frames);
}
