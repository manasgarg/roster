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

/// Split a message at a platform's length limit, preferring line boundaries
/// (Discord: 2000). A single overlong line is hard-split at a char boundary.
pub fn chunk_message(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.len() + line.len() > limit && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if line.len() > limit {
            let mut rest = line;
            while rest.len() > limit {
                let mut cut = limit;
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                let (head, tail) = rest.split_at(cut);
                chunks.push(head.to_string());
                rest = tail;
            }
            current.push_str(rest);
        } else {
            current.push_str(line);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod chunk_tests {
    #[test]
    fn chunks_on_lines_and_hard_splits() {
        let short = super::chunk_message("hello", 2000);
        assert_eq!(short, vec!["hello"]);
        let text = format!("{}\n{}", "a".repeat(1500), "b".repeat(1500));
        let chunks = super::chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() <= 2000));
        let long_line = "x".repeat(4100);
        let chunks = super::chunk_message(&long_line, 2000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() <= 2000));
    }
}
