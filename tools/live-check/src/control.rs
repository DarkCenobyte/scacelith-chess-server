//! The control server of a C++ live test: plain HTTP/1.1 on 127.0.0.1, one request per
//! connection, JSON answers. The C++ test drives the harness through it (the games played, the
//! mails, a session expired in the database...), each part with its own [`Routes`].

use std::collections::HashMap;
use std::convert::Infallible;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use scacelith_server::http::url::parse_urlencoded;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// A request of the C++ test.
#[derive(Debug)]
pub struct Call {
    /// `GET`, `POST`...
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query parameters, decoded.
    pub query: HashMap<String, String>,
    /// The JSON body (`{}` when there is none or it is not JSON).
    pub body: Value,
}

impl Call {
    /// The query parameter `name`, `""` when absent.
    pub fn q(&self, name: &str) -> &str {
        self.query.get(name).map_or("", String::as_str)
    }
}

/// An answer: its status, its JSON body and its extra headers.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub headers: Vec<(String, String)>,
}

impl Reply {
    /// 200 with `body`.
    pub fn ok(body: Value) -> Reply {
        Reply { status: 200, body, headers: Vec::new() }
    }

    /// `status` with `body`.
    pub fn with_status(status: u16, body: Value) -> Reply {
        Reply { status, body, headers: Vec::new() }
    }

    /// An error answer: `status` with `{"error": code}`.
    pub fn error(status: u16, code: &str) -> Reply {
        Reply::with_status(status, json!({ "error": code }))
    }
}

/// The routes of a control server.
pub(crate) trait Routes {
    /// The answer to `call` (404 `no_route` for a route it does not have).
    async fn answer(&self, call: &Call) -> Reply;
}

/// A control server bound to a port of 127.0.0.1.
#[derive(Debug)]
pub struct Control {
    listener: TcpListener,
    /// The port the C++ test is given.
    pub port: u16,
}

impl Control {
    /// Binds a port of the system's choice.
    pub async fn bind() -> std::io::Result<Control> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok(Control { listener, port })
    }

    /// Serves `routes`, one connection after the other (the C++ test calls one route at a time),
    /// until the future is dropped.
    pub(crate) async fn serve(&self, routes: &impl Routes) {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else { continue };
            let service =
                service_fn(move |req| async move { Ok::<_, Infallible>(handle(routes, req).await) });
            let _ =
                http1::Builder::new().keep_alive(false).serve_connection(TokioIo::new(stream), service).await;
        }
    }
}

async fn handle(routes: &impl Routes, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let raw_query = req.uri().query().unwrap_or("").to_owned();
    let bytes = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    let call = Call { method, path, query: parse_urlencoded(&raw_query).into_iter().collect(), body };
    let reply = routes.answer(&call).await;
    let text = reply.body.to_string();
    let shown: String = text.chars().take(160).collect();
    let query = if raw_query.is_empty() { String::new() } else { format!("?{raw_query}") };
    println!("[control] {} {}{query} -> {} {shown}", call.method, call.path, reply.status);
    let mut res = Response::builder()
        .status(StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json");
    for (k, v) in &reply.headers {
        res = res.header(k.as_str(), v.as_str());
    }
    res.body(Full::new(Bytes::from(text))).unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
