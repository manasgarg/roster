//! Small shared helpers.

use time::format_description::well_known::Rfc3339;

pub type BErr = Box<dyn std::error::Error>;

/// Resolve an id or unique prefix against a set of ids (tasks, gates).
pub fn resolve_prefix<'a>(
    what: &str,
    id_or_prefix: &str,
    ids: impl Iterator<Item = &'a str>,
) -> Result<String, BErr> {
    let matches: Vec<&str> = ids.filter(|id| id.starts_with(id_or_prefix)).collect();
    match matches.len() {
        0 => Err(format!("no such {what} {id_or_prefix}").into()),
        1 => Ok(matches[0].to_string()),
        n => Err(format!(
            "{what} prefix {id_or_prefix} is ambiguous ({n} matches: {})",
            matches.join(", ")
        )
        .into()),
    }
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
