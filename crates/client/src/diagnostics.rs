//! Minimal release diagnostics for input-mode failures.
//!
//! This intentionally does not subscribe to `tracing`: several instrumented composition
//! methods contain user-entered text. Only explicit, numeric input-mode events are written.

use std::{
    env,
    fmt::Arguments,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use windows::{
    core::HRESULT,
    Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER},
        System::Threading::{GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_LOG_FILES: usize = 64;
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

struct DiagnosticLog {
    file: File,
    path: PathBuf,
    bytes_written: u64,
}

impl DiagnosticLog {
    fn open() -> io::Result<Self> {
        let local_app_data = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
        let directory = local_app_data.join("Azookey").join("logs").join("client");
        fs::create_dir_all(&directory)?;

        let path = directory.join(format!(
            "client-{}-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            env::consts::ARCH
        ));
        cleanup_old_logs(&directory, &path);
        // A PID identifies one host lifetime. Truncating also bounds files after PID reuse.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;

        Ok(Self {
            file,
            path,
            bytes_written: 0,
        })
    }

    fn reset(&mut self) -> io::Result<()> {
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.bytes_written = 0;
        Ok(())
    }

    fn write_line(&mut self, mut line: String) -> io::Result<()> {
        line.push('\n');
        let line_bytes = line.len() as u64;
        if self.bytes_written.saturating_add(line_bytes) > MAX_LOG_BYTES {
            self.reset()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.bytes_written = self.bytes_written.saturating_add(line_bytes);
        Ok(())
    }
}

#[derive(Default)]
struct LoggerState {
    log: Option<DiagnosticLog>,
    retry_after: Option<Instant>,
    failure_count: u32,
    last_error: Option<io::ErrorKind>,
}

static LOG: OnceLock<Mutex<LoggerState>> = OnceLock::new();
static INITIALIZED: OnceLock<()> = OnceLock::new();

fn logger() -> &'static Mutex<LoggerState> {
    LOG.get_or_init(|| Mutex::new(LoggerState::default()))
}

fn log_file_pid(path: &std::path::Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("client-")?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

fn process_is_running(pid: u32) -> bool {
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(error) => {
            // Access denied is treated conservatively as a live process. Windows reports an
            // invalid parameter for a PID that no longer exists.
            error.code() != HRESULT::from_win32(ERROR_INVALID_PARAMETER.0)
        }
    }
}

fn cleanup_old_logs(directory: &std::path::Path, current_path: &std::path::Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("log") {
                return None;
            }
            let pid = log_file_pid(&path)?;
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, pid, modified))
        })
        .collect();
    logs.sort_by(|left, right| right.2.cmp(&left.2));

    for (position, (path, pid, _)) in logs.into_iter().enumerate() {
        if position < MAX_LOG_FILES || path == current_path || process_is_running(pid) {
            continue;
        }
        let _ = fs::remove_file(path);
    }
}

fn formatted_line(event: &str, details: Arguments<'_>) -> String {
    format!(
        "{} event={} pid={} tid={:?} {}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
        event,
        std::process::id(),
        unsafe { GetCurrentThreadId() },
        details
    )
}

/// Initializes the per-host log outside `DllMain`'s loader lock.
pub fn initialize() {
    INITIALIZED.get_or_init(|| {
        event(
            "session_start",
            format_args!(
                "version={} arch={}",
                env!("CARGO_PKG_VERSION"),
                env::consts::ARCH
            ),
        );
    });
}

/// Writes one explicitly privacy-reviewed diagnostic event.
pub fn event(name: &'static str, details: Arguments<'_>) {
    let line = formatted_line(name, details);
    let Ok(mut state) = logger().lock() else {
        return;
    };
    let now = Instant::now();

    if state.log.is_none()
        && state
            .retry_after
            .map_or(true, |retry_after| now >= retry_after)
    {
        match DiagnosticLog::open() {
            Ok(mut log) => {
                if state.failure_count != 0 {
                    let recovered = formatted_line(
                        "diagnostic_recovered",
                        format_args!(
                            "failures={} last_error={:?}",
                            state.failure_count, state.last_error
                        ),
                    );
                    let _ = log.write_line(recovered);
                }
                state.log = Some(log);
                state.retry_after = None;
            }
            Err(error) => {
                state.failure_count = state.failure_count.saturating_add(1);
                state.last_error = Some(error.kind());
                state.retry_after = Some(now + RETRY_INTERVAL);
            }
        }
    }

    let write_error = state
        .log
        .as_mut()
        .and_then(|log| log.write_line(line).err());
    if let Some(error) = write_error {
        state.log = None;
        state.failure_count = state.failure_count.saturating_add(1);
        state.last_error = Some(error.kind());
        state.retry_after = Some(now + RETRY_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_line_contains_only_the_explicit_event_fields() {
        let line = formatted_line("mode_key", format_args!("vk=192 alt=true eaten=true"));
        assert!(line.contains("event=mode_key"));
        assert!(line.contains("vk=192 alt=true eaten=true"));
        assert!(!line.contains("candidate="));
        assert!(!line.contains("text="));
    }

    #[test]
    fn diagnostic_log_filename_exposes_only_its_numeric_pid() {
        assert_eq!(
            log_file_pid(std::path::Path::new("client-1234-123456789-x86_64.log")),
            Some(1234)
        );
        assert_eq!(log_file_pid(std::path::Path::new("unrelated.log")), None);
        assert_eq!(
            log_file_pid(std::path::Path::new("client-nope-x86.log")),
            None
        );
    }
}
