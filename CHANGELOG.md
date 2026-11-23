# Changelog

## 0.1.0 (unreleased)

The first release: one `zharp` crate with a feature per framework, built on OpenTelemetry Rust
(`opentelemetry` 0.33, `tracing-opentelemetry` 0.34). Rust 1.85+ (with `sqlx`: 1.94+, SQLx 0.9's own minimum).

- `let _zharp = zharp::init!();` first in `main`: settings from `ZHARP_KEY`, `ZHARP_SERVICE`,
  `ZHARP_ENVIRONMENT`, `ZHARP_RELEASE`, `ZHARP_ENDPOINT`, `ZHARP_SAMPLE_RATE`, `ZHARP_TRACING`,
  `ZHARP_LOGS`, `ZHARP_LOG_CORRELATION`, `ZHARP_CAPTURE_HEADERS`, `ZHARP_CAPTURE_QUERY_ERRORS`,
  `ZHARP_DB_SYSTEM`, `ZHARP_BUILD_ID`, `ZHARP_DISABLED`, or `zharp::Options`. The service defaults
  to the package's name, the release to its version (plus the git commit with the build helper).
  OTLP over HTTP/protobuf to `https://api.zharp.io/api/v1/otlp`, sent from the SDK's own threads:
  no Tokio runtime needed.
- Axum (`features = ["axum"]`, `.layer(zharp::axum::layer())`), any Tower service such as tonic
  (`"tower"`), Actix Web (`"actix"`, `.wrap(zharp::actix::Middleware)`): a server span per request
  named by its route, incoming `traceparent`/`baggage`, all request and response headers with
  secrets as `[REDACTED]`, a panicking handler answered with a 500.
- Panics reported with a backtrace taken where they happened (no `RUST_BACKTRACE` needed); a panic
  outside a request waits up to two seconds to be sent.
- Errors counted once per request: `tracing::error!` with an `error` field, `zharp::capture_error`,
  `zharp::axum::Reported<E>` (the stack of the `?` that made it), Actix handler errors that become
  a 5xx; `anyhow` errors keep the stack of where they were created (`"anyhow"`).
- SQLx (`"sqlx"`): queries as client spans; a failed query's system, code, constraint and statement
  on its error, values scrubbed from the message.
- reqwest (`"reqwest"`): `zharp::reqwest::wrap(client)` makes a client span per call and sends
  `traceparent`.
- ` trace_id=… span_id=…` at the end of each line logged in a request (`zharp::fmt_layer`,
  `zharp::stamp`); `ZHARP_LOGS=true` sends lines to Zharp.
- `zharp::build::id()` for `build.rs`: the build ID the source bundle from
  `npx @zharp/cli sources upload` is filed under, sent with each error as `zharp.build_id`.
- `capture_error`, `capture_message`, `capture_anyhow`, `set_user`, `clear_user`, `set_tag`, `log`,
  `flush`.
