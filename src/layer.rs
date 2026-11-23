//! The `tracing` side: [`layer`] (spans to Zharp, error events to issues, SQLx queries to spans,
//! lines to Zharp with `ZHARP_LOGS`) and the subscriber `init!` sets up when the app has none.

use std::borrow::Cow;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use opentelemetry::trace::noop::NoopSpan;
use opentelemetry::trace::{SpanBuilder, SpanContext, Status};
use opentelemetry::{Context as OtelContext, KeyValue};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{EnvFilter, FilterFn};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::errors::{self, Report};
use crate::setup::{announce, state};
use crate::stack;

static LAYER_BUILT: AtomicBool = AtomicBool::new(false);

pub(crate) fn layer_built() -> bool {
    LAYER_BUILT.load(Ordering::Relaxed)
}

/// Targets whose events are never Zharp's business: OpenTelemetry's own (and so the exporter's
/// HTTP client), and Zharp's.
fn internal(target: &str) -> bool {
    target.starts_with("opentelemetry")
        || (target.starts_with("zharp::") && !target.starts_with("zharp::tests"))
}

/// The layer that sends `tracing` to Zharp — add it to your subscriber when you set one up
/// yourself (`init!` sets one up otherwise):
///
/// ```no_run
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(tracing_subscriber::EnvFilter::new("info").and_then(zharp::fmt_layer()))
///     .with(zharp::layer())
///     .init();
/// let _zharp = zharp::init!(zharp::Options::new().install_subscriber(false));
/// ```
///
/// It brings its own filters, so your `RUST_LOG` decides what is printed, not what Zharp sees:
/// - spans at `info` and above go to Zharp as spans (under the request's span);
/// - an `error!` with an `error` (or `err`) field becomes the request's error, with the stack of
///   where it was logged — an anyhow error's own stack when it has one;
/// - SQLx's query events (`sqlx::query`, any level) become client spans (`sqlx` feature);
/// - with `ZHARP_LOGS=true`, lines (by `RUST_LOG`, default `info`) go to Zharp as logs.
///
/// It works whether it is built before or after `init!`.
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    LAYER_BUILT.store(true, Ordering::Relaxed);
    let spans = tracing_opentelemetry::OpenTelemetryLayer::new(LazyTracer)
        .with_error_events_to_exceptions(false)
        .with_error_fields_to_exceptions(false)
        .with_error_records_to_exceptions(false)
        .with_error_events_to_status(true)
        .with_threads(false)
        .with_filter(FilterFn::new(|meta: &Metadata<'_>| {
            if internal(meta.target()) || meta.target().starts_with("sqlx::query") {
                return false;
            }
            if meta.is_span() {
                *meta.level() <= Level::INFO
            } else {
                *meta.level() <= Level::WARN
            }
        }));
    let events = Events.with_filter(FilterFn::new(|meta: &Metadata<'_>| {
        !internal(meta.target())
            && (meta.target() == "sqlx::query"
                || (meta.is_event() && *meta.level() == Level::ERROR))
    }));
    let logs = Logs.with_filter(logs_filter());
    spans.and_then(events).and_then(logs)
}

/// `RUST_LOG`, default `info`, without OpenTelemetry's and the HTTP stack's own lines (the exporter
/// sending a batch must not log a line that is sent in the next one).
fn logs_filter() -> EnvFilter {
    let base = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "info".into());
    EnvFilter::builder()
        .parse_lossy(format!("{base},opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,opentelemetry_http=off,hyper=off,h2=off,reqwest=off"))
}

/// The subscriber `init!` sets up when the app has none: lines printed with the stamp, filtered by
/// `RUST_LOG` (default `info`), plus [`layer`]; the `log` crate's lines go through it too.
pub(crate) fn install_subscriber() {
    if tracing::dispatcher::has_been_set() {
        if !layer_built() {
            announce(
                "A tracing subscriber is set already: add zharp::layer() to it to send its spans, errors and queries.",
            );
        }
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let result = tracing_subscriber::registry()
        .with(crate::stamp::fmt_layer().with_filter(filter))
        .with(layer())
        .try_init();
    if let Err(error) = result {
        announce(&format!(
            "Could not set up tracing: {error}. Add zharp::layer() to your subscriber."
        ));
    }
}

// ── Spans ──────────────────────────────────────────────────────────────────

/// The tracer the span layer uses: the running SDK's, looked up as each span starts, so the layer
/// can be built before `init!`.
#[derive(Clone, Copy, Debug)]
struct LazyTracer;

impl opentelemetry::trace::Tracer for LazyTracer {
    type Span = LazySpan;

    fn build_with_context(&self, builder: SpanBuilder, parent_cx: &OtelContext) -> LazySpan {
        match state() {
            Some(s) if s.config.tracing => {
                LazySpan::Sdk(Box::new(s.tracer.build_with_context(builder, parent_cx)))
            }
            _ => LazySpan::Noop(NoopSpan::DEFAULT),
        }
    }
}

#[derive(Debug)]
enum LazySpan {
    Sdk(Box<opentelemetry_sdk::trace::Span>),
    Noop(NoopSpan),
}

impl opentelemetry::trace::Span for LazySpan {
    fn add_event_with_timestamp<T>(
        &mut self,
        name: T,
        timestamp: SystemTime,
        attributes: Vec<KeyValue>,
    ) where
        T: Into<Cow<'static, str>>,
    {
        match self {
            LazySpan::Sdk(s) => s.add_event_with_timestamp(name, timestamp, attributes),
            LazySpan::Noop(s) => s.add_event_with_timestamp(name, timestamp, attributes),
        }
    }
    fn span_context(&self) -> &SpanContext {
        match self {
            LazySpan::Sdk(s) => s.span_context(),
            LazySpan::Noop(s) => s.span_context(),
        }
    }
    fn is_recording(&self) -> bool {
        match self {
            LazySpan::Sdk(s) => s.is_recording(),
            LazySpan::Noop(_) => false,
        }
    }
    fn set_attribute(&mut self, attribute: KeyValue) {
        if let LazySpan::Sdk(s) = self {
            s.set_attribute(attribute)
        }
    }
    fn set_status(&mut self, status: Status) {
        if let LazySpan::Sdk(s) = self {
            s.set_status(status)
        }
    }
    fn update_name<T>(&mut self, new_name: T)
    where
        T: Into<Cow<'static, str>>,
    {
        if let LazySpan::Sdk(s) = self {
            s.update_name(new_name)
        }
    }
    fn add_link(&mut self, span_context: SpanContext, attributes: Vec<KeyValue>) {
        if let LazySpan::Sdk(s) = self {
            s.add_link(span_context, attributes)
        }
    }
    fn end_with_timestamp(&mut self, timestamp: SystemTime) {
        if let LazySpan::Sdk(s) = self {
            s.end_with_timestamp(timestamp)
        }
    }
}

// ── Error events and SQLx queries ──────────────────────────────────────────

struct Events;

impl<S: Subscriber> Layer<S> for Events {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if state().is_none() {
            return;
        }
        let meta = event.metadata();
        if meta.target() == "sqlx::query" {
            let mut query = QueryFields::default();
            event.record(&mut query);
            let statement = if query.statement.trim().is_empty() {
                query.summary
            } else {
                query.statement.trim().to_string()
            };
            crate::db::record_query(statement, query.elapsed, query.rows);
            return;
        }
        let mut fields = ErrorFields::default();
        event.record(&mut fields);
        let Some(mut report) = fields.report else {
            return;
        };
        if let Some(message) = fields.message {
            report.attributes.push(("log.message".into(), message));
        }
        report.attributes.extend(fields.attributes);
        report
            .attributes
            .push(("code.namespace".into(), meta.target().to_string()));
        errors::send(&OtelContext::current(), report);
    }
}

#[derive(Default)]
struct QueryFields {
    summary: String,
    statement: String,
    elapsed: Option<f64>,
    rows: Option<i64>,
}

impl Visit for QueryFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "summary" => self.summary = value.trim_end_matches(" …").to_string(),
            "db.statement" => self.statement = value.to_string(),
            _ => {}
        }
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.elapsed = Some(value);
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows_returned" && value > 0 {
            self.rows = Some(value as i64);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let text = format!("{value:?}");
        let text = text.trim_matches('"').to_string();
        self.record_str(field, &text);
    }
}

#[derive(Default)]
struct ErrorFields {
    report: Option<Report>,
    message: Option<String>,
    attributes: Vec<(String, String)>,
}

fn is_error_field(name: &str) -> bool {
    name == "error" || name == "err"
}

impl Visit for ErrorFields {
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if !is_error_field(field.name()) {
            self.attributes
                .push((field.name().to_string(), value.to_string()));
            return;
        }
        let (message, causes) = errors::chain(value);
        self.report = Some(Report {
            type_name: errors::dyn_type_name(value),
            message,
            causes,
            frames: stack::for_report(stack::capture()),
            handled: false,
            query: crate::db::failure_of(value),
            ..Default::default()
        });
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_string()),
            name if is_error_field(name) => self.report = Some(report_from_text(value)),
            name => self.attributes.push((name.to_string(), value.to_string())),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // `%e` arrives here too (its Debug is its Display).
        self.record_str(field, &format!("{value:?}"));
    }
}

/// An error logged as text (`error = %e`, `error = ?e`): anyhow's `{:?}` is read for its causes and
/// the backtrace taken where the error was created.
fn report_from_text(text: &str) -> Report {
    let (head, backtrace) = match text.find("\n\nStack backtrace:") {
        Some(i) => (&text[..i], stack::from_debug_text(&text[i..])),
        None => (text, None),
    };
    let (message, causes) = match head.find("\n\nCaused by:") {
        Some(i) => {
            let causes = head[i + "\n\nCaused by:".len()..]
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                // anyhow numbers the causes ("0: …") only when there are several.
                .map(|line| match line.split_once(": ") {
                    Some((number, rest))
                        if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        rest.to_string()
                    }
                    _ => line.to_string(),
                })
                .collect();
            (head[..i].to_string(), causes)
        }
        None => (head.to_string(), Vec::new()),
    };
    let anyhow_shaped = backtrace.is_some() || head.contains("\n\nCaused by:");
    let type_name = if anyhow_shaped {
        "anyhow::Error".to_string()
    } else {
        errors::type_from_debug(&message).unwrap_or_else(|| "Error".into())
    };
    Report {
        type_name,
        message,
        causes,
        frames: backtrace.unwrap_or_else(|| stack::for_report(stack::capture())),
        handled: false,
        ..Default::default()
    }
}

// ── Logs (ZHARP_LOGS=true) ─────────────────────────────────────────────────

struct Logs;

impl<S> Layer<S> for Logs
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(state) = state() else { return };
        if state.config.logs && !internal(event.metadata().target()) {
            Layer::<S>::on_event(&state.log_bridge, event, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_anyhow_debug_text() {
        let report = report_from_text(
            "could not load the cart\n\nCaused by:\n    0: connection refused\n    1: os error 111\n\nStack backtrace:\n   0: anyhow::error::<impl anyhow::Error>::msg\n   1: shop::load\n             at ./src/main.rs:9:5\n",
        );
        assert_eq!(report.type_name, "anyhow::Error");
        assert_eq!(report.message, "could not load the cart");
        assert_eq!(report.causes, ["connection refused", "os error 111"]);
        assert_eq!(report.frames[0].function, "shop::load");
    }

    #[test]
    fn names_a_debug_printed_enum_by_its_variant() {
        assert_eq!(report_from_text("NotFound(42)").type_name, "NotFound");
        assert_eq!(report_from_text("connection refused").type_name, "Error");
    }
}
