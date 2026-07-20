//! Logical channels — the link registry (docs/plans/channel-semantics.md).
//! A channel is a group of surfaces experienced as one conversation; every
//! surface is implicitly its own singleton channel until an operator links
//! it, so this file holds only the exceptions. v1 links 1:1-shaped surfaces
//! only (DM-class and terminal), which makes uniform trust a theorem rather
//! than a rule to enforce. Linking merges conversation, never authority.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Registry {
    /// logical channel name -> its member surface ids, in link order.
    channels: BTreeMap<String, Vec<String>>,
}

pub(crate) fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
}

fn registry_path() -> PathBuf {
    crate::paths::channels_dir().join("links.json")
}

fn load() -> Registry {
    match crate::statefile::read_if_present(&registry_path()) {
        Ok(Some(s)) => serde_json::from_str(&s).unwrap_or_default(),
        _ => Registry::default(),
    }
}

fn save(reg: &Registry) -> Result<(), String> {
    let path = registry_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(reg).map_err(|e| e.to_string())?
    );
    crate::statefile::write_atomic(&path, body.as_bytes()).map_err(|e| e.to_string())
}

/// The logical channel a surface belongs to. Identity for every surface no
/// link names — the singleton default that makes the current deployment the
/// degenerate case of the model.
pub fn logical_of(surface_id: &str) -> String {
    let reg = load();
    for (name, members) in &reg.channels {
        if members.iter().any(|m| m == surface_id) {
            return name.clone();
        }
    }
    surface_id.to_string()
}

/// A channel's member surfaces: the linked set, or the surface itself
/// (singleton). Order is link order — the first member is the channel's
/// original surface.
pub fn surfaces_of(logical: &str) -> Vec<String> {
    let reg = load();
    reg.channels
        .get(logical)
        .cloned()
        .unwrap_or_else(|| vec![logical.to_string()])
}

/// Is this surface 1:1-shaped — a DM-class surface or a terminal channel?
/// v1 links only these: group rooms have audiences, and merging audiences
/// leaks by construction.
fn one_to_one(surface_id: &str) -> bool {
    if surface_id.starts_with("term-") {
        return true;
    }
    let Some(meta) = crate::channel::discord::channel_meta(surface_id) else {
        return false;
    };
    match meta.get("class").and_then(|v| v.as_str()) {
        Some(class) => class == "dm",
        // Meta from before surfaces were classified: only the DM path ever
        // wrote meta without a server; guild channels always carry one. The
        // listener rewrites the meta with a class on the next message, so
        // this reading serves only records it hasn't touched since.
        None => meta.get("server").is_none(),
    }
}

/// The linked channel this id appears in — as the channel's own name or as
/// a member surface. None for ids the registry doesn't mention (singletons).
pub fn in_registry(id: &str) -> Option<String> {
    let reg = load();
    if reg.channels.contains_key(id) {
        return Some(id.to_string());
    }
    reg.channels
        .iter()
        .find(|(_, members)| members.iter().any(|m| m == id))
        .map(|(name, _)| name.clone())
}

/// Link surfaces into one logical channel, creating it or extending it.
/// The operator's act (CLI, authenticated): the link merges conversation,
/// never authority. Fails closed on group-shaped surfaces, on a name that
/// collides with an existing surface's own id, and on surfaces already
/// linked elsewhere.
pub fn link(name: &str, surface_ids: &[String]) -> Result<(), String> {
    if !safe_component(name) {
        return Err("channel name must be a safe path component".into());
    }
    if surface_ids.is_empty() {
        return Err("link needs at least one surface id".into());
    }
    let mut reg = load();
    // The name must not shadow a real surface's singleton channel (unless
    // it names the group one of its own members belongs to).
    if !reg.channels.contains_key(name)
        && crate::paths::channel_meta_file(name).exists()
        && !surface_ids.iter().any(|s| s == name)
    {
        return Err(format!(
            "\"{name}\" is an existing surface id — pick a fresh name for the linked channel"
        ));
    }
    for sid in surface_ids {
        if !safe_component(sid) {
            return Err(format!("\"{sid}\" is not a safe surface id"));
        }
        if !one_to_one(sid) {
            return Err(format!(
                "\"{sid}\" is not a 1:1 surface — v1 links DM-class and terminal surfaces only"
            ));
        }
        for (other, members) in &reg.channels {
            if other != name && members.iter().any(|m| m == sid) {
                return Err(format!(
                    "\"{sid}\" is already linked into channel \"{other}\" — unlink it first"
                ));
            }
        }
    }
    let members = reg.channels.entry(name.to_string()).or_default();
    let fresh: Vec<String> = surface_ids
        .iter()
        .filter(|sid| !members.iter().any(|m| m == *sid))
        .cloned()
        .collect();
    members.extend(fresh.iter().cloned());
    save(&reg)?;
    // One-time merge: each newly linked surface's conversation material
    // (history, shared files, purpose) folds into the channel's dir, and its
    // settings entry is absorbed. The surface's own dir keeps host
    // bookkeeping (meta, replay cursors) plus a marker where its history
    // was, so a re-link never double-merges.
    for sid in &fresh {
        merge_surface_material(name, sid);
    }
    crate::channel::discord::absorb_settings(name, &fresh)
}

/// Fold one surface's conversation material into the logical channel's dir.
/// Interleaves history by `ts` (records carry the platform's own send
/// time), moves shared files, and seeds the channel's purpose from the
/// first member that has one. Best-effort by design: a partial merge loses
/// no source data (originals are renamed, never deleted).
fn merge_surface_material(name: &str, sid: &str) {
    if sid == name {
        return;
    }
    let src_dir = crate::paths::channel_dir(sid);
    let dst_dir = crate::paths::channel_dir(name);
    let _ = std::fs::create_dir_all(&dst_dir);

    let src_msgs = src_dir.join("messages.jsonl");
    let dst_msgs = dst_dir.join("messages.jsonl");
    if src_msgs.is_file() {
        let mut records: Vec<(String, String)> = Vec::new();
        for path in [&dst_msgs, &src_msgs] {
            for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
                let ts = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("ts").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_default();
                records.push((ts, line.to_string()));
            }
        }
        // Stable by timestamp: RFC 3339 sorts lexicographically, and equal
        // stamps keep their original relative order.
        records.sort_by(|a, b| a.0.cmp(&b.0));
        let body: String = records.into_iter().map(|(_, l)| l + "\n").collect();
        if crate::statefile::write_atomic(&dst_msgs, body.as_bytes()).is_ok() {
            let _ = std::fs::rename(&src_msgs, src_dir.join("messages.jsonl.linked"));
        }
    }

    let src_files = src_dir.join("files");
    if src_files.is_dir() {
        let dst_files = dst_dir.join("files");
        let _ = std::fs::create_dir_all(&dst_files);
        for entry in std::fs::read_dir(&src_files)
            .into_iter()
            .flatten()
            .flatten()
        {
            let to = dst_files.join(entry.file_name());
            if !to.exists() {
                let _ = std::fs::rename(entry.path(), to);
            }
        }
        let _ = std::fs::remove_dir(&src_files);
    }

    let src_purpose = src_dir.join("purpose.md");
    let dst_purpose = dst_dir.join("purpose.md");
    if src_purpose.is_file() && !dst_purpose.exists() {
        let _ = std::fs::rename(&src_purpose, &dst_purpose);
    }
}

/// Remove a surface from its linked channel. The channel (and its history
/// and store) stays with the remaining members — nothing is un-shared; the
/// departing surface starts over as a fresh singleton. A group left with
/// one member dissolves back to a singleton entry-free state.
pub fn unlink(surface_id: &str) -> Result<String, String> {
    let mut reg = load();
    let Some((name, members)) = reg
        .channels
        .iter_mut()
        .find(|(_, m)| m.iter().any(|s| s == surface_id))
        .map(|(n, m)| (n.clone(), m))
    else {
        return Err(format!("\"{surface_id}\" is not linked into any channel"));
    };
    members.retain(|m| m != surface_id);
    if members.len() <= 1 {
        reg.channels.remove(&name);
    }
    save(&reg)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_resolve_unlink_lifecycle() {
        let _guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());

        // Unlinked surfaces are their own singleton channels.
        assert_eq!(logical_of("term-manas-dobby"), "term-manas-dobby");
        assert_eq!(surfaces_of("term-manas-dobby"), vec!["term-manas-dobby"]);

        // A DM surface (classified by the listener) links with a terminal.
        crate::channel::discord::write_channel_meta(
            "900",
            &serde_json::json!({ "platform": "discord", "class": "dm" }),
        );
        // Pre-link history on both surfaces, out of order across them.
        crate::channel::discord::persist_message(
            "term-manas-dobby",
            &serde_json::json!({ "ts": "2026-07-01T10:00:00Z", "content": "first" }),
        );
        crate::channel::discord::persist_message(
            "900",
            &serde_json::json!({ "ts": "2026-07-01T09:00:00Z", "content": "earlier" }),
        );
        link("manas", &["term-manas-dobby".into(), "900".into()]).unwrap();
        assert_eq!(logical_of("900"), "manas");
        assert_eq!(logical_of("term-manas-dobby"), "manas");
        assert_eq!(surfaces_of("manas"), vec!["term-manas-dobby", "900"]);

        // Histories interleaved by timestamp under the channel; originals
        // are markers now, and post-link traffic on either surface lands in
        // the channel's file. The channel is trusted (1:1 members only).
        let merged =
            std::fs::read_to_string(crate::paths::channel_dir("manas").join("messages.jsonl"))
                .unwrap();
        let posts: Vec<&str> = merged.lines().collect();
        assert!(
            posts[0].contains("earlier") && posts[1].contains("first"),
            "{merged}"
        );
        assert!(crate::paths::channel_dir("900")
            .join("messages.jsonl.linked")
            .is_file());
        crate::channel::discord::persist_message(
            "900",
            &serde_json::json!({ "ts": "2026-07-01T11:00:00Z", "content": "after" }),
        );
        let merged =
            std::fs::read_to_string(crate::paths::channel_dir("manas").join("messages.jsonl"))
                .unwrap();
        assert_eq!(merged.lines().count(), 3);
        assert!(crate::channel::discord::channel_trusted("900"));
        assert!(crate::channel::discord::channel_trusted("manas"));

        // A group-shaped surface is refused; so is double-linking.
        crate::channel::discord::write_channel_meta(
            "800",
            &serde_json::json!({ "platform": "discord", "class": "public" }),
        );
        assert!(link("manas", &["800".into()]).unwrap_err().contains("1:1"));

        // Legacy meta (pre-class builds): a DM's record has no server and
        // links; a guild channel's has one and is refused.
        crate::channel::discord::write_channel_meta(
            "700",
            &serde_json::json!({ "platform": "discord", "name": "DM with jane" }),
        );
        crate::channel::discord::write_channel_meta(
            "600",
            &serde_json::json!({ "platform": "discord", "server": "acme", "name": "general" }),
        );
        link("manas", &["700".into()]).unwrap();
        assert_eq!(logical_of("700"), "manas");
        assert!(link("manas", &["600".into()]).unwrap_err().contains("1:1"));
        assert_eq!(unlink("700").unwrap(), "manas");
        assert!(link("other", &["900".into()])
            .unwrap_err()
            .contains("already linked"));

        // Unlink: the group survives while >1 member remains, then dissolves.
        assert_eq!(unlink("900").unwrap(), "manas");
        assert_eq!(logical_of("900"), "900");
        assert_eq!(logical_of("term-manas-dobby"), "term-manas-dobby");
        assert!(unlink("900").is_err());

        std::env::remove_var("ROSTER_ROOT");
    }
}
