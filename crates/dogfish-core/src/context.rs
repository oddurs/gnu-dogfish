use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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

    /// GNU's `--color` arguments (`ls`), or `None` for anything else.
    pub fn from_when(when: &str) -> Option<Self> {
        match when {
            "always" | "yes" | "force" => Some(Self::Always),
            "never" | "no" | "none" => Some(Self::Never),
            "auto" | "tty" | "if-tty" => Some(Self::Auto),
            _ => None,
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

/// The Dogfish switches a utility's options may set while it parses them
/// (`--stats`, `--color`, `--no-color`), shared by every clone of a context.
#[derive(Debug)]
pub struct Switches {
    stats: AtomicBool,
    color: AtomicU8,
}

impl Switches {
    fn new(stats: bool) -> Self {
        Self {
            stats: AtomicBool::new(stats),
            color: AtomicU8::new(ColorMode::Auto as u8),
        }
    }

    pub fn set_stats(&self, on: bool) {
        self.stats.store(on, Ordering::Relaxed);
    }

    pub fn stats(&self) -> bool {
        self.stats.load(Ordering::Relaxed)
    }

    pub fn set_color(&self, mode: ColorMode) {
        self.color.store(mode as u8, Ordering::Relaxed);
    }

    pub fn color(&self) -> ColorMode {
        match self.color.load(Ordering::Relaxed) {
            x if x == ColorMode::Always as u8 => ColorMode::Always,
            x if x == ColorMode::Never as u8 => ColorMode::Never,
            _ => ColorMode::Auto,
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
    pub force_json: bool,
    /// `--stats` and the colour mode: off and automatic until the utility's
    /// own option parser sees a Dogfish switch it accepts (`DOGFISH_STATS`
    /// in the environment turns `--stats` on for every utility).
    pub switches: Arc<Switches>,
    pub telemetry: Arc<Telemetry>,
}

impl DogfishContext {
    /// The context for one run of `tool_name`. Arguments are left alone:
    /// each utility parses its own, Dogfish switches included, so a utility
    /// GNU gives no such options (`echo --color`) keeps GNU's behaviour.
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            start_time: Instant::now(),
            stdin_is_tty: io::stdin().is_terminal(),
            stdout_is_tty: io::stdout().is_terminal(),
            stderr_is_tty: io::stderr().is_terminal(),
            force_json: false,
            switches: Arc::new(Switches::new(std::env::var_os("DOGFISH_STATS").is_some())),
            telemetry: Arc::new(Telemetry::default()),
        }
    }

    pub fn color_mode(&self) -> ColorMode {
        self.switches.color()
    }

    pub fn show_stats(&self) -> bool {
        self.switches.stats()
    }

    pub fn use_color(&self) -> bool {
        self.color_mode().should_color(self.stdout_is_tty)
    }

    pub fn use_stderr_color(&self) -> bool {
        self.color_mode().should_color(self.stderr_is_tty)
    }

    pub fn style(&self) -> Style {
        Style::new(self.use_color())
    }

    pub fn stderr_style(&self) -> Style {
        Style::new(self.use_stderr_color())
    }

    /// Emits the `--stats` telemetry footer to stderr if enabled.
    pub fn finish(&self) {
        if !self.show_stats() {
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
