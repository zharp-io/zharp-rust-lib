//! Starting and stopping: the providers, exporters and the global state everything else reads.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry::{Context, InstrumentationScope, KeyValue};
use opentelemetry_otlp::{Compression, Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{
    BatchLogProcessor, LogBatch, LogExporter, SdkLogger, SdkLoggerProvider,
};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData, SpanExporter,
};

use crate::config::{self, Config};
use crate::context::{ContextLogProcessor, ContextSpanProcessor};

pub(crate) const SCOPE: &str = "zharp";

/// The running SDK, after [`init!`](crate::init).
pub(crate) struct State {
    pub config: Config,
    pub tracer_provider: SdkTracerProvider,
    pub tracer: SdkTracer,
    pub logger_provider: SdkLoggerProvider,
    pub logger: SdkLogger,
    /// What `ZHARP_LOGS=true` sends `tracing` events through.
    pub log_bridge: opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge<
        SdkLoggerProvider,
        SdkLogger,
    >,
}

static STATE: RwLock<Option<Arc<State>>> = RwLock::new(None);

/// The running SDK, if any. Never blocks for long and never panics (it is read in the panic hook).
pub(crate) fn state() -> Option<Arc<State>> {
    match STATE.try_read() {
        Ok(guard) => guard.clone(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().clone(),
        Err(std::sync::TryLockError::WouldBlock) => STATE.read().ok().and_then(|g| g.clone()),
    }
}

pub(crate) fn announce(message: &str) {
    eprintln!("[Zharp] {message}");
}

/// Settings for [`init!`](crate::init). Anything not set is read from the environment, then
/// defaulted.
///
/// ```no_run
/// let _zharp = zharp::init!(zharp::Options::new().environment("production").logs(true));
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    key: Option<String>,
    service: Option<String>,
    environment: Option<String>,
    release: Option<String>,
    endpoint: Option<String>,
    sample_rate: Option<f64>,
    tracing: Option<bool>,
    logs: Option<bool>,
    log_correlation: Option<bool>,
    capture_headers: Option<bool>,
    capture_query_errors: Option<bool>,
    build_id: Option<String>,
    install_subscriber: bool,
    panic_hook: bool,
    db_system: Option<String>,
    #[cfg(feature = "anyhow")]
    anyhow_backtraces: bool,
    app: Option<App>,
    #[cfg(test)]
    pub(crate) testing: Option<(
        opentelemetry_sdk::trace::InMemorySpanExporter,
        opentelemetry_sdk::logs::InMemoryLogExporter,
    )>,
}

#[derive(Clone, Debug)]
struct App {
    name: &'static str,
    version: &'static str,
    build_id: Option<&'static str>,
    git_sha: Option<&'static str>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            key: None,
            service: None,
            environment: None,
            release: None,
            endpoint: None,
            sample_rate: None,
            tracing: None,
            logs: None,
            log_correlation: None,
            capture_headers: None,
            capture_query_errors: None,
            build_id: None,
            install_subscriber: true,
            panic_hook: true,
            db_system: None,
            #[cfg(feature = "anyhow")]
            anyhow_backtraces: true,
            app: None,
            #[cfg(test)]
            testing: None,
        }
    }
}

impl Options {
    /// Nothing set: everything from the environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// The app's key (`zpk_…`) from Applications → your app → Setup. Default: `ZHARP_KEY`.
    /// Without one, nothing is sent.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// The `service.name` everything is filed under. Default: `ZHARP_SERVICE`, then
    /// `OTEL_SERVICE_NAME`, then your package's name.
    pub fn service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    /// The environment, e.g. `"production"`. Default: `ZHARP_ENVIRONMENT`.
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// The release (`service.version`). Default: `ZHARP_RELEASE`, then your package's version
    /// plus the git commit when [`build::id`](crate::build::id) ran (`1.4.0+3f2a9c1d0b7e`).
    pub fn release(mut self, release: impl Into<String>) -> Self {
        self.release = Some(release.into());
        self
    }

    /// For Zharp's own testing; leave it unset. Default: `ZHARP_ENDPOINT`.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// The share of traces kept, 0 < rate ≤ 1 (default 1, or `ZHARP_SAMPLE_RATE`). Errors and
    /// logs are always sent.
    pub fn sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = Some(rate);
        self
    }

    /// `false` sends errors and logs only: no spans (`ZHARP_TRACING=false`). Logs and errors still
    /// carry an incoming request's trace ID.
    pub fn tracing(mut self, on: bool) -> Self {
        self.tracing = Some(on);
        self
    }

    /// Sends what the app logs through `tracing` (and the `log` crate) to Zharp. Off by default
    /// unless `ZHARP_LOGS=true`: where the Zharp agent reads the app's output it already brings the
    /// lines in, stamped with their request, and sending them too would make every line arrive
    /// twice. Turn it on where there is no agent.
    pub fn logs(mut self, on: bool) -> Self {
        self.logs = Some(on);
        self
    }

    /// Ends each line logged in a request with ` trace_id=… span_id=…` (see
    /// [`fmt_layer`](crate::fmt_layer)). On by default; `ZHARP_LOG_CORRELATION=false` turns it off.
    pub fn log_correlation(mut self, on: bool) -> Self {
        self.log_correlation = Some(on);
        self
    }

    /// Request and response headers on HTTP spans, secrets as `[REDACTED]`. On by default;
    /// `ZHARP_CAPTURE_HEADERS=false` turns it off.
    pub fn capture_headers(mut self, on: bool) -> Self {
        self.capture_headers = Some(on);
        self
    }

    /// SQLx queries as spans, and a failed query's statement and code on its error. On by default;
    /// `ZHARP_CAPTURE_QUERY_ERRORS=false` turns it off.
    pub fn capture_query_errors(mut self, on: bool) -> Self {
        self.capture_query_errors = Some(on);
        self
    }

    /// The database SQLx talks to (`"postgresql"`, `"mysql"`, `"sqlite"`), when Zharp can't tell
    /// from the queries. Default: `ZHARP_DB_SYSTEM`, else read from the queries' placeholders and
    /// the database's errors.
    pub fn db_system(mut self, system: impl Into<String>) -> Self {
        self.db_system = Some(system.into());
        self
    }

    /// The build ID the source bundle was uploaded under. Default: the one
    /// [`build::id`](crate::build::id) wrote, else `ZHARP_BUILD_ID`.
    pub fn build_id(mut self, id: impl Into<String>) -> Self {
        self.build_id = Some(id.into());
        self
    }

    /// `false` leaves `tracing` alone: add [`layer`](crate::layer) (and
    /// [`fmt_layer`](crate::fmt_layer)) to your own subscriber. By default `init!` sets up a
    /// subscriber (lines printed with the stamp, filtered by `RUST_LOG`, default `info`) unless one
    /// is set already.
    pub fn install_subscriber(mut self, on: bool) -> Self {
        self.install_subscriber = on;
        self
    }

    /// `false` leaves the panic hook alone (panics then aren't reported).
    pub fn panic_hook(mut self, on: bool) -> Self {
        self.panic_hook = on;
        self
    }

    /// `false` stops `init!` from setting `RUST_LIB_BACKTRACE=1` when neither it nor
    /// `RUST_BACKTRACE` is set. anyhow takes a backtrace where an error is created only when one
    /// of them is on.
    #[cfg(feature = "anyhow")]
    pub fn anyhow_backtraces(mut self, on: bool) -> Self {
        self.anyhow_backtraces = on;
        self
    }

    #[doc(hidden)]
    pub fn __app(
        mut self,
        name: &'static str,
        version: &'static str,
        build_id: Option<&'static str>,
        git_sha: Option<&'static str>,
    ) -> Self {
        self.app = Some(App {
            name,
            version,
            build_id,
            git_sha,
        });
        self
    }

    fn resolve(&self) -> Config {
        let app = self.app.as_ref();
        let release_default = app.map(|a| match a.git_sha {
            Some(sha) if !sha.is_empty() => format!("{}+{sha}", a.version),
            _ => a.version.to_string(),
        });
        let mut sample_rate = self
            .sample_rate
            .or_else(|| config::var("ZHARP_SAMPLE_RATE").and_then(|v| v.parse().ok()))
            .unwrap_or(1.0);
        if !(sample_rate > 0.0 && sample_rate <= 1.0) {
            sample_rate = 1.0;
        }
        Config {
            key: config::first([
                self.key.clone(),
                config::var("ZHARP_KEY"),
                config::var("ZHARP_API_KEY"),
            ])
            .unwrap_or_default(),
            service: config::first([
                self.service.clone(),
                config::var("ZHARP_SERVICE"),
                config::var("OTEL_SERVICE_NAME"),
                app.map(|a| a.name.to_string()),
                config::exe_name(),
            ])
            .unwrap_or_else(|| "unknown-service".into()),
            environment: config::first([
                self.environment.clone(),
                config::var("ZHARP_ENVIRONMENT"),
                config::var("ZHARP_ENV"),
            ]),
            release: config::first([
                self.release.clone(),
                config::var("ZHARP_RELEASE"),
                config::var("ZHARP_VERSION"),
                release_default,
            ]),
            endpoint: config::first([self.endpoint.clone(), config::var("ZHARP_ENDPOINT")])
                .unwrap_or_else(|| config::DEFAULT_ENDPOINT.into())
                .trim_end_matches('/')
                .to_string(),
            sample_rate,
            tracing: config::switch(self.tracing, "ZHARP_TRACING", true),
            logs: config::switch(self.logs, "ZHARP_LOGS", false),
            log_correlation: config::switch(self.log_correlation, "ZHARP_LOG_CORRELATION", true),
            headers: config::switch(self.capture_headers, "ZHARP_CAPTURE_HEADERS", true),
            query_errors: config::switch(
                self.capture_query_errors,
                "ZHARP_CAPTURE_QUERY_ERRORS",
                true,
            ),
            build_id: config::first([
                self.build_id.clone(),
                app.and_then(|a| a.build_id.map(String::from)),
                config::var("ZHARP_BUILD_ID"),
            ]),
            install_subscriber: self.install_subscriber,
            panic_hook: self.panic_hook,
            db_system: config::first([self.db_system.clone(), config::var("ZHARP_DB_SYSTEM")]),
        }
    }

    /// Starts sending to Zharp. Prefer [`init!`](crate::init), which also knows your package's
    /// name, version and build ID.
    pub fn init(self) -> Guard {
        if config::var("ZHARP_DISABLED").is_some_and(|v| config::is_true(&v)) {
            return Guard { state: None };
        }
        let config = self.resolve();
        #[cfg(not(test))]
        if config.key.is_empty() {
            announce("No key: set ZHARP_KEY or Options::key. Nothing is sent.");
            return Guard { state: None };
        }
        #[cfg(feature = "anyhow")]
        if self.anyhow_backtraces
            && config::var("RUST_LIB_BACKTRACE").is_none()
            && config::var("RUST_BACKTRACE").is_none()
        {
            // SAFETY: at the top of main, before the app reads its environment on other threads.
            // std reads it once, when the first backtrace is captured.
            unsafe { std::env::set_var("RUST_LIB_BACKTRACE", "1") };
        }
        let state = match start(config, &self) {
            Ok(state) => Arc::new(state),
            Err(error) => {
                announce(&format!("Could not start: {error}"));
                return Guard { state: None };
            }
        };
        let previous = match STATE.write() {
            Ok(mut guard) => guard.replace(state.clone()),
            Err(p) => p.into_inner().replace(state.clone()),
        };
        if let Some(previous) = previous {
            let _ = previous.tracer_provider.shutdown();
            let _ = previous.logger_provider.shutdown();
        }
        let config = &state.config;
        if config.panic_hook {
            crate::panic::install();
        }
        if config.install_subscriber {
            crate::layer::install_subscriber();
        } else if !crate::layer::layer_built() {
            announce("Add zharp::layer() to your tracing subscriber to send its spans and errors.");
        }
        let mut mode = String::new();
        if !config.tracing {
            mode.push_str(", errors and logs only (tracing off)");
        }
        if config.logs {
            mode.push_str(", sending logs");
        }
        let env = config
            .environment
            .as_deref()
            .map(|e| format!("environment \"{e}\""))
            .unwrap_or_else(|| "no environment set".into());
        announce(&format!(
            "Application Monitoring active: service \"{}\", {env}{mode}",
            config.service
        ));
        Guard { state: Some(state) }
    }
}

fn start(config: Config, options: &Options) -> Result<State, String> {
    let resource = resource(&config);
    let headers = HashMap::from([(
        "Authorization".to_string(),
        format!("Bearer {}", config.key),
    )]);
    let sampler = if !config.tracing {
        Sampler::AlwaysOff
    } else {
        Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(config.sample_rate)))
    };

    let mut tracer_builder = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(sampler)
        .with_span_processor(ContextSpanProcessor);
    let mut logger_builder = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_log_processor(ContextLogProcessor);

    #[cfg(test)]
    let testing = options.testing.clone();
    #[cfg(not(test))]
    let testing: Option<((), ())> = {
        let _ = options;
        None
    };
    match testing {
        #[cfg(test)]
        Some((spans, logs)) => {
            tracer_builder = tracer_builder.with_simple_exporter(spans);
            logger_builder = logger_builder.with_simple_exporter(logs);
        }
        #[cfg(not(test))]
        Some(_) => {}
        None => {
            let spans = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(format!("{}/v1/traces", config.endpoint))
                .with_headers(headers.clone())
                .with_compression(Compression::Gzip)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| e.to_string())?;
            let logs = opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_endpoint(format!("{}/v1/logs", config.endpoint))
                .with_headers(headers)
                .with_compression(Compression::Gzip)
                .with_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| e.to_string())?;
            tracer_builder = tracer_builder.with_span_processor(
                BatchSpanProcessor::builder(Announcing(spans))
                    .with_batch_config(
                        opentelemetry_sdk::trace::BatchConfigBuilder::default()
                            .with_scheduled_delay(Duration::from_secs(5))
                            .build(),
                    )
                    .build(),
            );
            logger_builder = logger_builder.with_log_processor(
                BatchLogProcessor::builder(Announcing(logs))
                    .with_batch_config(
                        opentelemetry_sdk::logs::BatchConfigBuilder::default()
                            .with_scheduled_delay(Duration::from_secs(5))
                            .build(),
                    )
                    .build(),
            );
        }
    }

    let tracer_provider = tracer_builder.build();
    let logger_provider = logger_builder.build();
    let scope = InstrumentationScope::builder(SCOPE)
        .with_version(crate::VERSION)
        .build();
    let tracer = tracer_provider.tracer_with_scope(scope.clone());
    let logger = logger_provider.logger_with_scope(scope);
    let log_bridge =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);

    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_text_map_propagator(
        opentelemetry::propagation::TextMapCompositePropagator::new(vec![
            Box::new(TraceContextPropagator::new()),
            Box::new(BaggagePropagator::new()),
        ]),
    );
    Ok(State {
        config,
        tracer_provider,
        tracer,
        logger_provider,
        logger,
        log_bridge,
    })
}

fn resource(config: &Config) -> Resource {
    let mut attributes = vec![
        KeyValue::new("telemetry.sdk.name", "zharp"),
        KeyValue::new("telemetry.sdk.language", "rust"),
        KeyValue::new("telemetry.sdk.version", crate::VERSION),
        KeyValue::new("process.pid", std::process::id() as i64),
    ];
    if let Some(release) = &config.release {
        attributes.push(KeyValue::new("service.version", release.clone()));
    }
    if let Some(environment) = &config.environment {
        attributes.push(KeyValue::new(
            "deployment.environment.name",
            environment.clone(),
        ));
    }
    if let Some(host) = config::host_name() {
        attributes.push(KeyValue::new("host.name", host));
    }
    // OTEL_RESOURCE_ATTRIBUTES first, ours over it.
    Resource::builder()
        .with_service_name(config.service.clone())
        .with_attributes(attributes)
        .build()
}

/// An exporter that says, once per distinct error, why sending failed (a wrong key, no network).
#[derive(Debug)]
struct Announcing<E>(E);

static ANNOUNCED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn announce_once(error: &dyn std::fmt::Display) {
    let mut text = error.to_string();
    text.truncate(300);
    let mut seen = ANNOUNCED.lock().unwrap_or_else(|p| p.into_inner());
    if seen.get_or_insert_with(HashSet::new).insert(text.clone()) {
        announce(&format!("Could not send: {text}"));
    }
}

impl<E: SpanExporter> SpanExporter for Announcing<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.0.export(batch).await;
        if let Err(error) = &result {
            announce_once(error);
        }
        result
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource)
    }
}

impl<E: LogExporter> LogExporter for Announcing<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let result = self.0.export(batch).await;
        if let Err(error) = &result {
            announce_once(error);
        }
        result
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource)
    }
}

/// Keeps Zharp running; when it drops (at the end of `main`), what's queued is sent and Zharp
/// stops. Keep it in a variable: `let _zharp = zharp::init!();` (not `let _ = …`, which drops it
/// at once).
#[must_use = "Zharp stops when the guard drops: keep it, `let _zharp = zharp::init!();`"]
pub struct Guard {
    state: Option<Arc<State>>,
}

impl Guard {
    /// Whether Zharp is sending (false without a key, or with `ZHARP_DISABLED=true`).
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// Sends what's queued now, waiting at most `timeout`. See [`flush`](crate::flush).
    pub fn flush(&self, timeout: Duration) -> bool {
        flush(timeout)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let current = match STATE.write() {
            Ok(mut guard) => {
                if guard.as_ref().is_some_and(|s| Arc::ptr_eq(s, &state)) {
                    guard.take();
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        };
        let _ = state.tracer_provider.force_flush();
        let _ = state.logger_provider.force_flush();
        if current {
            let _ = state
                .tracer_provider
                .shutdown_with_timeout(Duration::from_secs(5));
            let _ = state
                .logger_provider
                .shutdown_with_timeout(Duration::from_secs(5));
        }
    }
}

/// Sends everything queued now — at the end of a job, or before a serverless function returns —
/// waiting at most `timeout`. Returns whether it finished in time.
pub fn flush(timeout: Duration) -> bool {
    let Some(state) = state() else { return true };
    let (done, wait) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("zharp-flush".into())
        .spawn(move || {
            let _ = state.tracer_provider.force_flush();
            let _ = state.logger_provider.force_flush();
            let _ = done.send(());
        });
    spawned.is_ok() && wait.recv_timeout(timeout).is_ok()
}

/// A log line's level, for [`log`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warning.
    Warn,
    /// Error (a line, not an issue: report errors with [`capture_error`](crate::capture_error)).
    Error,
}

/// Sends one log line to Zharp, with the current request's trace and user, whether or not
/// `ZHARP_LOGS` is on.
///
/// ```
/// zharp::log(zharp::Level::Info, "order placed", &[("order_id", "42")]);
/// ```
pub fn log(level: Level, message: &str, attributes: &[(&str, &str)]) {
    let Some(state) = state() else { return };
    let (severity, text) = match level {
        Level::Debug => (Severity::Debug, "DEBUG"),
        Level::Info => (Severity::Info, "INFO"),
        Level::Warn => (Severity::Warn, "WARN"),
        Level::Error => (Severity::Error, "ERROR"),
    };
    let mut record = state.logger.create_log_record();
    let now = SystemTime::now();
    record.set_timestamp(now);
    record.set_observed_timestamp(now);
    record.set_severity_number(severity);
    record.set_severity_text(text);
    record.set_body(AnyValue::from(message.to_string()));
    for (key, value) in attributes {
        record.add_attribute(key.to_string(), value.to_string());
    }
    let cx = Context::current();
    let span = cx.span();
    let sc = span.span_context();
    if sc.is_valid() {
        record.set_trace_context(sc.trace_id(), sc.span_id(), Some(sc.trace_flags()));
    }
    state.logger.emit(record);
}
