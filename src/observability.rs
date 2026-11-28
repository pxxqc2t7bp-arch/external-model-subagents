//! Opt-in append-only logging; configuration is frozen on the first event.
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{self, Cursor, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

struct Config {
    file: Mutex<File>,
    level: u8,
    json: bool,
}
static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

fn rank(level: &str) -> Option<u8> {
    match level {
        "error" => Some(0), "warn" => Some(1), "info" => Some(2), "debug" => Some(3),
        _ => None,
    }
}
fn setting(name: &str, default: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default.to_owned()),
        result => result,
    }
}
fn config() -> Option<&'static Config> {
    CONFIG.get_or_init(|| {
        let path = std::env::var_os("EXT_MODELS_LOG_FILE")?;
        let load = || -> Result<Config, Box<dyn std::error::Error>> {
            let level = rank(&setting("EXT_MODELS_LOG_LEVEL", "info")?)
                .ok_or("EXT_MODELS_LOG_LEVEL must be error|warn|info|debug")?;
            let json = match setting("EXT_MODELS_LOG_FORMAT", "human")?.as_str() {
                "human" => false, "json" => true,
                _ => return Err("EXT_MODELS_LOG_FORMAT must be human|json".into()),
            };
            match std::fs::metadata(&path) {
                Ok(meta) if !meta.is_file() => return Err("log path must be a regular file".into()),
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
                _ => {}
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            Ok(Config { file: Mutex::new(file), level, json })
        };
        match load() {
            Ok(config) => Some(config),
            Err(err) => { eprintln!("observability configuration: {err}"); None }
        }
    }).as_ref()
}

/// Fields are evaluated only for enabled events. Records over 8 KiB are rejected on stderr.
pub fn event(level: &str, event: &str, fields: impl FnOnce() -> Value) {
    let Some(config) = config() else { return };
    let Some(priority) = rank(level) else {
        eprintln!("observability: invalid event level {level:?}");
        return;
    };
    if priority > config.level { return; }
    let append = || -> Result<(), Box<dyn std::error::Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let mut buffer = Cursor::new([0u8; 8192]);
        if config.json {
            write!(buffer, "{{\"timestamp_ms\":{timestamp},\"level\":\"{level}\",\"event\":")?;
        } else {
            write!(buffer, "{timestamp} {level} ")?;
        }
        serde_json::to_writer(&mut buffer, event)?;
        buffer.write_all(if config.json { b",\"fields\":" } else { b" " })?;
        serde_json::to_writer(&mut buffer, &fields())?;
        buffer.write_all(if config.json { b"}\n" } else { b"\n" })?;
        let length = buffer.position() as usize;
        config.file.lock().map_err(|_| "log file lock poisoned")?
            .write_all(&buffer.get_ref()[..length])?;
        Ok(())
    };
    if let Err(err) = append() { eprintln!("observability append: {err}"); }
}
