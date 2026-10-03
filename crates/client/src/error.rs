//! Errors of the client, and how a WebSocket connection ended ([`CloseInfo`]).

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use scacelith_protocol::{EncodeError, ErrorCode, error_code_for_close};
use serde_json::Value;

/// Who started the end of a WebSocket connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closer {
    /// The server sent a close frame first.
    Server,
    /// The client sent a close frame first (a call to `close`, a dropped connection, or a
    /// protocol error found by the client).
    Client,
    /// The transport ended without a closing handshake (close code 1006).
    Transport,
}

/// How a WebSocket connection ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseInfo {
    /// The close code: the one the initiator sent (1005 when its close frame had none, 1006 when
    /// the transport ended without a close frame).
    pub code: u16,
    /// The reason sent with the code (empty when none).
    pub reason: String,
    /// Who closed first.
    pub closer: Closer,
}

impl CloseInfo {
    /// Code of a connection that ended without a closing handshake.
    pub const ABNORMAL: u16 = 1006;
    /// Code of a close frame without a status code.
    pub const NO_STATUS: u16 = 1005;

    /// The protocol error code this close code stands for (`4000 + code`, `4300 + code - 240`),
    /// when it follows the close-code rule of the realtime protocol.
    pub fn error_code(&self) -> Option<ErrorCode> {
        error_code_for_close(self.code)
    }

    pub(crate) fn abnormal(reason: impl Into<String>) -> CloseInfo {
        CloseInfo { code: Self::ABNORMAL, reason: reason.into(), closer: Closer::Transport }
    }
}

impl fmt::Display for CloseInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let who = match self.closer {
            Closer::Server => "server",
            Closer::Client => "client",
            Closer::Transport => "transport",
        };
        write!(f, "{} (closed by the {who}", self.code)?;
        if !self.reason.is_empty() {
            write!(f, ": {}", self.reason)?;
        }
        f.write_str(")")
    }
}

/// An HTTPS API answer that is not the success the call expects.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// HTTP status.
    pub status: u16,
    /// The `error` code of the body (`invalid_credentials`...), or `http_<status>` when the body
    /// is not an API error.
    pub error: String,
    /// The `message` of the body (empty when absent).
    pub message: String,
    /// `Retry-After` (seconds), when the answer has one.
    pub retry_after: Option<u64>,
    /// The parsed body (`Null` when it is not JSON).
    pub body: Value,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}", self.status, self.error)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

/// Errors of the client.
#[derive(Debug)]
pub enum ClientError {
    /// A socket error (connect, read, write).
    Io(std::io::Error),
    /// A TLS setting that cannot be used: an invalid server name or certificate.
    Tls(String),
    /// A deadline passed; the text names the step.
    Timeout(&'static str),
    /// The HTTP answer could not be read (malformed, cut short, too large).
    Http(String),
    /// The HTTPS API answered an error.
    Api(Box<ApiError>),
    /// The WebSocket upgrade was answered with another status than 101.
    UpgradeRefused {
        /// HTTP status (426 for a wrong subprotocol, 429 with `Retry-After`, 503...).
        status: u16,
        /// `Retry-After` in seconds, when present.
        retry_after: Option<u64>,
        /// The body of the answer.
        body: Bytes,
    },
    /// The WebSocket handshake or a frame broke RFC 6455.
    WebSocket(String),
    /// The connection is closed.
    Closed(CloseInfo),
    /// A message could not be encoded (a field outside its bounds).
    Encode(EncodeError),
    /// The server refused the Hello with a fatal `Error`.
    Refused {
        /// The error code.
        code: ErrorCode,
        /// How the connection ended after it, when it did within the wait.
        close: Option<CloseInfo>,
    },
    /// A message that the flow in progress did not expect.
    Unexpected(String),
}

impl ClientError {
    /// The close information when the error is a closed connection or a refused Hello.
    pub fn close_info(&self) -> Option<&CloseInfo> {
        match self {
            ClientError::Closed(info) => Some(info),
            ClientError::Refused { close, .. } => close.as_ref(),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "I/O error: {e}"),
            ClientError::Tls(e) => write!(f, "TLS: {e}"),
            ClientError::Timeout(step) => write!(f, "timeout: {step}"),
            ClientError::Http(e) => write!(f, "HTTP: {e}"),
            ClientError::Api(e) => write!(f, "API: {e}"),
            ClientError::UpgradeRefused { status, .. } => {
                write!(f, "WebSocket upgrade refused: HTTP {status}")
            }
            ClientError::WebSocket(e) => write!(f, "WebSocket: {e}"),
            ClientError::Closed(info) => write!(f, "connection closed: {info}"),
            ClientError::Encode(e) => write!(f, "{e}"),
            ClientError::Refused { code, close } => {
                write!(f, "Hello refused: {code:?}")?;
                if let Some(close) = close {
                    write!(f, ", close {close}")?;
                }
                Ok(())
            }
            ClientError::Unexpected(what) => write!(f, "unexpected: {what}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Io(e) => Some(e),
            ClientError::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

impl From<EncodeError> for ClientError {
    fn from(e: EncodeError) -> Self {
        ClientError::Encode(e)
    }
}

/// Runs `fut` with a deadline: [`ClientError::Timeout`] naming `step` when it passes.
pub(crate) async fn with_timeout<T>(
    limit: Duration,
    step: &'static str,
    fut: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(limit, fut).await.map_err(|_| ClientError::Timeout(step))?
}

/// Result of the client.
pub type Result<T, E = ClientError> = std::result::Result<T, E>;
