use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: OnceLock<Mutex<BufWriter<File>>> = OnceLock::new();

pub fn init(path: &Path) {
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = SINK.set(Mutex::new(BufWriter::new(f)));
        log("=== session start ===");
    }
}

fn now_hms() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let tod = d.as_secs() % 86400; // UTC seconds of day
    let ms = d.subsec_millis();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        ms
    )
}

pub fn log(msg: &str) {
    if let Some(lock) = SINK.get() {
        if let Ok(mut w) = lock.lock() {
            let _ = writeln!(w, "[{}] {}", now_hms(), msg);
            let _ = w.flush();
        }
    }
}

/// Write raw command output (stdout + stderr) to the log, prefixed per line.
pub fn log_output(stdout: &[u8], stderr: &[u8]) {
    if SINK.get().is_none() {
        return;
    }
    let prefix = now_hms();
    let mut combined = String::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        combined.push_str(&format!("[{prefix}]   {line}\n"));
    }
    for line in String::from_utf8_lossy(stderr).lines() {
        combined.push_str(&format!("[{prefix}]   {line}\n"));
    }
    if combined.is_empty() {
        return;
    }
    if let Some(lock) = SINK.get() {
        if let Ok(mut w) = lock.lock() {
            let _ = w.write_all(combined.as_bytes());
            let _ = w.flush();
        }
    }
}

/// Convenience macro — call from any module in the crate.
#[macro_export]
macro_rules! llog {
    ($($arg:tt)*) => {
        $crate::launcher_log::log(&format!($($arg)*))
    };
}
