import { test } from "node:test";
import assert from "node:assert/strict";
import { needsRefresh } from "../src/vault.ts";
import { mapTokenResponse, PROVIDERS } from "../src/providers.ts";
import type { Credential } from "../src/vault.ts";

const oauth = (expires: number): Credential => ({ type: "oauth", access: "a", refresh: "r", expires });

test("needsRefresh: expired token refreshes", () => {
  assert.equal(needsRefresh(oauth(Date.now() - 1000)), true);
});

test("needsRefresh: token far in the future does not", () => {
  assert.equal(needsRefresh(oauth(Date.now() + 3600_000)), false);
});

test("needsRefresh: within the skew window refreshes (avoid mid-flight lapse)", () => {
  assert.equal(needsRefresh(oauth(Date.now() + 30_000)), true); // < 60s skew
});

test("needsRefresh: a credential with no expiry never refreshes", () => {
  assert.equal(needsRefresh({ type: "oauth", access: "a", refresh: "r" }), false);
});

test("mapTokenResponse: maps fields and applies provider skew", () => {
  const before = Date.now();
  const out = mapTokenResponse(PROVIDERS["openai-codex"], {
    access_token: "new-access",
    refresh_token: "new-refresh",
    expires_in: 3600,
  });
  assert.equal(out.access, "new-access");
  assert.equal(out.refresh, "new-refresh");
  // openai-codex skew is 0 → expires ≈ now + 3600s
  assert.ok(out.expires >= before + 3600_000 && out.expires <= Date.now() + 3600_000);
});

test("mapTokenResponse: anthropic subtracts its 5-minute skew", () => {
  const before = Date.now();
  const out = mapTokenResponse(PROVIDERS["anthropic"], {
    access_token: "a",
    refresh_token: "r",
    expires_in: 3600,
  });
  assert.ok(out.expires <= Date.now() + 3600_000 - 5 * 60_000);
  assert.ok(out.expires >= before + 3600_000 - 5 * 60_000 - 5000);
});

test("mapTokenResponse: a malformed response throws (fail closed)", () => {
  assert.throws(() => mapTokenResponse(PROVIDERS["openai-codex"], { access_token: "a" }));
  assert.throws(() => mapTokenResponse(PROVIDERS["openai-codex"], { error: "invalid_grant" }));
});
