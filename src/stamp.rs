//! The request's ids at the end of each line logged in it: ` trace_id=<32 hex> span_id=<16 hex>`
//! on a text line, two fields on a JSON line (which stays valid JSON). The Zharp agent reads the
//! app's output and links each line to its request by them. Lines outside a request, blank lines,
//! lines already carrying a `trace_id` and Zharp's own lines are left as they are.

use std::io::{self, Write};

use opentelemetry::Context;
use opentelemetry::trace::TraceContextExt as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::{DefaultFields, Format};
use tracing_subscriber::registry::LookupSpan;

/// What [`fmt_layer`] returns: `tracing_subscriber::fmt::Layer` printing to stdout, stamped.
pub type StampedFmtLayer<S> =
    tracing_subscriber::fmt::Layer<S, DefaultFields, Format, Stamped<fn() -> io::Stdout>>;

/// `tracing_subscriber::fmt::layer()` with the stamp: use it in place of
/// `tracing_subscriber::fmt::layer()`. `init!` uses it for the subscriber it sets up.
pub fn fmt_layer<S>() -> StampedFmtLayer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer().with_writer(stamp(io::stdout as fn() -> io::Stdout))
}

/// Stamps what a `fmt` layer writes, for one set up your way:
///
/// ```no_run
/// use tracing_subscriber::prelude::*;
///
/// tracing_subscriber::registry()
///     .with(tracing_subscriber::fmt::layer().compact().with_writer(zharp::stamp(std::io::stderr)))
///     .with(zharp::layer())
///     .init();
/// ```
pub fn stamp<W>(writer: W) -> Stamped<W> {
    Stamped { inner: writer }
}

/// A `MakeWriter` whose lines end with the request's ids. See [`stamp`].
#[derive(Clone, Debug)]
pub struct Stamped<W> {
    inner: W,
}

impl<'a, W: MakeWriter<'a>> MakeWriter<'a> for Stamped<W> {
    type Writer = StampedWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        StampedWriter {
            inner: self.inner.make_writer(),
            ids: current_ids(),
            buffer: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        StampedWriter {
            inner: self.inner.make_writer_for(meta),
            ids: current_ids(),
            buffer: Vec::new(),
        }
    }
}

/// The writer [`Stamped`] makes: it keeps one record's bytes and writes them stamped when the
/// record is done.
pub struct StampedWriter<W: Write> {
    inner: W,
    ids: Option<(String, String)>,
    buffer: Vec<u8>,
}

impl<W: Write> Write for StampedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.ids.is_none() {
            return self.inner.write(buf);
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for StampedWriter<W> {
    fn drop(&mut self) {
        if let Some(ids) = &self.ids {
            if !self.buffer.is_empty() {
                let text = String::from_utf8_lossy(&self.buffer);
                let _ = self.inner.write_all(stamp_record(&text, ids).as_bytes());
            }
        }
    }
}

fn current_ids() -> Option<(String, String)> {
    if crate::setup::state().is_some_and(|s| !s.config.log_correlation) {
        return None;
    }
    let cx = Context::current();
    crate::context::ids(cx.span().span_context())
}

/// One record (which may span lines: a backtrace) stamped once, at the end of its last line.
pub(crate) fn stamp_record(text: &str, ids: &(String, String)) -> String {
    if text.contains("trace_id") {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(index) = lines.iter().rposition(|l| !l.trim().is_empty()) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len() + 70);
    for (i, line) in lines.iter().enumerate() {
        if i == index {
            out.push_str(&stamp_line(line, ids))
        } else {
            out.push_str(line)
        }
    }
    out
}

fn stamp_line(line: &str, (trace_id, span_id): &(String, String)) -> String {
    let newline_len = if line.ends_with("\r\n") {
        2
    } else if line.ends_with('\n') {
        1
    } else {
        0
    };
    let (body, newline) = line.split_at(line.len() - newline_len);
    let trimmed = body.trim_end();
    let tail = &body[trimmed.len()..];
    if trimmed.trim().is_empty() || body.contains("[Zharp]") {
        return line.to_string();
    }
    let start = trimmed.len() - trimmed.trim_start().len();
    if trimmed.ends_with('}')
        && trimmed[start..].starts_with('{')
        && looks_like_json_object(&trimmed[start..])
    {
        let close = trimmed.len() - 1;
        let inner = trimmed[start + 1..close].trim();
        let comma = if inner.is_empty() { "" } else { "," };
        return format!(
            "{}{comma}\"trace_id\":\"{trace_id}\",\"span_id\":\"{span_id}\"}}{tail}{newline}",
            &trimmed[..close]
        );
    }
    format!("{trimmed} trace_id={trace_id} span_id={span_id}{tail}{newline}")
}

/// Balanced braces outside strings: enough to tell a JSON line from a text line ending in `}`.
fn looks_like_json_object(text: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 && i != text.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string && text[1..].trim_start().starts_with(['"', '}'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (String, String) {
        (
            "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            "00f067aa0ba902b7".into(),
        )
    }

    #[test]
    fn stamps_text_and_json_lines() {
        assert_eq!(
            stamp_record("2026-11-22T10:00:00Z  INFO shop: order placed\n", &ids()),
            "2026-11-22T10:00:00Z  INFO shop: order placed trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7\n"
        );
        assert_eq!(
            stamp_record(
                "{\"level\":\"INFO\",\"fields\":{\"message\":\"x\"}}\n",
                &ids()
            ),
            "{\"level\":\"INFO\",\"fields\":{\"message\":\"x\"},\"trace_id\":\"4bf92f3577b34da6a3ce929d0e0e4736\",\"span_id\":\"00f067aa0ba902b7\"}\n"
        );
        // A text line that happens to end in a brace stays text.
        assert!(stamp_record("cart {id} done {ok}\n", &ids()).ends_with(
            "{ok} trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7\n"
        ));
        // Already stamped, Zharp's own, blank: unchanged.
        assert_eq!(stamp_record("x trace_id=1\n", &ids()), "x trace_id=1\n");
        assert_eq!(stamp_record("[Zharp] active\n", &ids()), "[Zharp] active\n");
        assert_eq!(stamp_record("\n", &ids()), "\n");
        // A multi-line record is stamped once, on its last line.
        assert_eq!(
            stamp_record("a\nb\n", &ids()),
            "a\nb trace_id=4bf92f3577b34da6a3ce929d0e0e4736 span_id=00f067aa0ba902b7\n"
        );
    }
}
