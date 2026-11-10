//! `wasi:http` requests that are polled once per tick and never block on the network.

use std::time::{Duration, Instant};

use wasi::http::outgoing_handler;
use wasi::http::types::{
    FutureIncomingResponse, IncomingBody, IncomingResponse, Method as WasiMethod, OutgoingBody,
    OutgoingRequest, RequestOptions, Scheme,
};
use wasi::io::streams::{InputStream, StreamError};

use crate::source::{HttpRequest, HttpResponse, Method};

/// Whole-request deadline, and the per-phase timeouts handed to the host (nanoseconds).
const DEADLINE: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT_NS: u64 = 3_000_000_000;
const PHASE_TIMEOUT_NS: u64 = 5_000_000_000;
/// Profiles and bulk answers are a few KiB; anything bigger is not what we asked for.
const MAX_BODY: usize = 64 * 1024;
const USER_AGENT: &[u8] = concat!("pumpkin-skinrestorer/", env!("CARGO_PKG_VERSION")).as_bytes();

pub struct Request {
    future: Option<FutureIncomingResponse>,
    status: u16,
    retry_after: Option<u64>,
    // Field order matters: the stream must drop before its body, the body before the
    // response.
    stream: Option<InputStream>,
    body: Option<IncomingBody>,
    response: Option<IncomingResponse>,
    bytes: Vec<u8>,
    started: Instant,
}

fn header(fields: &wasi::http::types::Fields, name: &str, value: &[u8]) -> Result<(), String> {
    fields
        .append(name, value)
        .map_err(|e| format!("header {name}: {e:?}"))
}

impl Request {
    pub fn start(req: &HttpRequest) -> Result<Request, String> {
        let fields = wasi::http::types::Fields::new();
        header(&fields, "user-agent", USER_AGENT)?;
        header(&fields, "accept", b"application/json")?;
        if let Some(body) = &req.body {
            header(&fields, "content-type", b"application/json")?;
            header(&fields, "content-length", body.len().to_string().as_bytes())?;
        }
        let outgoing = OutgoingRequest::new(fields);
        let method = match req.method {
            Method::Get => WasiMethod::Get,
            Method::Post => WasiMethod::Post,
        };
        outgoing.set_method(&method).map_err(|()| "bad method")?;
        outgoing
            .set_scheme(Some(&Scheme::Https))
            .map_err(|()| "bad scheme")?;
        outgoing
            .set_authority(Some(req.host))
            .map_err(|()| "bad host")?;
        outgoing
            .set_path_with_query(Some(&req.path))
            .map_err(|()| "bad path")?;

        let options = RequestOptions::new();
        options
            .set_connect_timeout(Some(CONNECT_TIMEOUT_NS))
            .map_err(|()| "bad timeout")?;
        options
            .set_first_byte_timeout(Some(PHASE_TIMEOUT_NS))
            .map_err(|()| "bad timeout")?;
        options
            .set_between_bytes_timeout(Some(PHASE_TIMEOUT_NS))
            .map_err(|()| "bad timeout")?;

        let body = outgoing.body().map_err(|()| "no request body")?;
        let future = outgoing_handler::handle(outgoing, Some(options))
            .map_err(|e| format!("request refused: {e:?}"))?;
        {
            let stream = body.write().map_err(|()| "no request body stream")?;
            if let Some(bytes) = &req.body {
                // A few hundred bytes at most: a bulk list of names.
                stream
                    .blocking_write_and_flush(bytes)
                    .map_err(|e| format!("writing request: {e:?}"))?;
            }
        }
        OutgoingBody::finish(body, None).map_err(|e| format!("finishing request: {e:?}"))?;

        Ok(Request {
            future: Some(future),
            status: 0,
            retry_after: None,
            stream: None,
            body: None,
            response: None,
            bytes: Vec::new(),
            started: Instant::now(),
        })
    }

    /// `None` while still running.
    pub fn poll(&mut self) -> Option<Result<HttpResponse, String>> {
        if self.started.elapsed() > DEADLINE {
            return Some(Err("timed out".into()));
        }
        if let Some(future) = &self.future {
            if !future.subscribe().ready() {
                return None;
            }
            let response = match future.get() {
                Some(Ok(Ok(r))) => r,
                Some(Ok(Err(e))) => return Some(Err(format!("{e:?}"))),
                Some(Err(())) | None => return Some(Err("response lost".into())),
            };
            self.future = None;
            self.status = response.status();
            self.retry_after = response
                .headers()
                .get("retry-after")
                .first()
                .and_then(|v| std::str::from_utf8(v).ok()?.trim().parse().ok());
            let body = match response.consume() {
                Ok(b) => b,
                Err(()) => return Some(Err("no response body".into())),
            };
            self.stream = match body.stream() {
                Ok(s) => Some(s),
                Err(()) => return Some(Err("unreadable response body".into())),
            };
            self.body = Some(body);
            self.response = Some(response);
        }
        let stream = self.stream.as_ref()?;
        // Read everything that is ready, so the end of the body is seen in the same tick.
        loop {
            if !stream.subscribe().ready() {
                return None;
            }
            match stream.read(8192) {
                // Ready without data: ask again next tick rather than spin.
                Ok(chunk) if chunk.is_empty() => return None,
                Ok(chunk) => {
                    self.bytes.extend_from_slice(&chunk);
                    if self.bytes.len() > MAX_BODY {
                        return Some(Err("response too large".into()));
                    }
                }
                Err(StreamError::Closed) => {
                    return Some(Ok(HttpResponse {
                        status: self.status,
                        retry_after: self.retry_after,
                        body: std::mem::take(&mut self.bytes),
                    }));
                }
                Err(e) => return Some(Err(format!("reading response: {e:?}"))),
            }
        }
    }
}
