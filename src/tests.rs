//! The SDK end to end in one process: Axum and Actix apps with the layers on, what they send read
//! back from in-memory exporters.
#![allow(clippy::unnecessary_literal_unwrap)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use opentelemetry::trace::{SpanKind, Status};
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use tower::ServiceExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

use crate::{Guard, Options};

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    spans: InMemorySpanExporter,
    logs: InMemoryLogExporter,
    guard: Option<Guard>,
    printed: Arc<Mutex<Vec<u8>>>,
    _subscriber: tracing::subscriber::DefaultGuard,
    _serial: MutexGuard<'static, ()>,
}

#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn start(options: Options) -> Harness {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let spans = InMemorySpanExporter::default();
    let logs = InMemoryLogExporter::default();
    let mut options = options
        .key("zpk_test")
        .service("shop")
        .install_subscriber(false);
    options.testing = Some((spans.clone(), logs.clone()));
    let guard = options.init();
    let printed = Arc::new(Mutex::new(Vec::new()));
    let buffer = Buffer(printed.clone());
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(crate::stamp(move || buffer.clone())),
        )
        .with(crate::layer());
    let default = tracing::subscriber::set_default(subscriber);
    Harness {
        spans,
        logs,
        guard: Some(guard),
        printed,
        _subscriber: default,
        _serial: serial,
    }
}

struct Error {
    attributes: HashMap<String, String>,
    trace_id: String,
    span_id: String,
}

fn text(value: &opentelemetry::logs::AnyValue) -> String {
    match value {
        opentelemetry::logs::AnyValue::String(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

impl Harness {
    fn spans(&self) -> Vec<SpanData> {
        self.spans.get_finished_spans().unwrap()
    }

    fn span(&self, name: &str) -> SpanData {
        let spans = self.spans();
        let names: Vec<_> = spans.iter().map(|s| s.name.to_string()).collect();
        spans
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no span {name}: {names:?}"))
    }

    /// (attributes, trace id, span id, body) of each log record.
    fn records(&self) -> Vec<(HashMap<String, String>, String, String, String)> {
        self.logs
            .get_emitted_logs()
            .unwrap()
            .into_iter()
            .map(|l| {
                let attributes = l
                    .record
                    .attributes_iter()
                    .map(|(k, v)| (k.to_string(), text(v)))
                    .collect();
                let (trace, span) = l
                    .record
                    .trace_context()
                    .map(|t| (t.trace_id.to_string(), t.span_id.to_string()))
                    .unwrap_or_default();
                let body = l.record.body().map(text).unwrap_or_default();
                (attributes, trace, span, body)
            })
            .collect()
    }

    fn errors(&self) -> Vec<Error> {
        self.records()
            .into_iter()
            .filter(|(a, ..)| a.contains_key("exception.type"))
            .map(|(attributes, trace_id, span_id, _)| Error {
                attributes,
                trace_id,
                span_id,
            })
            .collect()
    }

    fn printed(&self) -> String {
        String::from_utf8_lossy(&self.printed.lock().unwrap()).into_owned()
    }

    /// Everything sent (simple processors export as things end; this waits for nothing else).
    fn stop(&mut self) {
        assert!(
            self.guard
                .as_ref()
                .unwrap()
                .flush(std::time::Duration::from_secs(2))
        );
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

fn read_order(id: u64) -> Result<String, std::io::Error> {
    if id == 0 {
        return Err(std::io::Error::other("disk on fire"));
    }
    Ok(format!("order {id}"))
}

#[derive(Debug, thiserror::Error)]
enum OrderError {
    #[error("could not read the order")]
    Io(#[from] std::io::Error),
    #[error("order {0} not found")]
    NotFound(u64),
}

impl axum::response::IntoResponse for OrderError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            OrderError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            OrderError::NotFound(_) => StatusCode::NOT_FOUND,
        };
        (status, self.to_string()).into_response()
    }
}

async fn show_order(
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Result<String, crate::axum::Reported<OrderError>> {
    let order = read_order(id)?;
    if id > 1000 {
        return Err(crate::axum::Reported::new(OrderError::NotFound(id)));
    }
    Ok(order)
}

async fn logged() -> (StatusCode, &'static str) {
    let error = read_order(0).unwrap_err();
    tracing::error!(
        error = &error as &dyn std::error::Error,
        "could not show the order"
    );
    // Logged again higher up, wrapped: still the same error.
    let wrapped = OrderError::Io(error);
    tracing::error!(error = &wrapped as &dyn std::error::Error, "request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "no")
}

async fn panics() -> String {
    let empty: Option<u32> = None;
    empty.unwrap().to_string()
}

async fn failed_query() -> (StatusCode, &'static str) {
    tracing::debug!(target: "sqlx::query", summary = "INSERT INTO users (email) …", db.statement = "\n\nINSERT INTO users (email) VALUES ($1)\n", rows_affected = 0u64, rows_returned = 0u64, elapsed_secs = 0.003f64);
    let text = "error returned from database: duplicate key value violates unique constraint \"users_email_key\"";
    tracing::error!(error = %text, "could not sign up");
    (StatusCode::INTERNAL_SERVER_ERROR, "no")
}

async fn with_user() -> &'static str {
    crate::set_user(crate::User::id("u_42").with_email("ada@example.com"));
    tracing::info!("hello");
    "ok"
}

fn app() -> Router {
    Router::new()
        .route("/orders/{id}", get(show_order))
        .route("/logged", get(logged))
        .route("/panic", get(panics))
        .route("/query", get(failed_query))
        .route("/user", get(with_user))
        .layer(crate::axum::layer())
}

async fn call(path: &str, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut request = Request::builder().uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_is_a_server_span_named_by_its_route_with_headers_and_parent() {
    let mut h = start(Options::new());
    let (status, _) = call(
        "/orders/7?token=abc&page=2",
        &[
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
            ("authorization", "Bearer secret"),
            ("x-request-id", "r1"),
            ("baggage", "enduser.id=u_7,session.id=s_1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    h.stop();
    let span = h.span("GET /orders/{id}");
    assert_eq!(span.span_kind, SpanKind::Server);
    assert_eq!(
        span.span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(span.parent_span_id.to_string(), "00f067aa0ba902b7");
    assert_eq!(attr(&span, "http.route").as_deref(), Some("/orders/{id}"));
    assert_eq!(
        attr(&span, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        attr(&span, "url.query").as_deref(),
        Some("token=[REDACTED]&page=2")
    );
    assert_eq!(
        attr(&span, "http.request.header.authorization").as_deref(),
        Some("[REDACTED]")
    );
    assert_eq!(
        attr(&span, "http.request.header.x-request-id").as_deref(),
        Some("r1")
    );
    assert_eq!(attr(&span, "enduser.id").as_deref(), Some("u_7"));
    assert_eq!(attr(&span, "session.id").as_deref(), Some("s_1"));
    assert_eq!(span.status, Status::Unset);
    assert!(h.errors().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_panicking_handler_answers_500_and_is_reported_once_with_its_stack() {
    let mut h = start(Options::new());
    let (status, _) = call("/panic", &[]).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    h.stop();
    let span = h.span("GET /panic");
    assert!(matches!(span.status, Status::Error { .. }));
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(e.attributes["exception.type"], "panic");
    assert_eq!(e.attributes["zharp.handled"], "false");
    assert!(
        e.attributes["exception.message"].contains("on a `None` value"),
        "{:?}",
        e.attributes
    );
    let stack = &e.attributes["exception.stacktrace"];
    assert!(stack.starts_with("thread '"), "{stack}");
    assert!(stack.contains("panicked at src/tests.rs:"), "{stack}");
    assert!(stack.contains("\nstack backtrace:\n   0: "), "{stack}");
    assert!(stack.contains("zharp::tests::panics"), "{stack}");
    // From the frame that panicked: the panic machinery and the hook are left out.
    assert!(
        stack.contains(
            "
stack backtrace:
   0: core::option::unwrap_failed
"
        ),
        "{stack}"
    );
    assert_eq!(e.trace_id, span.span_context.trace_id().to_string());
}

#[tokio::test(flavor = "current_thread")]
async fn an_error_logged_with_an_error_field_is_the_requests_one_error() {
    let mut h = start(Options::new());
    let (status, _) = call("/logged", &[]).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    h.stop();
    let errors = h.errors();
    assert_eq!(
        errors.len(),
        1,
        "{:?}",
        errors.iter().map(|e| &e.attributes).collect::<Vec<_>>()
    );
    let e = &errors[0];
    assert_eq!(e.attributes["exception.type"], "std::io::Error");
    assert_eq!(e.attributes["exception.message"], "disk on fire");
    assert_eq!(e.attributes["log.message"], "could not show the order");
    let stack = &e.attributes["exception.stacktrace"];
    assert!(
        stack.starts_with(
            "std::io::Error: disk on fire\nstack backtrace:\n   0: zharp::tests::logged"
        ),
        "{stack}"
    );
    let span = h.span("GET /logged");
    assert_eq!(e.span_id, span.span_context.span_id().to_string());
    assert!(matches!(span.status, Status::Error { .. }));
    // The line printed carries the request's ids.
    let printed = h.printed();
    let line = printed
        .lines()
        .find(|l| l.contains("could not show the order"))
        .unwrap();
    assert!(
        line.ends_with(&format!(
            "trace_id={} span_id={}",
            span.span_context.trace_id(),
            span.span_context.span_id()
        )),
        "{line}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reported_reports_a_5xx_with_the_original_error_and_not_a_404() {
    let mut h = start(Options::new());
    assert_eq!(
        call("/orders/0", &[]).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(call("/orders/2000", &[]).await.0, StatusCode::NOT_FOUND);
    h.stop();
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(e.attributes["exception.type"], "std::io::Error");
    assert_eq!(e.attributes["exception.message"], "disk on fire");
    assert!(
        e.attributes["exception.stacktrace"].contains("   0: zharp::tests::show_order"),
        "{}",
        e.attributes["exception.stacktrace"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_query_carries_its_statement_and_a_span() {
    let mut h = start(Options::new());
    call("/query", &[]).await;
    h.stop();
    let query = h.span("INSERT users");
    assert_eq!(query.span_kind, SpanKind::Client);
    assert_eq!(attr(&query, "db.system").as_deref(), Some("postgresql"));
    assert_eq!(
        attr(&query, "db.statement").as_deref(),
        Some("INSERT INTO users (email) VALUES ($1)")
    );
    let server = h.span("GET /query");
    assert_eq!(query.parent_span_id, server.span_context.span_id());
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(
        e.attributes["db.statement"],
        "INSERT INTO users (email) VALUES ($1)"
    );
    assert_eq!(e.attributes["db.system"], "postgresql");
    assert_eq!(e.attributes["zharp.mechanism"], "db");
    assert_eq!(e.span_id, query.span_context.span_id().to_string());
}

#[tokio::test(flavor = "current_thread")]
async fn set_user_and_logs_when_on() {
    let mut h = start(Options::new().logs(true));
    call("/user", &[]).await;
    h.stop();
    let span = h.span("GET /user");
    assert_eq!(attr(&span, "enduser.id").as_deref(), Some("u_42"));
    let records = h.records();
    let hello = records
        .iter()
        .find(|(_, _, _, body)| body == "hello")
        .expect("the line sent");
    assert_eq!(hello.0.get("enduser.id").map(String::as_str), Some("u_42"));
    assert_eq!(hello.1, span.span_context.trace_id().to_string());
}

#[test]
fn capture_error_outside_a_request_is_handled_with_the_callers_stack() {
    let mut h = start(Options::new().build_id("7c1d2ad0e1d4bb0b0c8d1f3a2e4b5c6d"));
    let error = read_order(0).unwrap_err();
    crate::capture_error(&error);
    h.stop();
    let errors = h.errors();
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(e.attributes["zharp.handled"], "true");
    assert_eq!(
        e.attributes["zharp.build_id"],
        "7c1d2ad0e1d4bb0b0c8d1f3a2e4b5c6d"
    );
    let stack = &e.attributes["exception.stacktrace"];
    assert!(
        stack.contains(
            "   0: zharp::tests::capture_error_outside_a_request_is_handled_with_the_callers_stack"
        ),
        "{stack}"
    );
}

#[test]
fn anyhow_errors_logged_as_debug_keep_their_causes() {
    let mut h = start(Options::new());
    fn load() -> anyhow::Result<()> {
        Err(anyhow::anyhow!("connection refused").context("could not load the cart"))
    }
    let error = load().unwrap_err();
    tracing::error!(error = ?error, "giving up");
    h.stop();
    let errors = h.errors();
    let e = &errors[0];
    assert_eq!(e.attributes["exception.type"], "anyhow::Error");
    assert_eq!(
        e.attributes["exception.message"],
        "could not load the cart\n\nCaused by:\n    0: connection refused"
    );
    assert!(
        e.attributes["exception.stacktrace"]
            .contains("zharp::tests::anyhow_errors_logged_as_debug_keep_their_causes")
    );
}

#[actix_web::test]
async fn actix_names_the_route_reports_5xx_errors_and_survives_panics() {
    use actix_web::{App, HttpResponse, test, web};
    let mut h = start(Options::new());
    let app = test::init_service(
        App::new()
            .wrap(crate::actix::Middleware)
            .route(
                "/carts/{id}",
                web::get().to(|id: web::Path<u32>| async move {
                    if *id == 0 {
                        return Err(actix_web::error::ErrorInternalServerError(
                            "the cart store is down",
                        ));
                    }
                    Ok(HttpResponse::Ok().body("ok"))
                }),
            )
            .route(
                "/panic",
                web::get().to(|| async {
                    let empty: Option<u32> = None;
                    HttpResponse::Ok().body(empty.unwrap().to_string())
                }),
            ),
    )
    .await;
    let ok = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/carts/3")
            .insert_header(("cookie", "a=b"))
            .to_request(),
    )
    .await;
    assert_eq!(ok.status().as_u16(), 200);
    let failed =
        test::call_service(&app, test::TestRequest::get().uri("/carts/0").to_request()).await;
    assert_eq!(failed.status().as_u16(), 500);
    // The panic comes back as an error, which Actix answers with a 500.
    let panicked =
        test::try_call_service(&app, test::TestRequest::get().uri("/panic").to_request()).await;
    assert_eq!(
        panicked
            .err()
            .map(|e| e.as_response_error().status_code().as_u16()),
        Some(500)
    );
    h.stop();
    let span = h.span("GET /carts/{id}");
    assert_eq!(
        attr(&span, "http.request.header.cookie").as_deref(),
        Some("[REDACTED]")
    );
    let errors = h.errors();
    let types: Vec<_> = errors
        .iter()
        .map(|e| e.attributes["exception.type"].clone())
        .collect();
    assert_eq!(types.len(), 2, "{types:?}");
    assert!(
        errors
            .iter()
            .any(|e| e.attributes["exception.message"] == "the cart store is down")
    );
    assert!(types.contains(&"panic".to_string()));
    // A panic ends the request before Actix says which route matched: the span is named by the method.
    assert!(
        h.spans()
            .iter()
            .any(|s| attr(s, "url.path").as_deref() == Some("/panic")
                && matches!(s.status, Status::Error { .. }))
    );
}
