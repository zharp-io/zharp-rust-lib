//! anyhow: errors reported with the stack of where they were created.
//!
//! anyhow takes that backtrace only when `RUST_LIB_BACKTRACE` or `RUST_BACKTRACE` is on; with this
//! feature `init!` sets `RUST_LIB_BACKTRACE=1` when neither is set (turn that off with
//! `Options::anyhow_backtraces(false)`). An anyhow error logged with `tracing::error!(error = ?e, …)`
//! is read the same way.

use std::backtrace::BacktraceStatus;

use opentelemetry::Context;

use crate::errors::{self, Report};
use crate::stack;

/// Reports an anyhow error you handled, with the backtrace taken where it was created (else the
/// stack of this call).
///
/// ```
/// # fn load() -> anyhow::Result<()> { Ok(()) }
/// if let Err(e) = load() {
///     zharp::capture_anyhow(&e);
/// }
/// ```
pub fn capture_anyhow(error: &anyhow::Error) {
    send(error, true);
}

pub(crate) fn report(error: &anyhow::Error, handled: bool) -> Report {
    let root: &(dyn std::error::Error + 'static) = error.as_ref();
    let frames = match error.backtrace().status() {
        BacktraceStatus::Captured => {
            stack::for_report(stack::parse(&error.backtrace().to_string()))
        }
        _ => stack::for_report(stack::capture()),
    };
    let mut chain = error.chain();
    let message = chain.next().map(|e| e.to_string()).unwrap_or_default();
    let causes = chain
        .map(|e| e.to_string())
        .filter(|c| !c.is_empty() && !message.contains(c.as_str()))
        .collect();
    let type_name = match errors::dyn_type_name(root) {
        name if name == "Error" => "anyhow::Error".to_string(),
        name => name,
    };
    let query = error.chain().find_map(crate::db::failure_of);
    Report {
        type_name,
        message,
        causes,
        frames,
        handled,
        query,
        ..Default::default()
    }
}

fn send(error: &anyhow::Error, handled: bool) {
    errors::send(&Context::current(), report(error, handled));
}
