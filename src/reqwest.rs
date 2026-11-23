//! Outgoing calls with reqwest: a client span per call, and `traceparent` (and the user's
//! `baggage`) sent along, so the service called joins the same trace.
//!
//! ```no_run
//! # async fn run() -> Result<(), reqwest_middleware::Error> {
//! let client = zharp::reqwest::wrap(reqwest::Client::new());
//! let response = client.get("https://payments.internal/charges").send().await?;
//! # Ok(()) }
//! ```
//!
//! Or add [`Middleware`] to a `reqwest_middleware::ClientBuilder` of your own.

use http::Extensions;
use opentelemetry::trace::{FutureExt as _, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_http::HeaderInjector;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Next};

use crate::headers;
use crate::setup::state;

/// A client that traces its calls: `zharp::reqwest::wrap(reqwest::Client::new())`.
pub fn wrap(client: reqwest::Client) -> ClientWithMiddleware {
    ClientBuilder::new(client).with(Middleware).build()
}

/// The reqwest-middleware middleware. See the [module](self).
#[derive(Clone, Copy, Debug, Default)]
pub struct Middleware;

fn url_without_secrets(url: &reqwest::Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_password(None);
    let _ = shown.set_username("");
    if let Some(query) = url.query() {
        shown.set_query(Some(&headers::mask_query(query)));
    }
    shown.to_string()
}

#[async_trait::async_trait]
impl reqwest_middleware::Middleware for Middleware {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        let Some(state) = state() else {
            return next.run(request, extensions).await;
        };
        if !state.config.tracing {
            return next.run(request, extensions).await;
        }
        let parent = Context::current();
        let method = request.method().as_str().to_string();
        let url = request.url();
        let mut attributes = vec![
            KeyValue::new("http.request.method", method.clone()),
            KeyValue::new("url.full", url_without_secrets(url)),
        ];
        if let Some(host) = url.host_str() {
            attributes.push(KeyValue::new("server.address", host.to_string()));
        }
        if let Some(port) = url.port_or_known_default() {
            attributes.push(KeyValue::new("server.port", port as i64));
        }
        if state.config.headers {
            attributes.extend(headers::attributes(
                "request",
                request
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_bytes())),
            ));
        }
        let span = state
            .tracer
            .span_builder(method)
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start_with_context(&state.tracer, &parent);
        let cx = parent.with_span(span);
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut HeaderInjector(request.headers_mut()))
        });

        let result = next.run(request, extensions).with_context(cx.clone()).await;
        let span = cx.span();
        match &result {
            Ok(response) => {
                let status = response.status().as_u16();
                span.set_attribute(KeyValue::new("http.response.status_code", status as i64));
                if state.config.headers {
                    span.set_attributes(headers::attributes(
                        "response",
                        response
                            .headers()
                            .iter()
                            .map(|(n, v)| (n.as_str(), v.as_bytes())),
                    ));
                }
                if status >= 400 {
                    span.set_status(Status::error(format!("HTTP {status}")));
                }
            }
            Err(error) => {
                span.set_attribute(KeyValue::new(
                    "error.type",
                    if error.is_timeout() {
                        "timeout"
                    } else {
                        "request_failed"
                    },
                ));
                span.set_status(Status::error(error.to_string()));
            }
        }
        span.end();
        result
    }
}
