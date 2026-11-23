//! Settings: what `Options` says, else the environment, else the defaults.

use std::env;

/// Where telemetry goes.
pub(crate) const DEFAULT_ENDPOINT: &str = "https://api.zharp.io/api/v1/otlp";

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub key: String,
    pub service: String,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub endpoint: String,
    pub sample_rate: f64,
    pub tracing: bool,
    pub logs: bool,
    pub log_correlation: bool,
    #[cfg_attr(
        not(any(feature = "tower", feature = "actix", feature = "reqwest")),
        allow(dead_code)
    )]
    pub headers: bool,
    pub query_errors: bool,
    pub build_id: Option<String>,
    pub install_subscriber: bool,
    pub panic_hook: bool,
    pub db_system: Option<String>,
}

pub(crate) fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub(crate) fn first(values: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

pub(crate) fn is_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub(crate) fn is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// The option when set, else the variable when it says yes or no, else `fallback`.
pub(crate) fn switch(option: Option<bool>, variable: &str, fallback: bool) -> bool {
    if let Some(v) = option {
        return v;
    }
    match var(variable) {
        Some(v) if is_true(&v) => true,
        Some(v) if is_false(&v) => false,
        _ => fallback,
    }
}

/// The running binary's name: the service when nothing else names it.
pub(crate) fn exe_name() -> Option<String> {
    let exe = env::current_exe().ok()?;
    Some(exe.file_stem()?.to_string_lossy().into_owned())
}

/// The host's name, without a dependency: what the environment or /etc/hostname says.
pub(crate) fn host_name() -> Option<String> {
    first([
        var("HOSTNAME"),
        var("COMPUTERNAME"),
        std::fs::read_to_string("/etc/hostname").ok(),
    ])
}
