//! Small shared helpers.

use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The repo root: `ROSTER_ROOT` or the current working directory. Used to
/// locate `runs/` for the credential log.
pub fn root() -> PathBuf {
    std::env::var("ROSTER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
