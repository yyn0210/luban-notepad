//! File logging, rotation and crash diagnostics.
//!
//! Logging is deliberately best-effort: an unwritable configuration directory
//! must never stop the editor from starting. The terminal output and filter
//! come from `env_logger`; this module adds a rotating file next to it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Log, Metadata, Record};
use luban_core::settings::config_dir;

/// Rotate the active log at this size.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Number of rotated files kept besides the active one.
pub const KEEP_ROTATED: usize = 3;
pub const LOG_FILE: &str = "luban.log";
pub const CRASH_FILE: &str = "crash.log";

/// Directory holding application logs.
pub fn log_dir() -> PathBuf {
    config_dir().join("logs")
}

/// Initialize terminal and file logging, plus the panic crash log.
pub fn init() {
    let directory = log_dir();
    let file = match prepare_log_file(&directory) {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!(
                "luban-notepad: cannot open logs in {}: {error}",
                directory.display()
            );
            None
        }
    };
    install_panic_hook(&directory);
    let console = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("luban_notepad=info,gpui=warn"),
    )
    .build();
    let level = console.filter();
    let logger = TeeLogger {
        console,
        file: Mutex::new(file),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}

/// Open the active log, rotating it first when it exceeds [`MAX_LOG_BYTES`].
fn prepare_log_file(directory: &Path) -> io::Result<File> {
    fs::create_dir_all(directory)?;
    rotate_if_needed(directory, MAX_LOG_BYTES)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(LOG_FILE))
}

fn rotate_if_needed(directory: &Path, max_bytes: u64) -> io::Result<()> {
    let active = directory.join(LOG_FILE);
    let size = match fs::metadata(&active) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if size <= max_bytes {
        return Ok(());
    }
    let rotated = |index: usize| directory.join(format!("{LOG_FILE}.{index}"));
    let _ = fs::remove_file(rotated(KEEP_ROTATED));
    for index in (1..KEEP_ROTATED).rev() {
        let from = rotated(index);
        let to = rotated(index + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::rename(&active, rotated(1))?;
    Ok(())
}

fn install_panic_hook(directory: &Path) {
    let default_hook = std::panic::take_hook();
    let directory = directory.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown location".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        if let Err(error) =
            append_crash_log(&directory, &message, &location, &backtrace.to_string())
        {
            eprintln!("luban-notepad: cannot write crash log: {error}");
        }
        default_hook(info);
    }));
}

fn append_crash_log(
    directory: &Path,
    message: &str,
    location: &str,
    backtrace: &str,
) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(CRASH_FILE))?;
    writeln!(
        file,
        "[{}] panic at {location}: {message}\n{backtrace}\n",
        format_timestamp(SystemTime::now())
    )?;
    file.flush()
}

struct TeeLogger {
    console: env_logger::Logger,
    file: Mutex<Option<File>>,
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.console.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.console.enabled(record.metadata()) {
            return;
        }
        self.console.log(record);
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(
                    file,
                    "[{}] {:<5} {}: {}",
                    format_timestamp(SystemTime::now()),
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }
    }

    fn flush(&self) {
        self.console.flush();
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// Format a timestamp as UTC `YYYY-MM-DD HH:MM:SS`.
pub fn format_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_time(seconds);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// Convert Unix seconds to a UTC civil date. The algorithm is Howard Hinnant's
/// `civil_from_days`, valid for the entire range of `u64` seconds.
fn civil_time(seconds: u64) -> (i64, u64, u64, u64, u64, u64) {
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let hour = remainder / 3_600;
    let minute = (remainder % 3_600) / 60;
    let second = remainder % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_utc_and_padded() {
        assert_eq!(format_timestamp(UNIX_EPOCH), "1970-01-01 00:00:00");
        assert_eq!(
            format_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(86_400 + 3661)),
            "1970-01-02 01:01:01"
        );
        assert_eq!(
            format_timestamp(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            "2023-11-14 22:13:20"
        );
    }

    #[test]
    fn rotation_shifts_files_and_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LOG_FILE), vec![b'a'; 11]).unwrap();
        fs::write(dir.path().join(format!("{LOG_FILE}.1")), b"one").unwrap();
        fs::write(dir.path().join(format!("{LOG_FILE}.2")), b"two").unwrap();
        fs::write(dir.path().join(format!("{LOG_FILE}.3")), b"three").unwrap();
        rotate_if_needed(dir.path(), 10).unwrap();
        assert_eq!(
            fs::read(dir.path().join(format!("{LOG_FILE}.1"))).unwrap(),
            vec![b'a'; 11]
        );
        assert_eq!(
            fs::read(dir.path().join(format!("{LOG_FILE}.2"))).unwrap(),
            b"one"
        );
        assert_eq!(
            fs::read(dir.path().join(format!("{LOG_FILE}.3"))).unwrap(),
            b"two"
        );
        assert!(!dir.path().join(format!("{LOG_FILE}.4")).exists());
    }

    #[test]
    fn rotation_is_skipped_below_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LOG_FILE), b"small").unwrap();
        rotate_if_needed(dir.path(), 10).unwrap();
        assert!(dir.path().join(LOG_FILE).exists());
        assert!(!dir.path().join(format!("{LOG_FILE}.1")).exists());
    }

    #[test]
    fn crash_log_appends_message_location_and_backtrace() {
        let dir = tempfile::tempdir().unwrap();
        append_crash_log(dir.path(), "boom", "src/main.rs:10:2", "stack").unwrap();
        append_crash_log(dir.path(), "again", "src/lib.rs:3:4", "trace").unwrap();
        let text = fs::read_to_string(dir.path().join(CRASH_FILE)).unwrap();
        assert!(text.contains("boom"));
        assert!(text.contains("src/main.rs:10:2"));
        assert!(text.contains("stack"));
        assert!(text.contains("again"));
        assert_eq!(text.matches("panic at").count(), 2);
    }

    #[test]
    fn log_dir_is_under_the_config_directory() {
        assert_eq!(log_dir(), config_dir().join("logs"));
    }
}
