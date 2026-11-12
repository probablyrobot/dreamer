//! The daemon's log: a file in the state directory, or standard error in
//! the foreground. Never standard output and never a protocol socket.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static SINK: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();

pub fn init(sink: Box<dyn Write + Send>) {
    let _ = SINK.set(Mutex::new(sink));
}

pub fn write(level: &str, message: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let line = format!(
        "{}.{:03} {level} pid={} {message}\n",
        ms / 1000,
        ms % 1000,
        std::process::id()
    );
    match SINK.get() {
        Some(sink) => {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(line.as_bytes());
                let _ = w.flush();
            }
        }
        None => eprint!("{line}"),
    }
}

pub fn info(message: impl AsRef<str>) {
    write("info", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    write("warn", message.as_ref());
}
