// Owned OAuth refresh. The gateway refreshes provider tokens itself — no
// dependency on pi's code in the credential path. Each provider needs only a
// few public constants (token endpoint, client id, body encoding) plus a
// standard `refresh_token` grant. The constants were lifted once from each
// provider's own published OAuth client; they're public, not secret.
// See docs/injection-spec.md.

export interface ProviderConfig {
  tokenUrl: string;
  clientId: string;
  encoding: "form" | "json";
  /** Subtracted from the computed expiry — matches what each provider's own
   * client bakes in (refresh a little before the real expiry). */
  skewMs: number;
}

export const PROVIDERS: Record<string, ProviderConfig> = {
  "openai-codex": {
    tokenUrl: "https://auth.openai.com/oauth/token",
    clientId: "app_EMoamEEZ73f0CkXaXp7hrann",
    encoding: "form",
    skewMs: 0,
  },
  anthropic: {
    tokenUrl: "https://platform.claude.com/v1/oauth/token",
    clientId: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    encoding: "json",
    skewMs: 5 * 60 * 1000,
  },
};

export interface RefreshedTokens {
  access: string;
  refresh: string;
  expires: number;
}

/** Map a provider token-endpoint response to our credential fields. Pure
 * (aside from Date.now for the absolute expiry). Throws on a malformed
 * response so the caller fails closed. */
export function mapTokenResponse(cfg: ProviderConfig, json: unknown): RefreshedTokens {
  const j = json as { access_token?: unknown; refresh_token?: unknown; expires_in?: unknown };
  if (typeof j?.access_token !== "string" || typeof j?.refresh_token !== "string" || typeof j?.expires_in !== "number") {
    throw new Error(`token response missing fields: ${JSON.stringify(json)}`);
  }
  return {
    access: j.access_token,
    refresh: j.refresh_token,
    expires: Date.now() + j.expires_in * 1000 - cfg.skewMs,
  };
}

/** Perform the OAuth `refresh_token` grant for a provider. A direct host-side
 * call (the gateway's own trusted action, not the box's egress). Throws on any
 * failure — the caller must fail closed, never inject a stale token. */
export async function refreshOAuth(name: string, refreshToken: string): Promise<RefreshedTokens> {
  const cfg = PROVIDERS[name];
  if (!cfg) throw new Error(`no refresh config for provider "${name}"`);
  const params = { grant_type: "refresh_token", refresh_token: refreshToken, client_id: cfg.clientId };
  const [body, contentType] =
    cfg.encoding === "form"
      ? [new URLSearchParams(params).toString(), "application/x-www-form-urlencoded"]
      : [JSON.stringify(params), "application/json"];

  let res: Response;
  try {
    res = await fetch(cfg.tokenUrl, { method: "POST", headers: { "content-type": contentType }, body });
  } catch (err) {
    throw new Error(`refresh request to ${cfg.tokenUrl} failed: ${err instanceof Error ? err.message : String(err)}`);
  }
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`refresh for "${name}" returned ${res.status}: ${text.slice(0, 200)}`);
  }
  let json: unknown;
  try {
    json = await res.json();
  } catch {
    throw new Error(`refresh for "${name}" returned non-JSON`);
  }
  return mapTokenResponse(cfg, json);
}
