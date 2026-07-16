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

/// Split a message at a platform's length limit, preferring natural
/// boundaries in order: paragraph break, sentence end, line break, word
/// break — hard character split only as the last resort (a single unbroken
/// overlong token). Chunks never exceed `limit` bytes.
pub fn chunk_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > limit {
        let mut window_end = limit;
        while !rest.is_char_boundary(window_end) {
            window_end -= 1;
        }
        let window = &rest[..window_end];
        let cut = cut_point(window);
        chunks.push(window[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start_matches(['\n', ' ']);
    }
    if !rest.trim().is_empty() {
        chunks.push(rest.to_string());
    }
    chunks.retain(|c| !c.is_empty());
    chunks
}

/// The best split position within a window, by boundary preference.
fn cut_point(window: &str) -> usize {
    if let Some(p) = window.rfind("\n\n") {
        if p > 0 {
            return p;
        }
    }
    // Sentence ends: punctuation followed by whitespace.
    let mut best = 0usize;
    for pat in [". ", ".\n", "! ", "!\n", "? ", "?\n"] {
        if let Some(p) = window.rfind(pat) {
            best = best.max(p + 1); // cut after the punctuation
        }
    }
    if best > 0 {
        return best;
    }
    if let Some(p) = window.rfind('\n') {
        if p > 0 {
            return p;
        }
    }
    if let Some(p) = window.rfind(' ') {
        if p > 0 {
            return p;
        }
    }
    window.len()
}

#[cfg(test)]
mod chunk_tests {
    use super::chunk_message;

    #[test]
    fn prefers_paragraphs_then_sentences_then_words() {
        assert_eq!(chunk_message("hello", 2000), vec!["hello"]);

        // paragraph boundary wins
        let text = format!("{}\n\n{}", "a".repeat(1500), "b".repeat(1500));
        let chunks = chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(1500));
        assert_eq!(chunks[1], "b".repeat(1500));

        // sentence boundary when no paragraph fits
        let text = format!("{}. {}", "c".repeat(1500), "d".repeat(1500));
        let chunks = chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('.'));
        assert_eq!(chunks[1], "d".repeat(1500));

        // word boundary when no sentence fits
        let text = format!("{} {}", "e".repeat(1500), "f".repeat(1500));
        let chunks = chunk_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "e".repeat(1500));

        // hard split only for one unbroken token
        let chunks = chunk_message(&"x".repeat(4100), 2000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.len() <= 2000));

        // multibyte safety at the hard split
        let chunks = chunk_message(&"é".repeat(1200), 2000);
        assert!(chunks.iter().all(|c| c.len() <= 2000));
        assert_eq!(chunks.concat(), "é".repeat(1200));
    }
}
