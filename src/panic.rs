//! Panics: a hook, chained to the one already set, reports each panic with a backtrace taken right
//! there (`Backtrace::force_capture`, so `RUST_BACKTRACE` isn't needed), on the current request's
//! span. Outside a request (a panic in `main`, a worker) it waits up to two seconds for the report
//! to be sent, then the panic goes on as it would have.

use std::cell::Cell;
use std::panic::PanicHookInfo;
use std::sync::Once;
use std::sync::atomic::Ordering;
use std::time::Duration;

use opentelemetry::Context;

use crate::errors::{self, Report};
use crate::stack;

static INSTALL: Once = Once::new();

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let nested = IN_HOOK.with(|h| h.replace(true));
            if !nested {
                report(info);
                IN_HOOK.with(|h| h.set(false));
            }
            previous(info);
        }));
    });
}

/// A panic's payload as text: the `&str` or `String` it was given, else a placeholder.
pub(crate) fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

fn report(info: &PanicHookInfo<'_>) {
    if crate::setup::state().is_none() {
        return;
    }
    let cx = Context::current();
    let request = crate::context::request(&cx);
    if let Some(request) = &request {
        request.panicked.store(true, Ordering::Relaxed);
    }
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>").to_string();
    let headline = match info.location() {
        Some(at) => format!(
            "thread '{name}' panicked at {}:{}:{}:",
            at.file(),
            at.line(),
            at.column()
        ),
        None => format!("thread '{name}' panicked:"),
    };
    let report = Report {
        type_name: "panic".into(),
        message: payload_text(info.payload()),
        frames: stack::for_panic(stack::capture()),
        headline: Some(headline),
        handled: false,
        fatal: request.is_none(),
        ..Default::default()
    };
    errors::send(&cx, report);
    // Outside a request the program may be about to end (or abort): send it now.
    if request.is_none() || cfg!(panic = "abort") {
        crate::setup::flush(Duration::from_secs(2));
    }
}
