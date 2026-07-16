# Plan: configurable box agent — pi | claude | codex

Status: WP0 spike complete (green, 2026-07-17) — implementation not started
Scope: let a worker's one-shot task runs execute under Claude Code (`claude`) or
OpenAI Codex (`codex`) instead of pi, selected per worker in config. Warm
conversation sessions stay pi.

## Goal

```
agent = "claude"        # worker.toml — this worker's task runs use Claude Code
```

with an org-wide default:

```
[engine]
agent = "codex"         # org.toml — default for workers that don't say
```

Everything else — the governed door, sentinel credentials, action gates, task
attestation — behaves identically regardless of agent.

## Non-goals

- Warm sessions (`run_session`, `src/run/boxed.rs:387`) for non-pi agents. The
  stdin RPC protocol, `agent_end` turn tracking, and `assistant_text()` are
  pi-specific and conversations weren't asked for. Sessions always run pi.
- Per-task (as opposed to per-worker) agent selection. Can be layered on later;
  the plumbing below keys off `RunSpec`, so it wouldn't be invasive.
- Arbitrary/bring-your-own agents. The value is an enum, not a command string —
  a free-form command would bypass everything provisioning guarantees.

## What couples the box to pi today

| Coupling | Where | Severity for one-shot runs |
|---|---|---|
| Spawn command | `pi_prefix()` `src/run/boxed.rs:607`; call sites `:310` (one-shot), `:445` (session) | must generalize |
| CLI flags (`--mode`, `--session-dir`, `--session-id`, `--append-system-prompt`) | `boxed.rs:310-323, 607-632, 662` | per-agent mapping |
| Sentinel auth shape (`/pihome/agent/auth.json`) | `prepare_pihome()` `boxed.rs:1254` | per-agent writer |
| Env wiring (`PI_CODING_AGENT_DIR`) | `boxed.rs:869-873`; `RESERVED_ENV` `src/config.rs:661` | add claude/codex homes |
| Tool surface (pi extensions → gateway action host) | `box/extensions/actions.ts`, `web.ts` | port to MCP (WP5) |
| Credential availability probe | `model_credentials_available()` `boxed.rs:224` | per-agent |
| Image contents (only pi baked) | `box/Dockerfile:56-64` | bake CLIs |
| Output protocol parsing | sessions only (`assistant_text()`); one-shot stdout is streamed unparsed to `stdout.jsonl` (`boxed.rs:339-357`) | none |

What is already agent-agnostic — verified, not assumed:

- **Credential injection.** The gateway *overwrites* auth headers in transit
  (`src/gateway/proxy.rs:695-715` drops the box's header, applies the rendered
  inject). The box's sentinel value never has to be acceptable to the API —
  only to the CLI's own "am I logged in" check.
- **The vault credentials are already the right kind.** The `anthropic`
  registry entry (`src/credential/providers.default.json`) is the Claude Code
  OAuth client (scope includes `user:sessions:claude_code`); `openai-codex` is
  the codex CLI OAuth client. `roster connection add anthropic` mints exactly
  the token claude needs; ditto codex.
- **Egress grants.** Model connections compile to grants on `model_hosts`
  (`api.anthropic.com`, `chatgpt.com`) — the hosts claude/codex actually call.
- **Task attestation.** `finalize()` (`src/work/dispatch.rs:258`) keys on host
  evidence (exit code, ceiling, pending gates, gateway refusal count) plus the
  `/outcome` report POSTed to the action host — none of it parses pi output.
  A clean exit with no report and no refusals attests completed
  (`dispatch.rs:307`), which makes a tool-less milestone shippable.
- **Briefing text.** `src/worker/context.rs` names tools (`task_complete`,
  `message_user`, `set_tasks`, …) but not the agent — identical MCP tool names
  keep every compiled prompt working unchanged.

## Design decisions

1. **Selection is per-worker** (`agent = "…"` in worker.toml, top level), with
   `[engine] agent` as the org default and `"pi"` as the built-in default.
   Tasks belong to workers; heartbeat runs are queued tasks, so they follow the
   worker's agent too (see WP5 note).
2. **Enum, fail-closed.** `pi | claude | codex`. Unknown value = validation
   error naming the allowed set (style of `config.rs:333`).
3. **Sessions pin pi.** A worker with `[channels]` and a non-pi agent gets a
   *warning* (sessions run pi), not an error.
4. **One image, all agents baked.** Same philosophy as the toolbelt
   (`box/Dockerfile:2`): binaries add no egress power on the NAT-less network.
   CLIs are npm-pinned in `package.json` so versions ride the lockfile, like pi.
5. **`run-claude` / `run-codex` wrapper scripts in the image**, mirroring
   `run-pi`: the wrapper owns agent-specific fixed flags (output format, MCP
   registration, permission bypass), the host stays image-agnostic.
6. **Same sentinel scheme.** Write believable-but-worthless credentials in each
   CLI's native format; far-future expiry so the CLI never attempts a refresh;
   gateway injects the real credential in transit. No secret enters the box.
7. **One tool surface via MCP.** The pi extensions become thin wrappers over a
   shared tool module also served by a stdio MCP server. Same names, same
   `submit()` envelope to `https://actions.roster.internal/*`, same governance.
8. **Codex system prompt: prepend to the input prompt.** Codex has no
   `--append-system-prompt`; writing AGENTS.md into the cwd would dirty code
   worktrees that `propose_changes` later commits. Prepending with a clear
   delimiter is safe and reversible; revisit if codex stabilizes an
   instructions flag.

---

## WP0 — Spike: prove sentinel auth end-to-end (do this first)

No roster code changes. Build a scratch image with both CLIs, start a box with
the exact provisioning roster uses (`[engine] image` + a manual `docker run`
copying the args from a real run), hand-write sentinel files, run one prompt
per agent through the gateway.

Answers we need out of it:

- Exact on-disk credential shape per pinned CLI version:
  claude `$CLAUDE_CONFIG_DIR/.credentials.json` (`claudeAiOauth: {accessToken,
  refreshToken, expiresAt, scopes, …}` — confirm) and codex
  `$CODEX_HOME/auth.json` (`{OPENAI_API_KEY, tokens: {access_token,
  refresh_token, account_id, …}, last_refresh}` — confirm).
- Does `vault::render_injection` render both anthropic inject headers when the
  vault credential is OAuth-only (no `{key}`)? The claude CLI sends
  `authorization: Bearer …` for OAuth — confirm the injected request
  authenticates.
- Which extra hosts each CLI phones (statsig/sentry for claude, telemetry for
  codex) and that env suppression (`DISABLE_TELEMETRY=1`,
  `DISABLE_ERROR_REPORTING=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`,
  `DISABLE_AUTOUPDATER=1`; codex config equivalents) silences them — every
  unsuppressed call is a judge denial that counts toward `take_refusals`.
- Proxy + CA compliance: claude is Node (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS`
  — should just work); codex is Rust/reqwest (`HTTPS_PROXY` + `SSL_CERT_FILE`
  — verify it honors both).

Acceptance:
- **Given** a box on the lockdown network with only sentinel files, **when**
  `claude -p "reply DONE" --output-format stream-json
  --dangerously-skip-permissions` runs, **then** a completion returns, the
  ledger shows the injected call to `api.anthropic.com`, and no real token
  exists anywhere in the run dir.
- **Given** the same box, **when** `codex exec --json "reply DONE"` runs with a
  sentinel `auth.json`, **then** same result via `chatgpt.com`.
- **Given** either CLI with telemetry suppression env set, **when** the run
  ends, **then** the gateway ledger shows zero denied requests.

Exit criterion: if either CLI refuses the sentinel (e.g. insists on refreshing
against `platform.claude.com` / `auth.openai.com`), capture the exact requests
and decide between (a) far-future expiry adjustments, (b) allowing the refresh
endpoint as a special no-inject grant, or (c) gateway-side refresh. Do not
proceed to WP3+ until green.

### WP0 results (run 2026-07-17 — GREEN, with one design change)

Both CLIs completed a governed one-shot run end-to-end on sentinel
credentials with correct header injection and zero secrets in the box.
Versions tested: claude-code 2.1.211, codex (npm latest same day), baked over
`ghcr.io/manasgarg/roster-box:latest`.

- **Confirmed sentinel shapes** (golden-test these in WP4):
  - claude `$CLAUDE_CONFIG_DIR/.credentials.json`:
    `{"claudeAiOauth": {accessToken, refreshToken, expiresAt,
    refreshTokenExpiresAt, scopes[], subscriptionType, rateLimitTier}}` +
    `$HOME/.claude.json` with `{"hasCompletedOnboarding": true}`.
  - codex `$CODEX_HOME/auth.json`: `{"auth_mode": "chatgpt",
    "OPENAI_API_KEY": null, "tokens": {id_token, access_token, refresh_token,
    account_id}, "last_refresh"}` — access/id tokens must be
    structurally-valid JWTs (`sentinel_jwt()` works as-is).
  - Neither CLI attempted a token refresh (far-future expiry suffices).
- **Design change — claude needs an in-box auth shim.** `claude` is a
  Bun-compiled binary; Bun's fetch does not support userinfo credentials in
  proxy URLs (`http://token@host:port` → `getaddrinfo ENOTFOUND token@host`).
  pi (Node/undici) and codex (Rust/reqwest) handle userinfo fine. Fix proven
  in the spike: a ~25-line Node localhost proxy (`127.0.0.1:3128`, no auth)
  that chains CONNECT to the gateway adding `Proxy-Authorization` from
  `ROSTER_PROXY_TOKEN`; claude gets the credential-less
  `HTTPS_PROXY=http://127.0.0.1:3128`. Attribution and injection verified
  intact (`org/<worker>` in the ledger). WP3: `run-claude` starts the shim;
  WP4: pass `ROSTER_PROXY_TOKEN`/`ROSTER_GATEWAY` env (add to
  `RESERVED_ENV`).
- **Claude telemetry suppression works**: with `DISABLE_TELEMETRY=1`,
  `DISABLE_ERROR_REPORTING=1`, `DISABLE_AUTOUPDATER=1`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` the run produced exactly one
  gateway request (`POST api.anthropic.com/v1/messages`, allow,
  `authorization` injected — the OAuth-only credential correctly skips the
  `x-api-key` template).
- **Codex telemetry — one open item**: one denied request to
  `ab.chatgpt.com/otlp/v1/metrics` (OTEL export). Find the config.toml knob
  in WP4; until then it would count one refusal per run (attestation trap for
  silent runs). Its `/backend-api/codex/analytics-events` calls ride the
  existing model-api grant (allowed).
- **Codex quirk**: `codex exec` prints "Reading additional input from
  stdin..." — the WP3 spawn should close stdin or pass `-` appropriately.
- **Security observation (independent of this plan)**: `resolve_identity`
  (`src/gateway/proxy.rs:258`) maps a *credential-less* proxy request to
  subject `org` ("trusted host-side caller") — but boxes on the locked
  network can also send credential-less requests, laundering attribution and
  escaping per-worker budgets/refusal counts while still exercising
  org-scoped grants. Consider distinguishing host-side callers by peer
  address instead. (Discovered because claude-without-shim accidentally ran
  this way and succeeded.)

## WP1 — Config: the `agent` key

Touch: `src/config.rs`, `docs/configuration.md`.

- `enum AgentKind { Pi, Claude, Codex }` with `FromStr` (lowercase names).
- `Loaded` gains `agents: HashMap<String, AgentKind>` (resolved per worker:
  worker.toml `agent` → `[engine] agent` → `Pi`).
- Validation errors: unknown value (lists allowed), wrong type.
- Warning (not error): non-pi agent on a worker with `[channels]` listeners —
  "sessions run pi; agent applies to task runs".
- Unit tests beside the existing config tests.

Acceptance:
- **Given** no `agent` key anywhere, **then** every worker resolves to `Pi` and
  behavior is unchanged.
- **Given** `agent = "claude"` in one worker.toml, **when** `roster server
  validate` runs, **then** it passes and `Loaded.agents` maps that worker to
  `Claude`.
- **Given** `agent = "gpt5"`, **then** validate fails naming `pi | claude |
  codex`.
- **Given** a claude worker with `[channels] discord = …`, **then** validate
  passes with the sessions-stay-pi warning.

## WP2 — Image: bake the CLIs

Touch: `package.json` (+ lockfile), `box/Dockerfile`.

- Add `@anthropic-ai/claude-code` and `@openai/codex` as pinned deps (versions
  ride `npm ci`, same as pi).
- Engine layer: resolve each package's bin (same symlink dance as
  `Dockerfile:62`) → `/opt/roster/engine/claude`, `/opt/roster/engine/codex`.
  Both are native executables, not node entries (claude is Bun-compiled;
  codex ships platform binaries via optionalDependencies) — symlink and exec
  directly, no `node` prefix.
- Ship the auth shim (WP0 results) as `/opt/roster/engine/box/shim.js`.
- Generate wrappers:
  - `run-claude`: start the auth shim (`node …/box/shim.js &`, reads
    `ROSTER_PROXY_TOKEN`/`ROSTER_GATEWAY`), rewrite `HTTP(S)_PROXY` to
    `http://127.0.0.1:3128`, then `exec /opt/roster/engine/claude -p
    --output-format stream-json --verbose --dangerously-skip-permissions
    --mcp-config /opt/roster/engine/mcp.json --strict-mcp-config "$@"` (MCP
    flags arrive with WP5; until then omitted).
  - `run-codex`: `exec /opt/roster/engine/codex exec --json
    --skip-git-repo-check "$@" < /dev/null` (codex reads stdin — close it;
    approval/sandbox bypass + MCP servers land in `$CODEX_HOME/config.toml`,
    WP4 — the box is already the sandbox). No shim: reqwest parses the
    token-bearing proxy URL correctly.
- Verify multi-arch: codex's npm package delivers platform binaries via
  optionalDependencies — confirm amd64 + arm64 builds.

Acceptance:
- **Given** the built image, **when** `docker run … run-claude --version` and
  `… run-codex --version` execute, **then** both print the pinned versions,
  offline.
- **Given** a pi-only deployment pulling the new image, **then** pi runs are
  byte-identical (new layers are inert).

## WP3 — Spawn plumbing

Touch: `src/run/boxed.rs`, `src/work/dispatch.rs` (threading only).

- Thread `AgentKind` into `RunSpec` (resolved from config at dispatch;
  CLI-run entry points resolve the same way).
- `pi_prefix()` → `agent_prefix(engine, agent, mode, session_dir)`:
  - `Pi`: unchanged, both modes.
  - `Claude`/`Codex`: one-shot only; `mode == "rpc"` is unreachable
    (`run_session` pins `Pi` — enforce with a debug assert + comment).
  - Claude: `run-claude` + `--append-system-prompt <compiled>`; input prompt
    stays the positional arg the shared code already appends
    (`boxed.rs:318-323`). The wrapper owns the auth shim + proxy rewrite
    (WP2); the host passes the token via `ROSTER_PROXY_TOKEN` instead of the
    proxy URL for claude runs (WP0: Bun cannot parse userinfo proxy URLs).
  - Codex: `run-codex`; compiled system prompt prepended to the input prompt
    with a delimiter (decision 8) — factor a small
    `compose_prompt(agent, system, input) -> (args, prompt)` helper, unit-testable.
  - `--session-dir` / `--session-id` (cache route key): pi-only. Claude's
    `--session-id` demands a UUID; skip initially, optionally derive UUIDv5
    from `route_key` later for cache affinity.
- Transcripts: point the agent homes into the session mount so they land in
  the host-visible run dir — `CLAUDE_CONFIG_DIR=$SESSION_MOUNT/claude`,
  `CODEX_HOME=$SESSION_MOUNT/codex` (sentinels contain no secrets by design,
  so host visibility is fine and aids debugging).
- Ceiling/signal/finalize paths untouched — they act on the container.

Acceptance:
- **Given** each agent, **when** args assemble for a task run, **then**
  snapshot tests on `agent_prefix` + `compose_prompt` show the exact expected
  command lines, and the pi snapshot is unchanged from today's output.
- **Given** a claude worker task, **when** dispatched for real, **then** the
  box runs to exit and `stdout.jsonl` holds claude's stream-json transcript.

## WP4 — Auth provisioning, env, credential gating

Touch: `boxed.rs` (`prepare_pihome` → per-agent `prepare_home`,
`model_credentials_available`, provision env), `src/config.rs`
(`RESERVED_ENV`), `src/work/dispatch.rs` (queue gating).

- `prepare_home(agent, …)`:
  - Pi: today's behavior, verbatim.
  - Claude: write `$CLAUDE_CONFIG_DIR/.credentials.json` in the WP0-confirmed
    shape (sentinel access/refresh tokens, `expiresAt` now+100y — reuse
    `SENTINEL`/`sentinel_jwt()` `boxed.rs:1356`), plus the minimal settings
    file that marks onboarding complete. Seed sources, in pi-parity order: a
    host claude login (`~/.claude/.credentials.json`) sentinelized, else the
    `anthropic` vault credential.
  - Codex: write `$CODEX_HOME/auth.json` (sentinel `tokens`, account id
    `roster-sentinel-account` — `sentinel_jwt()` already carries the
    `chatgpt_account_id` claim) and `$CODEX_HOME/config.toml`
    (`approval_policy = "never"`, `sandbox_mode = "danger-full-access"`,
    `mcp_servers.roster` per WP5, telemetry off). Seed: host `~/.codex/auth.json`
    sentinelized, else the `openai-codex` vault credential.
- Env: set `CLAUDE_CONFIG_DIR`/`CODEX_HOME` for those agents; claude runs
  additionally get `ROSTER_PROXY_TOKEN` + `ROSTER_GATEWAY` (for the shim) and
  the telemetry-suppression set from WP0. Add all of these to `RESERVED_ENV`.
  `ANTHROPIC_API_KEY` passthrough (`boxed.rs:933`) applies to claude runs too.
- Find the codex config knob that disables its OTEL export
  (`ab.chatgpt.com/otlp/v1/metrics` — the one denial left in WP0); until
  found, every codex run logs one refusal.
- `model_credentials_available()` → `model_credentials_available(agent)`:
  pi = today; claude = `anthropic` vault cred ∨ host claude login ∨
  `ANTHROPIC_API_KEY`; codex = `openai-codex` vault cred ∨ host codex login.
- Dispatch gating: hold only tasks whose worker's agent lacks credentials
  (today it's a single global check); provision error message names the
  agent-appropriate `roster connection add …` fix (`boxed.rs:758-764`).

Acceptance:
- **Given** only an `anthropic` vault credential, **when** the queue holds a
  claude-worker task and a codex-worker task, **then** the claude task runs
  and the codex task holds with "no openai-codex credential — run: roster
  connection add openai-codex".
- **Given** any provisioned non-pi run dir, **when** grepped for the real
  token material, **then** nothing matches (test asserts sentinels only).
- **Given** a claude run, **when** it completes, **then** the ledger shows
  zero denied telemetry calls.

## WP5 — The action surface as MCP

Touch: new `box/lib/tools.ts`, new `box/mcp/server.ts`, slim
`box/extensions/actions.ts`/`web.ts` to wrappers, `package.json`
(`@modelcontextprotocol/sdk`, pinned), `box/Dockerfile` (mcp.json for claude,
copy `box/lib` + `box/mcp`), `run-claude` MCP flags, codex `config.toml` entry.

- Extract every tool definition (`{name, description, parameters, execute}`)
  from `actions.ts` + `web.ts` into `box/lib/tools.ts`. `execute` bodies are
  already transport-pure: plain `fetch` to `https://actions.roster.internal/*`
  through `HTTP_PROXY` (built-in fetch only — undici routes through the
  gateway; native clients don't. See the header comment in `actions.ts`).
- `box/extensions/actions.ts`/`web.ts` become ~10-line adapters:
  `for (const t of tools()) api.registerTool(t)` — pi behavior unchanged.
- `box/mcp/server.ts`: stdio MCP server over the same module (Node 24 runs TS
  directly). `ROSTER_TASK_ID` gating of `task_complete`/`task_fail` carries
  over as-is (env-driven).
- Registration: claude via `--mcp-config /opt/roster/engine/mcp.json
  --strict-mcp-config` in `run-claude`; codex via `mcp_servers.roster` in the
  WP4 `config.toml`. Mounted-engine dev mode resolves the server path from the
  checkout (parallel to `box_extensions()` `boxed.rs:1385`).
- Tool naming: claude surfaces them as `mcp__roster__task_complete` etc.;
  briefings say `task_complete`. Expected to resolve fine; if runs show
  friction, add one per-agent sentence to the task-surface briefing
  (`context.rs:705`) — deliberately deferred.
- Note: until WP5 lands, a non-pi worker whose heartbeat curates tasks
  (`set_tasks`) has no way to do so — switch real workers to claude/codex only
  after WP5, or restrict pre-WP5 use to pure-exec/code tasks.

Acceptance:
- **Given** a claude task run, **when** the model calls `task_complete`,
  **then** the action host receives the identical `/outcome` POST a pi run
  sends (run/task provenance intact) and `finalize()` attests completed.
- **Given** a claude code task, **when** `propose_changes` is called after
  edits, **then** a gate files and the task attests needs-review
  (`dispatch.rs:291`).
- **Given** `web_search` via MCP, **then** the DDG request rides the same
  grant as today's pi path.
- **Given** a pi run on the refactored extensions, **then** the registered
  tool list and behaviors are unchanged (regression pass).

## WP6 — Verification pass on lifecycle & output consumers

Mostly verification, small patches where needed.

- E2E per agent (manual script or gated test): trivial task → attests
  completed; failing task (`task_fail`) → failed with reason; ceiling task →
  failed with ceiling message.
- Audit `stdout.jsonl` consumers beyond streaming (`roster server runs show`,
  any transcript pretty-printer) for pi-event assumptions; degrade gracefully
  to raw JSONL for foreign schemas.
- Confirm budget meters/ledger entries for `chatgpt.com`/`api.anthropic.com`
  calls record sanely for non-pi traffic shapes.

## WP7 — Docs & polish

- `docs/configuration.md` (the `agent` key at both scopes, credentials each
  agent needs, sessions-stay-pi), `docs/box.md` (engine section),
  `docs/architecture.md` mention.
- `roster worker show`/`ls`: display the resolved agent.
- Validate/error message review against the fail-closed style.

## Test plan

- **Unit**: config resolution matrix (default/org/worker × valid/invalid/
  channels-warning); `agent_prefix` + `compose_prompt` snapshots (pi snapshot
  frozen to current behavior); sentinel writers' golden files; per-agent
  `model_credentials_available`.
- **Regression**: full existing suite — pi paths must not drift.
- **E2E (docker + real creds, manual/gated)**: matrix {claude, codex} ×
  {plain task, code task + propose_changes, task_fail, ceiling}. Scripted as a
  repo task so it's re-runnable at CLI version bumps.

## Risks & open questions

1. **Credential-file shape drift**: both formats are CLI internals, validated
   2026-07-17 against claude-code 2.1.211 and current codex. Mitigated by
   lockfile pinning + golden tests; version bumps become deliberate,
   spike-checked events.
2. **The auth shim is a new in-box moving part** (claude runs only). Kept
   tiny (~25 lines, CONNECT-only, loopback-only) and unit-tested; its failure
   mode is loud (immediate connection errors in the transcript), not silent.
3. **Refresh attempts**: a CLI deciding to refresh the sentinel hits
   `platform.claude.com`/`auth.openai.com` — denied by the judge, run degrades.
   Far-future expiry should prevent it; WP0 confirms.
4. **Codex prompt composition**: prepending the system prompt into the input
   may weaken instruction adherence vs. a true system slot. Acceptable for
   task runs; revisit when codex exposes a stable instructions flag.
5. **Refusal-count attestation interplay**: any unsuppressed phone-home call
   turns a silent-but-successful run into an attested failure
   (`dispatch.rs:300`). WP0's suppression list is load-bearing; keep it under
   test.
6. **MCP tool-name prefixes** in briefings (claude's `mcp__roster__…`): low
   risk, deferred mitigation identified.
7. **Multi-arch codex binary** in the image build.
8. **Flag churn** in fast-moving CLIs (`--output-format stream-json`,
   `--append-system-prompt`, `codex exec --json`): pinned versions contain it;
   bumps go through the E2E matrix.

## Sequencing

1. **WP0 spike** — ✅ done 2026-07-17, green (see WP0 results above).
2. **WP1 config → WP2 image → WP3 spawn → WP4 auth** — milestone A:
   *pure-exec agents*: a claude/codex worker runs tasks end-to-end (clean exit
   ⇒ completed, `dispatch.rs:307`); code tasks work via host-side worktree,
   but no governed actions, no PR proposals, no task curation.
4. **WP5 MCP bridge** — milestone B: *full parity for task runs* (actions,
   attestation reports, memory, web, task curation — heartbeats included).
5. **WP6 verification → WP7 docs** — milestone C: documented, e2e-tested,
   safe to point real workers at.

Each WP lands independently with pi behavior frozen by tests; nothing here
touches the session path.
