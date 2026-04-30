use std::{fmt, sync::Mutex};

use once_cell::sync::OnceCell;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_appender::rolling;
use tracing_subscriber::{
    EnvFilter, Registry,
    field::RecordFields,
    fmt::{
        self as subscriber_fmt, FmtContext, FormatEvent, FormatFields,
        format::Writer,
        time::{FormatTime, SystemTime},
    },
    layer::SubscriberExt,
    registry::LookupSpan,
    reload::{Handle, Layer as ReloadLayer},
    util::SubscriberInitExt,
};

/// Chosen by eyeballing the longest target in the workspace
/// (`de_mls::app::user::consensus_events` ≈ 35 chars) with breathing room.
const TARGET_PAD: usize = 37;

/// Message padding — the longest event message is around 36 chars
/// ("steward election proposal accepted").
const MESSAGE_PAD: usize = 40;

/// Global reload handle so the UI can change the filter at runtime.
static RELOAD: OnceCell<Mutex<Handle<EnvFilter, Registry>>> = OnceCell::new();

/// Initialize logging: console + rolling daily file ("logs/de_mls_ui.log").
/// Returns the initial level string actually applied.
pub fn init_logging(default_level: &str) -> String {
    let env_level = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());

    let filter = EnvFilter::try_new(&env_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let (reload_layer, handle) = ReloadLayer::new(filter);

    let file_appender = rolling::daily("logs", "de_mls_ui.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    Box::leak(Box::new(guard));

    tracing_subscriber::registry()
        .with(reload_layer)
        .with(
            subscriber_fmt::layer()
                .event_format(AlignedFormat)
                .fmt_fields(AlignedFields)
                .with_writer(std::io::stdout),
        )
        .with(
            subscriber_fmt::layer()
                .event_format(AlignedFormat)
                .fmt_fields(AlignedFields)
                .with_writer(file_writer)
                .with_ansi(false),
        )
        .init();

    RELOAD.set(Mutex::new(handle)).ok();

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

    handle
        .modify(|inner: &mut EnvFilter| *inner = filter)
        .map_err(|e| format!("failed to apply filter: {e}"))?;

    Ok(())
}

/// Fixed-column log layout: `<timestamp> <level> <target_pad> <message_pad> <fields>`.
/// Same intent as Nim Chronicles — every log line puts the message at the
/// same column so structured fields scan as a table.
struct AlignedFormat;

impl<S, N> FormatEvent<S, N> for AlignedFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let ansi = writer.has_ansi_escapes();

        if ansi {
            write!(writer, "{DIM}")?;
        }
        SystemTime.format_time(&mut writer)?;
        if ansi {
            write!(writer, "{RESET}")?;
        }
        write!(writer, " ")?;

        write_level(&mut writer, meta.level(), ansi)?;
        write!(writer, " ")?;

        if ansi {
            write!(writer, "{DIM}")?;
        }
        let target = meta.target();
        if target.len() < TARGET_PAD {
            write!(writer, "{:<TARGET_PAD$}", target)?;
        } else {
            write!(writer, "{target}")?;
        }
        if ansi {
            write!(writer, "{RESET}")?;
        }
        write!(writer, "  ")?;

        // `AlignedFields` handles message padding + key=value formatting.
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

// ANSI escapes. Stored as constants so the format_event code stays readable.
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

/// Write the level in a 5-char right-aligned column, colored when ANSI is
/// enabled. Palette matches the `tracing_subscriber` defaults:
/// ERROR=red, WARN=yellow, INFO=green, DEBUG=blue, TRACE=magenta, all bold.
fn write_level(writer: &mut Writer<'_>, level: &Level, ansi: bool) -> fmt::Result {
    if !ansi {
        return write!(writer, "{level:>5}");
    }
    let color = match *level {
        Level::ERROR => "\x1b[1;31m",
        Level::WARN => "\x1b[1;33m",
        Level::INFO => "\x1b[1;32m",
        Level::DEBUG => "\x1b[1;34m",
        Level::TRACE => "\x1b[1;35m",
    };
    write!(writer, "{color}{level:>5}{RESET}")
}

/// `FormatFields` impl that writes the message first, pads to `MESSAGE_PAD`
/// chars, then writes `key=value` pairs.
struct AlignedFields;

impl<'w> FormatFields<'w> for AlignedFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut visitor = FieldCollector::default();
        fields.record(&mut visitor);
        let message = visitor.message.unwrap_or_default();

        if visitor.fields.is_empty() {
            return write!(writer, "{message}");
        }

        if message.len() < MESSAGE_PAD {
            write!(writer, "{:<MESSAGE_PAD$}", message)?;
        } else {
            write!(writer, "{message} ")?;
        }

        let mut first = true;
        for (name, value) in &visitor.fields {
            if first {
                first = false;
            } else {
                writer.write_char(' ')?;
            }
            write!(writer, "{name}={value}")?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct FieldCollector {
    message: Option<String>,
    fields: Vec<(&'static str, String)>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            // `record_debug` on the implicit message field formats it as
            // Debug — for a plain `&str` message that wraps it in quotes.
            // Strip those so the rendered line reads naturally.
            let trimmed = formatted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_owned)
                .unwrap_or(formatted);
            self.message = Some(trimmed);
        } else {
            self.fields.push((field.name(), formatted));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields.push((field.name(), format!("\"{value}\"")));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push((field.name(), value.to_string()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.push((field.name(), value.to_string()));
    }
}
