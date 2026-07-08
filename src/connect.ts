// `connect` — create a credential in the vault (the gateway's own login flow,
// the OneCLI-style "connect a service" step done our own way). Generalized over
// a provider registry (providers.json): api-key providers just take a key; OAuth
// providers run device-code or PKCE. The result lands in ~/.roster/vault/<id>.json
// and the gateway keeps it alive by refreshing. See docs/injection-spec.md.

import { createHash, randomBytes } from "node:crypto";
import { createServer } from "node:http";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { createInterface } from "node:readline/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const VAULT_DIR = join(homedir(), ".roster", "vault");

type Dict = Record<string, unknown>;

function registry(): Record<string, Dict> {
  const path = join(repoRoot, "providers.json");
  if (!existsSync(path)) throw new Error(`no provider registry at ${path}`);
  return JSON.parse(readFileSync(path, "utf8")) as Record<string, Dict>;
}

function writeCredential(name: string, cred: Dict): void {
  mkdirSync(VAULT_DIR, { recursive: true });
  writeFileSync(join(VAULT_DIR, `${name}.json`), JSON.stringify(cred, null, 2) + "\n", { mode: 0o600 });
}

async function ask(question: string): Promise<string> {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    return (await rl.question(question)).trim();
  } finally {
    rl.close();
  }
}

/** Run the login flow for `name` and store the resulting credential. */
export async function connect(name: string): Promise<void> {
  const provider = registry()[name];
  if (!provider) throw new Error(`unknown provider "${name}" (not in providers.json)`);

  let cred: Dict;
  if (provider.auth === "api_key") {
    cred = await connectApiKey();
  } else if (provider.auth === "oauth") {
    const login = (provider.login as Dict | undefined) ?? {};
    if (login.flow === "device_code") cred = await connectDeviceCode(name, provider, login);
    else if (login.flow === "pkce") cred = await connectPkce(provider, login);
    else throw new Error(`provider "${name}": unknown oauth flow "${String(login.flow)}"`);
  } else {
    throw new Error(`provider "${name}": unknown auth "${String(provider.auth)}"`);
  }

  writeCredential(name, cred);
  console.log(`\nconnected: credential for "${name}" written to the vault`);
}

// ── api-key ──────────────────────────────────────────────────────────────────

async function connectApiKey(): Promise<Dict> {
  const key = await ask("paste the API key: ");
  if (!key) throw new Error("no key entered");
  return { type: "api_key", key };
}

// ── OAuth device-code (e.g. openai-codex) ────────────────────────────────────

async function connectDeviceCode(name: string, provider: Dict, login: Dict): Promise<Dict> {
  const clientId = provider.client_id as string;

  const start = await postJson(login.device_authorization_url as string, { client_id: clientId });
  const deviceAuthId = start.device_auth_id as string;
  const userCode = start.user_code as string;
  const interval = Number(start.interval ?? 5);
  if (!deviceAuthId || !userCode) throw new Error(`device-code start failed: ${JSON.stringify(start)}`);

  console.log(`\n  1. open: ${login.verification_url}`);
  console.log(`  2. enter code: ${userCode}\n`);
  console.log("waiting for you to authorize…");

  const deadline = Date.now() + 15 * 60_000;
  let wait = interval;
  let authCode = "", verifier = "";
  while (Date.now() < deadline) {
    await sleep(wait * 1000);
    const res = await fetch(login.device_token_url as string, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ device_auth_id: deviceAuthId, user_code: userCode }),
    });
    if (res.ok) {
      const j = (await res.json()) as Dict;
      if (j.authorization_code && j.code_verifier) {
        authCode = j.authorization_code as string;
        verifier = j.code_verifier as string;
        break;
      }
    } else if (res.status === 403 || res.status === 404) {
      continue; // still pending
    } else {
      const text = await res.text().catch(() => "");
      const code = safeErrorCode(text);
      if (code === "deviceauth_authorization_pending") continue;
      if (code === "slow_down") {
        wait += 2;
        continue;
      }
      throw new Error(`device-code poll failed (${res.status}): ${text}`);
    }
  }
  if (!authCode) throw new Error("device-code login timed out");

  // Exchange the authorization code for tokens (form-encoded).
  const tok = await postForm(provider.token_url as string, {
    grant_type: "authorization_code",
    client_id: clientId,
    code: authCode,
    code_verifier: verifier,
    redirect_uri: login.exchange_redirect_uri as string,
  });
  return oauthCred(provider, tok);
}

// ── OAuth PKCE (e.g. anthropic) ──────────────────────────────────────────────

async function connectPkce(provider: Dict, login: Dict): Promise<Dict> {
  const clientId = provider.client_id as string;
  const redirectUri = login.redirect_uri as string;
  const verifier = base64url(randomBytes(32));
  const challenge = base64url(createHash("sha256").update(verifier).digest());
  const state = base64url(randomBytes(16));

  const url = new URL(login.authorize_url as string);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", clientId);
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("scope", login.scope as string);
  url.searchParams.set("code_challenge", challenge);
  url.searchParams.set("code_challenge_method", "S256");
  url.searchParams.set("state", state);

  console.log(`\n  open this URL in a browser and authorize:\n\n  ${url}\n`);

  // Capture the redirect on a local callback server; fall back to manual paste.
  const port = Number(login.callback_port ?? 0);
  const captured = port ? await captureCallback(port).catch(() => null) : null;
  let code: string, gotState: string;
  if (captured) {
    ({ code, state: gotState } = captured);
  } else {
    const pasted = await ask("paste the redirect URL (or the code) you land on: ");
    ({ code, state: gotState } = parseCallback(pasted, state));
  }
  if (!code) throw new Error("no authorization code captured");
  if (gotState && gotState !== state) throw new Error("state mismatch — aborting");

  const tok = await postJson(provider.token_url as string, {
    grant_type: "authorization_code",
    client_id: clientId,
    code,
    state,
    redirect_uri: redirectUri,
    code_verifier: verifier,
  });
  return oauthCred(provider, tok);
}

function captureCallback(port: number): Promise<{ code: string; state: string }> {
  return new Promise((resolve, reject) => {
    const server = createServer((req, res) => {
      const u = new URL(req.url ?? "/", `http://localhost:${port}`);
      const code = u.searchParams.get("code");
      const state = u.searchParams.get("state") ?? "";
      res.writeHead(200, { "content-type": "text/html" });
      res.end("<h3>Roster: you can close this tab.</h3>");
      server.close();
      if (code) resolve({ code, state });
      else reject(new Error("callback had no code"));
    });
    server.on("error", reject);
    server.listen(port, "127.0.0.1");
    setTimeout(() => {
      server.close();
      reject(new Error("callback timed out"));
    }, 15 * 60_000);
  });
}

function parseCallback(value: string, _state: string): { code: string; state: string } {
  try {
    const u = new URL(value);
    return { code: u.searchParams.get("code") ?? "", state: u.searchParams.get("state") ?? "" };
  } catch {
    // bare code, or code#state
    if (value.includes("#")) {
      const [code, state] = value.split("#", 2);
      return { code, state };
    }
    return { code: value, state: "" };
  }
}

// ── shared ───────────────────────────────────────────────────────────────────

/** Shape an OAuth token response into a vault credential, extracting accountId
 * from the access-token JWT when the provider declares a claim path. */
function oauthCred(provider: Dict, tok: Dict): Dict {
  if (!tok.access_token || !tok.refresh_token || typeof tok.expires_in !== "number") {
    throw new Error(`token response missing fields: ${JSON.stringify(tok)}`);
  }
  const skew = Number(provider.skew_ms ?? 0);
  const cred: Dict = {
    type: "oauth",
    access: tok.access_token,
    refresh: tok.refresh_token,
    expires: Date.now() + (tok.expires_in as number) * 1000 - skew,
  };
  const claimPath = provider.account_id_claim as string[] | undefined;
  if (claimPath) {
    const accountId = jwtClaim(tok.access_token as string, claimPath);
    if (accountId) cred.accountId = accountId;
  }
  return cred;
}

function jwtClaim(jwt: string, path: string[]): string | undefined {
  try {
    const payload = JSON.parse(Buffer.from(jwt.split(".")[1], "base64url").toString("utf8")) as Dict;
    let node: unknown = payload;
    for (const key of path) node = (node as Dict)?.[key];
    return typeof node === "string" ? node : undefined;
  } catch {
    return undefined;
  }
}

async function postJson(url: string, body: Dict): Promise<Dict> {
  const res = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  const text = await res.text();
  if (!res.ok) throw new Error(`POST ${url} → ${res.status}: ${text.slice(0, 300)}`);
  return JSON.parse(text) as Dict;
}

async function postForm(url: string, body: Record<string, string>): Promise<Dict> {
  const res = await fetch(url, { method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(body).toString() });
  const text = await res.text();
  if (!res.ok) throw new Error(`POST ${url} → ${res.status}: ${text.slice(0, 300)}`);
  return JSON.parse(text) as Dict;
}

function base64url(b: Buffer): string {
  return b.toString("base64url");
}

function safeErrorCode(text: string): string | undefined {
  try {
    const j = JSON.parse(text) as { error?: unknown };
    const e = j.error;
    return typeof e === "object" && e !== null ? (e as { code?: string }).code : (e as string | undefined);
  } catch {
    return undefined;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
