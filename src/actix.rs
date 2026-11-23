//! Actix Web: `.wrap(zharp::actix::Middleware)` on the `App`.
//!
//! ```no_run
//! use actix_web::{App, HttpServer, web};
//!
//! # async fn run() -> std::io::Result<()> {
//! let _zharp = zharp::init!();
//! HttpServer::new(|| App::new().wrap(zharp::actix::Middleware).route("/orders/{id}", web::get().to(|| async { "ok" })))
//!     .bind(("0.0.0.0", 8080))?
//!     .run()
//!     .await
//! # }
//! ```
//!
//! Each request gets a server span named by its route pattern (`GET /orders/{id}`), with its status
//! and headers (secrets as `[REDACTED]`); the handler runs in the span's context. An error a
//! handler returns that becomes a 5xx is reported once (unless the request reported it already,
//! say with `tracing::error!`). A panicking handler is answered with a 500 and the panic reported
//! once, by the panic hook.

use std::future::{Future, Ready, ready};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform, forward_ready};
use actix_web::http::header::HeaderMap;
use opentelemetry::propagation::Extractor;
use pin_project_lite::pin_project;

use crate::errors;
use crate::headers;
use crate::server::{self, Incoming, ServerSpan};

/// The middleware: `App::new().wrap(zharp::actix::Middleware)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Middleware;

impl<S, B> Transform<S, ServiceRequest> for Middleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = ZharpMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(ZharpMiddleware { service }))
    }
}

/// The service [`Middleware`] wraps around the app.
pub struct ZharpMiddleware<S> {
    service: S,
}

struct Headers<'a>(&'a HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

fn pairs(map: &HeaderMap) -> impl Iterator<Item = (&str, &[u8])> {
    map.iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
}

fn start(request: &ServiceRequest) -> Option<ServerSpan> {
    let info = request.connection_info().clone();
    let version = format!("{:?}", request.version());
    let protocol = version.strip_prefix("HTTP/");
    let request_headers = if server::headers_on() {
        headers::attributes("request", pairs(request.headers()))
    } else {
        Vec::new()
    };
    let host = info
        .host()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    let incoming = Incoming {
        method: request.method().as_str(),
        path: request.path(),
        query: Some(request.query_string()),
        scheme: Some(info.scheme()),
        host: Some(host.as_str()),
        route: None,
        peer: info
            .realip_remote_addr()
            .map(|a| a.split(':').next().unwrap_or(a).to_string()),
        protocol,
        user_agent: request
            .headers()
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        grpc: false,
    };
    server::start(incoming, &Headers(request.headers()), request_headers)
}

impl<S, B> Service<ServiceRequest> for ZharpMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error> + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = ZharpFuture<S::Future>;

    forward_ready!(service);

    fn call(&self, request: ServiceRequest) -> Self::Future {
        let span = start(&request);
        let inner = {
            let _attached = span.as_ref().map(|s| s.cx.clone().attach());
            self.service.call(request)
        };
        ZharpFuture { inner, span }
    }
}

pin_project! {
    /// The response future of [`ZharpMiddleware`].
    pub struct ZharpFuture<F> {
        #[pin]
        inner: F,
        span: Option<ServerSpan>,
    }
}

impl<F, B> Future for ZharpFuture<F>
where
    F: Future<Output = Result<ServiceResponse<B>, actix_web::Error>>,
    B: MessageBody,
{
    type Output = Result<ServiceResponse<B>, actix_web::Error>;

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
                    let status = response.status().as_u16();
                    if status >= 500 {
                        if let Some(error) = response.response().error() {
                            let type_name = errors::type_from_debug(&format!(
                                "{:?}",
                                error.as_response_error()
                            ))
                            .unwrap_or_else(|| "actix_web::Error".into());
                            errors::report_text(
                                &span.cx,
                                type_name,
                                error.to_string(),
                                Vec::new(),
                                false,
                            );
                        }
                    }
                    let route = response.request().match_pattern();
                    let response_headers = if server::headers_on() {
                        headers::attributes("response", pairs(response.headers()))
                    } else {
                        Vec::new()
                    };
                    span.finish(status, route.as_deref(), None, response_headers);
                }
                Poll::Ready(Ok(response))
            }
            Ok(Poll::Ready(Err(error))) => {
                if let Some(span) = this.span.take() {
                    let status = error.as_response_error().status_code().as_u16();
                    if status >= 500 {
                        let type_name =
                            errors::type_from_debug(&format!("{:?}", error.as_response_error()))
                                .unwrap_or_else(|| "actix_web::Error".into());
                        errors::report_text(
                            &span.cx,
                            type_name,
                            error.to_string(),
                            Vec::new(),
                            false,
                        );
                    }
                    span.finish(status, None, None, Vec::new());
                }
                Poll::Ready(Err(error))
            }
            Err(_panic) => {
                if let Some(span) = this.span.take() {
                    span.request.panicked.store(true, Ordering::Relaxed);
                    span.finish(500, None, None, Vec::new());
                }
                // Actix answers the error with a 500 (the request can't be cloned to build one here).
                Poll::Ready(Err(actix_web::error::ErrorInternalServerError(
                    "Internal Server Error",
                )))
            }
        }
    }
}
