use once_cell::sync::OnceCell;
use std::sync::Mutex;

use tracing_appender::rolling;
use tracing_subscriber::{
    fmt,
    layer::SubscriberExt,
    reload::{Handle, Layer as ReloadLayer},
    util::SubscriberInitExt,
    EnvFilter, Registry,
};

/// Global reload handle so the UI can change the filter at runtime.
static RELOAD: OnceCell<Mutex<Handle<EnvFilter, Registry>>> = OnceCell::new();

/// Initialize logging: console + rolling daily file ("logs/de_mls_ui.log").
/// Returns the initial level string actually applied.
pub fn init_logging(default_level: &str) -> String {
    // Use env var if present, else the provided default
    let env_level = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());

    // Build a reloadable EnvFilter
    let filter = EnvFilter::try_new(&env_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let (reload_layer, handle) = ReloadLayer::new(filter);

    // File sink (non-blocking)
    let file_appender = rolling::daily("logs", "de_mls_ui.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    // Keep guard alive for the whole process to flush on drop
    Box::leak(Box::new(guard));

    // Build the subscriber: registry + reloadable filter + console + file
    tracing_subscriber::registry()
        .with(reload_layer)
        .with(fmt::layer().with_writer(std::io::stdout)) // console
        .with(fmt::layer().with_writer(file_writer).with_ansi(false)) // file
        .init();

    RELOAD.set(Mutex::new(handle)).ok();

    // Return the level we consider “active” for the UI dropdown
    std::env::var("RUST_LOG").unwrap_or(env_level)
}

/// Set the global log level dynamically, e.g. "error", "warn", "info", "debug", "trace",
/// or a full filter string like "info,de_mls_gateway=debug".
pub fn set_level(new_level: &str) -> Result<(), String> {
    let handle = RELOAD
        .get()
        .ok_or_else(|| "logger not initialized".to_string())?
        .lock()
        .map_err(|_| "reload handle poisoned".to_string())?;

    let filter = EnvFilter::try_new(new_level)
        .map_err(|e| format!("invalid level/filter '{new_level}': {e}"))?;

    // Replace the inner EnvFilter of the reloadable layer
    handle
        .modify(|inner: &mut EnvFilter| *inner = filter)
        .map_err(|e| format!("failed to apply filter: {e}"))?;

    Ok(())
}
