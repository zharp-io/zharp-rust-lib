//! SQLx queries: spans made from the `tracing` event SQLx logs for each query (target
//! `sqlx::query`, with the statement and how long it took), and — when a `sqlx::Error::Database`
//! is reported — the failed query's system, code, constraint and statement on that one report.
//!
//! SQLx has no spans and no error path of its own: the statement of a failed query is the last
//! query the request ran before its error was reported (SQLx logs a query when it finishes, failed
//! or not, and the error comes back up after that).

use std::cell::RefCell;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::trace::{SpanContext, SpanKind, Tracer as _};
use opentelemetry::{Context, KeyValue};

use crate::context::RequestState;
use crate::errors::Report;
use crate::setup::state;

/// A query SQLx ran.
#[derive(Clone, Debug)]
pub(crate) struct Query {
    pub statement: String,
    pub span: SpanContext,
    pub at: Instant,
}

/// A failed query, as its report carries it.
#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub system: Option<String>,
    pub statement: Option<String>,
    pub code: Option<String>,
    pub constraint: Option<String>,
    pub span: SpanContext,
}

impl Default for Failure {
    fn default() -> Self {
        Failure {
            system: None,
            statement: None,
            code: None,
            constraint: None,
            span: SpanContext::empty_context(),
        }
    }
}

impl Failure {
    pub fn attributes(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let system = self
            .system
            .clone()
            .or_else(|| self.statement.as_deref().map(guess_system))
            .unwrap_or_else(|| "other_sql".into());
        out.push(("db.system", system));
        if let Some(statement) = &self.statement {
            out.push(("db.statement", truncate(statement, 4000)));
        }
        if let Some(code) = &self.code {
            out.push(("db.error.code", code.clone()));
        }
        if let Some(constraint) = &self.constraint {
            out.push(("db.error.constraint", constraint.clone()));
        }
        out
    }
}

thread_local! {
    /// Outside a request: the last query this thread ran.
    static LAST: RefCell<Option<Query>> = const { RefCell::new(None) };
}

/// The database learned from its errors (PgDatabaseError → postgresql).
static LEARNED: Mutex<Option<String>> = Mutex::new(None);

/// How long after a query its error can come back and still be matched to it.
const FRESH: Duration = Duration::from_secs(30);

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The database a statement is for, by what the settings say, what its errors said, else its
/// placeholders: `$1` is Postgres's, `?` MySQL's (and SQLite's).
pub(crate) fn guess_system(statement: &str) -> String {
    if let Some(system) = state().and_then(|s| s.config.db_system.clone()) {
        return system;
    }
    if let Some(system) = LEARNED.try_lock().ok().and_then(|l| l.clone()) {
        return system;
    }
    let bytes = statement.as_bytes();
    if bytes
        .windows(2)
        .any(|w| w[0] == b'$' && w[1].is_ascii_digit())
    {
        "postgresql".into()
    } else if statement.contains('?') {
        "mysql".into()
    } else {
        "other_sql".into()
    }
}

/// "SELECT users", "INSERT orders": the operation and the table it names first.
fn span_name(statement: &str) -> String {
    let words: Vec<&str> = statement.split_whitespace().take(12).collect();
    let Some(op) = words.first() else {
        return "query".into();
    };
    let op = op.to_ascii_uppercase();
    let after = match op.as_str() {
        "SELECT" | "DELETE" => "FROM",
        "INSERT" | "REPLACE" => "INTO",
        "UPDATE" => "UPDATE",
        _ => return op,
    };
    let table = words
        .iter()
        .position(|w| w.eq_ignore_ascii_case(after))
        .and_then(|i| words.get(i + 1));
    match table {
        Some(t) => format!(
            "{op} {}",
            t.trim_matches(|c: char| c == '"' || c == '`' || c == '(' || c == ';')
        ),
        None => op,
    }
}

/// A query SQLx logged, as a client span under the current context, remembered for its error.
pub(crate) fn record_query(statement: String, elapsed: Option<f64>, rows: Option<i64>) {
    let Some(state) = state() else { return };
    if !state.config.query_errors || statement.is_empty() {
        return;
    }
    let cx = Context::current();
    let now = SystemTime::now();
    let start = elapsed
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .and_then(|d| now.checked_sub(d))
        .unwrap_or(now);
    let mut sc = SpanContext::empty_context();
    if state.config.tracing {
        let mut attributes = vec![
            KeyValue::new("db.system", guess_system(&statement)),
            KeyValue::new("db.statement", truncate(&statement, 4000)),
        ];
        if let Some(op) = statement.split_whitespace().next() {
            attributes.push(KeyValue::new("db.operation", op.to_ascii_uppercase()));
        }
        if let Some(rows) = rows {
            attributes.push(KeyValue::new("db.response.returned_rows", rows));
        }
        let span = state
            .tracer
            .span_builder(span_name(&statement))
            .with_kind(SpanKind::Client)
            .with_start_time(start)
            .with_attributes(attributes)
            .start_with_context(&state.tracer, &cx);
        sc = opentelemetry::trace::Span::span_context(&span).clone();
        let mut span = span;
        opentelemetry::trace::Span::end_with_timestamp(&mut span, now);
    }
    let query = Query {
        statement,
        span: sc,
        at: Instant::now(),
    };
    match crate::context::request(&cx) {
        Some(request) => {
            if let Ok(mut last) = request.last_query.try_lock() {
                *last = Some(query);
            }
        }
        None => LAST.with(|last| *last.borrow_mut() = Some(query)),
    }
}

fn last_query(request: Option<&RequestState>) -> Option<Query> {
    let query = match request {
        Some(request) => request.last_query.try_lock().ok().and_then(|q| q.clone()),
        None => LAST.with(|last| last.borrow().clone()),
    };
    query.filter(|q| q.at.elapsed() < FRESH)
}

/// The failed query an error came from, by the error itself (a `sqlx::Error::Database`, anywhere
/// in its chain). The statement is filled in when the report is sent.
#[allow(unused_variables)]
pub(crate) fn failure_of(error: &(dyn std::error::Error + 'static)) -> Option<Failure> {
    #[cfg(feature = "sqlx")]
    {
        let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(e) = next {
            if let Some(sqlx_core::Error::Database(db)) = e.downcast_ref::<sqlx_core::Error>() {
                let debug = format!("{db:?}");
                let system = if debug.starts_with("PgDatabaseError") {
                    Some("postgresql")
                } else if debug.starts_with("MySqlDatabaseError") {
                    Some("mysql")
                } else if debug.starts_with("SqliteError") {
                    Some("sqlite")
                } else {
                    None
                };
                if let Some(system) = system {
                    if let Ok(mut learned) = LEARNED.try_lock() {
                        *learned = Some(system.to_string());
                    }
                }
                return Some(Failure {
                    system: system.map(String::from),
                    code: db.code().map(|c| c.into_owned()),
                    constraint: db.constraint().map(String::from),
                    ..Default::default()
                });
            }
            next = e.source();
        }
    }
    None
}

/// A report's failed query: the one its error named, or — for an error that only says it came
/// from the database (SQLx's "error returned from database: …" as text) — a new one; with the
/// statement and span of the request's last query.
pub(crate) fn failure_for(
    _cx: &Context,
    request: Option<&RequestState>,
    report: &Report,
) -> Option<Failure> {
    let mut failure = report.query.clone().or_else(|| {
        let from_db = report.message.starts_with("error returned from database:")
            || report
                .causes
                .iter()
                .any(|c| c.starts_with("error returned from database:"));
        from_db.then(Failure::default)
    })?;
    if let Some(query) = last_query(request) {
        failure.statement = Some(query.statement);
        failure.span = query.span;
    }
    Some(failure)
}

/// A database error's message without the row's values: MySQL's `Duplicate entry 'a@b.c'`,
/// invalid-input values. The statement, the constraint and the code say enough.
pub(crate) fn scrub(message: &str, _failure: &Failure) -> String {
    let mut text = message.to_string();
    for (before, open, close, after) in [
        ("Duplicate entry ", '\'', '\'', " for key"),
        (" value: ", '\'', '\'', ""),
        ("invalid input syntax for type ", '"', '"', ""),
        ("invalid input value for enum ", '"', '"', ""),
        ("value ", '"', '"', " is out of range"),
    ] {
        text = mask(&text, before, open, close, after);
    }
    text
}

/// Each quoted value after `before` (the quote opening within a few words), up to a closing quote
/// followed by `after`, as `?`.
fn mask(text: &str, before: &str, open: char, close: char, after: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(before) {
        let head_end = i + before.len();
        out.push_str(&rest[..head_end]);
        rest = &rest[head_end..];
        let Some(q) = rest.find(open) else { break };
        // "invalid input syntax for type integer: \"abc\"": the quote follows a type name and ": ".
        if q > 40 || rest[..q].contains('\n') {
            continue;
        }
        let body = &rest[q + 1..];
        let end = if after.is_empty() {
            body.find(close)
        } else {
            body.find(&format!("{close}{after}"))
        };
        let Some(end) = end else { continue };
        out.push_str(&rest[..=q]);
        out.push('?');
        out.push(close);
        rest = &body[end + close.len_utf8()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_query_spans_by_operation_and_table() {
        assert_eq!(
            span_name("SELECT id, email FROM users WHERE id = $1"),
            "SELECT users"
        );
        assert_eq!(
            span_name("insert into \"orders\" (id) values (?)"),
            "INSERT orders"
        );
        assert_eq!(span_name("UPDATE carts SET total = $1"), "UPDATE carts");
        assert_eq!(span_name("BEGIN"), "BEGIN");
    }

    #[test]
    fn scrubs_values_out_of_database_messages() {
        let f = Failure::default();
        assert_eq!(
            scrub(
                "error returned from database: 1062 (23000): Duplicate entry 'ada@example.com' for key 'users.email'",
                &f
            ),
            "error returned from database: 1062 (23000): Duplicate entry '?' for key 'users.email'"
        );
        assert_eq!(
            scrub("invalid input syntax for type integer: \"abc\"", &f),
            "invalid input syntax for type integer: \"?\""
        );
        assert_eq!(
            scrub(
                "value \"123456789012\" is out of range for type integer",
                &f
            ),
            "value \"?\" is out of range for type integer"
        );
        assert_eq!(
            scrub(
                "Incorrect integer value: 'abc' for column 'age' at row 1",
                &f
            ),
            "Incorrect integer value: '?' for column 'age' at row 1"
        );
        assert_eq!(
            scrub(
                "duplicate key value violates unique constraint \"users_email_key\"",
                &f
            ),
            "duplicate key value violates unique constraint \"users_email_key\""
        );
    }
}
