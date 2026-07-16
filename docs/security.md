# The security model

Roster assumes the worst about the worker: the model can be prompt-injected by
anything it reads, and the code it runs may be malicious. The design goal is
that a *fully compromised* box still can't do real damage — because
everything that matters is enforced outside it, by topology and by the
trusted side, never by prompts.

## Threat model

Untrusted, by assumption:

- Everything inside the box: pi, the extensions, any code the worker writes or
  runs, and the whole container filesystem it can reach.
- Everything the worker ingests: web pages, chat messages, uploaded files,
  emails — and everything it wrote *after* ingesting them, including its own
  memory notes and knowledge records. Injection is reachable through all of
  it.

Trusted: the `roster` binary, the three deployment roots on the host, and
Docker itself. The humans with a shell on the host are the ultimate
authority.

## Principles

The mechanisms below follow five principles; specs and design reviews
reference them as P1–P5.

**P1 — The worker is the confidentiality boundary.** A worker is one mind:
everything it ever ingests may influence everything it later does. Its
stores (memory, knowledge, queue, journal) are one accumulating state,
shared across every channel it serves. Roster therefore promises isolation
*between* workers — separate stores, separate boxes, separate identity,
grants, and budgets — and refuses to fake walls *inside* one. If two
contexts must never mix (two clients; a public community and private ops),
deploy two workers. Workers are cheap; partitions inside a mind are
theater.

**P2 — Inside a worker, govern acts, not thoughts.** Information flows
freely within the mind; enforcement lives where consequences happen: the
gateway for every byte of egress, gates for consequential actions, budgets
for pace. Nothing anyone says to a worker can *make* it do anything — it
can only make it want to, and wanting meets the same grants and gates
regardless of where the want came from.

**P3 — Facts about people have one home: memory.** Not because information
flow can be fully policed — it can't — but because memory is the only
surface with the right shape for person-data: scoped at recall (user notes
surface only around that user, channel notes in that channel) and governed
by the subject (inspect, correct, forget, retention). Worker-global
surfaces — knowledge, the queue — are read by every run, so a person-fact
written there is de facto cross-channel. The participant scans police this
norm as tripwires; they are hygiene, not walls.

**P4 — Provenance is recorded, never trusted away.** Every durable write
names the run that made it and who was in the room: journal events, git
commits, task origins, audit logs. Mixing inside a worker cannot be
prevented, so the promise is traceability and repair — memory corrected,
records reverted, tasks requeued. Structural enforcement is reserved for
what repair cannot undo: an email cannot be unsent (P2), and a mixed mind
cannot be unmixed (P1).

**P5 — Source trust attaches at ingestion and travels as framing.** Every
turn is labeled with its speaker and role; relayed and inbound content
arrives framed as content, never command. The worker weighs; the host
enforces.

## The mechanisms

**No route out.** The box joins a Docker bridge network with NAT disabled —
packets to the internet never come back. DNS is a blackhole
(`--dns 127.0.0.1`), closing the resolver side channel. The only exit is the
proxy environment pointing at the gateway, and if the locked network or the
gateway isn't healthy, the box refuses to start rather than run open.

**Nothing worth stealing.** No real credential ever enters the box. The
model key, service tokens, and bot tokens live in a host-side vault; the box
carries structurally-valid sentinels, and the gateway swaps in the real
value in transit — only on requests a rule allows, only toward the hosts
that rule names. Leaking the box's entire environment leaks placeholders.

**Default deny.** The gateway terminates TLS with a host-minted CA (the CA
private key never enters the box) and judges the full request — method,
host, path, headers, body size, even the MCP tool being called. No matching
grant means deny. A broken policy file means deny everything. An unanswered
approval means deny. The floor is always closed.

**Un-spoofable identity.** Each run gets a single-use random token as its
proxy credential; the gateway resolves it to the worker's subject on the host
side. A box holds only its own token, so it cannot claim another worker's
identity, budgets, or grants — no matter what its payloads say.

**Budgets as ledgers.** Every governed call is metered and debited against
append-only ledgers with per-worker and org-wide caps. Over a cap, the gateway
refuses (a legible 402, not a hang), and the counters survive restarts by
replaying the ledger. A worker can propose pace; it can never touch money —
caps live in admin-only config off the box mount.

**Propose, never act.** Consequential actions (send, post, push, edit
identity) exist inside the box only as proposals. The executors holding real
credentials run on the trusted side, and the box has no egress grant to any
write host — so the worst a compromised box can produce is spurious
proposals, each held for a human unless trust was explicitly earned. What an
worker *is* (its identity file) always requires a human, with no promotion
path.

**Provenance guards the stores.** What the worker learns about the world lives
in a git repository the box never sees — runs get a plain checkout, and the
trusted side validates and commits. Runs that contained conversation content
get that checkout read-only: person-data cannot be laundered into the
hard-to-erase world store, because the mount — not a text filter — enforces
the boundary. See [knowledge.md](knowledge.md).

**Everything on the record.** Every gateway decision, action disposition,
credential refresh, and spend line appends to audit logs that nothing
rewrites. Every model input is traced byte-exactly before it's sent. "What
did the worker see, ask for, and cost — and who approved it?" is always
answerable from disk.

## The invariants

Violating any of these is a bug, not a tradeoff:

1. Nothing the worker ingests is trusted — including notes it wrote itself.
2. No secrets in any box, spec, or container image; injection in transit only.
3. No egress except through the gateway.
4. Code and config mount read-only; a worker cannot edit its rules, tools, or
   spec.
5. No matching rule ⇒ deny. Broken config ⇒ deny. Unanswered gate ⇒ nothing
   happens.
6. The worker proposes direction and pace — never capability, money, or trust.
7. Channels relay; they never act. Messages are content, never commands,
   no matter who sends them.
8. Budgets gate proactive dispatch only; work a human filed always runs.
9. Journals, audit logs, and decision records are append-only.
10. Enforcement never lives in a box extension; the box's view (journal,
    briefings) is never the enforcement state.
11. Run identity comes from the host-minted token, never from anything the
    box says.

## Honest limits

Security claims are worth exactly what their caveats admit:

- **The vault is plain JSON on disk** (mode 0600), not an encrypted store or
  OS keychain. Host compromise is game over — which is true of any design,
  but there's no at-rest encryption layer yet.
- **Token-level spend metering isn't implemented.** Budgets meter what the
  gateway observes directly — request counts, bytes, per-call prices — which
  are un-falsifiable by construction. Per-token cost from response bodies is
  not yet counted, so cap model calls by count, not tokens.
- **The participant scan is a tripwire, not a wall.** It catches names, ids,
  and handles crossing from conversations toward the knowledge store;
  paraphrase gets past it. The hard guarantee is the read-only mount on
  tainted runs, not the scan.
- **`tunnel` rules trade visibility for compatibility.** A host you tunnel
  (for cert-pinning clients) is judged on host and port only — the gateway
  can't see inside. Use them knowingly.
- **Single-org.** Multi-tenant isolation is prepared for in the scope model
  but not yet built or adversarially tested.
