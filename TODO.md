# To-do

## CLI simplification

- [ ] **Make `init` implicit and automatic.** No first-run ceremony: any
  command that needs the config/data/state roots creates them on demand
  (the creation is already idempotent), and the starter `org.toml` is
  written the first time config is read and found absent. `roster init`
  either disappears or stays as a harmless explicit form. First-run
  becomes: install → `roster worker init yuko` → go.

- [ ] **Bootstrap LLM credentials on launch.** When roster starts and the
  vault holds no LLM-provider credential:
  1. If a host pi login exists (`~/.pi/agent/auth.json`), **ask the user
     to confirm** before importing it (→ vault entries for openai-codex /
     anthropic) — "found a pi login for openai-codex; use it for roster?
     [y/N]". Never import silently.
  2. If there is no pi login to import (or the user declines), walk the
     user through the provider login right there — ask which provider
     (Anthropic or OpenAI) and run the existing `credential add` flow
     (PKCE / device code) inline.
  3. On a non-interactive launch (daemon under systemd), do neither —
     print the hint and skip.

  One wrinkle to decide for the import path: after import, roster's
  gateway-owned refresh rotates the token, and pi's own copy can fall out
  of sync — either import-and-own (pi re-logs-in when it next needs to) or
  re-import on expiry; pick one and say so in the docs.

- [x] **`roster talk` — the terminal as a first-class channel.** *(shipped)*
  A chat command with the Discord/Slack interaction model, not a bare REPL:
  the terminal becomes a third channel platform, reusing the
  channel-id-keyed machinery — recorded history under `data/channels/<id>/`,
  a purpose, channel + user memory scopes, warm-session turns. Trusted like
  a DM (it's the operator's own shell); replies print directly in the
  terminal. Existing `worker chat` stays as the bare-REPL test harness.
