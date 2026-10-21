//! Opt-in Responses WebSocket transport and connection-local continuation state.
//! HTTP remains the default. Provider-wide retry policy belongs to the caller.
mod error;
mod native;
mod observer;
mod request;
mod session;
mod socket;
mod transport;

pub use error::{SocketErrorCode, WebSocketError};
pub use native::NativeConnector;
pub use observer::{WebSocketAttemptObserver, WebSocketAttemptOutcome, WebSocketObserver};
pub use request::WebSocketRequest;
pub use session::{HeaderSource, HttpEvents, HttpFallback, Session, SessionCall, SessionEvent};
pub use socket::{
    ConnectedSocket, Connector, FrameKind, Headers, ReceivedFrame, Socket, SocketState,
    WebSocketClient, has_authority,
};
pub use transport::{
    DEFAULT_IDLE_TIMEOUT, Exchange, ExchangeStats, FRAME_BYTES, MAXIMUM_MESSAGE_BYTES, REUSE_LIMIT,
    ResponsesWebSocket,
};
