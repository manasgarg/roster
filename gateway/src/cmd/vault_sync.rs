//! `roster vault-sync` — import an existing pi login into the vault (the
//! shortcut alternative to `roster connect`).

use serde_json::Value;
use std::fs;
use std::path::Path;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let src = Path::new(&home).join(".pi/agent/auth.json");
    let text = fs::read_to_string(&src).map_err(|_| format!("no pi auth to sync from at {}", src.display()))?;
    let auth: Value = serde_json::from_str(&text)?;
    let entries = auth.as_object().ok_or("pi auth.json is not an object")?;

    let vault = Path::new(&home).join(".roster/vault");
    fs::create_dir_all(&vault)?;
    let mut names = Vec::new();
    for (name, cred) in entries {
        let path = vault.join(format!("{name}.json"));
        fs::write(&path, format!("{}\n", serde_json::to_string_pretty(cred)?))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        names.push(name.clone());
    }
    println!("vault synced from pi auth: {}", names.join(", "));
    println!("(stored at ~/.roster/vault — off the box mount; the gateway injects these in transit)");
    Ok(())
}
