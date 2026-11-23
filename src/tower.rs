//! A Tower layer for any HTTP service: Axum, tonic (gRPC), Poem, Salvo, plain hyper.
//!
//! ```no_run
//! # fn wrap<S>(service: S) -> zharp::tower::ZharpService<S> {
//! use tower_layer::Layer as _;
//! zharp::tower::layer().layer(service)
//! # }
//! ```
//!
//! Each request gets a server span (the incoming `traceparent` its parent) named by its route,
//! with its status and its request and response headers (secrets as `[REDACTED]`). The handler
//! runs inside the span's context, so its `tracing` spans, logs, errors and queries belong to the
//! request. A panicking handler is answered with a 500 (gRPC: status INTERNAL) and the panic is
//! reported once, by the panic hook. For Axum use [`crate::axum::layer`], which is this layer.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};

use http::{HeaderValue, Request, Response, StatusCode};
use opentelemetry_http::HeaderExtractor;
use pin_project_lite::pin_project;

use crate::headers;
use crate::server::{self, Incoming, ServerSpan};

/// The layer: `.layer(zharp::tower::layer())`.
pub fn layer() -> ZharpLayer {
    ZharpLayer { _private: () }
}

/// See [`layer`].
#[derive(Clone, Debug)]
pub struct ZharpLayer {
    _private: (),
}

impl<S> tower_layer::Layer<S> for ZharpLayer {
    type Service = ZharpService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ZharpService { inner }
    }
}

/// The service [`ZharpLayer`] wraps around yours.
#[derive(Clone, Debug)]
pub struct ZharpService<S> {
    inner: S,
}

fn route_of<B>(_request: &Request<B>) -> Option<String> {
    #[cfg(feature = "axum")]
    if let Some(path) = _request.extensions().get::<axum::extract::MatchedPath>() {
        return Some(path.as_str().to_string());
    }
    None
}

fn peer_of<B>(_request: &Request<B>) -> Option<String> {
    #[cfg(feature = "axum")]
    if let Some(info) = _request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return Some(info.0.ip().to_string());
    }
    None
}

fn protocol(version: http::Version) -> Option<&'static str> {
    match version {
        http::Version::HTTP_10 => Some("1.0"),
        http::Version::HTTP_11 => Some("1.1"),
        http::Version::HTTP_2 => Some("2"),
        http::Version::HTTP_3 => Some("3"),
        _ => None,
    }
}

pub(crate) fn header_pairs(map: &http::HeaderMap) -> impl Iterator<Item = (&str, &[u8])> {
    map.iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
}

fn start<B>(request: &Request<B>) -> Option<ServerSpan> {
    let headers_map = request.headers();
    let grpc = headers_map
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/grpc"));
    let host = request.uri().host().map(String::from).or_else(|| {
        headers_map
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
    });
    let route = route_of(request);
    let request_headers = if server::headers_on() {
        headers::attributes("request", header_pairs(headers_map))
    } else {
        Vec::new()
    };
    let incoming = Incoming {
        method: request.method().as_str(),
        path: request.uri().path(),
        query: request.uri().query(),
        scheme: Some(request.uri().scheme_str().unwrap_or("http")),
        host: host.as_deref(),
        route: route.as_deref(),
        peer: peer_of(request),
        protocol: protocol(request.version()),
        user_agent: headers_map
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        grpc,
    };
    server::start(incoming, &HeaderExtractor(headers_map), request_headers)
}

impl<S, ReqBody, ResBody> tower_service::Service<Request<ReqBody>> for ZharpService<S>
where
    S: tower_service::Service<Request<ReqBody>, Response = Response<ResBody>>,
    ResBody: Default,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let span = start(&request);
        let grpc = request
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/grpc"));
        let inner = {
            let _attached = span.as_ref().map(|s| s.cx.clone().attach());
            self.inner.call(request)
        };
        ResponseFuture { inner, span, grpc }
    }
}

pin_project! {
    /// The response future of [`ZharpService`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        span: Option<ServerSpan>,
        grpc: bool,
    }
}

impl<F, ResBody, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
    ResBody: Default,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let polled = {
            let _attached = this.span.as_ref().map(|s| s.cx.clone().attach());
            let inner = this.inner;
            catch_unwind(AssertUnwindSafe(|| inner.poll(cx)))
        };
        match polled {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(response))) => {
                if let Some(span) = this.span.take() {
                    let grpc_status = response
                        .headers()
                        .get("grpc-status")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok());
                    let response_headers = if server::headers_on() {
                        headers::attributes("response", header_pairs(response.headers()))
                    } else {
                        Vec::new()
                    };
                    span.finish(
                        response.status().as_u16(),
                        None,
                        grpc_status,
                        response_headers,
                    );
                }
                Poll::Ready(Ok(response))
            }
            Ok(Poll::Ready(Err(error))) => {
                if let Some(span) = this.span.take() {
                    span.finish(500, None, None, Vec::new());
                }
                Poll::Ready(Err(error))
            }
            Err(_panic) => {
                let (status, header) = server::panic_status(*this.grpc);
                let mut response = Response::new(ResBody::default());
                *response.status_mut() =
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if let Some((name, value)) = header {
                    response
                        .headers_mut()
                        .insert(name, HeaderValue::from_static(value));
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/grpc"),
                    );
                }
                if let Some(span) = this.span.take() {
                    span.request.panicked.store(true, Ordering::Relaxed);
                    span.finish(status, None, header.map(|_| 13), Vec::new());
                }
                Poll::Ready(Ok(response))
            }
        }
    }
}
