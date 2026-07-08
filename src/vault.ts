// The vault: the gateway's own credential store, host-side.
//
// Lives at ~/.roster/vault/ — outside the repo and outside the box mount, so
// the box never sees it. The gateway reads credentials from here to inject
// them in transit (src/gateway.ts); the box holds only sentinels. For now
// the store is plain JSON files seeded from the host's pi auth by
// `vault-sync`; a real secrets manager replaces the files later without
// moving the door. See docs/injection-spec.md.

import { appendFileSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { refreshOAuth } from "./providers.ts";

const VAULT_DIR = join(homedir(), ".roster", "vault");
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const CRED_LOG = join(repoRoot, "runs", "credentials.jsonl");
/** Refresh this long before the real expiry, so a token can't lapse mid-flight. */
const REFRESH_SKEW_MS = 60_000;

/** A stored credential. OAuth today; api-key shapes later. Kept raw so the
 * injector renders headers at call time (and refresh can update `access`). */
export interface Credential {
  type: string;
  access?: string;
  refresh?: string;
  expires?: number;
  accountId?: string;
  [k: string]: unknown;
}

export function getCredential(name: string): Credential | null {
  const path = join(VAULT_DIR, `${name}.json`);
  if (!existsSync(path)) return null;
  return JSON.parse(readFileSync(path, "utf8")) as Credential;
}

/** Seed the vault from the host's pi auth (dev path). Returns the names
 * written. Overwrites — the host auth is the source of truth for now. */
export function syncFromPiAuth(): string[] {
  const src = join(homedir(), ".pi/agent/auth.json");
  if (!existsSync(src)) throw new Error(`no pi auth to sync from at ${src}`);
  mkdirSync(VAULT_DIR, { recursive: true });
  const auth = JSON.parse(readFileSync(src, "utf8")) as Record<string, Credential>;
  const written: string[] = [];
  for (const [name, cred] of Object.entries(auth)) {
    writeFileSync(join(VAULT_DIR, `${name}.json`), JSON.stringify(cred, null, 2) + "\n", { mode: 0o600 });
    written.push(name);
  }
  return written;
}

export function vaultNames(): string[] {
  if (!existsSync(VAULT_DIR)) return [];
  return readdirSync(VAULT_DIR).filter((f) => f.endsWith(".json")).map((f) => f.slice(0, -5));
}

/** Is an OAuth credential expired (or within the refresh skew window)? */
export function needsRefresh(cred: Credential, now: number = Date.now()): boolean {
  return typeof cred.expires === "number" && now >= cred.expires - REFRESH_SKEW_MS;
}

/** Atomic vault write: a crash mid-write must never leave a half-rotated
 * credential (that would lock us out — the old refresh token is already spent). */
function writeCredential(name: string, cred: Credential): void {
  mkdirSync(VAULT_DIR, { recursive: true });
  const path = join(VAULT_DIR, `${name}.json`);
  const tmp = `${path}.tmp`;
  writeFileSync(tmp, JSON.stringify(cred, null, 2) + "\n", { mode: 0o600 });
  renameSync(tmp, path);
}

function logRefresh(event: Record<string, unknown>): void {
  try {
    mkdirSync(dirname(CRED_LOG), { recursive: true });
    appendFileSync(CRED_LOG, JSON.stringify({ ts: new Date().toISOString(), ...event }) + "\n");
  } catch {
    // audit best-effort; never block a request on the log
  }
}

// One refresh lane per credential: concurrent callers hitting an expired token
// share a single refresh, so the second doesn't fail on the token the first
// already rotated ("one writer per surface").
const inflight = new Map<string, Promise<Credential | null>>();

/** Get a credential guaranteed usable now: refreshes (and persists) if the
 * OAuth token has expired. Returns null if the credential isn't in the vault.
 * Throws if a needed refresh fails — the gateway must then deny, never inject
 * a stale token. */
export function getFreshCredential(name: string): Promise<Credential | null> {
  const existing = inflight.get(name);
  if (existing) return existing;
  const p = refreshIfNeeded(name);
  inflight.set(name, p);
  p.finally(() => {
    if (inflight.get(name) === p) inflight.delete(name);
  });
  return p;
}

async function refreshIfNeeded(name: string): Promise<Credential | null> {
  const cred = getCredential(name);
  if (cred === null) return null;
  if (cred.type !== "oauth" || typeof cred.refresh !== "string" || !needsRefresh(cred)) return cred;
  try {
    const fresh = await refreshOAuth(name, cred.refresh);
    // Merge so account id / type survive (refresh returns only access/refresh/expires).
    const merged: Credential = { ...cred, ...fresh };
    writeCredential(name, merged);
    logRefresh({ event: "refresh", credential: name, ok: true, expires: merged.expires });
    return merged;
  } catch (err) {
    logRefresh({ event: "refresh", credential: name, ok: false, error: err instanceof Error ? err.message : String(err) });
    throw err;
  }
}
