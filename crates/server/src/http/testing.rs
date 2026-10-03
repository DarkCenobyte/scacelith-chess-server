//! In-process test harness of the API: requests go through [`Api::handle`] without sockets.
//!
//! ```ignore
//! let mut router = Router::new();
//! router.get("/hello", RouteOpts::new(), |_ctx| async { Ok(Answer::json(json!({"hi": 1}))) });
//! let t = TestApi::from_router(router);
//! let res = t.get("/api/v1/hello").send().await;
//! assert_eq!((res.status, res.json()), (200, json!({"hi": 1})));
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Method, Request};
use hyper::body::{Body, Frame};
use serde_json::Value;
use tokio::sync::mpsc;

use super::api::Api;
use super::json;
use super::router::Router;
use crate::config::Config;
use crate::net::guard::AddressKeys;

/// The error of an aborted [`TestBody`].
#[derive(Debug, Clone, Copy)]
pub struct Aborted;

impl fmt::Display for Aborted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the client aborted the body")
    }
}

impl std::error::Error for Aborted {}

/// A request body: fixed chunks, then optionally chunks sent through a [`BodySender`].
#[derive(Debug, Default)]
pub struct TestBody {
    chunks: VecDeque<Bytes>,
    rx: Option<mpsc::UnboundedReceiver<Result<Bytes, Aborted>>>,
}

/// Feeds a [`TestBody`] chunk by chunk; dropping it ends the body.
#[derive(Debug)]
pub struct BodySender(mpsc::UnboundedSender<Result<Bytes, Aborted>>);

impl BodySender {
    /// Sends a chunk.
    pub fn send(&self, chunk: impl Into<Bytes>) {
        let _ = self.0.send(Ok(chunk.into()));
    }

    /// Breaks the body off with an error.
    pub fn abort(self) {
        let _ = self.0.send(Err(Aborted));
    }
}

impl TestBody {
    /// No body.
    pub fn empty() -> TestBody {
        TestBody::default()
    }

    /// One chunk.
    pub fn full(bytes: impl Into<Bytes>) -> TestBody {
        TestBody { chunks: VecDeque::from([bytes.into()]), rx: None }
    }

    /// Several chunks.
    pub fn chunks(parts: &[&str]) -> TestBody {
        TestBody { chunks: parts.iter().map(|p| Bytes::copy_from_slice(p.as_bytes())).collect(), rx: None }
    }

    /// A body fed by the returned sender.
    pub fn channel() -> (BodySender, TestBody) {
        let (tx, rx) = mpsc::unbounded_channel();
        (BodySender(tx), TestBody { chunks: VecDeque::new(), rx: Some(rx) })
    }
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = Aborted;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Aborted>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        match self.rx.as_mut() {
            None => Poll::Ready(None),
            Some(rx) => rx.poll_recv(cx).map(|item| item.map(|r| r.map(Frame::data))),
        }
    }
}

/// The API under test and the address its requests come from.
#[derive(Debug, Clone)]
pub struct TestApi {
    /// The API.
    pub api: Arc<Api>,
    client: IpAddr,
}

impl TestApi {
    /// Tests `api`; requests come from 203.0.113.10.
    pub fn new(api: Arc<Api>) -> TestApi {
        TestApi { api, client: IpAddr::from([203, 0, 113, 10]) }
    }

    /// Tests `router` with [`Config::for_tests`] and the defaults of [`Api::builder`].
    pub fn from_router(router: Router) -> TestApi {
        TestApi::new(Api::builder(Arc::new(Config::for_tests()), router).build())
    }

    /// Requests come from `ip`.
    pub fn at(mut self, ip: &str) -> TestApi {
        self.client = ip.parse().expect("a client address");
        self
    }

    /// A request.
    pub fn request(&self, method: Method, target: &str) -> TestRequest {
        TestRequest {
            api: self.api.clone(),
            client: self.client,
            method,
            target: target.to_string(),
            headers: Vec::new(),
            body: TestBody::empty(),
        }
    }

    /// A GET request.
    pub fn get(&self, target: &str) -> TestRequest {
        self.request(Method::GET, target)
    }

    /// A POST request.
    pub fn post(&self, target: &str) -> TestRequest {
        self.request(Method::POST, target)
    }
}

/// A request being built.
#[derive(Debug)]
pub struct TestRequest {
    api: Arc<Api>,
    client: IpAddr,
    method: Method,
    target: String,
    headers: Vec<(String, String)>,
    body: TestBody,
}

impl TestRequest {
    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> TestRequest {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Adds `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> TestRequest {
        self.header("authorization", &format!("Bearer {token}"))
    }

    /// Sends `value` as `application/json` with its `Content-Length`.
    pub fn json(self, value: &Value) -> TestRequest {
        self.body("application/json", json::stringify(value))
    }

    /// Sends `bytes` with a content type and its `Content-Length`.
    pub fn body(self, content_type: &str, bytes: impl Into<Bytes>) -> TestRequest {
        let bytes = bytes.into();
        let len = bytes.len().to_string();
        let mut r = self.header("content-type", content_type).header("content-length", &len);
        r.body = TestBody::full(bytes);
        r
    }

    /// Uses `body` as it is (no header added).
    pub fn raw_body(mut self, body: TestBody) -> TestRequest {
        self.body = body;
        self
    }

    /// Runs the request through the pipeline.
    pub async fn send(self) -> TestResponse {
        let mut req = Request::builder().method(self.method).uri(self.target.as_str());
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let req = req.body(self.body).expect("a valid test request");
        let res = self.api.handle(req, AddressKeys::of(self.client)).await;
        let (parts, body) = res.into_parts();
        TestResponse { status: parts.status.as_u16(), headers: parts.headers, body }
    }
}

/// An answer.
#[derive(Debug, Clone)]
pub struct TestResponse {
    /// The status.
    pub status: u16,
    /// The headers, in order.
    pub headers: HeaderMap,
    /// The body (empty for HEAD and 204).
    pub body: Bytes,
}

impl TestResponse {
    /// The body as JSON (panics when it is not).
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }

    /// The body as text.
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("a UTF-8 body")
    }

    /// A header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The header names, in order.
    pub fn header_names(&self) -> Vec<&str> {
        self.headers.keys().map(|k| k.as_str()).collect()
    }
}
