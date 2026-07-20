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

fn safe_component(value: &str) -> bool {
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
    crate::channel::discord::channel_meta(surface_id)
        .and_then(|m| m.get("class").and_then(|v| v.as_str()).map(String::from))
        .is_some_and(|c| c == "dm")
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
    for sid in surface_ids {
        if !members.iter().any(|m| m == sid) {
            members.push(sid.clone());
        }
    }
    save(&reg)
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
        link("manas", &["term-manas-dobby".into(), "900".into()]).unwrap();
        assert_eq!(logical_of("900"), "manas");
        assert_eq!(logical_of("term-manas-dobby"), "manas");
        assert_eq!(surfaces_of("manas"), vec!["term-manas-dobby", "900"]);

        // A group-shaped surface is refused; so is double-linking.
        crate::channel::discord::write_channel_meta(
            "800",
            &serde_json::json!({ "platform": "discord", "class": "public" }),
        );
        assert!(link("manas", &["800".into()]).unwrap_err().contains("1:1"));
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
