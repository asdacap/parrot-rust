use super::WebSocketError;

/// Creates diagnostics for each actual WebSocket attempt, including reconnects.
pub trait WebSocketObserver: Send + Sync {
    fn start(&self) -> Box<dyn WebSocketAttemptObserver>;
}

pub trait WebSocketAttemptObserver: Send {
    fn dump_request(&mut self, _body: &[u8]) {}
    fn record_request_bytes(&mut self, _bytes: u64) {}
    fn record_response_bytes(&mut self, _bytes: u64) {}
    fn record_close_status(&mut self, _status: u16) {}
    fn finish(&mut self, _outcome: WebSocketAttemptOutcome<'_>) {}
}

pub enum WebSocketAttemptOutcome<'a> {
    Completed,
    Failed(&'a WebSocketError),
    Disposed,
}
