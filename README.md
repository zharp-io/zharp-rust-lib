# Zharp for Rust

[Zharp](https://zharp.io) Application Monitoring for Rust: panics and errors with their stacks
and the code that ran, a trace per request, failed SQL queries, outgoing calls and logs. Built on
[OpenTelemetry Rust](https://github.com/open-telemetry/opentelemetry-rust) and `tracing`.

Rust 1.85+ (the `sqlx` feature: 1.94+, as SQLx 0.9 itself). Nothing needs Tokio: telemetry is sent from the SDK's own threads, so web services,
workers and command-line tools all work.

## Install

```sh
cargo add zharp -F axum            # Axum
cargo add zharp -F actix           # Actix Web
cargo add zharp -F tower           # tonic, Poem, Salvo, hyper: any Tower service
cargo add zharp                    # anything else
```

Add `sqlx`, `reqwest` and `anyhow` to the features when the app uses them
(`cargo add zharp -F axum,sqlx,reqwest,anyhow`). Or let `npm init zharp` do all of this.

Set the app's key (Applications → your app → Setup) in the environment:

```sh
ZHARP_KEY=zpk_…
```

## Set up

**Axum**

```rust
#[tokio::main]
async fn main() {
    let _zharp = zharp::init!(); // first; sends what's left when it drops

    let app = axum::Router::new()
        .route("/orders/{id}", axum::routing::get(show))
        .layer(zharp::axum::layer()); // after the routes, so they are named
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

**Actix Web**

```rust
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _zharp = zharp::init!();
    HttpServer::new(|| App::new().wrap(zharp::actix::Middleware).route("/orders/{id}", web::get().to(show)))
        .bind(("0.0.0.0", 8080))?
        .run()
        .await
}
```

**tonic and other Tower services:** `.layer(zharp::tower::layer())`.

**Anything else:** `let _zharp = zharp::init!();` first in `main`.

`init!` is a macro so that it knows your crate: the service defaults to the package's name and
the release to its version. Keep the guard in a variable (`let _ = …` would drop it at once).

## Readable stacks in release builds

Since Rust 1.77 a release build has no line numbers. Add line tables (the binary grows, often
10–30%; speed doesn't change):

```toml
[profile.release]
debug = "line-tables-only"
```

`npm init zharp` writes this for you.

## The code that ran

Link the app's repository in Zharp, or upload the code with the build. The build helper puts a
build ID into the binary, and the CLI uploads `src/` under the same ID:

```toml
[build-dependencies]
zharp = { version = "0.1", default-features = false }
```

```rust
// build.rs
fn main() {
    zharp::build::id();
}
```

```sh
ZHARP_AUTH_TOKEN=… npx @zharp/cli sources upload --app <app id>
```

## What is sent

- **Panics**, with a backtrace taken where they happened (no `RUST_BACKTRACE` needed). A
  panicking handler gets a 500. A panic outside a request waits up to two seconds to be sent.
- **Errors, once per request.** Rust handlers return errors, so Zharp takes them where apps already
  report them:
  - `tracing::error!(error = &e as &dyn std::error::Error, "…")` (or `error = %e`, `error = ?e`):
    the request's error, with the stack of where it was logged;
  - `zharp::capture_error(&e)` for an error you handled;
  - `zharp::axum::Reported<E>` around your error type: reported when it turns into a 5xx, with the
    stack of the `?` that made it;
  - Actix: a handler's error that becomes a 5xx;
  - `anyhow` (feature): errors keep the stack of where they were created (`init!` sets
    `RUST_LIB_BACKTRACE=1` unless you set it), `zharp::capture_anyhow(&e)`.
  The same error logged again higher up, or wrapped, is not reported twice.
- **Traces:** a server span per request named by its route, with status and all request and
  response headers (secrets such as `Authorization`, cookies and API keys as `[REDACTED]`); your
  `#[tracing::instrument]` spans under it.
- **SQLx** (feature): each query as a client span; a failed query's database, error code,
  constraint and statement on its error, with values scrubbed from the message.
- **reqwest** (feature): `zharp::reqwest::wrap(reqwest::Client::new())` traces each call and sends
  `traceparent`, so the service called joins the trace.
- **User and session** from a front end using a Zharp SDK (W3C baggage), or `zharp::set_user`.

## Logs

Each line logged in a request ends with ` trace_id=… span_id=…`, so the Zharp agent links the
line to its request. Lines are not sent by the SDK unless `ZHARP_LOGS=true` (where the agent reads
the app's output it already brings them in); turn it on where there is no agent.

`init!` sets up a `tracing` subscriber (printed lines filtered by `RUST_LOG`, default `info`) when
the app has none. With your own, add Zharp's layers to it:

```rust
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(zharp::fmt_layer().with_filter(tracing_subscriber::EnvFilter::from_default_env()))
    .with(zharp::layer())
    .init();
let _zharp = zharp::init!(zharp::Options::new().install_subscriber(false));
```

## Settings

| Environment | `Options` | Default |
|---|---|---|
| `ZHARP_KEY` | `.key()` | — (nothing is sent without one) |
| `ZHARP_SERVICE` | `.service()` | the package's name |
| `ZHARP_ENVIRONMENT` | `.environment()` | — |
| `ZHARP_RELEASE` | `.release()` | the package's version (+ git commit with the build helper) |
| `ZHARP_SAMPLE_RATE` | `.sample_rate()` | 1 |
| `ZHARP_TRACING=false` | `.tracing(false)` | on (off: errors and logs only) |
| `ZHARP_LOGS=true` | `.logs(true)` | off |
| `ZHARP_LOG_CORRELATION=false` | `.log_correlation(false)` | on |
| `ZHARP_CAPTURE_HEADERS=false` | `.capture_headers(false)` | on |
| `ZHARP_CAPTURE_QUERY_ERRORS=false` | `.capture_query_errors(false)` | on |
| `ZHARP_DB_SYSTEM` | `.db_system()` | read from the queries and errors |
| `ZHARP_DISABLED=true` | | |

API: `capture_error`, `capture_message`, `capture_anyhow`, `set_user`, `clear_user`, `set_tag`,
`log`, `flush`.

## License

MIT
