//! Service log management for Rev.
//!
//! Each service gets its own log file at:
//!   Debug:      ./logs/<service-name>.log
//!   Production: /Construct/AppState/rev/logs/<service-name>.log
//!
//! Logs are append-only, rotated by size. Old logs renamed to .log.1, .log.2, etc.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use chrono::Utc;

const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024; // 10 MB per log file
const MAX_LOG_FILES: u32 = 5; // keep up to 5 rotated logs

/// Returns the log directory path.
fn log_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from("./logs")
    } else {
        PathBuf::from("/Vault/Chronicle/rev")
    }
}

/// Returns the log file path for a given service name.
pub fn log_path(service_name: &str) -> PathBuf {
    // Sanitize: replace / with _ to flatten vendor.app.func names
    let safe_name = service_name.replace('/', "_");
    log_dir().join(format!("{}.log", safe_name))
}

/// Open (or create) the log file for a service. Returns a writable File handle.
pub fn open_log(service_name: &str) -> io::Result<File> {
    let path = log_path(service_name);
    fs::create_dir_all(path.parent().unwrap())?;

    // Rotate if too large
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() >= MAX_LOG_SIZE {
            rotate(&path)?;
        }
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
}

/// Write a timestamped log line.
pub fn write_log(service_name: &str, message: &str) {
    if let Ok(mut f) = open_log(service_name) {
        let ts = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {}", ts, message);
    }
}

/// Get file descriptors (stdout, stderr) for a child process to write to.
/// Returns (stdout_fd, stderr_fd) as raw file descriptors.
pub fn open_log_fds(service_name: &str) -> io::Result<(File, File)> {
    let stdout = open_log(service_name)?;
    let stderr = stdout.try_clone()?; // both go to same file
    Ok((stdout, stderr))
}

/// Read the last N lines from a service's log file.
pub fn tail_log(service_name: &str, lines: usize) -> Vec<String> {
    let path = log_path(service_name);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return vec![format!("No logs for {}", service_name)],
    };
    let reader = BufReader::new(file);
    let all_lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
    let start = all_lines.len().saturating_sub(lines);
    all_lines[start..].to_vec()
}

/// Rotate log files: .log → .log.1, .log.1 → .log.2, etc.
fn rotate(path: &Path) -> io::Result<()> {
    // Remove oldest
    let oldest = format!("{}.{}", path.display(), MAX_LOG_FILES);
    let _ = fs::remove_file(&oldest);

    // Shift existing: .log.4 → .log.5, .log.3 → .log.4, etc.
    for i in (1..MAX_LOG_FILES).rev() {
        let from = format!("{}.{}", path.display(), i);
        let to = format!("{}.{}", path.display(), i + 1);
        if Path::new(&from).exists() {
            fs::rename(&from, &to)?;
        }
    }

    // Current → .log.1
    let first_rotated = format!("{}.1", path.display());
    fs::rename(path, &first_rotated)?;

    Ok(())
}
