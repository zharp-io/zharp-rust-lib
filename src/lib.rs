//! Zharp Application Monitoring for Rust: errors and panics with their stacks, request traces,
//! failed queries and logs, sent to [Zharp](https://zharp.io) over OpenTelemetry.
//!
//! ```no_run
//! # #[cfg(feature = "axum")]
//! # async fn run() {
//! #[tokio::main]
//! async fn main() {
//!     let _zharp = zharp::init!(); // reads ZHARP_KEY; sends what's left when it drops
//!
//!     let app = axum::Router::new()
//!         .route("/orders/{id}", axum::routing::get(|| async { "ok" }))
//!         .layer(zharp::axum::layer());
//!     let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
//!     axum::serve(listener, app).await.unwrap();
//! }
//! # }
//! ```
//!
//! - **Panics** are reported with a backtrace taken where they happened. The Axum layer and the
//!   Actix middleware answer a panicking handler with a 500.
//! - **Errors:** a `tracing::error!` with an `error` field becomes the request's error, with the
//!   stack of where it was logged ([`anyhow`] errors keep the stack of where they were created).
//!   [`capture_error`] reports anything else. Each error is reported once per request.
//! - **Traces:** a server span per request (route, status, headers with secrets filtered), the
//!   app's `tracing` spans under it, SQLx queries (`sqlx` feature) and outgoing reqwest calls
//!   (`reqwest` feature) as client spans.
//! - **Logs** carry ` trace_id=… span_id=…` ([`fmt_layer`], [`stamp`]); `ZHARP_LOGS=true` also
//!   sends them to Zharp.
//!
//! Features: `axum`, `tower` (any Tower service: tonic, Poem, Salvo), `actix`, `sqlx`, `reqwest`,
//! `anyhow`. Nothing needs Tokio: telemetry is sent from the SDK's own threads, so plain binaries
//! work too.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![allow(clippy::needless_doctest_main)]

pub mod build;

#[cfg(feature = "sdk")]
mod config;
#[cfg(feature = "sdk")]
mod context;
#[cfg(feature = "sdk")]
mod db;
#[cfg(feature = "sdk")]
mod errors;
#[cfg(any(feature = "tower", feature = "actix", feature = "reqwest"))]
mod headers;
#[cfg(feature = "sdk")]
mod layer;
#[cfg(feature = "sdk")]
mod panic;
#[cfg(feature = "sdk")]
mod setup;
#[cfg(feature = "sdk")]
mod stack;
#[cfg(feature = "sdk")]
mod stamp;

#[cfg(any(feature = "tower", feature = "actix"))]
mod server;

#[cfg(feature = "actix")]
#[cfg_attr(docsrs, doc(cfg(feature = "actix")))]
pub mod actix;
#[cfg(feature = "anyhow")]
#[cfg_attr(docsrs, doc(cfg(feature = "anyhow")))]
pub mod anyhow;
#[cfg(feature = "axum")]
#[cfg_attr(docsrs, doc(cfg(feature = "axum")))]
pub mod axum;
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub mod reqwest;
#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub mod tower;

#[cfg(feature = "sdk")]
pub use context::{User, clear_user, set_tag, set_user};
#[cfg(feature = "sdk")]
pub use errors::{capture_error, capture_message};
#[cfg(feature = "sdk")]
pub use layer::layer;
#[cfg(feature = "sdk")]
pub use setup::{Guard, Level, Options, flush, log};
#[cfg(feature = "sdk")]
pub use stamp::{Stamped, StampedFmtLayer, fmt_layer, stamp};

#[cfg(feature = "anyhow")]
pub use crate::anyhow::capture_anyhow;

/// This crate's version, sent as `telemetry.sdk.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Starts sending to Zharp and returns the [`Guard`] that sends what's left when it drops. Put it
/// first in `main` and keep the guard:
///
/// ```no_run
/// let _zharp = zharp::init!();
/// ```
///
/// With settings: `zharp::init!(zharp::Options::new().environment("production"))`. Anything not
/// set is read from the environment (`ZHARP_KEY`, `ZHARP_SERVICE`, `ZHARP_ENVIRONMENT`,
/// `ZHARP_RELEASE`, `ZHARP_ENDPOINT`, `ZHARP_LOGS`…).
///
/// It is a macro so that it knows your crate: the service defaults to your package's name, the
/// release to its version (plus the git commit when [`build::id`] ran in `build.rs`), and the
/// build ID that matches a source bundle comes from `build.rs` too. It never panics and never
/// fails the app: without a key it prints one line and sends nothing.
#[cfg(feature = "sdk")]
#[macro_export]
macro_rules! init {
    () => {
        $crate::init!($crate::Options::new())
    };
    ($options:expr) => {
        $crate::Options::init($crate::Options::__app(
            $options,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            option_env!("ZHARP_BUILD_ID"),
            option_env!("ZHARP_GIT_SHA"),
        ))
    };
}

#[cfg(all(test, feature = "axum", feature = "actix", feature = "anyhow"))]
mod tests;
