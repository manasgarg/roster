// Vault bootstrap (orchestration side). The gateway (Rust) owns credential
// reading, refresh, and injection; this file only holds `vault-sync`, the
// one-time host-side command that seeds the vault from pi's auth. See D17 and
// gateway/src/vault.rs.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const VAULT_DIR = join(homedir(), ".roster", "vault");

/** Seed the vault from the host's pi auth (dev bootstrap). Returns the names
 * written. Overwrites — the host auth is the source of truth for the initial
 * load; after the gateway first refreshes a token, the vault is authoritative
 * and this should not be re-run (it would import a now-stale refresh token). */
export function syncFromPiAuth(): string[] {
  const src = join(homedir(), ".pi/agent/auth.json");
  if (!existsSync(src)) throw new Error(`no pi auth to sync from at ${src}`);
  mkdirSync(VAULT_DIR, { recursive: true });
  const auth = JSON.parse(readFileSync(src, "utf8")) as Record<string, unknown>;
  const written: string[] = [];
  for (const [name, cred] of Object.entries(auth)) {
    writeFileSync(join(VAULT_DIR, `${name}.json`), JSON.stringify(cred, null, 2) + "\n", { mode: 0o600 });
    written.push(name);
  }
  return written;
}
