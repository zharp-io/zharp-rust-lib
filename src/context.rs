//! Who and what a request is about: user, session, tags; and the state of the request in flight.
//!
//! A browser or mobile app using a Zharp SDK sends `enduser.id` and `session.id` in W3C baggage;
//! the processors below copy them (and the user set with [`set_user`], and the tags) onto every
//! span and log of that request.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use opentelemetry::baggage::BaggageExt as _;
use opentelemetry::logs::LogRecord as _;
use opentelemetry::trace::{Span as _, SpanContext};
use opentelemetry::{Context, InstrumentationScope, KeyValue};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord};
use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};

pub(crate) const USER_ID: &str = "enduser.id";
pub(crate) const USER_EMAIL: &str = "enduser.email";
pub(crate) const USER_NAME: &str = "enduser.name";
pub(crate) const SESSION_ID: &str = "session.id";

/// Who a request is for. A front end using a Zharp SDK sends its user with each request; set it
/// here when the server knows better.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct User {
    /// `enduser.id`
    pub id: Option<String>,
    /// `enduser.email`
    pub email: Option<String>,
    /// `enduser.name`
    pub name: Option<String>,
}

impl User {
    /// A user known by their ID.
    pub fn id(id: impl Into<String>) -> Self {
        User {
            id: Some(id.into()),
            ..Default::default()
        }
    }

    /// With an email.
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// With a name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn attributes(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for (key, value) in [
            (USER_ID, &self.id),
            (USER_EMAIL, &self.email),
            (USER_NAME, &self.name),
        ] {
            if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                out.push((key, v.clone()));
            }
        }
        out
    }
}

/// A request in flight, kept in its OpenTelemetry context by the Axum, Tower and Actix layers.
#[derive(Debug, Default)]
pub(crate) struct RequestState {
    /// The messages (error and causes) of each error reported in this request: a second report
    /// sharing one is the same error seen again.
    pub reported: Mutex<Vec<Vec<String>>>,
    pub user: Mutex<Option<User>>,
    /// An error was reported unhandled, or the handler panicked.
    pub failed: AtomicBool,
    pub panicked: AtomicBool,
    /// The last query this request ran (SQLx), for its error.
    pub last_query: Mutex<Option<crate::db::Query>>,
}

impl RequestState {
    /// Records the error's messages; false when one of them was reported already.
    pub fn first_report(&self, messages: &[String]) -> bool {
        let Ok(mut reported) = self.reported.try_lock() else {
            return true;
        };
        if reported
            .iter()
            .any(|seen| seen.iter().any(|m| messages.contains(m)))
        {
            return false;
        }
        reported.push(messages.to_vec());
        true
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }
}

/// The request the context belongs to, if any.
pub(crate) fn request(cx: &Context) -> Option<Arc<RequestState>> {
    cx.get::<Arc<RequestState>>().cloned()
}

static TAGS: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());
static GLOBAL_USER: Mutex<Option<User>> = Mutex::new(None);

/// Sets who the current request is for: its spans, logs and errors carry `enduser.id`,
/// `enduser.email` and `enduser.name`. Outside a request (a CLI, a worker) it is the user of
/// everything from now on.
///
/// ```
/// zharp::set_user(zharp::User::id("u_42").with_email("ada@example.com"));
/// ```
pub fn set_user(user: User) {
    let cx = Context::current();
    if let Some(request) = request(&cx) {
        let span = opentelemetry::trace::TraceContextExt::span(&cx);
        span.set_attributes(
            user.attributes()
                .into_iter()
                .map(|(k, v)| KeyValue::new(k, v)),
        );
        *request.user.lock().unwrap_or_else(|p| p.into_inner()) = Some(user);
    } else {
        *GLOBAL_USER.lock().unwrap_or_else(|p| p.into_inner()) = Some(user);
    }
}

/// Forgets the user [`set_user`] set (for the current request, or everywhere outside one).
pub fn clear_user() {
    let cx = Context::current();
    if let Some(request) = request(&cx) {
        *request.user.lock().unwrap_or_else(|p| p.into_inner()) = None;
    } else {
        *GLOBAL_USER.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Puts an attribute on every span, log and error from now on, across the program. An empty
/// value removes it.
pub fn set_tag(key: impl Into<String>, value: impl Into<String>) {
    let (key, value) = (key.into(), value.into());
    let mut tags = TAGS.write().unwrap_or_else(|p| p.into_inner());
    if value.is_empty() {
        tags.remove(&key);
    } else {
        tags.insert(key, value);
    }
}

/// Tags, then user and session from baggage, then the user set with [`set_user`] (later wins).
pub(crate) fn context_attributes(cx: &Context) -> BTreeMap<String, String> {
    let mut out = match TAGS.try_read() {
        Ok(tags) => tags.clone(),
        Err(_) => BTreeMap::new(),
    };
    let baggage = cx.baggage();
    for key in [USER_ID, SESSION_ID] {
        if let Some(value) = baggage.get(key) {
            let value = value.as_str().to_string();
            if !value.is_empty() {
                out.insert(key.to_string(), value);
            }
        }
    }
    let user = match request(cx) {
        Some(request) => request.user.try_lock().ok().and_then(|u| u.clone()),
        None => None,
    }
    .or_else(|| GLOBAL_USER.try_lock().ok().and_then(|u| u.clone()));
    if let Some(user) = user {
        out.remove(USER_ID);
        for (key, value) in user.attributes() {
            out.insert(key.to_string(), value);
        }
    }
    out
}

/// Puts user, session and tags on every span as it starts.
#[derive(Debug)]
pub(crate) struct ContextSpanProcessor;

impl SpanProcessor for ContextSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        for (key, value) in context_attributes(cx) {
            span.set_attribute(KeyValue::new(key, value));
        }
    }
    fn on_end(&self, _span: SpanData) {}
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// Puts user, session and tags on every log record (errors included), before it is batched. A
/// record emitted for another span than the current one (a failed query's) carries its own.
#[derive(Debug)]
pub(crate) struct ContextLogProcessor;

impl LogProcessor for ContextLogProcessor {
    fn emit(&self, record: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        let mut attributes = context_attributes(&Context::current());
        if attributes.is_empty() {
            return;
        }
        for (key, _) in record.attributes_iter() {
            attributes.remove(key.as_str());
        }
        for (key, value) in attributes {
            record.add_attribute(key, value);
        }
    }
    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
}

/// A span context's ids as hex, when valid.
pub(crate) fn ids(sc: &SpanContext) -> Option<(String, String)> {
    sc.is_valid()
        .then(|| (sc.trace_id().to_string(), sc.span_id().to_string()))
}
