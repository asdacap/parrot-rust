use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::{
    CallToolResult, ClientInfo, ContentBlock, HttpServerParams, McpClient, McpError,
    StdioServerParams,
};

const SHORT: Duration = Duration::from_millis(100);
const GENEROUS: Duration = Duration::from_secs(5);

fn stdio_params(script: &str) -> StdioServerParams {
    StdioServerParams {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Default::default(),
        cwd: None,
    }
}

fn client_info() -> ClientInfo {
    ClientInfo::new("parrot-test", "0")
}

/// Fake stdio server shell script. Each line is answered by method; the id of
/// the request is echoed so the transport accepts the response.
fn fake_stdio_script() -> String {
    r#"
resp() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*)
      resp "$id" '{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}' ;;
    *'"method":"tools/list"'*'"cursor"'*)
      resp "$id" '{"tools":[{"name":"second","description":"page two tool","inputSchema":{"type":"object"}}]}' ;;
    *'"method":"tools/list"'*)
      resp "$id" '{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object"}}],"nextCursor":"page2"}' ;;
    *'"method":"tools/call"'*'"boom"'*)
      resp "$id" '{"content":[{"type":"text","text":"tool failed"}],"isError":true}' ;;
    *'"method":"tools/call"'*)
      resp "$id" '{"content":[{"type":"text","text":"echo-ok"}],"isError":false}' ;;
    *) ;;
  esac
done
"#
    .to_owned()
}

#[tokio::test]
async fn stdio_handshake_lists_and_calls() {
    let mut client = McpClient::connect_stdio(&stdio_params(&fake_stdio_script())).unwrap();
    let handshake = client.initialize(client_info(), GENEROUS).await.unwrap();
    assert_eq!(handshake.protocol_version, "2025-06-18");
    assert_eq!(handshake.server_info.unwrap().name, "fake");

    let tools = client.list_tools(GENEROUS).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "second"]);
    assert_eq!(tools[0].description.as_deref(), Some("echoes"));

    let result = client
        .call_tool("echo", json!({"value": "x"}), GENEROUS)
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.text(), "echo-ok");
    client.close().await;
}

#[tokio::test]
async fn stdio_tool_error_result_is_reported() {
    let mut client = McpClient::connect_stdio(&stdio_params(&fake_stdio_script())).unwrap();
    client.initialize(client_info(), GENEROUS).await.unwrap();
    let result = client
        .call_tool("echo", json!({"value": "boom"}), GENEROUS)
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.text(), "tool failed");
    client.close().await;
}

#[tokio::test]
async fn stdio_closed_connection_surfaces_as_closed() {
    let script = "read line; exit 0";
    let mut client = McpClient::connect_stdio(&stdio_params(script)).unwrap();
    let error = client
        .initialize(client_info(), GENEROUS)
        .await
        .unwrap_err();
    assert_eq!(error, McpError::Closed);
}

#[tokio::test]
async fn stdio_handshake_times_out_when_server_is_silent() {
    let script = "sleep 5";
    let mut client = McpClient::connect_stdio(&stdio_params(script)).unwrap();
    let error = client.initialize(client_info(), SHORT).await.unwrap_err();
    assert_eq!(error, McpError::Timeout);
    client.close().await;
}

#[tokio::test]
async fn protocol_content_blocks_parse() {
    let result: CallToolResult = serde_json::from_value(json!({
        "content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///x", "text": "body"}}
        ],
        "isError": false
    }))
    .unwrap();
    assert_eq!(result.content.len(), 3);
    assert!(matches!(result.content[1], ContentBlock::Image { .. }));
    assert_eq!(result.text(), "hello");
}

/// Reads one HTTP request from the stream, headers and body included.
async fn read_request(stream: &mut BufReader<TcpStream>) -> Option<(Vec<String>, String)> {
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let read = stream.read_line(&mut line).await.ok()?;
        if read == 0 {
            return None;
        }
        let trimmed = line.trim_end().to_owned();
        if trimmed.is_empty() {
            break;
        }
        headers.push(trimmed);
    }
    let length = headers
        .iter()
        .find(|header| header.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|header| header.split_once(':'))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body).await.ok()?;
    }
    Some((headers, String::from_utf8(body).unwrap()))
}

async fn write_response(
    stream: &mut BufReader<TcpStream>,
    status: &str,
    headers: &[(&str, &str)],
    body: &str,
) {
    let mut response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// How the fake server answers a `tools/call` request.
enum ToolReply {
    /// Single JSON document.
    Json,
    /// SSE event stream carrying one response event.
    Sse,
}

fn call_result(id: u64, text: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{text}"}}],"isError":false}}}}"#
    )
}

/// Fake streamable-HTTP server answering the canned replies below. Observed
/// request headers arrive on `observed`.
async fn fake_http_server(
    listener: TcpListener,
    reply: ToolReply,
    observed: mpsc::Sender<Vec<String>>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(stream);
    loop {
        let Some((headers, body)) = read_request(&mut stream).await else {
            return;
        };
        let parsed: Value = serde_json::from_str(&body).unwrap();
        let method = parsed["method"].as_str().unwrap_or_default().to_owned();
        let id = parsed["id"].as_u64();
        match method.as_str() {
            "initialize" => {
                let payload = format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"fake-http","version":"0"}}}}}}"#,
                    id.unwrap()
                );
                write_response(
                    &mut stream,
                    "200 OK",
                    &[
                        ("Content-Type", "application/json"),
                        ("Mcp-Session-Id", "sess-1"),
                    ],
                    &payload,
                )
                .await;
            }
            "notifications/initialized" => {
                write_response(
                    &mut stream,
                    "202 Accepted",
                    &[("Content-Type", "text/plain")],
                    "",
                )
                .await;
            }
            "tools/call" => {
                let _ = observed.send(headers).await;
                let id = id.unwrap();
                let payload = match reply {
                    ToolReply::Json => call_result(id, "http-ok"),
                    ToolReply::Sse => format!(
                        ": ping\r\n\r\nevent: message\r\ndata: {}\r\n\r\n",
                        call_result(id, "sse-ok")
                    ),
                };
                let content_type = match reply {
                    ToolReply::Json => "application/json",
                    ToolReply::Sse => "text/event-stream",
                };
                write_response(
                    &mut stream,
                    "200 OK",
                    &[("Content-Type", content_type)],
                    &payload,
                )
                .await;
            }
            other => panic!("unexpected method {other}"),
        }
    }
}

fn http_params(url: String) -> HttpServerParams {
    HttpServerParams {
        url,
        headers: Default::default(),
        allow_insecure_localhost: true,
    }
}

async fn connect_fake_http(reply: ToolReply) -> (McpClient, mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (observed, receiver) = mpsc::channel(1);
    tokio::spawn(fake_http_server(listener, reply, observed));
    let client = McpClient::connect_http(&http_params(url)).unwrap();
    (client, receiver)
}

#[tokio::test]
async fn http_handshake_captures_session_and_sends_headers() {
    let (mut client, mut observed) = connect_fake_http(ToolReply::Json).await;
    let handshake = client.initialize(client_info(), GENEROUS).await.unwrap();
    assert_eq!(handshake.server_info.unwrap().name, "fake-http");

    let result = client
        .call_tool("echo", json!({"value": "x"}), GENEROUS)
        .await
        .unwrap();
    assert_eq!(result.text(), "http-ok");

    let headers = observed.recv().await.unwrap();
    let joined = headers.join("\n");
    assert!(
        joined
            .to_ascii_lowercase()
            .contains("mcp-session-id: sess-1")
    );
    assert!(
        joined
            .to_ascii_lowercase()
            .contains("mcp-protocol-version: 2025-06-18")
    );
    client.close().await;
}

#[tokio::test]
async fn http_tool_call_reads_sse_response() {
    let (mut client, _observed) = connect_fake_http(ToolReply::Sse).await;
    client.initialize(client_info(), GENEROUS).await.unwrap();
    let result = client.call_tool("echo", json!({}), GENEROUS).await.unwrap();
    assert_eq!(result.text(), "sse-ok");
    client.close().await;
}
