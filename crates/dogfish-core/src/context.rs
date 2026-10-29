use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::format::{Style, format_duration_human, format_size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            Some("always") | Some("yes") | Some("force") => Self::Always,
            Some("never") | Some("no") | Some("none") => Self::Never,
            _ => Self::Auto,
        }
    }

    pub fn should_color(self, is_tty: bool) -> bool {
        if std::env::var_os("NO_COLOR").is_some() && self != Self::Always {
            return false;
        }
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => is_tty,
        }
    }
}

/// Live telemetry tracker for `--stats` and progress reporting.
#[derive(Debug, Default)]
pub struct Telemetry {
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub items_processed: AtomicU64,
    pub strategy: Mutex<String>,
}

impl Telemetry {
    pub fn add_read(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_written(&self, bytes: u64) {
        self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_items(&self, count: u64) {
        self.items_processed.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_strategy(&self, s: impl Into<String>) {
        if let Ok(mut guard) = self.strategy.lock() {
            *guard = s.into();
        }
    }
}

/// Shared execution context passed to every Dogfish utility.
#[derive(Clone)]
pub struct DogfishContext {
    pub tool_name: String,
    pub start_time: Instant,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub stderr_is_tty: bool,
    pub show_stats: bool,
    pub force_json: bool,
    pub color_mode: ColorMode,
    /// The utility's arguments as given, before the global flags were taken
    /// out (`raw_args[0]` is the tool name). A utility with GNU semantics for
    /// `--color[=WHEN]` (where its position and validity matter, as in `ls`)
    /// parses it from here; empty when the context was built without arguments.
    pub raw_args: Vec<OsString>,
    pub telemetry: Arc<Telemetry>,
}

impl DogfishContext {
    /// Intercepts global Dogfish flags (`--stats`, `--color=...`) while preserving remaining args
    /// for the individual utility's option parser.
    pub fn from_raw_args(
        tool_name: impl Into<String>,
        raw_args: Vec<OsString>,
    ) -> (Self, Vec<OsString>) {
        let stdin_is_tty = io::stdin().is_terminal();
        let stdout_is_tty = io::stdout().is_terminal();
        let stderr_is_tty = io::stderr().is_terminal();

        let mut show_stats = std::env::var_os("DOGFISH_STATS").is_some();
        let mut color_mode = ColorMode::Auto;
        let mut filtered = Vec::with_capacity(raw_args.len());
        let mut passthrough_rest = false;
        let unfiltered = raw_args.clone();

        for (idx, arg) in raw_args.into_iter().enumerate() {
            if idx == 0 {
                filtered.push(arg);
                continue;
            }
            if passthrough_rest {
                filtered.push(arg);
                continue;
            }
            if arg == "--" {
                passthrough_rest = true;
                filtered.push(arg);
                continue;
            }
            if arg == "--stats" {
                show_stats = true;
                continue;
            }
            if let Some(val) = arg.to_str().and_then(|a| a.strip_prefix("--color=")) {
                color_mode = ColorMode::from_str_opt(Some(val));
                continue;
            }
            if arg == "--color" {
                color_mode = ColorMode::Always;
                continue;
            }
            if arg == "--no-color" {
                color_mode = ColorMode::Never;
                continue;
            }
            filtered.push(arg);
        }

        let ctx = Self {
            tool_name: tool_name.into(),
            start_time: Instant::now(),
            stdin_is_tty,
            stdout_is_tty,
            stderr_is_tty,
            show_stats,
            force_json: false,
            color_mode,
            raw_args: unfiltered,
            telemetry: Arc::new(Telemetry::default()),
        };

        (ctx, filtered)
    }

    pub fn use_color(&self) -> bool {
        self.color_mode.should_color(self.stdout_is_tty)
    }

    pub fn use_stderr_color(&self) -> bool {
        self.color_mode.should_color(self.stderr_is_tty)
    }

    pub fn style(&self) -> Style {
        Style::new(self.use_color())
    }

    pub fn stderr_style(&self) -> Style {
        Style::new(self.use_stderr_color())
    }

    /// Emits the `--stats` telemetry footer to stderr if enabled.
    pub fn finish(&self) {
        if !self.show_stats {
            return;
        }
        let elapsed = self.start_time.elapsed();
        let st = self.stderr_style();
        let r = self.telemetry.bytes_read.load(Ordering::Relaxed);
        let w = self.telemetry.bytes_written.load(Ordering::Relaxed);
        let items = self.telemetry.items_processed.load(Ordering::Relaxed);
        let strat = self
            .telemetry
            .strategy
            .lock()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();

        let total_bytes = r.max(w);
        let secs = elapsed.as_secs_f64().max(1e-9);
        let mut parts = vec![format!(
            "{} {}",
            st.bold_cyan(&format!("⚡ gnu/dogfish {}", self.tool_name)),
            st.bold_green(&format_duration_human(elapsed))
        )];

        if total_bytes > 0 {
            let bps = (total_bytes as f64 / secs) as u64;
            parts.push(format!(
                "io: {} ({}/s)",
                format_size(total_bytes, true),
                format_size(bps, true)
            ));
        }
        if items > 0 {
            parts.push(format!("items: {}", items));
        }
        if !strat.is_empty() {
            parts.push(format!("engine: {}", st.yellow(&strat)));
        }

        eprintln!(
            "{}{}{}",
            st.dim("["),
            parts.join(&st.dim(" │ ")),
            st.dim("]")
        );
    }
}
