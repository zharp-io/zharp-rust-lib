//! Errors: one report per error per request, sent as a log record with `exception.*` attributes
//! (the shape Zharp groups into issues) that carries the request's trace and span. Logs aren't
//! sampled, so an error is never lost to trace sampling.

use std::error::Error;
use std::time::SystemTime;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, Severity};
use opentelemetry::trace::{Status, TraceContextExt as _};
use opentelemetry::{Context, Key};

use crate::context;
use crate::db;
use crate::setup::state;
use crate::stack::{self, Frame};

/// What a report says.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub type_name: String,
    /// The error's own message, then its causes.
    pub message: String,
    pub causes: Vec<String>,
    /// Innermost first; empty when there is no stack to send.
    pub frames: Vec<Frame>,
    /// A panic's first line ("thread 'main' panicked at src/main.rs:5:5:"), above the message.
    pub headline: Option<String>,
    pub handled: bool,
    pub fatal: bool,
    pub attributes: Vec<(String, String)>,
    /// The failed query it came from (SQLx).
    pub query: Option<db::Failure>,
}

impl Report {
    fn full_message(&self) -> String {
        let mut text = self.message.clone();
        if !self.causes.is_empty() {
            text.push_str("\n\nCaused by:");
            for (i, cause) in self.causes.iter().enumerate() {
                text.push_str(&format!("\n    {i}: {cause}"));
            }
        }
        text
    }

    fn stacktrace(&self, message: &str) -> String {
        let mut text = match &self.headline {
            Some(headline) => format!("{headline}\n{message}"),
            None => format!("{}: {message}", self.type_name),
        };
        if !self.frames.is_empty() {
            text.push_str("\nstack backtrace:\n");
            text.push_str(&stack::render(&self.frames));
        }
        text
    }
}

/// Sends a report for the context's request (or none). Returns whether it was sent (false for an
/// error the request reported already).
pub(crate) fn send(cx: &Context, mut report: Report) -> bool {
    let Some(state) = state() else { return false };
    let request = context::request(cx);
    if let Some(request) = &request {
        let mut messages = vec![report.message.trim().to_string()];
        messages.extend(report.causes.iter().map(|c| c.trim().to_string()));
        messages.retain(|m| !m.is_empty());
        if !request.first_report(&messages) {
            return false;
        }
        if !report.handled {
            request.mark_failed();
        }
    }
    report.query = if state.config.query_errors {
        db::failure_for(cx, request.as_deref(), &report)
    } else {
        None
    };
    let message = match &report.query {
        Some(failure) => db::scrub(&report.full_message(), failure),
        None => report.full_message(),
    };
    let first_line = message.lines().next().unwrap_or_default().to_string();
    let span = cx.span();
    if !report.handled && span.is_recording() {
        span.set_status(Status::error(first_line.clone()));
    }

    let mut record = state.logger.create_log_record();
    let now = SystemTime::now();
    record.set_timestamp(now);
    record.set_observed_timestamp(now);
    record.set_severity_number(if report.fatal {
        Severity::Fatal
    } else {
        Severity::Error
    });
    record.set_severity_text(if report.fatal { "FATAL" } else { "ERROR" });
    record.set_body(AnyValue::from(format!(
        "{}: {first_line}",
        report.type_name
    )));
    for (key, value) in &report.attributes {
        record.add_attribute(Key::new(key.clone()), value.clone());
    }
    record.add_attribute("exception.type", report.type_name.clone());
    record.add_attribute("exception.message", message.clone());
    record.add_attribute("exception.stacktrace", report.stacktrace(&message));
    record.add_attribute(
        "zharp.handled",
        if report.handled { "true" } else { "false" },
    );
    if let Some(build_id) = &state.config.build_id {
        record.add_attribute("zharp.build_id", build_id.clone());
    }
    let mut sc = span.span_context().clone();
    if let Some(failure) = &report.query {
        record.add_attribute("zharp.mechanism", "db");
        for (key, value) in failure.attributes() {
            record.add_attribute(key, value);
        }
        if failure.span.is_valid() {
            sc = failure.span.clone();
        }
    }
    // The emitting thread's context is what the user/session processor reads: this one's.
    for (key, value) in context::context_attributes(cx) {
        record.add_attribute(Key::new(key), value);
    }
    if sc.is_valid() {
        record.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags()));
    }
    state.logger.emit(record);
    true
}

/// An error's message and its causes (each `source()`), the causes that only repeat what the
/// message already says left out.
pub(crate) fn chain(error: &(dyn Error + 'static)) -> (String, Vec<String>) {
    let message = error.to_string();
    let mut causes = Vec::new();
    let mut seen = message.clone();
    let mut next = error.source();
    while let Some(cause) = next {
        let text = cause.to_string();
        if !text.is_empty() && !seen.contains(&text) {
            seen.push('\n');
            seen.push_str(&text);
            causes.push(text);
        }
        if causes.len() >= 10 {
            break;
        }
        next = cause.source();
    }
    (message, causes)
}

/// A type's name as people know it: `sqlx::Error`, `std::io::Error`, `shop::OrderError`. None for
/// a `dyn Error`, whose concrete type the name doesn't say.
pub(crate) fn clean_type_name(name: &str) -> Option<String> {
    if name.contains("dyn ") {
        return None;
    }
    let name = name.trim_start_matches('&');
    let known = [
        ("sqlx_core::error::Error", "sqlx::Error"),
        ("std::io::error::Error", "std::io::Error"),
        ("core::io::error::Error", "std::io::Error"),
        ("reqwest::error::Error", "reqwest::Error"),
        (
            "reqwest_middleware::error::Error",
            "reqwest_middleware::Error",
        ),
        ("core::num::error::", "std::num::"),
        ("alloc::string::FromUtf8Error", "std::string::FromUtf8Error"),
        ("core::str::error::Utf8Error", "std::str::Utf8Error"),
        ("core::fmt::Error", "std::fmt::Error"),
    ];
    for (from, to) in known {
        if let Some(rest) = name.strip_prefix(from) {
            return Some(format!("{to}{rest}"));
        }
    }
    Some(name.to_string())
}

/// The name of a `dyn Error`'s type: the common ones by downcasting, else what its `Debug` starts
/// with (`NotFound(42)` → `NotFound`, an enum's variant), else "Error".
pub(crate) fn dyn_type_name(error: &(dyn Error + 'static)) -> String {
    macro_rules! known {
        ($($t:ty => $name:expr),* $(,)?) => {
            $(if error.is::<$t>() { return $name.to_string(); })*
        };
    }
    known!(
        std::io::Error => "std::io::Error",
        std::num::ParseIntError => "std::num::ParseIntError",
        std::num::ParseFloatError => "std::num::ParseFloatError",
        std::str::Utf8Error => "std::str::Utf8Error",
        std::string::FromUtf8Error => "std::string::FromUtf8Error",
        std::fmt::Error => "std::fmt::Error",
    );
    #[cfg(feature = "sqlx")]
    known!(sqlx_core::Error => "sqlx::Error");
    #[cfg(feature = "reqwest")]
    known!(reqwest::Error => "reqwest::Error", reqwest_middleware::Error => "reqwest_middleware::Error");
    type_from_debug(&format!("{error:?}")).unwrap_or_else(|| "Error".into())
}

/// `Database(PgDatabaseError { … })` → `Database`; a quoted string or a sentence → None.
pub(crate) fn type_from_debug(debug: &str) -> Option<String> {
    let end = debug
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(debug.len());
    let name = &debug[..end];
    let rest = &debug[end..];
    let looks_like_type =
        name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !name.ends_with(':');
    let shaped = rest.is_empty() || rest.starts_with('(') || rest.starts_with(" {");
    (looks_like_type && shaped).then(|| name.to_string())
}

fn report_error(
    error: &(dyn Error + 'static),
    type_name: String,
    frames: Vec<Frame>,
    handled: bool,
) -> Report {
    let (message, causes) = chain(error);
    Report {
        type_name,
        message,
        causes,
        frames,
        handled,
        query: db::failure_of(error),
        ..Default::default()
    }
}

/// Reports an error you handled. It becomes a Zharp issue (marked handled) with the request's
/// trace, user and session, and the stack of this call.
///
/// ```
/// # fn charge() -> Result<(), std::io::Error> { Ok(()) }
/// if let Err(e) = charge() {
///     zharp::capture_error(&e);
/// }
/// ```
pub fn capture_error<E: Error + 'static>(error: &E) {
    let type_name =
        clean_type_name(std::any::type_name::<E>()).unwrap_or_else(|| dyn_type_name(error));
    let report = report_error(error, type_name, stack::for_report(stack::capture()), true);
    send(&Context::current(), report);
}

/// Reports a message as a handled issue, with the stack of this call.
pub fn capture_message(message: &str) {
    let report = Report {
        type_name: "Message".into(),
        message: message.to_string(),
        frames: stack::for_report(stack::capture()),
        handled: true,
        ..Default::default()
    };
    send(&Context::current(), report);
}

/// A report for a value that is only `Display` (Reported<E>, Actix's response errors).
#[cfg(feature = "actix")]
pub(crate) fn report_text(
    cx: &Context,
    type_name: String,
    message: String,
    frames: Vec<Frame>,
    handled: bool,
) -> bool {
    send(
        cx,
        Report {
            type_name,
            message,
            frames,
            handled,
            ..Default::default()
        },
    )
}
