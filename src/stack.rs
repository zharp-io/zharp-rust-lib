//! Backtraces as Zharp reads them: std's own layout, two lines a frame —
//!
//! ```text
//!    0: shop::cart::total
//!              at ./src/cart.rs:12:9
//! ```
//!
//! — with the frames that say nothing about the error left out: the backtrace's own capture, the
//! panic machinery, Zharp's and `tracing`'s frames above the app's, and the runtime's start below
//! `main`. Release builds without line tables have the function lines only.

use std::backtrace::Backtrace;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub function: String,
    /// "./src/cart.rs:12:9", when the binary has line tables.
    pub location: Option<String>,
}

/// The frames of a backtrace as std displays it (or anyhow's "Stack backtrace:" section).
pub(crate) fn parse(text: &str) -> Vec<Frame> {
    let mut frames: Vec<Frame> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("at ") {
            if let Some(last) = frames.last_mut() {
                if last.location.is_none() {
                    last.location = Some(location.trim().to_string());
                }
            }
            continue;
        }
        let Some((number, rest)) = trimmed.split_once(':') else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let mut function = rest.trim();
        // The {:#} form: "0x55d1c2 - name".
        if let Some((address, name)) = function.split_once(" - ") {
            if address.starts_with("0x") {
                function = name.trim();
            }
        }
        if !function.is_empty() {
            frames.push(Frame {
                function: function.to_string(),
                location: None,
            });
        }
    }
    frames
}

pub(crate) fn capture() -> Vec<Frame> {
    parse(&Backtrace::force_capture().to_string())
}

/// Frames as std prints them, numbered from 0.
pub(crate) fn render(frames: &[Frame]) -> String {
    let mut out = String::new();
    for (i, frame) in frames.iter().enumerate() {
        out.push_str(&format!("{i:>4}: {}\n", frame.function));
        if let Some(location) = &frame.location {
            out.push_str(&format!("             at {location}\n"));
        }
    }
    out
}

/// Where this crate's own source lives, for frames that are only a file (inlined in a release
/// build with line tables).
const OWN_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn crate_of(frame: &Frame) -> &str {
    let name = frame.function.trim_start_matches('<');
    name.split("::").next().unwrap_or("")
}

fn in_registry_crate(frame: &Frame, crates: &[&str]) -> bool {
    let Some(location) = &frame.location else {
        return false;
    };
    let location = location.replace('\\', "/");
    crates.iter().any(|c| {
        location.split('/').any(|segment| {
            segment
                .strip_prefix(c)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|version| version.starts_with(|ch: char| ch.is_ascii_digit()))
        })
    })
}

fn is_own(frame: &Frame) -> bool {
    // This crate's own tests stand for an app.
    if frame.function.starts_with("zharp::tests")
        || frame
            .location
            .as_deref()
            .is_some_and(|l| l.ends_with(".rs") && l.contains("src/tests.rs"))
    {
        return false;
    }
    if crate_of(frame) == "zharp" {
        return true;
    }
    let Some(location) = &frame.location else {
        return false;
    };
    let own_src = format!("{}/src/", OWN_DIR.replace('\\', "/"));
    location.replace('\\', "/").starts_with(&own_src) || in_registry_crate(frame, &["zharp"])
}

/// Frames above the app's that only say how a report was made.
const MACHINERY: &[&str] = &[
    "std",
    "core",
    "alloc",
    "tracing",
    "tracing_core",
    "tracing_subscriber",
    "tracing_opentelemetry",
    "tracing_log",
    "log",
    "anyhow",
    "opentelemetry",
    "opentelemetry_sdk",
    "__rustc",
];
const MACHINERY_DIRS: &[&str] = &[
    "tracing",
    "tracing-core",
    "tracing-subscriber",
    "tracing-opentelemetry",
    "tracing-log",
    "log",
    "anyhow",
    "opentelemetry",
    "opentelemetry_sdk",
];

fn is_machinery(frame: &Frame) -> bool {
    if is_own(frame)
        || MACHINERY.contains(&crate_of(frame))
        || in_registry_crate(frame, MACHINERY_DIRS)
    {
        return true;
    }
    // Inlined std frames: a short name at /rustc/<commit>/library/….
    frame
        .location
        .as_deref()
        .is_some_and(|l| l.replace('\\', "/").contains("/library/") && l.contains("rustc"))
}

/// Below `main` (or a thread's start): the runtime's frames, never the error's.
fn trim_bottom(frames: &mut Vec<Frame>) {
    if let Some(end) = frames.iter().position(|f| {
        f.function.contains("__rust_begin_short_backtrace")
            || f.function.starts_with("std::rt::lang_start")
    }) {
        frames.truncate(end);
    }
}

/// A report's stack: from the first frame that is the app's (or a library's it called), so the
/// stack starts where `tracing::error!`, `capture_error` or `?` was.
pub(crate) fn for_report(mut frames: Vec<Frame>) -> Vec<Frame> {
    let start = frames.iter().position(|f| !is_machinery(f)).unwrap_or(0);
    frames.drain(..start);
    trim_bottom(&mut frames);
    frames
}

/// A panic's stack: from just below the panic machinery (so `core::option::unwrap_failed`, then
/// the app's frame that unwrapped).
pub(crate) fn for_panic(mut frames: Vec<Frame>) -> Vec<Frame> {
    let machinery = |f: &Frame| {
        let name = &f.function;
        let location = f.location.as_deref().unwrap_or("").replace('\\', "/");
        [
            "panicking::",
            "rust_begin_unwind",
            "__rust_end_short_backtrace",
            "panic_fmt",
            "begin_panic",
        ]
        .iter()
        .any(|m| name.contains(m))
            || [
                "/library/std/src/panicking.rs",
                "/library/core/src/panicking.rs",
                "/library/std/src/sys/backtrace.rs",
            ]
            .iter()
            .any(|m| location.ends_with(m) || location.contains(&format!("{m}:")))
    };
    // The first run of panic machinery from the top (a catch_unwind further down is not it).
    let window = frames.len().min(40);
    match frames[..window].iter().position(machinery) {
        Some(first) => {
            let end = frames[first..]
                .iter()
                .position(|f| !machinery(f))
                .map_or(frames.len(), |n| first + n);
            frames.drain(..end);
        }
        None => {
            let start = frames.iter().position(|f| !is_machinery(f)).unwrap_or(0);
            frames.drain(..start);
        }
    }
    trim_bottom(&mut frames);
    frames
}

/// An anyhow error's "Stack backtrace:" section, from its `{:?}` text.
pub(crate) fn from_debug_text(text: &str) -> Option<Vec<Frame>> {
    let at = text.find("Stack backtrace:")?;
    let frames = parse(&text[at + "Stack backtrace:".len()..]);
    (!frames.is_empty()).then(|| for_report(frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE_TABLES: &str = "   0: {closure#0}
             at ./src/main.rs:8:18
   1: <alloc::boxed::Box<dyn Fn> as core::ops::function::Fn<(&std::panic::PanicHookInfo,)>>::call
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/alloc/src/boxed.rs:2409:9
   2: std::panicking::panic_with_hook
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/std/src/panicking.rs:819:13
   3: __rustc::rust_begin_unwind
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/std/src/panicking.rs:676:5
   4: core::panicking::panic_fmt
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/core/src/panicking.rs:80:14
   5: core::option::unwrap_failed
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/core/src/option.rs:2253:5
   6: total
             at ./src/main.rs:3:42
   7: main
             at ./src/main.rs:15:20
   8: std::sys::backtrace::__rust_begin_short_backtrace::<fn(), ()>
             at /rustc/0ef1bc386059596dada8fb65b50425aab2ea637a/library/std/src/sys/backtrace.rs:166:18
   9: std::rt::lang_start::<()>::{closure#0}
";

    #[test]
    fn a_panic_starts_below_the_machinery_and_ends_at_main() {
        let frames = for_panic(parse(LINE_TABLES));
        let names: Vec<_> = frames.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, ["core::option::unwrap_failed", "total", "main"]);
        assert_eq!(frames[1].location.as_deref(), Some("./src/main.rs:3:42"));
        assert_eq!(
            render(&frames[1..]),
            "   0: total\n             at ./src/main.rs:3:42\n   1: main\n             at ./src/main.rs:15:20\n"
        );
    }

    #[test]
    fn a_report_starts_at_the_first_frame_outside_zharp_and_tracing() {
        let text = "   0: std::backtrace::Backtrace::force_capture
   1: zharp::errors::report
   2: report
             at /usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/zharp-0.1.0/src/errors.rs:10:5
   3: <tracing_subscriber::layer::layered::Layered<L,S> as tracing_core::subscriber::Subscriber>::event
   4: dispatch
             at /usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tracing-core-0.1.34/src/dispatcher.rs:1:1
   5: shop::orders::create::{{closure}}
             at ./src/orders.rs:41:9
   6: tokio::runtime::task::harness::Harness<T,S>::poll
";
        let names: Vec<_> = for_report(parse(text))
            .into_iter()
            .map(|f| f.function)
            .collect();
        assert_eq!(
            names,
            [
                "shop::orders::create::{{closure}}",
                "tokio::runtime::task::harness::Harness<T,S>::poll"
            ]
        );
    }

    #[test]
    fn reads_the_alternate_form_and_anyhow_debug_text() {
        let frames = parse(
            "   2:     0x632afad4bc18 - shop::main::{closure#0}\n  10:     0x632afad4bd76 - shop::main",
        );
        assert_eq!(frames[0].function, "shop::main::{closure#0}");
        let debug = "could not load\n\nCaused by:\n    0: refused\n\nStack backtrace:\n   0: anyhow::error::<impl anyhow::Error>::msg\n   1: shop::load\n             at ./src/main.rs:9:5\n";
        let frames = from_debug_text(debug).unwrap();
        assert_eq!(frames[0].function, "shop::load");
        assert_eq!(frames.len(), 1);
    }
}
