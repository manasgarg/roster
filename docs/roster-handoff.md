# Roster — implementation handoff

**Audience: a coding agent** starting implementation of Roster with no prior
context. This document is self-contained. Where it references the working
reference implementation ("Yuko", the repo this doc was authored in), the path
is `/home/manas/projects/yuko/1` and its KB is `~/research-kb`; if you have
read access, port from it — if not, this document plus the design doc are
sufficient to rebuild.

**Companion documents** (same `docs/` dir, read in this order):
1. `digital-worker-platform.md` — the design, plain-English, owner-approved
   structure (the 13 pieces). **The design is settled; do not re-litigate it.**
2. `vendor-nanoclaw.md`, `spike-nanoclaw-2026-07-06.md`, `vendor-letta.md` —
   evaluated references with borrow/reject verdicts.
3. `how-it-works.md`, `build-plan.md` — how the reference implementation works
   and how it was built (increment-by-increment).

---

## 1. What you are building

**Roster**: a platform where the owner describes a "digital worker" in a
folder of config files, deploys it in minutes, and the worker does proactive,
ongoing agentic work (research, monitoring, curation, correspondence) — while
**every action passes through governance machinery the worker cannot touch**:
a policy judge with default-deny, budget ledgers, per-action-class trust that
is *earned*, human approval gates on everything irreversible, and a permanent
audit record.

One line: **rented intelligence, owned governance.** The LLM harness is a
swappable-in-principle, pinned-in-practice engine; the product is the
organization around it.

What differentiates Roster from every existing framework (verified by four
evaluations — pi, NanoClaw, OpenClaw, Letta): runtimes, channels, and memory
tricks exist off the shelf; **governed identity, earned trust,
budgets-as-ledgers, and decisions-as-records exist nowhere.** That is the
build.

---

## 2. Decision log (settled — do not reopen without the owner)

| # | Decision | Date | Rationale (short) |
|---|---|---|---|
| D1 | Design doc first; code only when owner green-lights | 2026-07-08 | Owner call |
| D2 | **Productizable / multi-tenant trust model from day one** | 2026-07-08 | Owner call; org = hard boundary; but build order still starts single-org |
| D3 | **pi is the only engine. No provider seam, no adapter layer** | 2026-07-08 | Owner call. The container contract (§7.3) is the de-facto coarse boundary; do not design an in-process abstraction |
| D4 | **TypeScript everywhere** is the destination | 2026-07-08 | pi extensions force TS worker-side; shared contract types; Node 24 type-stripping = no build step |
| D5 | Existing Rust judge (PDP) stays **until first major surgery** (org-boundary increment), then ports to TS | 2026-07-08 | It works; it parses no hostile input (memory-safety argument void); live policy corpus uses only `true`, `==`, `!=`, `\|\|`, `in` → ~80-line TS evaluator replaces CEL; `node:sqlite` replaces the Rust SQLite ledgers |
| D6 | **Budget gates proactive work only** — owner-filed/chat work ALWAYS runs | 2026-07-07 | Owner correction, now invariant. Classification happens at dispatch (supervisor), not in-run |
| D7 | Budget/spend caps live in **owner-only config**; agent may propose pace (cadence) but never money | 2026-07-07 | The agent must never be able to raise its own budget |
| D8 | Enforcement never lives in pi extensions | 2026-07-08 | Extensions run inside the container (untrusted zone). Soft budget stop = supervisor at dispatch; hard stop = gateway stops serving credentials (incl. the model key) |
| D9 | Briefs (finished deliverables) live in the **disk cache**, not the KB git repo | 2026-07-08 | Owner call. Notes are the durable substrate; briefs are derived artifacts |
| D10 | **Promotion rule** (from Letta, repurposed for security): workers append notes; only the curator session promotes into the always-loaded core; charter edits always gate to owner | 2026-07-08 | Closes the injection→self-programming hole |
| D11 | Trust **never ports** on worker export/import; imported workers restart at T0 | 2026-07-08 | Track record only means something where earned |
| D12 | Channels **relay, never act**; inbound email = content, never commands (spoofable); outbound email gated at T0 | 2026-07-08 | Security invariant |
| D13 | Adopt-vs-build verdicts: NanoClaw **declined**, Letta **declined**, OpenClaw never a candidate. Borrow ideas only | 2026-07-06/08 | See vendor docs; revisit triggers recorded there |
| D14 | Owner's 13-piece model is the build structure; six-block view is the work-division view | 2026-07-08 | Owner call |
| D15 | **No invented action-class taxonomy.** Governed requests are matched on the standard HTTP vocabulary (protocol/method/host/port/path/headers/payload) + MCP's own terms (`tools/call`, tool name); deployment-specific meaning attaches to owner-named rule `name`s. Replaces the reference's fixed classes (`acquire-source`, …). Budgets/trust/gates will bind to rule names | 2026-07-08 | Owner call. Standard vocabulary ports across deployments; invented taxonomies don't. See `docs/judge-spec.md` |
| D16 | **The gateway terminates TLS with a host-minted CA** (key at `~/.roster/ca/`, never on the box) to see full requests; `tunnel` verdict is the escape hatch for cert-pinning clients / interception-breaks-pi fallback | 2026-07-08 | Follows from D15 — matching all params requires seeing inside TLS. Verified: pi honors `NODE_EXTRA_CA_CERTS`, so `tunnel` is unused for pi |
| D17 | **The gateway is Rust; orchestration stays TypeScript.** The trusted core (TLS termination, CA/cert minting, judge, vault + OAuth refresh, injection, metering, call log) is a Rust binary; the box runner, docker lockdown, CLI, and future supervisor stay TS. Seam = the container contract (§7.3) + the JSON policy/config files. **Reverses D5's deferral and expands it** (D5 kept the Rust judge only "until first major surgery"; the surgery is now — the gateway parses hostile request/response bodies and evaluates expressions over them, so the memory-safety argument D5 called void is back), and **refines D3/D4** (TS is the destination for orchestration, not the trusted request path) | 2026-07-08 | Owner call. The gateway now terminates TLS and parses attacker-controlled bodies on the hot path — exactly the hostile-input parser Rust is for; CEL (D18) has a mature Rust impl. Port-first: port the working TS gateway to parity, then build metering on the Rust base |
| D18 | **CEL is the one expression language** — judge conditions, derived-field extraction from bodies, and currency/spend mapping all evaluate CEL against a shared context (`request.*`, `response.*`, `decision.*`, `subject`, `trust.*`, `environment.*`, `vars`). **Reverses D5's plan to shrink CEL** to a ~80-line matcher | 2026-07-08 | Owner call. Metering needs arbitrary currency = f(request, response) expressions; one language across judge/extract/meter beats three ad-hoc matchers. cel-interpreter (Rust) is mature. The structured judge matcher ports first (parity); CEL lands with the metering increment and can retrofit the judge |

**Note (2026-07-08):** build is proceeding **from scratch** in the Roster
repo in small increments (owner call), not porting Yuko as §6 suggests. D5's
~80-line TS judge is now the live `src/judge.ts` (built fresh, not ported);
its match language is D15's, not CEL. Shipped so far: CLI scaffold, the box
(§3.3), the judge + inspecting gateway (§3.7/§3.9 seed), and **credential
injection** — the model key now lives in a host-side vault
(`~/.roster/vault/`) and is injected in transit; the box carries only a
sentinel; and **gateway-owned OAuth refresh** (the gateway refreshes expired
tokens itself via a provider table — no pi dependency in the credential path;
single-flight, atomic vault write, fail-closed, audit to
`runs/credentials.jsonl`). This is build-plan increment 3's injection +
refresh. The hard-budget half (gateway declines to inject on an empty ledger)
attaches at the same pre-inject checkpoint and is still pending — see
`docs/injection-spec.md`.

**Note (2026-07-08, later):** the gateway is now **Rust** (`gateway/`), per
D17/D18 — TLS termination, CA/leaf minting (rcgen), judge, vault, injection,
and OAuth refresh all ported to parity and verified live end to end (the box
runs through it); the TS gateway modules are retired. Orchestration
(`src/`: box runner, lockdown, CLI, `vault-sync`) stays TS. See
`docs/rust-port.md`. Next: the metering/currency/budget model (call log →
namespaced identity → CEL currency mapping → drawdown limits) built on the
Rust base with CEL.

---

## 3. Architecture: the 13 pieces (technical version)

The design doc has the plain-English version; this is the implementer's cut.

### 3.1 Engine (pi)
- Package: `@earendil-works/pi-coding-agent` — **mind the fork**: this is the
  maintained fork lineage used by the reference; do not confuse with
  `@mariozechner/*` originals. Pin exact version.
- Invocation pattern (from reference): `pi --mode json -e <ext1> -e <ext2> ...
  --session-dir <dir>` with prompt on stdin/arg. Sessions record to
  `<session-dir>/*.jsonl`; each model call logs `usage`
  `{input, output, cacheRead, cacheWrite, totalTokens, cost:{total}}` — this
  is the token/cost accounting source (§7.6).

### 3.2 Behavior kit (pi extensions, TypeScript)
Extensions load in-process (`defineTool` + typebox `Type.Object` schemas).
Reference set (port from `packages/pi-extensions/src/`):
- `journal-emitter` / `journal` — append events to `journal/events.jsonl`
- `task-protocol` — tools: `declare_decision_point`, `escalate`, `close_task`,
  `message_user`, `queue_research_task`, `propose_charter_edit`,
  `propose_cadence`
- `mailbox-poll` — deliver owner steer messages at turn boundaries
- `gateway-client` — route governed actions to gateway; obey verdicts
- `web-search`, `tools` (`fetch_page`, `recall`) — thin wrappers over gateway
  endpoints
- `memory`, `notes` — `memory_write`, `note` (atomic `[[wiki-link]]` markdown)
- `store` — `persistSource`, `recall` search, Discord log append
Rule: extensions shape behavior; they never enforce safety (D8).

### 3.3 The box (container)
Per-session Docker container:
- repo/config mounted **read-only**; only task workspace, journal, mailbox
  writable
- **no secrets inside**; the repo's `.env` is **shadowed** inside the mount so
  keys can't be read off the read-only mount either
- **egress lockdown**: container joins a NAT-disabled Docker bridge; only
  route out is the host gateway (HTTP(S)_PROXY honored by pi — verify with a
  live task once per setup)
- hard ceiling timeout on session lifetime
- current caveat carried from reference: the model credential is copied into a
  throwaway per-run config the container reads. This changes at increment 3
  (model key behind gateway).

### 3.4 Filing system (four stores)
- **Config**: read-only mounted (the WorkerSpec).
- **Cache** (`~/loop-store` pattern in reference; per-worker namespace in
  Roster): raw fetched pages/searches (`sources/<kind>-<hash>.md`), channel
  logs (`discord/YYYY-MM-DD.jsonl`), finished briefs. Off-git. TTL + size
  caps (new requirement, not in reference).
- **Store** (git): linked atomic notes — one idea per note, `[[links]]`,
  frontmatter — used for facts, procedures, lessons. Every write is a git
  commit (audit + one-command revert).
- **Core**: the curated always-loaded set, assembled by a **deterministic
  context compiler** (new component — increment 0): labeled, size-budgeted
  blocks (charter / working set / procedures / note index / org context /
  task). Same assembly every session; log the compiled result so "what did
  the worker see" is answerable.
- **Promotion rule** (D10): workers append; a separate curator session (which
  never fetches raw web content) promotes into core; charter → owner gate.

### 3.5 Wake-ups
Heartbeat (periodic bounded triage session) + event triggers (message,
schedule fire). **A wake-up never does work inline — it files a task** (§3.6).
Cadence in the agent-proposable `cadence.yaml` (owner approves).

### 3.6 Task queue
One durable queue per worker; states `waiting → running → needs-review →
done`. All work becomes a task: owner-filed (reactive), heartbeat/planner
/schedule-filed (proactive, labeled as such — reference uses a
`loop:proactive` label). Queue is readable by the worker (dedup: "don't
re-propose what's queued"). Reference uses GitHub Issues
(`q/research`, `loop:in-progress`, `loop:needs-review` labels) — see open
question Q3 for built-in queue vs GitHub mirror.

### 3.7 Gateway (keys + judge)
One trusted host process (reference splits it: `broker/` TS on :7213 +
`pdp/` Rust on :7212; Roster may keep the split until D5's port merges them
functionally):
- holds all credentials (vault); injects **in transit**; workers never see keys
- default-deny; verdicts: `allow | deny | gate | budget-and-deduct`
- consults per-(worker, action-class) trust + ledgers before answering
- **writes a decision record for every answer** with full context
  (reference: `pdp/data/decisions.jsonl`)
- endpoints pattern (reference): `/v1/search`, `/v1/fetch`, `/v1/recall`
  (recall is ungoverned — reads already-acquired data); adding a capability =
  one endpoint + one thin tool; policy/credentials/lockdown come free
- persists all retrievals to the cache (raw-data custodian)
- increment 3: model calls also proxied here (hard budget + failover)

### 3.8 Budgets
- Ledgers: tokens/hour, currency/day, searches/day per worker; org aggregate
  cap above (fleet cannot run away collectively).
- **Soft stop (supervisor, dispatch-time)**: over-cap ⇒ don't start proactive
  work; reactive always runs (D6). Throttled owner notice when paused.
- **Hard stop (gateway)**: empty ledger ⇒ stop serving credentials.
- Accounting source: pi session `usage` entries, tallied per run into an
  append-only usage ledger (reference: `runs/.daemon/usage.jsonl`,
  `{ts, ref, kind, tokens, cost}` rows; tally logic in
  `packages/driver/src/usage.ts` — recursively sum objects bearing numeric
  `totalTokens` + `cost.total`, skip `stdout*` files).

### 3.9 Approval desk (gates + trust)
- Trust ladder per (worker, action-class): T0 all-irreversibles-gated (birth
  state) → T1 longer deadlines → T2 free + sampled review (pass rate decays
  sampling toward a 10% floor) → T3 audit-only.
- Failed review ⇒ sampling back to 100%. `--incident` ⇒ automatic tier
  demotion. Promotion **human-only**.
- Gates are durable records (reference: `gates/pending/`), fail closed
  (deadline ⇒ escalate-once or auto-deny per policy). Reference mechanism for
  KB publishes: PR against KB repo, merge=approve close=deny — with briefs
  moving to cache (D9), gates attach instead to note/procedure **promotion**
  and to `send-communication`.
- Action classes (reference set + designed additions): `spend-external-api`,
  `acquire-source`, `access-private-source` (owner-gated,
  **deny-on-timeout** — private data fails closed), `commit-to-kb` (becomes
  promote-to-core), `share-knowledge`, `send-communication`.

### 3.10 Channels (Discord first, email second)
- Edge relays only: ack (👀), file task or deliver steer; buttons for
  gates/reviews/proposals (Approve/Deny, Pass/Fail). **No model at the edge.**
- Discord: obey only `DISCORD_OWNER_ID`; hand-rolled gateway client in
  reference (`packages/daemon/src/discord.ts`) — resumes sessions, reconnects
  on blips; do NOT adopt discord.js (owner preference: thin,
  dependency-free).
- Email: inbound = content only (D12); outbound = `send-communication`, T0.

### 3.11 WorkerSpec
```
workers/<name>/
  worker.yaml    # OWNER-ONLY: template, engine image pin, tool grants
                 # (= gateway endpoints + action classes), budgets, channels,
                 # kb/store namespaces, escalation routing
  charter.md     # AGENT-PROPOSABLE via propose_charter_edit → owner button
  cadence.yaml   # AGENT-PROPOSABLE via propose_cadence → owner button
  procedures/    # seed procedures; grow via store under promotion rule
  policies/      # OWNER-ONLY worker overlay on org policy corpus
```
No credentials in specs, ever. No trust in specs, ever (trust = derived
ledger state). Export/import = spec + knowledge snapshot, secrets nulled,
trust stripped (D11). Lifecycle verbs: `hire` (scaffold from template),
`deploy` (validate → provision surfaces + ledger rows + T0 trust + first
heartbeat), `steer`, `suspend`, `audit`, `retire` (archive; journals +
decision records immutable forever).

### 3.12 Supervisor
One long-running process, **no model**. Watches queues, dispatches sessions,
enforces concurrency, fires heartbeats/schedules, runs digest clock, owns
worker lifecycles. Two laws from shipped bugs (§8):
1. **Every timer/cursor is durable** (disk-persisted; restart-at-any-moment
   is the normal case).
2. **One writer lane per surface** (journal relay, queue dispatch, ledger
   append, store commit — serialized per worker).

### 3.13 Observability
- Append-only journal per session (every search/fetch/decision/narration/file
  write) → reports rendered **from the journal**, digests computed **from
  ledgers** — never model-written.
- Digest cadence (reference: 12h + `/digest` on demand + spend line in
  `/status`): ran-by-kind, tokens+$ vs cap, queue/gates/reviews pending.
- Fleet view (later): per worker — state, queue depth, spend, per-class trust,
  gates waiting.

---

## 4. Work-division view (six blocks, six contracts)

For subdividing work: **A Definition** (spec/templates/lifecycle),
**B Governance** (judge/taxonomy/ledgers/trust/records), **C Execution**
(supervisor/queue/box/journal/engine), **D Custody** (vault/injection/egress/
endpoints), **E Knowledge** (store/core/recall/curation/retention),
**F Front Office** (channels/approvals/digests/fleet/roles). Contracts:
WorkerSpec (A→all), Task (→queue), DecisionRequest→Decision (C,D→B), Journal
events (C→E,F), Human verdicts (F→B), Container contract (inside C). Tenancy
is a property of all six, not a block. **Subdivide B then A first** — they own
the vocabulary (action classes) and the input (spec) everything else consumes.

---

## 5. Invariants (testable; violating any is a bug)

1. Nothing the worker ingests is trusted — including notes it wrote after
   ingesting (attacker-reachable via prompt injection).
2. No secrets in any worker, spec, or container. Injection in transit only.
3. No egress except through the gateway (NAT-disabled bridge).
4. Code/config mounts read-only; a worker cannot edit its rules, extensions,
   or spec.
5. No matching rule ⇒ deny. Unanswered gate ⇒ deny (fail closed).
   `access-private-source` ⇒ owner gate with deny-on-timeout.
6. The agent proposes direction and pace; never capability, money, or trust.
7. Channel edges relay; they never act. Only authenticated owner principals
   are obeyed.
8. Budget gates proactive dispatch only; owner-filed work always runs (D6).
9. Journals and decision records are append-only and permanent.
10. Digests/reports are computed, never model-generated.
11. Every timer durable; one writer per surface.
12. `frozen` mode gates all state-changing actions instantly (org-scoped).

---

## 6. Reference implementation map (Yuko repo)

Live and verified as of 2026-07-08. Port, don't rewrite, wherever it fits.

| Path | What it is | Roster disposition |
|---|---|---|
| `packages/driver/src/run.ts` | Per-task runner: housekeeping, gate sweep, prompt builders (attend/plan/curate/research), PDP autostart, container spawn | Generalize into session-runner; prompt builders → context compiler (increment 0 replaces ad-hoc assembly) |
| `packages/driver/src/usage.ts` | Token/cost tally from pi session JSONL | Port as-is |
| `packages/driver/src/{gates,trust,review,reviews}.ts` | Gate sweep/resolve, trust CLI, review verdicts | Port; extend to promotion + send gates |
| `packages/driver/src/{container,lockdown}.ts` | Docker run + egress lockdown (~40 lines, NAT-disabled bridge) | Port as-is |
| `packages/driver/src/{tracker,kb,report,steer,journal-read}.ts` | GitHub Issues client, KB publish, report renderer, steering | Port; tracker behind queue abstraction (Q3) |
| `packages/daemon/src/daemon.ts` | Supervisor: tick loop (20s poll), dispatch (issue/attend/plan/curate), journal relay w/ `processed` cursor + `supersedeJournal`, budget gate (`proactiveAllowed`), digest, cadence, proposals (charter/cadence buttons), usage recording | The Roster supervisor grows from this. Keep: durable stamps (`runs/.daemon/last-plan`, `last-curate`, `last-digest`), `loop:proactive` labeling, throttled pause notice |
| `packages/daemon/src/discord.ts` | Hand-rolled Discord gateway (resume, reconnect, buttons) | Port as channel adapter #1 |
| `packages/daemon/src/schedule.ts` | `schedules.json` timed research | Fold into wake-ups |
| `packages/pi-extensions/src/*` | The behavior kit (§3.2) | Port; add tool-rules layer (protocol shape only) |
| `broker/src/broker.ts` | Gateway TS half: fetch/search/recall + key injection + raw-store persist | Grows into §3.7 |
| `pdp/` (Rust, :7212) | Judge: YAML policies + 5-op conditions, SQLite ledgers/trust, decision records, `environment.yaml` mode dial | Keep running; port to TS at org-boundary increment (D5) |
| `policies/*/*.yaml` | org/team/learned precedence, conditions | Port format as-is |
| `schemas/` | DecisionRequest/Context/Decision + journal event schemas, action classes | **The narrow waist — port first, change least** |
| `loop.json` | `tracker_repo, kb_dir, principal_id, pdp_url, gateway_mode (shadow/enforce/off), run_mode (host/container), egress_lockdown, token_budget_hourly` | Becomes org+worker config split |
| `cadence.json` | `plan_every_hours, curate_every_hours, max_concurrent_research` | Becomes per-worker `cadence.yaml` |
| State dir `runs/.daemon/` | `usage.jsonl`, `last-digest`, `last-plan`, `last-curate` | Pattern generalizes to the durable scheduler table |

Env conventions: secrets in repo-root `.env` (gitignored), loaded by driver;
`LOOP_*` env vars override config. **Never** commit `.env`, `runs/`,
`mailbox/`, `gates/`, `reviews/`. Refer to keys by name only; never print
values.

---

## 7. Technical specifics

### 7.1 Toolchain
Node 24 (native TypeScript type-stripping — **no tsc, no build step**;
`node --check <file>` for syntax). `node:sqlite` for ledgers when the judge
ports. Dependencies: near-zero by policy — search the pi extension ecosystem
before building, hand-roll thin over adopting subsystems.

### 7.2 Condition evaluator (replaces CEL at D5 port)
Live corpus uses only: `"true"`, `==`, `!=`, `||`, `in [list]`, over bindings
`trust.tier`, `environment.mode`, `environment.change_freeze` (extend
bindings as classes grow). ~80 lines, zero deps. Property-test against the
Rust engine's outputs on the recorded decision corpus before cutover.

### 7.3 Container contract (the engine boundary — D3)
In: task file + mailbox dir. Out: `journal/events.jsonl` (append-only) +
workspace artifacts. Egress: gateway only. Lifecycle: run-until-`close_task`
or ceiling timeout. All scheduler state host-side. Any image honoring this is
a valid runtime; **do not** build an in-process abstraction on top of it.

### 7.4 Journal relay discipline (bug-scar critical)
The supervisor relays `events.jsonl` to channels tracking a per-run
`processed` count. **Before every re-dispatch into a reused task dir, call
`supersedeJournal` (rename `events.jsonl` → `events.prev.jsonl`)** — else a
new run with `processed=0` re-relays the previous session's events (this
shipped as duplicate Discord messages + a duplicate GitHub issue).

### 7.5 Boot-test trick
To boot the supervisor without double-connecting the real Discord bot:
`DISCORD_BOT_TOKEN=bogus node <supervisor>` (the exact var name matters — a
test once used `DISCORD_TOKEN=bogus` and connected as the real bot via the
`.env` value).

### 7.6 Accounting
Parse session JSONL lines containing `"usage"`; recursively tally nodes with
numeric `totalTokens` (+ `cost.total`), don't descend into counted nodes;
skip `stdout*` files. Real-world scale for sanity checks: a research run ≈
0.4–0.9M tokens / $0.8–1.1; curate ≈ 0.1M/$0.3; chat-attend ≈ 0.2M/$0.5.

### 7.7 Proposal flow (charter/cadence)
Extension emits `charter_proposed` / `cadence_proposed` journal events →
supervisor renders Approve/Decline buttons → on approve: write file
(charter → git commit scoped to that path; cadence → merge + live reload +
reset cadence stamps). Nothing applies without the button.

---

## 8. Ops scars — bugs actually shipped; do not re-ship

1. **In-memory cadence timers** reset on every restart ⇒ the 8h planner
   *never fired* (daemon restarted more often than the window). Fix pattern:
   persist stamps, load on boot, fire catch-up if overdue, small boot grace
   (~90s) to let channel reconnects settle.
2. **Journal replay race** (§7.4): reused task dir + `processed=0` ⇒ old
   events relayed as new. First fix attempt was a *prompt* guard — wrong
   layer; the real fix was mechanical (supersede). Lesson: fix state races in
   state, not in prompts.
3. **Stale gateway process**: after code changes, a detached broker keeps
   serving old code (supervisor restart doesn't restart it). Symptom: calls
   logged but side-effects missing. Kill by port and restart explicitly.
4. **JSDoc `*/` landmine**: a glob like `runs/*/journal` inside a block
   comment closes it. Write `runs/<run>/journal` in comments.
5. **Bash pipelines mask exit codes** (`cmd | head` hides `cmd`'s failure) —
   use `if cmd; then` when the exit code matters.
6. Machine TZ is CEST (UTC+2); logs are UTC ISO — reconcile before reasoning
   about "when did X fire".

---

## 9. Build plan (increments; each live + tested before the next)

**Increment 0 — prove two pieces inside Yuko (no Roster scaffolding):**
- Context compiler: replace ad-hoc `build*Prompt` with deterministic budgeted
  blocks; log compiled context per session. Accept: existing runs behave the
  same; "what did it see" is answerable from the log.
- Promotion rule: split store writes (worker append-only) from core promotion
  (curator only); charter already gates. Accept: a planted "malicious" note
  cannot enter the compiled core without a curator run; curator diff is
  git-visible.

**Increment 1 — WorkerSpec + hire/deploy (single org):**
CLI scaffolds `workers/<name>/` from a template; deploy validates spec,
provisions surfaces (queue, journal dir, store namespace, ledger rows, T0
trust), registers with supervisor. Re-express Yuko as `workers/yuko/`.
Accept: **zero behavior change** for Yuko running from its spec.

**Increment 2 — second worker (monitor template):**
Shares gateway/supervisor/judge; own identity, budgets, trust, namespaces.
Accept: two workers run concurrently; ledgers and trust records never bleed;
per-worker digest correct.

**Increment 3 — model key behind gateway (hard budget):**
Proxy model calls; serve credential only while ledger positive; failover
here. Accept: an over-hard-cap worker's model call fails at the gateway
mid-run; reactive soft-cap behavior unchanged (D6).

**Increment 4 — queue as first-class (Q3):**
Built-in durable queue; optional GitHub mirror. Accept: kill/restart
supervisor mid-flight, no lost/duplicated tasks (one-writer lanes hold).

**Increment 5 — email channel:** outbound first (`send-communication`, T0
gated); inbound as content-only into the cache. Accept: spoofed inbound mail
cannot cause any action; outbound requires button until promoted.

**Increment 6 — fleet view + org digest.** Computed only. Accept: matches
ledgers exactly.

**Increment 7 — roles + sign-in:** approver/auditor via OIDC; judge consumes
verified principal ids. Accept: an approver can resolve only granted classes;
auditor is read-only.

**Increment 8 — org boundary + judge port (D5) + adversarial pass:**
Namespace policies/ledgers/records/stores per org; port judge to TS
(property-tested against recorded decisions); then attack tenancy on purpose
(ledger bleed, store path traversal, gateway routing). Accept: documented
attacks fail.

---

## 10. Working with the owner

- **Thin pieces over subsystems** — prefers small dependency-free code over
  framework adoption (declined discord.js, NanoClaw, Letta on this basis).
  Search existing ecosystems before building, then build thin.
- **Always update docs** in the same change: README, how-it-works (plain
  English register for owner-facing docs), build-plan, package READMEs.
- **Test as you build; verify live.** Every increment proven on the running
  system before moving on. Report failures honestly with output.
- **Propose, don't assume, on forks**: pin genuinely owner-level decisions
  with short option sets (this is how D1–D3 were made). Don't re-ask settled
  ones (§2).
- **Commit only when asked.** Never commit `.env`, `runs/`, `mailbox/`,
  `gates/`, `reviews/`.
- Security habits: refer to API keys by name only; never echo values; treat
  charter/notes/memory as attacker-reachable text.

---

## 11. Non-goals (v1) and risks

**Non-goals:** engine swap layer; embeddings/vector store (keyword recall
until it measurably fails); cross-org sharing; agent-editable policy
(improvement loop proposes diffs, humans merge); model-written observability;
automatic trust promotion.

**Top risks:** pi is a load-bearing pinned rental · inbound email authn
(content-only until solved) · gate fatigue at T0 (gate the irreversible, not
the mundane; surface approval load in digests) · budget hard-stop absent
until increment 3 (dispatch-gate only; in-flight runs finish) · tenancy is a
claim until increment 8 attacks it · journals grow forever (cold-storage
answer needed) · OpenClaw-derived ideas were never code-verified.

## 12. Open questions for the owner

Q1 name ("Roster" is placeholder) · Q2 third/fourth templates (correspondent
vs ops-runner) · Q3 queue: built-in day one vs GitHub-backed until
increment 4 · Q4 inbound email authn: any scheme trustworthy, or content-only
permanent · Q5 journal cold storage: local archive vs object storage.
