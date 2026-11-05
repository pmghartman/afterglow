//! One log: lines to stderr and, when the config names one, a file.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

impl Level {
    /// Apollo's names and numbers: verbose and debug are debug, fatal is error
    pub fn parse(v: &str) -> Level {
        match v.trim().to_ascii_lowercase().as_str() {
            "verbose" | "debug" | "0" | "1" => Level::Debug,
            "warning" | "3" => Level::Warning,
            "error" | "fatal" | "none" | "4" | "5" | "6" => Level::Error,
            _ => Level::Info,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

struct Sink {
    min: Level,
    file: Option<File>,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// the file truncated: one run's log, as Apollo kept it
pub fn init(min: Level, path: Option<&std::path::Path>) {
    let file = path.and_then(|p| File::create(p).ok());
    let _ = SINK.set(Mutex::new(Sink { min, file }));
}

pub fn write(level: Level, msg: std::fmt::Arguments) {
    let mut guard = SINK
        .get_or_init(|| Mutex::new(Sink { min: Level::Info, file: None }))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if level < guard.min {
        return;
    }
    let line = format!("[{}] {}: {}\n", stamp(), level.name(), msg);
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(f) = guard.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

/// UTC, to the millisecond
fn stamp() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // days to a civil date (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[macro_export]
macro_rules! debug { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Debug, format_args!($($t)*)) } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Warning, format_args!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($t)*)) } }
