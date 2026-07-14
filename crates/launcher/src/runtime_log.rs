use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const RUNTIME_LOG_MAX_BYTES: u64 = 1024 * 1024;
const LOG_ENTRY_MAX_BYTES: usize = 16 * 1024;
pub(crate) const CHILD_OUTPUT_MAX_BYTES: usize = 4 * 1024;
const TRUNCATION_MARKER: &str = "...[truncated]";

#[derive(Clone, Default)]
pub(crate) struct RuntimeLog {
    writer: Arc<Mutex<Option<RuntimeLogWriter>>>,
}

impl RuntimeLog {
    pub(crate) fn open() -> Self {
        let writer = runtime_log_path()
            .and_then(|path| RuntimeLogWriter::open(path, RUNTIME_LOG_MAX_BYTES).ok());
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::default()
    }

    /// Records one diagnostic event without ever making launcher startup fail.
    ///
    /// After the first filesystem error the logger disables itself for this
    /// session. This avoids repeatedly doing synchronous I/O on a broken path.
    pub(crate) fn record(&self, message: impl AsRef<str>) {
        let Ok(mut writer) = self.writer.lock() else {
            return;
        };
        let Some(active_writer) = writer.as_mut() else {
            return;
        };
        if active_writer.record(message.as_ref()).is_err() {
            *writer = None;
        }
    }
}

struct RuntimeLogWriter {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
}

impl RuntimeLogWriter {
    fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime log size must be greater than zero",
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_append(&path)?;
        let length = file.metadata()?.len();
        let mut writer = Self {
            path,
            file: Some(file),
            length,
            max_bytes,
        };
        if writer.length >= writer.max_bytes {
            writer.rotate()?;
        }
        Ok(writer)
    }

    fn record(&mut self, message: &str) -> io::Result<()> {
        let timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let prefix = format!("[{timestamp_millis}] pid={} ", std::process::id());
        let payload_budget = self
            .max_bytes
            .saturating_sub((prefix.len() + 2) as u64)
            .min(LOG_ENTRY_MAX_BYTES as u64) as usize;
        let payload = sanitize_and_truncate(message, payload_budget);
        let entry = format!("{prefix}{payload}\r\n");

        // Another manually started launcher can share this file briefly. Refresh the actual
        // length before deciding whether to rotate instead of trusting this process's cache.
        if let Some(file) = self.file.as_ref() {
            self.length = file.metadata()?.len();
        }
        if self.length.saturating_add(entry.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("runtime log is not open"))?;
        file.write_all(entry.as_bytes())?;
        file.flush()?;
        self.length = self.length.saturating_add(entry.len() as u64);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        drop(self.file.take());

        let rotated_path = self.path.with_extension("log.1");
        match fs::remove_file(&rotated_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                // A stale rotated file held by another process must not allow
                // runtime.log itself to grow without a bound.
                truncate_file(&self.path)?;
                self.file = Some(open_append(&self.path)?);
                self.length = 0;
                return Ok(());
            }
        }

        match fs::rename(&self.path, &rotated_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => truncate_file(&self.path)?,
        }
        self.file = Some(open_append(&self.path)?);
        self.length = 0;
        Ok(())
    }
}

fn runtime_log_path() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA").map(|root| runtime_log_path_under(Path::new(&root)))
}

fn runtime_log_path_under(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join("Azookey")
        .join("logs")
        .join("runtime.log")
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn truncate_file(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map(drop)
}

fn sanitize_and_truncate(input: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(input.len().min(max_bytes));
    let mut truncated = false;
    for character in input.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len().saturating_add(character.len_utf8()) > max_bytes {
            truncated = true;
            break;
        }
        output.push(character);
    }

    if truncated {
        append_truncation_marker(&mut output, max_bytes);
    }
    output
}

fn append_truncation_marker(output: &mut String, max_bytes: usize) {
    if TRUNCATION_MARKER.len() > max_bytes {
        output.clear();
        return;
    }
    while output.len() + TRUNCATION_MARKER.len() > max_bytes {
        output.pop();
    }
    output.push_str(TRUNCATION_MARKER);
}

pub(crate) fn read_bounded_lines<R, F>(
    reader: &mut R,
    max_bytes: usize,
    mut on_line: F,
) -> io::Result<()>
where
    R: BufRead,
    F: FnMut(String),
{
    let mut line = Vec::with_capacity(max_bytes.min(1024));
    let mut truncated = false;

    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if !line.is_empty() || truncated {
                on_line(format_child_line(&line, truncated, max_bytes));
            }
            return Ok(());
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let segment_length = newline.unwrap_or(buffer.len());
        let remaining = max_bytes.saturating_sub(line.len());
        let copied = segment_length.min(remaining);
        line.extend_from_slice(&buffer[..copied]);
        if copied < segment_length {
            truncated = true;
        }

        let consumed = segment_length + usize::from(newline.is_some());
        reader.consume(consumed);

        if newline.is_some() {
            on_line(format_child_line(&line, truncated, max_bytes));
            line.clear();
            truncated = false;
        }
    }
}

fn format_child_line(bytes: &[u8], was_truncated: bool, max_bytes: usize) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    let mut output = sanitize_and_truncate(&text, max_bytes);
    if was_truncated && !output.ends_with(TRUNCATION_MARKER) {
        append_truncation_marker(&mut output, max_bytes);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn runtime_log_path_uses_local_app_data() {
        assert_eq!(
            runtime_log_path_under(Path::new(r"C:\Users\tester\AppData\Local")),
            PathBuf::from(r"C:\Users\tester\AppData\Local\Azookey\logs\runtime.log")
        );
    }

    #[test]
    fn bounded_reader_removes_newlines_and_limits_long_lines() {
        let input = format!("first\r\n{}\nlast", "x".repeat(10_000));
        let mut reader = io::BufReader::with_capacity(17, Cursor::new(input));
        let mut lines = Vec::new();

        read_bounded_lines(&mut reader, 64, |line| lines.push(line)).unwrap();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first");
        assert!(lines[1].len() <= 64);
        assert!(lines[1].ends_with(TRUNCATION_MARKER));
        assert_eq!(lines[2], "last");
        assert!(lines.iter().all(|line| !line.contains(['\r', '\n'])));
    }

    #[test]
    fn record_sanitizes_embedded_newlines() {
        let directory = unique_test_directory("sanitize");
        let path = directory.join("runtime.log");
        let mut writer = RuntimeLogWriter::open(path.clone(), 1024).unwrap();

        writer
            .record("launcher error: first\r\nsecond\u{1b}[31m")
            .unwrap();
        drop(writer);

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("launcher error: first  second"));
        assert!(!contents.contains('\u{1b}'));
        assert_eq!(contents.lines().count(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn record_rotates_before_exceeding_limit() {
        let directory = unique_test_directory("rotate");
        let path = directory.join("runtime.log");
        let rotated = directory.join("runtime.log.1");
        let mut writer = RuntimeLogWriter::open(path.clone(), 256).unwrap();

        for index in 0..10 {
            writer
                .record(&format!("event={index} payload={}", "x".repeat(70)))
                .unwrap();
        }
        drop(writer);

        assert!(fs::metadata(&path).unwrap().len() <= 256);
        assert!(fs::metadata(&rotated).unwrap().len() <= 256);
        let _ = fs::remove_dir_all(directory);
    }

    fn unique_test_directory(name: &str) -> PathBuf {
        let sequence = TEST_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "azookey-launcher-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
