use super::WebSocketError;

/// Creates diagnostics for each actual WebSocket attempt, including reconnects.
pub trait WebSocketObserver: Send + Sync {
    fn start(&self) -> Box<dyn WebSocketAttemptObserver>;
}

/// Observes one transport attempt without changing its request or response.
pub trait WebSocketAttemptObserver: Send {
    /// Captures wire bytes alongside the complete prepared provider body before
    /// continuation trimming. The full body excludes WebSocket envelope fields.
    fn dump_request(&mut self, _body: &[u8], _full_body: &[u8]) {}
    fn record_request_bytes(&mut self, _bytes: u64) {}
    fn record_response_bytes(&mut self, _bytes: u64) {}
    /// Captures each received non-close payload fragment before parsing or
    /// validation. The flag preserves message boundaries, including empty ones.
    fn capture_response(&mut self, _body: &[u8], _end_of_message: bool) {}
    fn record_close_status(&mut self, _status: u16) {}
    fn finish(&mut self, _outcome: WebSocketAttemptOutcome<'_>) {}
}

pub enum WebSocketAttemptOutcome<'a> {
    Completed,
    Failed(&'a WebSocketError),
    Disposed,
}
