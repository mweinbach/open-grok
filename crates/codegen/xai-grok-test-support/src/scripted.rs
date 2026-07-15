//! Data-driven scripted responses for the mock inference server: plain
//! status/header/body triples queued per path and rendered to HTTP at serve
//! time. Pure data — no router or handler types in the public surface.

use std::convert::Infallible;

use axum::Json;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde_json::Value;

/// One SSE event as data: optional `event:` name plus the `data:` payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    /// Event with a `data:` payload only.
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// Event with an `event:` name and a `data:` payload.
    pub fn with_event(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }
}

/// Body of a [`ScriptedResponse`].
#[derive(Debug, Clone)]
pub enum ScriptedBody {
    Json(Value),
    Sse(Vec<SseEvent>),
    /// Raw body bytes, served verbatim (byte-controllable malformed SSE etc.).
    Raw(String),
}

/// A scripted reply for a single request on one path, consumed FIFO.
/// Takes precedence over the response mode AND the required-auth check —
/// a script is full control over the next reply.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ScriptedBody,
}

impl ScriptedResponse {
    /// 200 SSE response built from an event list.
    pub fn sse(events: Vec<SseEvent>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ScriptedBody::Sse(events),
        }
    }

    /// JSON body with the given status.
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedBody::Json(body),
        }
    }

    /// Raw text body with the given status.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedBody::Raw(body.into()),
        }
    }

    /// Validate status and headers eagerly so a bad script panics at the
    /// enqueue call site rather than far away at serve time.
    pub(crate) fn validate(&self) {
        StatusCode::from_u16(self.status).expect("invalid scripted status code");
        for (name, value) in &self.headers {
            HeaderName::from_bytes(name.as_bytes()).expect("invalid scripted header name");
            HeaderValue::from_str(value).expect("invalid scripted header value");
        }
    }

    /// Render to HTTP with SSE events paced by `delay` (sleep before each
    /// event, mirroring the fixed/echo `paced_events` pacing) so
    /// `set_chunk_delay` also holds scripted turns open. `None` streams
    /// instantly. Non-SSE bodies ignore the delay.
    pub(crate) fn into_response_paced(self, delay: Option<std::time::Duration>) -> Response {
        use futures_util::StreamExt as _;
        let mut resp = match self.body {
            ScriptedBody::Json(v) => Json(v).into_response(),
            ScriptedBody::Raw(s) => s.into_response(),
            ScriptedBody::Sse(events) => {
                let events: Vec<axum::response::sse::Event> = events
                    .into_iter()
                    .map(|e| {
                        let ev = axum::response::sse::Event::default().data(e.data);
                        match e.event {
                            Some(name) => ev.event(name),
                            None => ev,
                        }
                    })
                    .collect();
                let stream = stream::iter(events.into_iter().map(Ok::<_, Infallible>)).then(
                    move |event| async move {
                        if let Some(d) = delay {
                            tokio::time::sleep(d).await;
                        }
                        event
                    },
                );
                Sse::new(stream)
                    .keep_alive(KeepAlive::default())
                    .into_response()
            }
        };
        *resp.status_mut() = StatusCode::from_u16(self.status).expect("valid scripted status code");
        for (k, v) in self.headers {
            resp.headers_mut().insert(
                HeaderName::from_bytes(k.as_bytes()).expect("valid scripted header name"),
                HeaderValue::from_str(&v).expect("valid scripted header value"),
            );
        }
        resp
    }
}
