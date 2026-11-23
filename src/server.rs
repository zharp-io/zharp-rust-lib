//! A request's server span, shared by the Tower layer and the Actix middleware: started with the
//! incoming `traceparent` and `baggage` as parent, carrying the request's state (errors reported,
//! user, last query) in its context, ended with the status and the response's headers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use opentelemetry::propagation::Extractor;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};

use crate::context::RequestState;
use crate::headers;
use crate::setup::state;

pub(crate) struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub scheme: Option<&'a str>,
    pub host: Option<&'a str>,
    pub route: Option<&'a str>,
    pub peer: Option<String>,
    pub protocol: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub grpc: bool,
}

pub(crate) struct ServerSpan {
    /// The request's context: its span and its [`RequestState`]. Attached while the handler runs.
    pub cx: Context,
    pub request: Arc<RequestState>,
    method: String,
    path: String,
    grpc: bool,
}

/// Whether request and response headers go on the span.
pub(crate) fn headers_on() -> bool {
    state().is_some_and(|s| s.config.headers && s.config.tracing)
}

pub(crate) fn start(
    incoming: Incoming<'_>,
    carrier: &dyn Extractor,
    request_headers: Vec<KeyValue>,
) -> Option<ServerSpan> {
    let state = state()?;
    let parent = opentelemetry::global::get_text_map_propagator(|p| {
        p.extract_with_context(&Context::new(), carrier)
    });
    let method = incoming.method.to_ascii_uppercase();
    let name = span_name(&method, incoming.route, incoming.path, incoming.grpc);
    let mut attributes = vec![
        KeyValue::new("http.request.method", method.clone()),
        KeyValue::new("url.path", incoming.path.to_string()),
    ];
    if let Some(query) = incoming.query.filter(|q| !q.is_empty()) {
        attributes.push(KeyValue::new("url.query", headers::mask_query(query)));
    }
    if let Some(route) = incoming.route {
        attributes.push(KeyValue::new("http.route", route.to_string()));
    }
    for (key, value) in [
        ("url.scheme", incoming.scheme),
        ("server.address", incoming.host),
        ("network.protocol.version", incoming.protocol),
        ("user_agent.original", incoming.user_agent),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            attributes.push(KeyValue::new(key, value.to_string()));
        }
    }
    if let Some(peer) = incoming.peer {
        attributes.push(KeyValue::new("client.address", peer));
    }
    if incoming.grpc {
        attributes.push(KeyValue::new("rpc.system", "grpc"));
        let mut parts = incoming.path.trim_start_matches('/').splitn(2, '/');
        if let (Some(service), Some(method)) = (parts.next(), parts.next()) {
            attributes.push(KeyValue::new("rpc.service", service.to_string()));
            attributes.push(KeyValue::new("rpc.method", method.to_string()));
        }
    }
    attributes.extend(request_headers);
    let span = state
        .tracer
        .span_builder(name)
        .with_kind(SpanKind::Server)
        .with_attributes(attributes)
        .start_with_context(&state.tracer, &parent);
    let request = Arc::new(RequestState::default());
    let cx = parent.with_span(span).with_value(request.clone());
    Some(ServerSpan {
        cx,
        request,
        method,
        path: incoming.path.to_string(),
        grpc: incoming.grpc,
    })
}

fn span_name(method: &str, route: Option<&str>, path: &str, grpc: bool) -> String {
    if grpc {
        return path.trim_start_matches('/').to_string();
    }
    match route {
        Some(route) => format!("{method} {route}"),
        None => method.to_string(),
    }
}

impl ServerSpan {
    /// Ends the span. `route` when it is only known after routing (Actix); `grpc_status` from a
    /// gRPC response's headers.
    pub fn finish(
        self,
        status: u16,
        route: Option<&str>,
        grpc_status: Option<i64>,
        response_headers: Vec<KeyValue>,
    ) {
        let span = self.cx.span();
        if let Some(route) = route {
            span.update_name(span_name(&self.method, Some(route), &self.path, self.grpc));
            span.set_attribute(KeyValue::new("http.route", route.to_string()));
        }
        span.set_attribute(KeyValue::new("http.response.status_code", status as i64));
        if let Some(code) = grpc_status {
            span.set_attribute(KeyValue::new("rpc.grpc.status_code", code));
        }
        span.set_attributes(response_headers);
        let panicked = self.request.panicked.load(Ordering::Relaxed);
        // gRPC codes that mean the server failed: UNKNOWN, DEADLINE_EXCEEDED, UNIMPLEMENTED,
        // INTERNAL, UNAVAILABLE, DATA_LOSS.
        let grpc_failed = grpc_status.is_some_and(|c| matches!(c, 2 | 4 | 12 | 13 | 14 | 15));
        if panicked {
            span.set_status(Status::error("panicked"));
        } else if status >= 500 || grpc_failed || self.request.failed.load(Ordering::Relaxed) {
            span.set_status(Status::error(format!("HTTP {status}")));
        }
        span.end();
    }
}

/// The response a panicking handler gets: 500, or for gRPC a trailers-only INTERNAL.
#[cfg(feature = "tower")]
pub(crate) fn panic_status(grpc: bool) -> (u16, Option<(&'static str, &'static str)>) {
    if grpc {
        (200, Some(("grpc-status", "13")))
    } else {
        (500, None)
    }
}
