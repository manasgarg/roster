//! The participant scan — police for the memory/knowledge boundary
//! (docs/knowledge.md). Generic PII detection is mushy; roster has
//! an unfair advantage: the host knows exactly who was in a run. Markers are
//! the run's own channel participants (ids + display names from the channel
//! history) plus chat-mention syntax. Applied at the two crossing points:
//! `file_task` payloads filed from tainted runs, and new knowledge records at
//! checkpoint. Known limit, stated honestly: paraphrase passes the scan — the
//! hard guarantee is the read-only mount; the scan polices the choke point.

use crate::worker::memory::RunContext;
use serde_json::Value;
use std::collections::BTreeSet;

/// Exact strings that identify the run's participants: the user id, and every
/// author id / display name seen in the run's channel history. Names shorter
/// than 3 chars are skipped (false-positive bait).
pub fn participant_markers(context: &RunContext) -> Vec<String> {
    let mut markers: BTreeSet<String> = BTreeSet::new();
    if let Some(user) = context.user_id.as_deref() {
        markers.insert(user.to_string());
    }
    if let Some(channel) = context.channel_id.as_deref() {
        markers.insert(channel.to_string());
        let history = crate::paths::channel_dir(channel).join("messages.jsonl");
        if let Ok(text) = std::fs::read_to_string(history) {
            for line in text.lines() {
                let Ok(record) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                for key in ["author_id", "author"] {
                    if let Some(v) = record[key].as_str() {
                        markers.insert(v.to_string());
                    }
                }
            }
        }
    }
    markers.into_iter().filter(|m| m.len() >= 3).collect()
}

/// The `file_task` choke point: may this run file this prompt? Clean runs pass
/// (they hold no person-data to launder); tainted runs are scanned against
/// their own participants. Err carries the worker-facing reason.
pub fn check_task_prompt(context: &RunContext, prompt: &str) -> Result<(), String> {
    if !context.tainted() {
        return Ok(());
    }
    match scan(prompt, &participant_markers(context), true) {
        None => Ok(()),
        Some(hit) => Err(format!(
            "the task prompt references a conversation participant (\"{hit}\") — that belongs in memory, not in a work order; rephrase without naming people"
        )),
    }
}

/// Does `text` reference a participant or carry chat-mention syntax? Returns
/// what was matched, for a legible denial. `match_emails` is used at the
/// task-prompt choke point (conversation-derived text) but not on knowledge
/// records, where world content legitimately contains public addresses.
pub fn scan(text: &str, markers: &[String], match_emails: bool) -> Option<String> {
    let lower = text.to_lowercase();
    for marker in markers {
        if lower.contains(&marker.to_lowercase()) {
            return Some(marker.clone());
        }
    }
    if let Some(mention) = find_mention(text) {
        return Some(mention);
    }
    if match_emails {
        if let Some(email) = find_email(text) {
            return Some(email);
        }
    }
    None
}

/// Chat-mention syntax: `<@…>` (Discord user/Slack member mentions).
fn find_mention(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        let tail = &rest[start + 2..];
        if let Some(end) = tail.find('>') {
            let inner = &tail[..end];
            if !inner.is_empty()
                && inner.len() <= 24
                && inner
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'!' || b == b'&')
            {
                return Some(format!("<@{inner}>"));
            }
            rest = &tail[end + 1..];
        } else {
            break;
        }
    }
    None
}

/// A conservative email matcher: word@word.tld with alnum/._- parts.
fn find_email(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'@' {
            continue;
        }
        let is_part =
            |c: u8| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-' || c == b'+';
        let start = (0..i).rev().take_while(|&j| is_part(bytes[j])).last();
        let end = (i + 1..bytes.len())
            .take_while(|&j| is_part(bytes[j]))
            .last();
        if let (Some(s), Some(e)) = (start, end) {
            // A sentence-final '.' is pulled into `end` (it's a valid local/domain
            // char), which would make the domain end in '.' and be rejected —
            // letting "email lead@example.com." slip past the scan. Trim trailing
            // dots first so the common end-of-sentence case is still caught.
            let mut e = e;
            while e > i && bytes[e] == b'.' {
                e -= 1;
            }
            let candidate = &text[s..=e];
            let domain = &candidate[candidate.find('@').unwrap() + 1..];
            if domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_at_end_of_sentence_is_caught() {
        assert!(scan("Ask lead@example.com.", &[], true).is_some());
        assert!(scan("Ask lead@example.com!", &[], true).is_some());
        assert!(scan("Bare lead@example.com here", &[], true).is_some());
        assert!(scan("no address at all.", &[], true).is_none());
    }

    #[test]
    fn task_prompt_gate_denies_person_references_from_tainted_runs_only() {
        let clean = RunContext::default();
        let relay = RunContext {
            inbound: true,
            ..Default::default()
        };
        let session = RunContext {
            channel_id: Some("C123".into()),
            user_id: Some("U0AB12CD3".into()),
            ..Default::default()
        };
        let personal = "Ask lead-person@example.com to review the XDG summary.";
        let worldly = "Research the XDG base directory specification and record a summary.";

        // A tainted run cannot smuggle a person across the boundary...
        let err = check_task_prompt(&relay, personal).unwrap_err();
        assert!(
            err.contains("lead-person@example.com") && err.contains("belongs in memory"),
            "{err}"
        );
        assert!(check_task_prompt(&session, "summarize what U0AB12CD3 asked").is_err());
        assert!(check_task_prompt(&session, "reply to <@1521174547326566530>").is_err());

        // ...but world-shaped work orders cross freely, from any run.
        assert!(check_task_prompt(&relay, worldly).is_ok());
        assert!(check_task_prompt(&session, worldly).is_ok());

        // A clean run holds no person-data to launder: never gated.
        assert!(check_task_prompt(&clean, personal).is_ok());
    }

    #[test]
    fn scan_matches_participants_mentions_and_emails() {
        let markers = vec!["U0AB12CD3".to_string(), "manas".to_string()];

        assert_eq!(
            scan("ask U0AB12CD3 about it", &markers, false),
            Some("U0AB12CD3".into())
        );
        assert_eq!(
            scan("per Manas's request", &markers, false),
            Some("manas".into())
        );
        assert_eq!(
            scan("ping <@1521174547326566530> later", &[], false),
            Some("<@1521174547326566530>".into())
        );
        assert_eq!(
            scan("write to a-lead@example.com", &[], true),
            Some("a-lead@example.com".into())
        );

        // Emails pass when not matched (knowledge records: world content).
        assert_eq!(
            scan("contact sales@vendor.com for pricing", &[], false),
            None
        );
        // Clean world text passes everything.
        assert_eq!(
            scan("summarize the RFC and cite sources", &markers, true),
            None
        );
        // Short markers are never produced; scan tolerates empty marker sets.
        assert_eq!(scan("anything", &[], false), None);
    }
}
