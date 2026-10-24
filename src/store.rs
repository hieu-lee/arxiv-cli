//! Local state: topic watches (watch/unwatch/watches/alerts) and cache maintenance.

use crate::net;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn watch_path() -> Result<PathBuf, String> {
    net::data_dir().map(|d| d.join("watches.json")).ok_or_else(|| "no HOME for data dir".to_string())
}

/// Only a missing file means "no watches"; any other read/parse failure is an error (never overwrite).
pub fn load_watches() -> Result<Value, String> {
    let p = watch_path()?;
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// Locked read-modify-write so concurrent arx processes never lose each other's edits.
pub fn update_watches<R>(f: impl FnOnce(&mut Value) -> Result<R, String>) -> Result<R, String> {
    let p = watch_path()?;
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.with_extension("lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| format!("lock: {e}"))?;
    let result = (|| {
        let mut v = load_watches()?;
        let r = f(&mut v)?;
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        Ok(r)
    })();
    let _ = lock.unlock();
    result
}

/// Quote tokens so they survive `args::tokenize` again.
pub fn join_tokens(t: &[String]) -> String {
    t.iter()
        .map(|s| {
            if !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '|' | '\\')) {
                s.clone()
            } else {
                format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn cache_clear() -> Result<String, String> {
    let Some(d) = net::cache_dir() else { return Ok("no cache dir".into()) };
    let (mut n, mut bytes) = (0u64, 0u64);
    if let Ok(rd) = fs::read_dir(&d) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_entry = (name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())) || name.contains(".tmp");
            if is_entry && e.file_type().map_or(false, |t| t.is_file()) {
                bytes += e.metadata().map_or(0, |m| m.len());
                if fs::remove_file(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
    }
    Ok(format!("cleared {n} files, {} KB", bytes / 1024))
}

pub fn cache_stats() -> String {
    let Some(d) = net::cache_dir() else { return "no cache dir".into() };
    let (mut n, mut bytes) = (0u64, 0u64);
    if let Ok(rd) = fs::read_dir(&d) {
        for e in rd.flatten() {
            if e.file_name().len() == 16 {
                n += 1;
                bytes += e.metadata().map_or(0, |m| m.len());
            }
        }
    }
    format!("{} {n} files {} KB", d.display(), bytes / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_roundtrip() {
        let t: Vec<String> = ["imo", "a b", "it's\"x\\y", "--cat", "math.CO", "|", ";"].iter().map(|s| s.to_string()).collect();
        assert_eq!(crate::args::tokenize(&join_tokens(&t)).unwrap(), t);
    }
}
