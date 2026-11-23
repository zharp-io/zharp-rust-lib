//! Axum: the [Tower layer](crate::tower), with each route named by its `MatchedPath`, and
//! [`Reported`] for handlers' errors.
//!
//! ```no_run
//! use axum::{Router, routing::get};
//!
//! # async fn run() {
//! let _zharp = zharp::init!();
//! let app: Router = Router::new()
//!     .route("/orders/{id}", get(|| async { "ok" }))
//!     .layer(zharp::axum::layer()); // after the routes, so the route names are known
//! # }
//! ```

use std::backtrace::Backtrace;
use std::fmt::Display;

use axum::response::{IntoResponse, Response};
use opentelemetry::Context;

use crate::errors;
use crate::stack;

pub use crate::tower::{ZharpLayer, ZharpService};

/// The layer: `.layer(zharp::axum::layer())` on the router, after its routes.
pub fn layer() -> ZharpLayer {
    crate::tower::layer()
}

/// Wraps a handler's error type so it is reported when it turns into a 5xx response: once, with the
/// stack of where it was created (the `?` that converted it), as the request's error.
///
/// ```no_run
/// use zharp::axum::Reported;
///
/// # #[derive(Debug, thiserror::Error)] #[error("x")] struct AppError;
/// # impl axum::response::IntoResponse for AppError { fn into_response(self) -> axum::response::Response { axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response() } }
/// # fn load() -> Result<String, AppError> { Ok(String::new()) }
/// async fn show() -> Result<String, Reported<AppError>> {
///     let cart = load()?; // an AppError becomes Reported<AppError> here, and its stack is taken
///     Ok(cart)
/// }
/// ```
///
/// `?` converts any error that converts into `E` (`sqlx::Error` into your `AppError`), keeping the
/// original error's type, message, causes and failed query for the report. A 4xx isn't reported.
pub struct Reported<E> {
    error: E,
    // Boxed: a handler's `Err` stays small.
    details: Box<Details>,
}

struct Details {
    origin: Option<Origin>,
    backtrace: Backtrace,
}

struct Origin {
    type_name: String,
    message: String,
    causes: Vec<String>,
    query: Option<crate::db::Failure>,
}

impl<E> Reported<E> {
    /// Wraps an error (for `.map_err(Reported::new)` where `?` can't convert).
    pub fn new(error: E) -> Self {
        Reported {
            error,
            details: Box::new(Details {
                origin: None,
                backtrace: Backtrace::force_capture(),
            }),
        }
    }

    /// The error inside.
    pub fn into_inner(self) -> E {
        self.error
    }

    /// The error inside.
    pub fn get_ref(&self) -> &E {
        &self.error
    }
}

impl<E, F> From<F> for Reported<E>
where
    F: std::error::Error + Into<E> + 'static,
{
    fn from(error: F) -> Self {
        let (message, causes) = errors::chain(&error);
        let origin = Origin {
            type_name: errors::clean_type_name(std::any::type_name::<F>())
                .unwrap_or_else(|| errors::dyn_type_name(&error)),
            message,
            causes,
            query: crate::db::failure_of(&error),
        };
        Reported {
            details: Box::new(Details {
                origin: Some(origin),
                backtrace: Backtrace::force_capture(),
            }),
            error: error.into(),
        }
    }
}

impl<E: std::fmt::Debug> std::fmt::Debug for Reported<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl<E: Display> Display for Reported<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl<E> IntoResponse for Reported<E>
where
    E: IntoResponse + Display + 'static,
{
    fn into_response(self) -> Response {
        let message = self.error.to_string();
        let response = self.error.into_response();
        if response.status().is_server_error() {
            let frames = stack::for_report(stack::parse(&self.details.backtrace.to_string()));
            let cx = Context::current();
            let report = match self.details.origin {
                Some(origin) => errors::Report {
                    type_name: origin.type_name,
                    message: origin.message,
                    causes: origin.causes,
                    frames,
                    query: origin.query,
                    handled: false,
                    ..Default::default()
                },
                None => errors::Report {
                    type_name: errors::clean_type_name(std::any::type_name::<E>())
                        .unwrap_or_else(|| "Error".into()),
                    message,
                    frames,
                    handled: false,
                    ..Default::default()
                },
            };
            errors::send(&cx, report);
        }
        response
    }
}
