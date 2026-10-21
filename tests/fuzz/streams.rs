use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use parrot_cli::terminal_input::{TerminalKey, TerminalKeyDecoder, TerminalKeyKind};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::{HttpError, SseDecoder, SseEvent};
use parrot_provider::responses::{ResponsesError, ResponsesEvent, ResponsesParser};
use parrot_provider::responses_websocket::{
    FRAME_BYTES, FrameKind, MAXIMUM_MESSAGE_BYTES, ReceivedFrame, ResponsesWebSocket, Socket,
    SocketState, WebSocketError,
};
use parrot_storage::events::NeverCancelled;
use serde_json::Value;

use crate::support::{Case, Generator, cases, for_cases, hex, seeds};

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../fixtures/regressions/T77/stream-regressions.json"
    ))
    .unwrap()
}

fn input(row: &Value) -> Vec<u8> {
    row.get("hex").map_or_else(
        || row["input"].as_str().unwrap().as_bytes().to_vec(),
        |value| hex(value.as_str().unwrap()),
    )
}

fn sse(bytes: &[u8], limit: i32, cuts: &[usize]) -> Vec<Result<SseEvent, HttpError>> {
    let mut decoder = SseDecoder::new(limit);
    let mut observed = Vec::new();
    let mut start = 0;
    for &end in cuts {
        observed.extend(decoder.feed(&bytes[start..end]));
        start = end;
    }
    observed.extend(decoder.feed(&bytes[start..]));
    observed.extend(decoder.finish());
    observed
}

fn cuts(length: usize, generator: &mut Generator) -> Vec<usize> {
    let mut position = 0;
    let mut cuts = Vec::new();
    while position < length {
        position = (position + 1 + generator.pick(13)).min(length);
        cuts.push(position);
    }
    cuts
}

#[test]
fn seeded_sse_fragmentation_duplicate_fields_utf8_and_exact_utf16_limits() {
    for row in corpus()["sse"].as_array().unwrap() {
        let bytes = input(row);
        let expected = if let Some(error) = row.get("error") {
            vec![Err(HttpError::Wire(error.as_str().unwrap().into()))]
        } else {
            row["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    Ok(SseEvent {
                        event: event["event"].as_str().unwrap().into(),
                        id: event["id"].as_str().unwrap().into(),
                        data: event["data"].as_str().unwrap().into(),
                    })
                })
                .collect()
        };
        for split in 0..=bytes.len() {
            assert_eq!(
                sse(&bytes, row["limit"].as_i64().unwrap() as i32, &[split]),
                expected,
                "SSE regression={} split={split} input={bytes:02x?}",
                row["name"]
            );
        }
    }
    for_cases("sse", |case, generator| {
        let length = generator.pick(48);
        let text = generator.text(length);
        let mut data = String::new();
        for line in text.split('\n') {
            write!(&mut data, "data: {line}\r\n").unwrap();
        }
        let wire = format!(
            ": comment\r\nunknown: ignored\r\nevent: first\r\nevent: chosen\r\nid: preserved\r\nid: bad\0id\r\n{data}\r\n"
        );
        let expected = vec![Ok(SseEvent {
            event: "chosen".into(),
            id: "preserved".into(),
            data: text,
        })];
        let cuts = cuts(wire.len(), generator);
        assert_eq!(
            sse(wire.as_bytes(), 4096, &cuts),
            expected,
            "{case} wire={wire:?} cuts={cuts:?}"
        );
    });
}

fn terminal(bytes: &[u8], cuts: &[usize]) -> Vec<TerminalKey> {
    let mut decoder = TerminalKeyDecoder::new();
    let mut observed = Vec::new();
    let mut start = 0;
    for &end in cuts {
        observed.extend(decoder.feed(&bytes[start..end]));
        start = end;
    }
    observed.extend(decoder.feed(&bytes[start..]));
    // One matching poll tick, never one flush per arbitrary fragment.
    observed.extend(decoder.flush());
    observed
}

#[test]
fn seeded_terminal_utf8_escape_paste_fragmentation_and_source_recovery() {
    for row in corpus()["terminal"].as_array().unwrap() {
        let bytes = input(row);
        let expected: Vec<_> = row["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| {
                let kind = match key["kind"].as_str().unwrap() {
                    "Character" => TerminalKeyKind::Character,
                    "Paste" => TerminalKeyKind::Paste,
                    "Up" => TerminalKeyKind::Up,
                    "Left" => TerminalKeyKind::Left,
                    "Escape" => TerminalKeyKind::Escape,
                    unknown => panic!("unsupported committed terminal fixture kind {unknown}"),
                };
                TerminalKey::with_text(kind, key["text"].as_str().unwrap())
            })
            .collect();
        for split in 0..=bytes.len() {
            assert_eq!(
                terminal(&bytes, &[split]),
                expected,
                "terminal regression={} split={split} bytes={bytes:02x?}",
                row["name"]
            );
        }
    }
    let mut decoder = TerminalKeyDecoder::new();
    assert!(decoder.feed(&[0xf0, 0x9f]).is_empty());
    decoder.reset();
    assert_eq!(
        decoder.feed(b"X"),
        vec![TerminalKey::with_text(TerminalKeyKind::Character, "X")]
    );
    for size in [65535, 65536, 65537] {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend(vec![b'x'; size]);
        bytes.extend_from_slice(b"\x1b[201~");
        let expected = vec![TerminalKey::with_text(
            TerminalKeyKind::Paste,
            "x".repeat(size.min(65536)),
        )];
        assert_eq!(
            terminal(&bytes, &[5, 65536.min(bytes.len())]),
            expected,
            "paste raw-byte boundary {size}"
        );
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(vec![b'x'; 65535]);
    bytes.extend_from_slice("🦜".as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    assert_eq!(
        terminal(&bytes, &[6, 65542]),
        vec![TerminalKey::with_text(
            TerminalKeyKind::Paste,
            format!("{}�", "x".repeat(65535))
        )]
    );
    for_cases("terminal", |case, generator| {
        let length = generator.pick(48);
        let text = generator.text(length).replace('\n', "");
        let mut expected: Vec<_> = text
            .chars()
            .map(|ch| TerminalKey::with_text(TerminalKeyKind::Character, ch.to_string()))
            .collect();
        expected.push(TerminalKey::new(TerminalKeyKind::Up));
        expected.push(TerminalKey::new(TerminalKeyKind::Left));
        let wire = format!("{text}\x1b[A\x1bOD");
        let cuts = cuts(wire.len(), generator);
        assert_eq!(
            terminal(wire.as_bytes(), &cuts),
            expected,
            "{case} wire={wire:?} cuts={cuts:?}"
        );
    });
}

struct Frame {
    bytes: Arc<Vec<u8>>,
    offset: usize,
    kind: FrameKind,
}

#[derive(Default)]
struct SocketLog {
    sends: Vec<(usize, bool)>,
    receives: usize,
    aborts: usize,
}

struct ScriptedSocket {
    frames: VecDeque<Frame>,
    log: Arc<Mutex<SocketLog>>,
    state: SocketState,
    fragment_bytes: usize,
}

impl Socket for ScriptedSocket {
    fn state(&self) -> SocketState {
        self.state
    }

    fn send<'a>(
        &'a mut self,
        data: &'a [u8],
        end: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            self.log.lock().unwrap().sends.push((data.len(), end));
            Ok(())
        })
    }

    fn receive<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ReceivedFrame, WebSocketError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            self.log.lock().unwrap().receives += 1;
            let Some(frame) = self.frames.front_mut() else {
                return Ok(ReceivedFrame {
                    count: 0,
                    kind: FrameKind::Close,
                    end_of_message: true,
                    close_status: Some(1000),
                    close_description: Some("fixture EOF".into()),
                });
            };
            let count = buffer
                .len()
                .min(self.fragment_bytes)
                .min(frame.bytes.len() - frame.offset);
            buffer[..count].copy_from_slice(&frame.bytes[frame.offset..frame.offset + count]);
            frame.offset += count;
            let end = frame.offset == frame.bytes.len();
            let kind = frame.kind;
            if end {
                self.frames.pop_front();
            }
            Ok(ReceivedFrame {
                count,
                kind,
                end_of_message: end,
                close_status: None,
                close_description: None,
            })
        })
    }

    fn abort(&mut self) {
        self.state = SocketState::Aborted;
        self.log.lock().unwrap().aborts += 1;
    }
}

fn connection(
    messages: Vec<(Arc<Vec<u8>>, FrameKind)>,
    maximum_request: usize,
    fragment_bytes: usize,
) -> (ResponsesWebSocket, Arc<Mutex<SocketLog>>) {
    let log = Arc::new(Mutex::new(SocketLog::default()));
    let socket = ScriptedSocket {
        frames: messages
            .into_iter()
            .map(|(bytes, kind)| Frame {
                bytes,
                kind,
                offset: 0,
            })
            .collect(),
        log: log.clone(),
        state: SocketState::Open,
        fragment_bytes,
    };
    (
        ResponsesWebSocket::new(Box::new(socket), Vec::new(), None, maximum_request),
        log,
    )
}

async fn exchange(
    messages: Vec<(Arc<Vec<u8>>, FrameKind)>,
    request: Vec<u8>,
    maximum: usize,
) -> (
    Result<Vec<ResponsesEvent>, WebSocketError>,
    Arc<Mutex<SocketLog>>,
) {
    exchange_fragmented(messages, request, maximum, 8192).await
}

async fn exchange_fragmented(
    messages: Vec<(Arc<Vec<u8>>, FrameKind)>,
    request: Vec<u8>,
    maximum: usize,
    fragment_bytes: usize,
) -> (
    Result<Vec<ResponsesEvent>, WebSocketError>,
    Arc<Mutex<SocketLog>>,
) {
    let (mut connection, log) = connection(messages, maximum, fragment_bytes);
    let result = async {
        let mut exchange = connection.send(request, "t77-synthetic")?;
        let mut observed = Vec::new();
        while let Some(event) = exchange.next(&NeverCancelled).await? {
            observed.push(event);
        }
        Ok(observed)
    }
    .await;
    (result, log)
}

fn message(text: impl Into<String>) -> (Arc<Vec<u8>>, FrameKind) {
    (Arc::new(text.into().into_bytes()), FrameKind::Text)
}

#[tokio::test]
async fn seeded_websocket_real_message_assembly_json_duplicates_and_sequence_errors() {
    for malformed in ["{", "[DONE]"] {
        let (result, _) = exchange(vec![message(malformed)], b"{}".to_vec(), 128).await;
        assert!(
            matches!(result, Err(WebSocketError::Response(_))),
            "{malformed:?}: {result:?}"
        );
    }
    let (result, _) = exchange(
        vec![(Arc::new(b"{}".to_vec()), FrameKind::Binary)],
        b"{}".to_vec(),
        128,
    )
    .await;
    assert_eq!(
        result,
        Err(WebSocketError::Wire(
            "responses: unexpected binary websocket event".into()
        ))
    );
    let (result, _) = exchange(Vec::new(), b"{}".to_vec(), 128).await;
    assert!(
        matches!(result, Err(WebSocketError::Wire(ref text)) if text.contains("closed before a terminal event")),
        "{result:?}"
    );
    let mut parser = ResponsesParser::new("t77-synthetic");
    parser
        .push_data(r#"{"type":"response.created","sequence_number":1,"response":{"id":"r"}}"#)
        .unwrap();
    assert_eq!(
        parser.push_data(r#"{"type":"response.metadata","sequence_number":1}"#),
        Err(ResponsesError::WireProtocol(
            "responses: sequence_number must be strictly increasing".into()
        ))
    );
    for seed in seeds() {
        let mut generator = Generator::new(seed);
        for index in 0..cases("websocket") {
            let case = Case {
                domain: "websocket",
                seed,
                index,
            };
            let length = generator.pick(48);
            let text = generator.text(length);
            let delta = format!(
                r#"{{"type":"ignored","type":"response.output_text.delta","sequence_number":2,"delta":"discarded","delta":{}}}"#,
                serde_json::to_string(&text).unwrap()
            );
            let messages = vec![
                message(r#"{"type":"response.created","sequence_number":1,"response":{"id":"r"}}"#),
                message(delta),
                message(
                    r#"{"type":"response.completed","sequence_number":3,"response":{"id":"r","output":[]}}"#,
                ),
            ];
            let fragment_bytes = 1 + generator.pick(13);
            let (result, log) =
                exchange_fragmented(messages, b"{}".to_vec(), 128, fragment_bytes).await;
            let observed = result.unwrap_or_else(|error| panic!("{case} text={text:?}: {error:?}"));
            let deltas: String = observed
                .iter()
                .filter_map(|event| match event {
                    ResponsesEvent::TextDelta(value) => Some(value.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, text, "{case}");
            let completion = observed
                .iter()
                .find_map(|event| match event {
                    ResponsesEvent::Completed(value) => Some(value),
                    _ => None,
                })
                .unwrap();
            assert_eq!(completion.assistant_text, text, "{case}");
            assert_eq!(completion.response_id, "r", "{case}");
            assert_eq!(completion.finish_reason, "stop", "{case}");
            assert_eq!(log.lock().unwrap().sends, vec![(2, true)], "{case}");
        }
        println!(
            "T77_END domain=websocket seed=0x{seed:016x} completed={}",
            cases("websocket")
        );
    }
}

#[tokio::test]
async fn websocket_real_request_event_and_total_stream_byte_boundaries() {
    let (result, log) = exchange(Vec::new(), vec![b'x'; 129], 128).await;
    assert_eq!(
        result,
        Err(WebSocketError::RequestTooLarge { maximum: 128 })
    );
    assert!(log.lock().unwrap().sends.is_empty());
    let (result, log) = exchange(
        Vec::new(),
        vec![b'x'; MAXIMUM_MESSAGE_BYTES + 1],
        MAXIMUM_MESSAGE_BYTES + 1,
    )
    .await;
    assert_eq!(result, Err(WebSocketError::MessageTooLarge));
    assert!(log.lock().unwrap().sends.is_empty());
    let terminal = message(r#"{"type":"response.completed","response":{"id":"r","output":[]}}"#);
    let (result, log) = exchange(
        vec![terminal],
        vec![b'x'; MAXIMUM_MESSAGE_BYTES],
        MAXIMUM_MESSAGE_BYTES,
    )
    .await;
    result.unwrap();
    assert_eq!(
        log.lock().unwrap().sends.len(),
        MAXIMUM_MESSAGE_BYTES / FRAME_BYTES
    );
    assert_eq!(log.lock().unwrap().sends.last(), Some(&(FRAME_BYTES, true)));
    let (result, _) = exchange(
        vec![(Arc::new(vec![b' '; (4 << 20) + 1]), FrameKind::Text)],
        b"{}".to_vec(),
        128,
    )
    .await;
    assert_eq!(
        result,
        Err(WebSocketError::Wire(
            "responses: websocket event exceeds 4194304 bytes".into()
        ))
    );
    let mut complete = vec![b' '; 4 << 20];
    complete[..2].copy_from_slice(b"{}");
    let block = Arc::new(complete);
    let mut messages = vec![(block, FrameKind::Text); 16];
    messages.push(message("{}"));
    let (result, _) = exchange(messages, b"{}".to_vec(), 128).await;
    assert_eq!(
        result,
        Err(WebSocketError::Wire(
            "responses: provider stream exceeds 67108864 bytes".into()
        ))
    );
}
